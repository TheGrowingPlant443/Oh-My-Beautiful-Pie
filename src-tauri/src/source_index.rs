//! On-demand, evidence-preserving repository source index.
//!
//! Jobs retain immutable app-owned source snapshots; parser facts are validated against the complete
//! inventory before queries or system visualizations can use them. Repository content is never executed.
mod inventory;
mod model;
mod query;
mod runner;
mod source_index_service;

pub use model::{
    FileStatus, Resolution, SourceGraphIndex, SourceIndexCapabilities, SourceIndexSelection,
    SourceIndexState, SourceIndexStatus,
};
#[cfg(test)]
use model::{ProfileCapability, RuntimeCapability};
#[cfg(test)]
use query::{Direction, GroupBy, Level};
pub use query::{SourceIndexPage, SourceIndexQuery};
pub use source_index_service::SourceIndexService;
