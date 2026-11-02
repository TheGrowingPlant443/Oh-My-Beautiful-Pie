//! Passive Cognee component inspection and the explicitly launched, app-owned local frontend.
#[path = "cognee_components.rs"]
mod components;
#[cfg(any(windows, unix))]
#[path = "cognee_live.rs"]
mod live;
#[cfg(unix)]
#[path = "cognee_ui_unix.rs"]
pub(crate) mod native;
#[cfg(windows)]
#[path = "cognee_ui_windows.rs"]
pub(crate) mod native;

pub(crate) use components::capability;
#[cfg(all(windows, test))]
pub(crate) use components::Backend;
#[cfg(not(any(windows, unix)))]
pub(crate) use components::CogneeUi;
#[cfg(any(windows, unix))]
pub(crate) use live::CogneeUi;
#[cfg(all(windows, test))]
pub(crate) use live::{verify_backends, Hooks, Listener, Row};
