//! Container runtime adapters and the shared engine contract.
mod cli_engine;
#[cfg(unix)]
pub(crate) mod native;

pub use cli_engine::*;
