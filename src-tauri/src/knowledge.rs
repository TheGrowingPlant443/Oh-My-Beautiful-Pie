//! Knowledge backend: human-registered Markdown vaults, a safe in-process vault reader/index, passive Obsidian,
//! Graphify and Cognee discovery, immutable Graphify admission and the explicit, app-owned Cognee UI launcher.
//!
//! Only a human picker/configuration registers a vault root (`prepare_knowledge_vault` → `confirm_knowledge_vault`);
//! no model or tool parameter can grant path access. Probes (`get_knowledge_capabilities`) never launch an
//! application, run a CLI, write another product's settings or copy notes. Vaults hold app-authored Markdown only;
//! private state (bindings, tokens, indexes, Graphify snapshots) lives under `<app-config>/knowledge/`.
//!
//! Layout: `fsio` (rooted, hardlink-safe IO), `markdown` (line scanner), `index` (link index/queries), `vault`
//! (registration, listing, document pages, writers), `integrations` (Obsidian/Cognee discovery), `cognee_ui` (the
//! explicit Cognee frontend launcher), `graphify`, `dto`.
mod cognee_ui;
#[cfg(all(test, windows, target_arch = "x86_64"))]
mod cognee_ui_regression;
mod dto;
pub(crate) mod fsio;
mod graphify;
#[cfg(test)]
mod graphify_regression;
mod index;
#[cfg(test)]
mod integration_regression;
mod integrations;
mod knowledge_service;
mod markdown;
mod note_publication;
mod publication_journal;
#[cfg(test)]
mod publication_recovery_regression;
mod rooted;
#[cfg(unix)]
mod rooted_unix;
#[cfg(test)]
mod tests;
mod vault;

pub use dto::*;
pub use graphify::{GraphifyArtifact, GraphifyArtifactCheck};
pub use knowledge_service::KnowledgeService;
pub(crate) use knowledge_service::{Result, StoredVault};
pub use vault::{VaultRoot, WritePrecondition, WriteReceipt};
