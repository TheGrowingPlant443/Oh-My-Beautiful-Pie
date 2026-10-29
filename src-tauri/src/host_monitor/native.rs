//! Native read-only collection. Windows uses ToolHelp and owner-PID socket tables; Linux reads `/proc`.
//! macOS uses libproc metadata and a bounded `/usr/sbin/lsof -nP` scan. No privileges are requested, no DNS/network
//! traffic is generated, and inaccessible evidence is reported as partial/unavailable. Unsupported targets expose
//! no live evidence.
#[cfg(target_os = "linux")]
pub use super::native_linux::{capture_identity, collect};
#[cfg(target_os = "macos")]
pub use super::native_macos_platform::{capture_identity, collect};
#[cfg(windows)]
pub use super::native_windows::{capture_identity, collect};

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn capture_identity(_pid: u32) -> Option<super::types::ProcessIdentity> {
    None
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn collect(
    _cancel: &std::sync::atomic::AtomicBool,
) -> Result<super::snapshot::RawCollection, String> {
    Err("unsupported_platform".into())
}
