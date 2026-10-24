//! Serializable contract shared with `src/lib/code-quality.ts`. Every DTO is camelCase on the wire.
//! Secrets never appear here: tokens/passwords live in the OS credential store (`secrets.rs`):
//! Windows Credential Manager, the freedesktop Secret Service on Linux, the Keychain on macOS.
use serde::{Deserialize, Serialize};

pub type Result<T, E = String> = std::result::Result<T, E>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QualityMode {
    SonarqubeCommunity,
    SonarqubeEnterprise,
    Alternate,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum QualityRuntime {
    DockerDesktop,
    DockerWsl { distro: String },
    /// Docker Engine running natively on this (Linux) host. Never available on Windows or macOS.
    DockerNative,
    Podman { connection: String },
}

impl QualityRuntime {
    /// Stable identity string used for credential targets, resource names and ownership records.
    pub fn key(&self) -> String {
        match self {
            QualityRuntime::DockerDesktop => "dockerDesktop".into(),
            QualityRuntime::DockerWsl { distro } => format!("dockerWsl:{distro}"),
            QualityRuntime::DockerNative => "dockerNative".into(),
            QualityRuntime::Podman { connection } => format!("podman:{connection}"),
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            QualityRuntime::DockerDesktop => "Docker Desktop",
            QualityRuntime::DockerWsl { .. } => "Docker in WSL",
            QualityRuntime::DockerNative => "Docker",
            QualityRuntime::Podman { .. } => "Podman",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QualityConfig {
    pub version: u32,
    pub mode: QualityMode,
    pub runtime: QualityRuntime,
    pub community_port: u16,
    pub enterprise_url: String,
}

impl Default for QualityConfig {
    fn default() -> Self {
        QualityConfig {
            version: 1,
            mode: QualityMode::SonarqubeCommunity,
            runtime: QualityRuntime::Podman { connection: String::new() },
            community_port: 9000,
            enterprise_url: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QualityPhase {
    Idle,
    Checking,
    Installing,
    Pulling,
    Stopping,
    Starting,
    WaitingForReady,
    Validating,
    Scanning,
    Cancelling,
    NeedsSetup,
    NeedsRestart,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CredentialTarget {
    Community,
    Enterprise,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CredentialAction {
    Replace,
    Clear,
}

/// Incoming credential change. `Debug` is redacted; the token is never serialized back out.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QualityCredentialUpdate {
    pub target: CredentialTarget,
    pub action: CredentialAction,
    #[serde(default)]
    pub token: Option<String>,
}

impl std::fmt::Debug for QualityCredentialUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QualityCredentialUpdate")
            .field("target", &self.target)
            .field("action", &self.action)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommunityState {
    pub running: Option<bool>,
    pub url: Option<String>,
    pub runtime_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CredentialPresence {
    pub community: bool,
    pub enterprise: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RulesState {
    pub manifest_hash: String,
    pub license_hash: String,
    pub acknowledged: bool,
    pub license_text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Prerequisite {
    pub runtime: QualityRuntime,
    pub installed: bool,
    pub ready: bool,
    pub needs_restart: bool,
    pub message: Option<String>,
}

/// Actual choices discovered on this machine (read-only probes).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QualityOptions {
    pub wsl_distros: Vec<String>,
    pub podman_connections: Vec<String>,
    pub winget_available: bool,
    /// The app-provisioned Podman machine connection once its setup receipt exists.
    pub podman_app_connection: Option<String>,
    /// `wsl.exe --status` reports that WSL's Windows optional components are missing.
    pub wsl_features_missing: bool,
}


#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SetupStepState {
    Done,
    Pending,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupStep {
    pub id: String,
    pub label: String,
    pub state: SetupStepState,
}

/// What the one-click "Install and setup" still has to do, derived from cached probes and state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupState {
    pub needed: bool,
    pub install_required: bool,
    pub resume: bool,
    pub steps: Vec<SetupStep>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OwnershipStatus {
    Owned,
    OtherProcess,
    Transferring,
    HandedOff,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityOwnership {
    pub status: OwnershipStatus,
    pub owner_pid: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QualityState {
    pub revision: u64,
    pub config: QualityConfig,
    pub activated: bool,
    pub ownership: QualityOwnership,
    pub suspended: bool,
    pub effective_mode: Option<QualityMode>,
    pub phase: QualityPhase,
    pub busy: bool,
    pub operation_id: Option<String>,
    pub requested_config: Option<QualityConfig>,
    pub error: Option<String>,
    pub community: CommunityState,
    pub credentials: CredentialPresence,
    pub rules: RulesState,
    pub prerequisites: Vec<Prerequisite>,
    pub options: QualityOptions,
    pub active_scan_id: Option<String>,
    /// Bounded recent progress/diagnostic lines for the setup UI (secrets redacted).
    pub diagnostics: Vec<String>,
    /// Resources whose stop could not be confirmed; non-empty blocks new Community starts and scans.
    pub unresolved: Vec<String>,
    pub setup: SetupState,
    /// Whether the managed analyzer is started when OMP Manager starts (default false).
    pub start_at_launch: bool,
    /// `windows`, `linux` or `macos`: decides which runtimes and setup actions the UI offers.
    pub platform: String,
    /// Where Sonar tokens and generated passwords are kept and whether that store works right now.
    pub credential_store: CredentialStoreStatus,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CredentialStoreStatus {
    /// Human-readable backend name, e.g. "Windows Credential Manager" or "Secret Service (secret-tool)".
    pub name: String,
    pub available: bool,
    /// Why it is unavailable and what to do about it; `None` while available.
    pub message: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scanner {
    Cli,
    Maven,
    Gradle,
    Dotnet,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QualityProject {
    pub folder: String,
    pub project_key: String,
    pub scanner: Scanner,
    pub entry_file: Option<String>,
    pub image: Option<String>,
    pub build_args: Vec<String>,
    pub prepare_npm_dependencies: bool,
    pub exclusions: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QualityScanStatus {
    Running,
    Cancelling,
    Cancelled,
    Complete,
    Partial,
    Error,
    Interrupted,
}

impl QualityScanStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, QualityScanStatus::Running | QualityScanStatus::Cancelling)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolStatus {
    Complete,
    Partial,
    Error,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityToolOutcome {
    pub tool: String,
    pub version: String,
    pub status: ToolStatus,
    pub exit_code: Option<i64>,
    pub analyzed_files: Option<u64>,
    pub finding_count: u64,
    pub error: Option<String>,
    pub report_names: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    Info,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityFinding {
    pub id: String,
    pub tools: Vec<String>,
    pub rule_id: String,
    pub category: String,
    pub severity: Severity,
    pub message: String,
    pub path: Option<String>,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub end_line: Option<u32>,
    pub help_url: Option<String>,
    pub hotspot: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QualityGate {
    Passed,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityScan {
    pub id: String,
    pub folder: String,
    pub config: QualityConfig,
    pub project: QualityProject,
    pub status: QualityScanStatus,
    pub phase: String,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub snapshot_hash: String,
    pub rules_hash: String,
    pub tools: Vec<QualityToolOutcome>,
    pub findings: Vec<QualityFinding>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
    pub analysis_id: Option<String>,
    pub dashboard_url: Option<String>,
    pub quality_gate: Option<QualityGate>,
    /// Bounded human-readable stage log shown while scanning.
    #[serde(default)]
    pub log: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityScanPreparation {
    pub project: QualityProject,
    pub recipe: Vec<String>,
    pub recipe_hash: String,
    pub requires_consent: bool,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanAdmission {
    pub scan_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportPreview {
    pub text: String,
    pub truncated: bool,
}

/// One slice of a saved report artifact. `next_offset` is `None` at the end of the file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportSlice {
    pub text: String,
    pub next_offset: Option<u64>,
    pub total: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentReportSection {
    Scans,
    Summary,
    Findings,
    Artifact,
}

/// Arguments of the agents' read-only `code_quality_report` tool. There is deliberately no folder, instance,
/// path or export field: the repository is derived from the authenticated agent, never from the request.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentReportRequest {
    pub section: AgentReportSection,
    #[serde(default)]
    pub scan_id: Option<String>,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub severity: Option<Severity>,
    #[serde(default)]
    pub report_name: Option<String>,
}

pub const BUSY_MESSAGE: &str = "Code quality is busy; wait for the current operation to finish.";
pub const STALE_MESSAGE: &str = "Code quality settings changed; reload and try again.";
pub const RULES_MESSAGE: &str = "Review and acknowledge the code quality rules license first.";

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_community_on_podman() {
        let c = QualityConfig::default();
        assert_eq!(c.mode, QualityMode::SonarqubeCommunity);
        assert_eq!(c.runtime, QualityRuntime::Podman { connection: String::new() });
        assert_eq!(c.community_port, 9000);
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["mode"], "sonarqubeCommunity");
        assert_eq!(json["runtime"]["kind"], "podman");
        assert_eq!(json["communityPort"], 9000);
    }

    #[test]
    fn conflicting_edition_flags_are_rejected() {
        let text = r#"{"version":1,"mode":"sonarqubeCommunity","enterprise":true,"runtime":{"kind":"dockerDesktop"},"communityPort":9000,"enterpriseUrl":""}"#;
        assert!(serde_json::from_str::<QualityConfig>(text).is_err());
        let text = r#"{"version":1,"mode":"both","runtime":{"kind":"dockerDesktop"},"communityPort":9000,"enterpriseUrl":""}"#;
        assert!(serde_json::from_str::<QualityConfig>(text).is_err());
    }

    #[test]
    fn credential_debug_hides_the_token() {
        let update = QualityCredentialUpdate { target: CredentialTarget::Enterprise, action: CredentialAction::Replace, token: Some("squ_secret".into()) };
        assert!(!format!("{update:?}").contains("squ_secret"));
    }

    #[test]
    fn wsl_runtime_roundtrips_with_distro() {
        let rt: QualityRuntime = serde_json::from_str(r#"{"kind":"dockerWsl","distro":"Ubuntu"}"#).unwrap();
        assert_eq!(rt, QualityRuntime::DockerWsl { distro: "Ubuntu".into() });
        assert_eq!(rt.key(), "dockerWsl:Ubuntu");
    }
}

/// Result of inspecting a folder's root manifests; `project` is the saved one when it exists,
/// otherwise the detected default (explicit `sonar.projectKey` honoured, else `omp-<sha256(folder)[..20]>`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityProjectDetection {
    pub project: QualityProject,
    /// Every build family found at the root (empty means plain CLI analysis).
    pub families: Vec<Scanner>,
    /// Candidate .NET entry files (.sln/.slnx/.csproj/.vbproj) relative to the folder.
    pub entry_files: Vec<String>,
    pub explicit_key: bool,
    /// True when mixed families or several .NET entry points require the user to choose.
    pub needs_selection: bool,
    pub warnings: Vec<String>,
    /// Human-readable list of what was detected at the root (e.g. "Node.js / TypeScript (package.json)").
    pub labels: Vec<String>,
}
