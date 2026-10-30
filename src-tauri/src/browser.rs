//! Browser supervision and per-profile native process ownership.
mod browser_supervisor;
#[cfg(any(windows, unix))]
mod native;

#[cfg(windows)]
pub use browser_supervisor::EMBEDDING_ERROR;
pub use browser_supervisor::{BrowserInfo, BrowserRect, BrowserSettings, BrowserSupervisor};
