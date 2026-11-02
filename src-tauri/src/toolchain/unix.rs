use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Searched after `PATH`, in this order.
pub(super) fn fallback_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = home {
        for relative in [".local/bin", ".bun/bin", ".nix-profile/bin", ".cargo/bin"] {
            dirs.push(home.join(relative));
        }
    }
    for absolute in [
        "/usr/local/bin",
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/nix/var/nix/profiles/default/bin",
        "/run/current-system/sw/bin",
        "/usr/bin",
        "/bin",
    ] {
        dirs.push(PathBuf::from(absolute));
    }
    dirs
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// First executable regular file called `name` in `path_variable`'s directories, then in `extra`.
/// Relative and empty `PATH` entries are skipped: the current directory is never searched.
pub(super) fn find_in(
    name: &str,
    path_variable: Option<&OsStr>,
    extra: &[PathBuf],
) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') {
        return None;
    }
    let from_path = path_variable
        .map(|value| std::env::split_paths(value).collect::<Vec<_>>())
        .unwrap_or_default();
    from_path
        .iter()
        .chain(extra.iter())
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

/// An executable called `name` on `PATH` or in the per-user/machine-wide fallback directories.
pub fn find_program(name: &str) -> Option<PathBuf> {
    find_in(
        name,
        std::env::var_os("PATH").as_deref(),
        &fallback_dirs(home().as_deref()),
    )
}

/// Appends the existing fallback directories that `PATH` lacks, so every `Command::new("git")`-style lookup in
/// the app behaves the same whether the app was started from a shell or from a desktop launcher. Nothing is
/// prepended: tools the user's `PATH` already provides keep priority. Call before any other thread starts.
pub fn extend_path_for_desktop_launch() {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut entries: Vec<PathBuf> = std::env::split_paths(&current).collect();
    let mut changed = false;
    for dir in fallback_dirs(home().as_deref()) {
        if dir.is_dir() && !entries.contains(&dir) {
            entries.push(dir);
            changed = true;
        }
    }
    if changed {
        if let Ok(joined) = std::env::join_paths(entries) {
            std::env::set_var("PATH", joined);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::slice;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("omp-toolchain-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn tool(dir: &Path, name: &str, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn path_wins_over_fallbacks_and_non_executables_are_skipped() {
        let first = scratch("first");
        let second = scratch("second");
        let fallback = scratch("fallback");
        tool(&first, "omp", 0o644);
        let expected = tool(&second, "omp", 0o755);
        tool(&fallback, "omp", 0o755);
        let path = std::env::join_paths([&first, &second]).unwrap();
        assert_eq!(
            find_in("omp", Some(&path), slice::from_ref(&fallback)),
            Some(expected)
        );
        let only_fallback = std::env::join_paths([&first]).unwrap();
        assert_eq!(
            find_in("omp", Some(&only_fallback), slice::from_ref(&fallback)),
            Some(fallback.join("omp"))
        );
        for dir in [first, second, fallback] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn directories_relative_entries_and_path_like_names_never_match() {
        let dir = scratch("odd");
        std::fs::create_dir(dir.join("omp")).unwrap();
        tool(&dir, "bun", 0o755);
        let path = std::env::join_paths([&dir]).unwrap();
        assert_eq!(
            find_in("omp", Some(&path), &[]),
            None,
            "a directory is not a program"
        );
        assert_eq!(find_in("../bun", Some(&path), &[]), None);
        assert_eq!(find_in("", Some(&path), &[]), None);
        assert_eq!(
            find_in(
                "bun",
                Some(OsStr::new("relative/dir::")),
                &[PathBuf::from("relative")]
            ),
            None
        );
        assert_eq!(
            find_in("bun", None, slice::from_ref(&dir)),
            Some(dir.join("bun"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fallbacks_cover_the_per_user_and_homebrew_locations() {
        let dirs = fallback_dirs(Some(Path::new("/home/someone")));
        for expected in [
            "/home/someone/.local/bin",
            "/home/someone/.bun/bin",
            "/home/someone/.nix-profile/bin",
            "/opt/homebrew/bin",
            "/usr/local/bin",
        ] {
            assert!(
                dirs.iter().any(|dir| dir == Path::new(expected)),
                "{expected}"
            );
        }
        assert!(fallback_dirs(None).iter().all(|dir| dir.is_absolute()));
    }
}
