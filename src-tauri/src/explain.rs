//! One-line explanations of a merged pull request, a commit, or the difference between two commits.
//!
//! The material is plain Git output (no external diff, no textconv). The model is asked once, headless, with no
//! tools and no extensions, so nothing can be changed and nothing else is loaded.
use crate::code_diff;
use crate::instance::{Origin, Registry};
use std::ffi::OsStr;
use std::fs;
use std::iter::Peekable;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::Chars;
use std::time::Duration;

pub type Result<T, E = String> = std::result::Result<T, E>;

const GIT_SECS: u64 = 30;
const MODEL_SECS: u64 = 150;
const PATCH_LIMIT: usize = 60_000;
const STAT_LIMIT: usize = 20_000;
const ANSWER_CHARS: usize = 400;
const NOT_FOUND: &str = "The omp command was not found; add it to PATH or set OMP_BIN";

/// The first `max` bytes of `text`, cut at a character boundary, with `note` appended when something was cut.
fn cut(mut text: String, max: usize, note: &str) -> String {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(note);
    text
}

fn git_text(root: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&code_diff::git_timed(root, args, None, GIT_SECS)?).into_owned())
}

/// Stat and patch of `from..to`; a root commit (`from == None`) is shown as everything it added.
fn changes(root: &Path, from: Option<&str>, to: &str) -> Result<String> {
    let run = |flag: &str| match from {
        Some(from) => git_text(
            root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "-M",
                flag,
                from,
                to,
            ],
        ),
        None => git_text(
            root,
            &[
                "show",
                "--no-ext-diff",
                "--no-textconv",
                "-M",
                "--format=",
                flag,
                to,
            ],
        ),
    };
    let stat = cut(run("--stat")?, STAT_LIMIT, "\n[stat truncated]");
    let patch = cut(run("--patch")?, PATCH_LIMIT, "\n[patch truncated]");
    Ok(format!("## Files changed\n{stat}\n\n## Patch\n{patch}\n"))
}

fn header(root: &Path, commit: &str) -> Result<String> {
    git_text(
        root,
        &["log", "-1", "--format=%H%n%an%n%aI%n%s%n%n%b", commit],
    )
}

fn parents(root: &Path, commit: &str) -> Result<Vec<String>> {
    let line = git_text(root, &["rev-list", "--parents", "-n", "1", commit])?;
    Ok(line.split_whitespace().skip(1).map(String::from).collect())
}

/// The text the model reads. `kind` is `pr` (a merge commit: what it brought into its first parent), `commit`
/// or `range` (`base` is the older commit, `target` the newer one).
pub fn material(root: &Path, kind: &str, base: Option<&str>, target: &str) -> Result<String> {
    code_diff::valid_commit(root, target)?;
    match (kind, base) {
        ("range", Some(base)) => {
            code_diff::valid_commit(root, base)?;
            let log = git_text(
                root,
                &[
                    "log",
                    "--no-merges",
                    "-n",
                    "100",
                    "--format=%h %an: %s",
                    &format!("{base}..{target}"),
                ],
            )?;
            Ok(format!("Difference between two commits, {base} to {target}.\n\n## Commits in between\n{log}\n{}", changes(root, Some(base), target)?))
        }
        ("range", None) => Err("A commit range needs two commits".into()),
        (_, Some(_)) => Err("Only a commit range takes a base commit".into()),
        ("pr", None) => {
            let parents = parents(root, target)?;
            if parents.len() < 2 {
                return Err("Not a merge commit".into());
            }
            let log = git_text(
                root,
                &[
                    "log",
                    "--no-merges",
                    "-n",
                    "100",
                    "--format=%h %an: %s",
                    &format!("{}..{target}", parents[0]),
                ],
            )?;
            Ok(format!(
                "Merged pull request, merge commit:\n{}\n## Commits brought in\n{log}\n{}",
                header(root, target)?,
                changes(root, Some(&parents[0]), target)?
            ))
        }
        ("commit", None) => {
            let parents = parents(root, target)?;
            Ok(format!(
                "Commit:\n{}\n{}",
                header(root, target)?,
                changes(root, parents.first().map(String::as_str), target)?
            ))
        }
        _ => Err(format!("Unknown explanation kind: {kind}")),
    }
}

fn instruction(kind: &str) -> &'static str {
    match kind {
        "pr" => "Write exactly ONE sentence (at most 25 words) saying what this pull request did, based on the attached material. Plain text only: no markdown, no quotes, no preamble.",
        "commit" => "Write exactly ONE sentence (at most 25 words) saying what this commit did, based on the attached material. Plain text only: no markdown, no quotes, no preamble.",
        _ => "Write exactly ONE sentence (at most 25 words) saying what changed between these two commits (the attached diff goes from the first to the second), based on the attached material. Plain text only: no markdown, no quotes, no preamble.",
    }
}

/// Consumes the rest of an escape sequence whose ESC byte was just read.
fn skip_escape(chars: &mut Peekable<Chars<'_>>) {
    match chars.next() {
        Some('[') => {
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        }
        Some(']') => {
            while let Some(next) = chars.next() {
                if next == '\u{7}' || (next == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                    break;
                }
            }
        }
        _ => {}
    }
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            skip_escape(&mut chars);
        } else {
            out.push(c);
        }
    }
    out
}

/// Removes the material file however the call ends.
struct Remove(PathBuf);
impl Drop for Remove {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Runs the model once over `material` and returns its first non-empty line.
#[allow(clippy::too_many_arguments)]
pub fn explain_with(
    program: &OsStr,
    folder: &Path,
    profile: &str,
    configs: &[PathBuf],
    model: &str,
    kind: &str,
    material: &str,
    limit: Duration,
) -> Result<String> {
    fs::create_dir_all(folder).map_err(|e| e.to_string())?;
    let file = folder.join(format!("{}.md", uuid::Uuid::new_v4()));
    fs::write(&file, material).map_err(|e| e.to_string())?;
    let material_file = Remove(file);
    let mut cmd = Command::new(program);
    cmd.args([
        "-p",
        "--no-session",
        "--no-tools",
        "--no-extensions",
        "--no-skills",
        "--no-rules",
        "--no-lsp",
        "--no-title",
        "--max-time=120",
        "--cwd",
    ])
    .arg(folder)
    .args([
        "--profile",
        if profile.is_empty() {
            "default"
        } else {
            profile
        },
    ]);
    for config in configs {
        cmd.arg("--config").arg(config);
    }
    if !model.is_empty() {
        cmd.args(["--model", model]);
    }
    cmd.arg(format!("@{}", material_file.0.display()))
        .arg(instruction(kind))
        .current_dir(folder);
    code_diff::no_console(&mut cmd);
    let output = code_diff::run_timed(cmd, None, limit, NOT_FOUND)?;
    if !output.status.success() {
        return Err(code_diff::failure_text(&output));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| strip_ansi(line).trim().to_string())
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(ANSWER_CHARS).collect())
        .ok_or_else(|| "The model returned no explanation".into())
}

pub fn explain(
    registry: &Registry,
    id: &str,
    kind: &str,
    base: Option<&str>,
    target: &str,
) -> Result<String> {
    let row = registry.inspect(id, Origin::Human)?;
    let root = code_diff::repository_root(Path::new(&row.project_path))?;
    let material = material(&root, kind, base, target)?;
    let program = crate::toolchain::omp_program();
    let folder = registry.config_root().join("git-explain");
    explain_with(
        &program,
        &folder,
        &row.profile,
        &row.configs,
        &row.model,
        kind,
        &material,
        Duration::from_secs(MODEL_SECS),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Repo(PathBuf);
    impl Repo {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("omp-explain-{name}-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            let repo = Repo(dir);
            repo.git(&["init", "-q"]);
            repo
        }
        fn git(&self, args: &[&str]) -> String {
            let out = Command::new("git")
                .arg("-C")
                .arg(&self.0)
                .args([
                    "-c",
                    "user.name=Tester",
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.autocrlf=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
        fn commit(&self, file: &str, body: &str, message: &str) -> String {
            fs::write(self.0.join(file), body).unwrap();
            self.git(&["add", "-A"]);
            self.git(&["commit", "-q", "-m", message]);
            self.git(&["rev-parse", "HEAD"])
        }
    }
    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn material_covers_commits_merges_ranges_and_pull_requests() {
        let repo = Repo::new("material");
        let first = repo.commit("a.txt", "one\n", "first");
        let second = repo.commit("a.txt", "one\ntwo\n", "second");
        repo.git(&["checkout", "-q", "-b", "feature"]);
        repo.commit("b.txt", "feature file\n", "add feature");
        repo.git(&["checkout", "-q", "-"]);
        repo.git(&[
            "merge",
            "--no-ff",
            "-q",
            "-m",
            "Merge pull request #1 from x/feature",
            "feature",
        ]);
        let merge = repo.git(&["rev-parse", "HEAD"]);
        let root = repo.0.as_path();

        let root_commit = material(root, "commit", None, &first).unwrap();
        assert!(
            root_commit.contains("first") && root_commit.contains("+one"),
            "a root commit shows what it added: {root_commit}"
        );
        let plain = material(root, "commit", None, &second).unwrap();
        assert!(plain.contains("a.txt") && plain.contains("+two"));
        let range = material(root, "range", Some(&first), &second).unwrap();
        assert!(
            range.contains("second") && range.contains("+two") && !range.contains("+one\n"),
            "the range diff holds only what changed between the two"
        );
        let pr = material(root, "pr", None, &merge).unwrap();
        assert!(
            pr.contains("Merge pull request #1")
                && pr.contains("add feature")
                && pr.contains("b.txt")
                && pr.contains("+feature file")
        );
        let merged_commit = material(root, "commit", None, &merge).unwrap();
        assert!(
            merged_commit.contains("+feature file"),
            "a merge commit is diffed against its first parent"
        );
        assert_eq!(
            material(root, "pr", None, &second).unwrap_err(),
            "Not a merge commit"
        );
        assert!(material(root, "range", None, &second).is_err());
        assert!(material(root, "commit", Some(&first), &second).is_err());
        assert!(material(root, "commit", None, "--output=x").is_err());
        assert!(material(root, "commit", None, &"0".repeat(40)).is_err());
    }

    #[test]
    fn long_patches_are_cut_at_a_character_boundary() {
        let repo = Repo::new("long");
        repo.commit("base.txt", "x\n", "base");
        let big = commit_big(&repo);
        let text = material(repo.0.as_path(), "commit", None, &big).unwrap();
        assert!(text.contains("[patch truncated]"));
        assert!(text.len() < PATCH_LIMIT + STAT_LIMIT + 4_000);
        assert_eq!(cut("é".repeat(10), 5, "…"), "éé…");
        assert_eq!(cut("short".into(), 50, "…"), "short");
    }

    fn commit_big(repo: &Repo) -> String {
        repo.commit("big.txt", &"é line of text\n".repeat(20_000), "big")
    }

    #[test]
    fn answers_are_one_plain_line() {
        assert_eq!(
            strip_ansi("\u{1b}[1mbold\u{1b}[0m and \u{1b}]0;title\u{7}plain"),
            "bold and plain"
        );
    }

    #[cfg(windows)]
    mod stub {
        use super::*;

        fn script(name: &str, body: &str) -> (PathBuf, PathBuf) {
            let dir = std::env::temp_dir()
                .join(format!("omp-explain-stub-{name}-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            let file = dir.join("omp.cmd");
            fs::write(&file, body).unwrap();
            (dir, file)
        }
        fn run(program: &Path, folder: &Path, limit: Duration) -> Result<String> {
            explain_with(
                program.as_os_str(),
                folder,
                "",
                &[PathBuf::from("C:/cfg/a.yml")],
                "m1",
                "pr",
                "material text",
                limit,
            )
        }

        #[test]
        fn the_first_non_empty_line_is_returned_without_escape_codes_and_the_material_file_is_removed(
        ) {
            let (dir, program) = script("ok", "@echo off\r\necho %* > \"%~dp0args.txt\"\r\necho.\r\necho \u{1b}[1mOne line.\u{1b}[0m\r\necho second\r\n");
            let folder = dir.join("work");
            assert_eq!(
                run(&program, &folder, Duration::from_secs(20)).unwrap(),
                "One line."
            );
            let args = fs::read_to_string(dir.join("args.txt")).unwrap();
            for part in [
                "-p",
                "--no-session",
                "--no-tools",
                "--no-extensions",
                "--profile default",
                "--config C:/cfg/a.yml",
                "--model m1",
                "@",
            ] {
                assert!(args.contains(part), "{part} missing from {args}");
            }
            assert_eq!(
                fs::read_dir(&folder).unwrap().count(),
                0,
                "the material file is deleted"
            );
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn failures_and_timeouts_are_reported() {
            let (dir, fail) = script(
                "fail",
                "@echo off\r\necho model unavailable 1>&2\r\nexit /b 3\r\n",
            );
            assert!(run(&fail, &dir.join("work"), Duration::from_secs(20))
                .unwrap_err()
                .contains("model unavailable"));
            let (slow_dir, slow) = script("slow", "@echo off\r\nping -n 20 127.0.0.1 >nul\r\n");
            let started = std::time::Instant::now();
            assert!(run(&slow, &slow_dir.join("work"), Duration::from_secs(1))
                .unwrap_err()
                .contains("Timed out"));
            assert!(started.elapsed() < Duration::from_secs(15));
            assert_eq!(fs::read_dir(slow_dir.join("work")).unwrap().count(), 0);
            let (empty_dir, empty) = script("empty", "@echo off\r\n");
            assert_eq!(
                run(&empty, &empty_dir.join("work"), Duration::from_secs(20)).unwrap_err(),
                "The model returned no explanation"
            );
            // The killed shell's child may still hold its folder for a moment.
            for d in [dir, slow_dir, empty_dir] {
                let _ = fs::remove_dir_all(d);
            }
        }
    }
}
