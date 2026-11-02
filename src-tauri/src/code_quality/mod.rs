//! Code quality service: one authoritative configuration, one operation gate, one scan at a time.
//! Module map: `types` (wire DTOs), `store` (versioned state), `secrets`/`secrets_unix` (OS credential
//! store), `runtime`/`install`/`job` (container engines; `runtime/native` is the Linux/macOS adapter), `sonar`/`scanners`/`snapshot` (SonarQube path),
//! `alternate`/`reports` (MegaLinter + Opengrep + Semgrep CE path), `lifecycle` (configuration and
//! Community server transitions), `scan` (scan jobs), `code_quality_service` (state and operation gate).
pub mod alternate;
#[cfg(unix)]
pub(crate) mod cli;
pub mod credentials;
pub mod install;
pub mod job;
mod lifecycle;
mod ownership;
mod process_lock;
mod report_html;
pub mod reports;
pub mod runtime;
mod scan;
pub mod scanners;
pub mod secrets;
#[cfg(unix)]
pub mod secrets_unix;
pub mod snapshot;
pub mod sonar;
pub mod store;
pub mod types;

mod code_quality_service;
#[cfg(test)]
pub(crate) use code_quality_service::ProbeFn;
pub use code_quality_service::{materialize_bridge, CodeQualityService};
pub(crate) use code_quality_service::{redact_line, ActiveScan, Gate, Inner};

#[cfg(test)]
mod tests;
