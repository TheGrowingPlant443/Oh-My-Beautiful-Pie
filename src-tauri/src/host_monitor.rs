//! Request-driven whole-computer process and socket monitoring with frozen explanation snapshots.
mod explanation;
mod host_monitor_service;
pub(crate) mod native;
#[cfg(target_os = "linux")]
mod native_linux;
#[cfg(any(target_os = "macos", test))]
mod native_macos_parser;
#[cfg(target_os = "macos")]
mod native_macos_platform;
#[cfg(windows)]
mod native_windows;
mod snapshot;
mod tables;
mod types;

pub use explanation::ExplainResult;
pub use host_monitor_service::HostMonitorService;
pub use native::capture_identity;
pub use types::*;

pub(crate) use host_monitor_service::PinnedSnapshot;
