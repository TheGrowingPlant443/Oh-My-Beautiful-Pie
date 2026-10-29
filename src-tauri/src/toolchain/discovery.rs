//! Locating the external programs the app drives (`omp`, `bun`, `code`, `gh`, ...).
//!
//! Windows keeps its original behaviour: an override variable, otherwise the bare name resolved by the OS.
//! On Unix an app started from a desktop launcher (or Finder) inherits a `PATH` that often lacks the places
//! these tools live (`~/.local/bin`, `~/.bun/bin`, `~/.nix-profile/bin`, Homebrew), so after the inherited `PATH`
//! a short fixed list of per-user and machine-wide directories is searched.
use std::ffi::OsString;

/// An override variable wins when set and non-empty; otherwise the program found on this machine, otherwise the
/// bare name, so a missing program fails with the OS's own "not found" and the callers' existing messages.
#[must_use]
pub fn program(override_variable: &str, name: &str) -> OsString {
    if let Some(explicit) = std::env::var_os(override_variable).filter(|value| !value.is_empty()) {
        return explicit;
    }
    #[cfg(unix)]
    if let Some(path) = super::unix::find_program(name) {
        return path.into_os_string();
    }
    name.into()
}

/// `OMP_BIN`, else `omp` from `PATH` or the per-user install locations.
#[must_use]
pub fn omp_program() -> OsString {
    program("OMP_BIN", "omp")
}

/// `BUN_BIN`, else `bun` from `PATH` or the per-user install locations.
#[must_use]
pub fn bun_program() -> OsString {
    program("BUN_BIN", "bun")
}
