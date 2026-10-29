//! Durable, human-admitted SRS jobs. The existing Architect is the only producer; reads never resume generation.
//! Bounded records/chunks are transport limits, never aggregate job budgets. Private journals and evidence stay
//! outside the vault. App close checkpoints and pauses; there is no daemon or detached provider loop.
mod documents;
mod inventory;
mod ledger;
mod query;
mod questions;
mod scheduler;
mod store;
#[cfg(test)]
mod tests;
mod tools;
mod types;
pub use types::{Answer, JobState, Phase};
mod srs_service;
#[cfg(test)]
pub use srs_service::Clock;
pub use srs_service::{SrsLifecycle, SrsService};
pub use store::MAX_RECORD;
pub use types::Result;
