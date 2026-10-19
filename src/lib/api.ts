import { invoke, Channel } from "@tauri-apps/api/core";
import type { Classification, DependencyGraph, DerivedStandard, KnownStandard } from "./dependency-graph";
import type { DependencyJobAdmission, GithubMode, HandoffPrepared, InhouseAdmission, InhouseStatus, PackageInsights, PackageInsightsStatus, ProjectReviewState, SecurityReviewState, UpdatePlanHandoff, UpdatePlanState, UsageExplanationState } from "./package-insights";
import type { KindAdmission, ProjectKindStatus } from "./project-filter";
import type { QualityConfig, QualityCredentialUpdate, QualityLoginDetails, QualityProject, QualityProjectDetection, QualityReportPreview, QualityRuntime, QualityScan, QualityScanPreparation, QualityState } from "./code-quality";

export type InstanceStatus = "stopped" | "starting" | "running" | "error";
export interface WorkspaceChange {
  requestId: string; coordinatorId: string; sourceInstanceId: string | null; targetFolder: string;
  kind: "manual" | "managerHandover" | "managerReturn" | "architectRepo" | "dependencyPlan";
  status: "pending" | "applying" | "applied" | "failed" | "cancelled"; error: string | null;
}
export interface CoordinatorBootstrap {
  manager: Instance;
  architect: Instance;
  temporaryFolders: string[];
}
export interface Subagent {
  id: string;
  name: string;
  task: string | null;
  status: string;
  agent: string | null;
  path: string;
}
export interface Instance {
  id: string;
  name: string;
  projectPath: string;
  profile: string;
  agentFile: string | null;
  configs: string[];
  model: string;
  status: InstanceStatus;
  activity: string;
  task: string | null;
  subagents: Subagent[];
  planPath: string | null;
  planKind: "todo" | "file" | "saved" | null;
  lastPlanHandoff: string | null;
  sessionPath: string | null;
  lastError: string | null;
  todoDone: number;
  todoTotal: number;
  cols: number;
  rows: number;
  generation: number;
  exitCode: number | null;
  kind: "instance" | "manager" | "architect";
  managerId: string | null;
  planTarget: string | null;
  helper: { target: string; purpose: "diff" | "git"; repo: string } | null;
  bridgeReady: boolean;
  capabilities?: { agentHub: boolean; nativeDecisions?: boolean; nativeDecisionsError?: string | null };
  pendingDecisionCount: number;
  humanRequest: { id: string; reason: string; decisionId: string | null } | null;
}
export interface HumanDecisionTarget {
  instanceId: string; requestId: string; generation: number; sessionId: string | null;
  ownershipEpoch: number; workspaceEpoch: number;
}
export interface NativeDecisionOption { label: string; description?: string; preview?: string; }
export interface NativeDecisionQuestion {
  id: string; question: string; options: (string | NativeDecisionOption)[];
  multi?: boolean; recommended?: number; disabledIndices?: number[];
}
export interface NativeDecisionPayload {
  method?: string; title?: string; message?: string; prompt?: { full: string; compact: string };
  options?: (string | NativeDecisionOption)[]; disabledIndices?: number[]; initialIndex?: number; helpText?: string;
  placeholder?: string; prefill?: string; questions?: NativeDecisionQuestion[];
  acceptImages?: boolean; supportsChat?: boolean; timeout?: number; cwd?: string;
}
export interface NativeDecisionImage { type: "image"; data: string; mimeType: string; }
export interface NativeDecisionResult {
  id: string; selectedOptions: string[]; customInput?: string; note?: string;
  customInputImages?: NativeDecisionImage[]; noteImages?: NativeDecisionImage[];
}
export type HumanDecisionResponse = boolean | string | { cancelled: true } | { kind: "chat" }
  | { kind: "submit"; results: NativeDecisionResult[] };
export interface HumanDecision {
  target: HumanDecisionTarget; createdAt: number; instanceName: string; instanceKind: string;
  agentOrigin: { kind: string; id?: string | null; name?: string | null } | null;
  repository: { folder: string | null; label: string; scope: "repository" | "workspace" | "unknown" };
  method: string; payload: NativeDecisionPayload; answerable: boolean; humanOnly: boolean;
}
export interface HumanDecisionSnapshot { revision: number; items: HumanDecision[]; }
export interface PlanBackup { id: string; savedAt: number; text: string; }
export interface SessionInfo {
  id: string;
  path: string;
  title: string;
  modifiedMs: number;
}
export interface CreateInstanceInput {
  name: string;
  projectPath: string;
  profile: string;
  configs: string[];
  model: string;
  resumeSessionId: string | null;
  useGlobalConfig?: boolean;
  agentFile?: string | null;
}
export type PtyChunk = ArrayBuffer;
export interface GlobalConfigInfo { path: string | null; error: string | null; }
export interface ConfigPreview { yaml: string; modelRoles: Record<string, string | null>; omitted: string[]; }
export type SetupScope = "global" | "manager" | "architect" | "repository";
export type SetupKind = "agents" | "rules" | "skill" | "tool" | "workflow" | "plugin" | "profile";
export type ScriptLanguage = "python" | "bash" | "typescript";
export interface SetupItem { kind: SetupKind; name: string; description: string; language?: ScriptLanguage; }
export interface SetupRequest { scope: SetupScope; items: SetupItem[]; projectInstanceId: string | null; }
export interface SetupDraft { scope: SetupScope | null; kinds: SetupKind[]; items: SetupItem[]; step: number; instanceId: string | null; projectInstanceId: string | null; }
export interface PlanSessionAdmission { planner: Instance; promptSent: boolean | null; error: string | null; }
export interface HelperAdmission { helper: Instance; promptSent: boolean | null; error: string | null; }
export interface ClassifyAdmission { architect: Instance | null; promptSent: boolean | null; requested: number; error: string | null; }
export interface SetupPaths { globalOmpFolder: string; globalAgentFolder: string; managerFolder: string; architectFolder: string; }
export interface SetupAdmission { instance: Instance; promptSent: boolean | null; error: string | null; }
export interface SavedConfig { path: string; modifiedMs: number; }
export interface SavedConfigs { current: string | null; snapshots: SavedConfig[]; nativeGlobal: string | null; error: string | null; }
export interface AgentFile { name: string; scope: "global" | "manager" | "architect"; path: string; }
export interface AgentFiles { files: AgentFile[]; error: string | null; }
export interface OmpProfile { name: string; hasLogin: boolean; }
export interface OmpProfiles { profiles: OmpProfile[]; error: string | null; }
export interface BrowserSettings { headless: boolean; embedded: boolean; }
export interface BrowserInfo {
  instanceId: string; generation: number; epoch: number;
  state: "closed" | "opening" | "ready" | "error";
  headless: boolean; embedded: boolean; pid: number | null; error: string | null;
}
export interface BrowserRect { x: number; y: number; width: number; height: number; }
export type DiffMode = "head" | "branch" | "uncommitted" | "commits";
export type DiffGroup = "staged" | "changes";
export type DiffFileAction = "stage" | "unstage" | "revert";
export interface DiffFile { path: string; oldPath: string | null; status: string; group: DiffGroup | null; }
export interface DiffOverview {
  snapshotId: string; root: string; mode: DiffMode; baseRef: string | null; baseCommit: string | null;
  baseLabel: string; branchRef: string | null; currentBranch: string | null; upstream: string | null; files: DiffFile[];
}
export interface DiffContent {
  path: string; original: string | null; modified: string | null; workingHash: string | null;
  eol: "lf" | "crlf"; bom: boolean; readOnlyReason: string | null; originalLabel: string; modifiedLabel: string;
}
export interface CommitResult { commit: string; summary: string; }
export interface PublishOptions { newBranch: string | null; createPr: boolean; }
export interface PublishResult { branch: string; remote: string; prUrl: string | null; steps: string[]; }
export interface GraphCommit { hash: string; parents: string[]; author: string; date: string; subject: string; refs: string[]; }
export interface GitGraph { root: string; head: string | null; mainCommit: string | null; currentBranch: string | null; commits: GraphCommit[]; truncated: boolean; }
export type CommandKind = "bash" | "js" | "py";
export type CommandStatus = "running" | "stopping" | "completed" | "failed" | "cancelled" | "interrupted";
export interface CommandSummary {
  executionId: string; sessionId: string; sourceSessionId: string; toolCallId: string; agentId: string;
  kind: CommandKind; description: string | null; preview: string; cwd: string;
  startedAt: number; endedAt: number | null; status: CommandStatus; exitCode: number | null; tab: number | null; historical: boolean; hasOutput: boolean;
}
export interface CommandOutput { executionId: string; revision: number; text: string; truncated: boolean; }
export interface UserShellInfo { shellId: string; cwd: string; cols: number; rows: number; exited: boolean; exitCode: number | null; }
export interface TerminalOwner {
  instanceId: string; name: string; generation: number; sessionId: string | null; supported: boolean; error: string | null;
  held: boolean; revision: number; active: CommandSummary[]; userShell: UserShellInfo | null;
}
export interface HistoryEntry { instanceId: string; instanceName: string; command: CommandSummary; }
export interface HistoryPage { entries: HistoryEntry[]; nextCursor: string | null; }
export type RequirementCategory = "runtime" | "buildTool" | "packageManager" | "containerEngine" | "osFeature" | "systemSetting" | "executable";
export interface RequirementEvidence { path: string; line?: number; text: string; }
export interface RequirementItem {
  id: string; name: string; category: RequirementCategory; need: "required" | "optional" | "unknown";
  environment: "host" | "wsl" | "container"; evidence: RequirementEvidence[];
  detected: { state: "found" | "missing" | "unknown"; path?: string; version?: string };
  install: { availability: "automatic" | "manual" | "unknown"; via?: string; note: string };
  explanation: string; source: "static" | "architect"; setupLink: "codeQuality" | null;
}
export interface ProjectRequirements { folder: string; items: RequirementItem[]; limitations: string[]; }
export type ManifestStatus = "present" | "missing";
export type VisualizationKind = "systemOverview" | "helicopter" | "structureCurrent" | "structureProposed";
export type ProjectInsightKind = "dependencyDiscovery" | "requirements" | VisualizationKind;
export type ProjectInsightState = "pending" | "submitted" | "completed" | "uncertain" | "failed";
/** An Architect finding from repository files. `verified` is true only when the cited path exists inside the project. */
export interface DiscoveryCitation { path: string; line: number | null; verified?: boolean }
export interface DiscoveredDependency {
  name: string; ecosystem: string | null; declaredVersion: string | null; resolvedVersion: null; relationship: string | null;
  citations: DiscoveryCitation[]; notes: string;
}
export type ProjectInsightResult =
  | { kind: "dependencyDiscovery"; items: DiscoveredDependency[]; searchedAreas: string[]; limitations: string[] }
  | { kind: "requirements"; items: RequirementItem[]; searchedAreas: string[]; limitations: string[] }
  | ({ kind: "systemOverview"; completedAt?: number } & SystemOverviewPayload)
  | ({ kind: "helicopter"; completedAt?: number } & HelicopterPayload)
  | ({ kind: "structureCurrent" | "structureProposed"; completedAt?: number } & StructurePayload);
/** The Architect session a saved request is bound to. */
export interface InsightProducer { instanceId: string; generation: number; launchId: string; sessionId: string }
/** Recorded human selection of a proposed structure preview: a discussion selection, never an implementation authorization. */
export interface StructureApproval {
  approvedAt: number; resultHash: string; sourceHash: string; hasConflicts: boolean;
  delivery: "pending" | "claimed" | "sent" | "uncertain" | "unavailable" | "failed"; deliveryError: string | null;
}
export interface ProjectInsightStatus {
  requestId: string; folder: string; kind: ProjectInsightKind; state: ProjectInsightState; message: string | null; result: ProjectInsightResult | null;
  // Present for saved visualization requests; computed by the backend, never copied from the model.
  createdAt?: number; resultHash?: string | null; snapshotId?: string | null; baseRequestId?: string | null; supersedesRequestId?: string | null;
  supersededBy?: string | null; sourceId?: string | null; sourceHash?: string | null; producer?: InsightProducer | null; approval?: StructureApproval | null;
}
export interface ManifestPreparation { confirmationToken: string; path: string; preview: string }
export interface ManifestCreated { path: string }

// ---- Repository source index (System overview) ----
export type SourceIndexState = "inventory" | "parsing" | "resolving" | "complete" | "partial" | "failed" | "cancelled";
export interface SourceIndexRuntime { source: string; python: string | null; version: string | null; treeSitter: string | null; error: string | null }
export interface SourceIndexProfile { language: string; version: string | null; available: boolean; error: string | null }
export interface SourceIndexCapabilities { available: boolean; probed: boolean; runtime: SourceIndexRuntime; profiles: SourceIndexProfile[]; hint: string | null }
export interface SourceLanguageCoverage { language: string; files: number; parsed: number; partial: number; failed: number; unsupported: number; diagnostics: string[] }
export interface SourceIndexCoverage {
  admittedFiles: number; parsedFiles: number; partialFiles: number; failedFiles: number; skippedFiles: number; unsupportedFiles: number;
  encounteredSites: number; resolvedSites: number; unresolvedSites: number; dynamicSites: number;
  languages: SourceLanguageCoverage[]; lastFullyProcessedFile: string | null; partial: boolean;
}
export interface SourceIndexStatus {
  jobId: string; snapshotId: string | null; folder: string; state: SourceIndexState; indexedAt: number | null; sourceHash: string | null;
  completedFiles: number; totalFiles: number; stale: boolean | null; error: string | null;
  capabilities: SourceIndexCapabilities; coverage: SourceIndexCoverage | null;
}
export interface SourceIndexSelection { selected: SourceIndexStatus | null; latestAttempt: SourceIndexStatus | null }
export type SourceIndexLevel = "files" | "functions";
export type SourceIndexDirection = "in" | "out" | "both";
export type SourceIndexGroupBy = "directory" | "scc";
export type SourceIndexQuery =
  | { kind: "search"; level: SourceIndexLevel; text: string; offset: number; limit: number }
  | { kind: "neighbors"; level: SourceIndexLevel; id: string; direction: SourceIndexDirection; offset: number; limit: number }
  | { kind: "files" | "symbols" | "coverage"; offset: number; limit: number }
  | { kind: "overview"; level: SourceIndexLevel; groupBy: SourceIndexGroupBy; offset: number; limit: number };
export interface SourceIndexPage<T = unknown> { total: number; offset: number; nextOffset: number | null; items: T[] }
export type SourceResolution = "resolvedStatic" | "candidate" | "dynamic" | "external" | "unresolved" | "unsupported";
export type SourceFileStatus = "parsed" | "partial" | "failed" | "skipped" | "unsupported";
export interface SourceSpan { byteStart: number; byteEnd: number; lineStart: number; lineEnd: number; columnStart: number; columnEnd: number }
export interface SourceFileRow { id: string; path: string; sha256: string; bytes: number; language: string | null; status: SourceFileStatus; diagnostics: string[] }
export interface SourceSymbolRow {
  id: string; fileId: string; path: string; language: string; region: string | null; ownerId: string | null; kind: string;
  name: string; qualifiedName: string; span: SourceSpan; exported?: boolean;
}
export interface SourceSiteRow {
  id: string; fileId: string; path: string; language: string; region: string | null; callerId: string | null; span: SourceSpan; relation: string;
  raw: string | null; targetId: string | null; resolution: SourceResolution; conditional: boolean; typeOnly: boolean; candidateIds: string[];
}
export interface SourceEdgeRow { id: string; sourceId: string; targetId: string; relation: string; resolution: SourceResolution; evidenceIds: string[] }
/** One aggregated arrow of an overview group: its `count` is the total, `edgeIds` a bounded sample of the underlying source edges. */
export interface OverviewLinkRow {
  targetGroupId: string; targetLabel: string; relation: string; resolution: SourceResolution; derived: boolean;
  count: number; edgeIds: string[]; edgeIdsTruncated: boolean;
}
export interface OverviewGroupRow {
  id: string; kind: "directory" | "scc"; level: SourceIndexLevel; label: string; path?: string; parentId?: string; layer?: number; cyclic: boolean;
  memberCount: number; members: string[]; membersTruncated: boolean; internalEdges: number; linksTotal: number; linksTruncated: boolean; links: OverviewLinkRow[];
}
export type NeighborRow =
  | { kind: "edge"; direction: "in" | "out"; self: boolean; peerId: string; peerLabel: string | null; edge: SourceEdgeRow; sites: SourceSiteRow[]; sitesTruncated: boolean }
  | { kind: "aggregate"; direction: "in" | "out"; peerId: string | null; peerLabel: string; relation: string; resolution: SourceResolution; count: number; edgeIds: string[]; edgeIdsTruncated: boolean }
  | { kind: "site"; direction: "out"; site: SourceSiteRow };
export type CoverageRow =
  | ({ kind: "summary" } & Omit<SourceIndexCoverage, "languages">)
  | ({ kind: "language" } & SourceLanguageCoverage)
  | { kind: "file"; fileId: string; path: string; language: string | null; status: SourceFileStatus; diagnostics: string[] }
  | { kind: "limitation"; text: string };

// ---- Architect system/structure visualizations ----
export type SystemNodeKind = "application" | "logicGroup" | "actor" | "service" | "sidecar" | "cache" | "database" | "queue" | "storage" | "external" | "entrypoint";
export type SystemRelation = "calls" | "imports" | "includes" | "reexports" | "references" | "renders" | "requests" | "responds" | "publishes" | "subscribes" | "reads" | "writes" | "authorizes";
export type SystemCertainty = "declared" | "inferred" | "unknown";
export interface EvidenceRef { id: string; path: string; lineStart: number; lineEnd: number; contentHash: string }
export interface SystemNodePayload { id: string; kind: SystemNodeKind; label: string; parentId: string | null; description: string; evidenceIds: string[] }
export interface SystemEdgePayload { id: string; sourceId: string; targetId: string; relation: SystemRelation; label: string; evidenceIds: string[]; certainty: SystemCertainty; staticFactId: string | null }
export interface SystemCoveragePayload { area: string; status: "inspected" | "notFound" | "unresolved" | "excluded"; evidenceIds: string[]; notes: string }
export interface SystemOverviewPayload {
  schemaVersion: 1; snapshotId: string; nodes: SystemNodePayload[]; edges: SystemEdgePayload[]; evidence: EvidenceRef[]; coverage: SystemCoveragePayload[]; limitations: string[];
}
export type HelicopterKind = "actor" | "trigger" | "activity" | "decision" | "outcome" | "externalParty" | "businessGroup";
export type HelicopterCertainty = "inferred" | "unknown";
export interface HelicopterNodePayload { id: string; kind: HelicopterKind; label: string; parentId: string | null; description: string; evidenceIds: string[] }
export interface HelicopterEdgePayload { id: string; sourceId: string; targetId: string; label: string; evidenceIds: string[]; certainty: HelicopterCertainty }
export interface HelicopterCapabilityPayload { id: string; label: string; description: string; nodeIds: string[]; evidenceIds: string[] }
export interface HelicopterPayload {
  schemaVersion: 1; snapshotId: string; purpose: string; nodes: HelicopterNodePayload[]; edges: HelicopterEdgePayload[];
  capabilities: HelicopterCapabilityPayload[]; evidence: EvidenceRef[]; coverage: SystemCoveragePayload[]; limitations: string[];
}
export type StructureOperation = "unchanged" | "rename" | "move" | "split" | "merge" | "create" | "remove";
export interface StructureEntryPayload { id: string; parentId: string | null; label: string; path: string | null; pattern: string | null; members: string[]; purpose: string; evidenceIds: string[] }
export interface StructureConventionPayload { scope: string; current: string; proposed: string | null; rationale: string; evidenceIds: string[] }
export interface StructureMappingPayload { fromPaths: string[]; toPaths: string[]; operation: StructureOperation; rationale: string }
export interface StructurePayload {
  schemaVersion: 1; snapshotId: string; sourceId: string | null; sourceHash: string | null; baseRequestId: string | null; supersedesRequestId: string | null;
  entries: StructureEntryPayload[]; conventions: StructureConventionPayload[]; mappings: StructureMappingPayload[]; conflicts: string[]; evidence: EvidenceRef[]; limitations: string[];
}
export interface StructureSourceRow { sourceId: string; capturedAt: number; kind: "interactiveMessage" | "interactiveLocalPaste"; excerpt: string; sha256: string; byteLength: number; error: string | null }
export interface StructureSourceList { sessionId: string | null; sources: StructureSourceRow[] }
export interface StructureSourcePage { sourceId: string; sha256: string; text: string; nextOffset: number | null }

export const api = {
  getCodeQualityState: () => invoke<QualityState>("get_code_quality_state"),
  takeOverCodeQuality: (consent: boolean) => invoke<QualityState>("take_over_code_quality", { consent }),
  shutdownCodeQualityServer: () => invoke<QualityState>("shutdown_code_quality_server"),
  revealCodeQualityCredentials: () => invoke<QualityLoginDetails>("reveal_code_quality_credentials"),
  checkCodeQualityPrerequisites: () => invoke<QualityState>("check_code_quality_prerequisites"),
  configureCodeQuality: (config: QualityConfig, expectedRevision: number, credential: QualityCredentialUpdate | null = null) => invoke<QualityState>("configure_code_quality", { config, expectedRevision, credential }),
  installCodeQualityRuntime: (runtime: QualityRuntime, expectedRevision: number, consent: boolean) => invoke<QualityState>("install_code_quality_runtime", { runtime, expectedRevision, consent }),
  enableCodeQualityWindowsFeatures: (expectedRevision: number, consent: boolean) => invoke<QualityState>("enable_code_quality_windows_features", { expectedRevision, consent }),
  startCodeQualityService: (expectedRevision: number) => invoke<QualityState>("start_code_quality_service", { expectedRevision }),
  acknowledgeCodeQualityRules: (manifestHash: string, licenseHash: string, expectedRevision: number, consent: boolean) => invoke<QualityState>("acknowledge_code_quality_rules", { manifestHash, licenseHash, expectedRevision, consent }),
  detectCodeQualityProject: (folder: string) => invoke<QualityProjectDetection>("detect_code_quality_project", { folder }),
  prepareCodeQualityScan: (project: QualityProject) => invoke<QualityScanPreparation>("prepare_code_quality_scan", { project }),
  startCodeQualityScan: (project: QualityProject, recipeHash: string, consent: boolean) => invoke<{ scanId: string }>("start_code_quality_scan", { project, recipeHash, consent }),
  readCodeQualityScan: (scanId: string) => invoke<QualityScan>("read_code_quality_scan", { scanId }),
  listCodeQualityScans: (folder: string) => invoke<QualityScan[]>("list_code_quality_scans", { folder }),
  listCodeQualityProjectFolders: () => invoke<string[]>("list_code_quality_project_folders"),
  cancelCodeQualityScan: (scanId: string) => invoke<QualityScan>("cancel_code_quality_scan", { scanId }),
  readCodeQualityReport: (scanId: string, reportName: string) => invoke<QualityReportPreview>("read_code_quality_report", { scanId, reportName }),
  openCodeQualityDashboard: (scanId: string | null = null) => invoke<void>("open_code_quality_dashboard", { scanId }),
  openCodeQualitySsoLogin: (url: string) => invoke<void>("open_code_quality_sso_login", { url }),
  enterCodeQuality: () => invoke<QualityState>("enter_code_quality"),
  setupCodeQuality: (expectedRevision: number, consent: boolean) => invoke<QualityState>("setup_code_quality", { expectedRevision, consent }),
  setCodeQualityStartAtLaunch: (enabled: boolean) => invoke<QualityState>("set_code_quality_start_at_launch", { enabled }),
  saveCodeQualityReport: (scanId: string, path: string) => invoke<void>("save_code_quality_report", { scanId, path }),
  listInstances: () => invoke<Instance[]>("list_instances"),
  configureBrowser: (settings: BrowserSettings) => invoke<void>("configure_browser", { settings }),
  listBrowsers: () => invoke<BrowserInfo[]>("list_browsers"),
  openSessionBrowser: (id: string, generation: number, restartHeadless: boolean) => invoke<BrowserInfo>("open_session_browser", { id, generation, restartHeadless }),
  getCodeDiff: (id: string, mode: DiffMode) => invoke<DiffOverview>("get_code_diff", { id, mode }),
  readCodeDiffFile: (id: string, snapshotId: string, path: string, group: DiffGroup | null) => invoke<DiffContent>("read_code_diff_file", { id, snapshotId, path, group }),
  saveCodeDiffFile: (id: string, snapshotId: string, path: string, group: DiffGroup | null, expectedHash: string | null, text: string) => invoke<DiffContent>("save_code_diff_file", { id, snapshotId, path, group, expectedHash, text }),
  changeCodeDiffFile: (id: string, snapshotId: string, path: string, group: DiffGroup | null, action: DiffFileAction) => invoke<void>("change_code_diff_file", { id, snapshotId, path, group, action }),
  commitCodeDiff: (id: string, snapshotId: string, message: string) => invoke<CommitResult>("commit_code_diff", { id, snapshotId, message }),
  publishCodeDiff: (id: string, snapshotId: string, options: PublishOptions) => invoke<PublishResult>("publish_code_diff", { id, snapshotId, options }),
  openCodeDiffPullRequest: (id: string) => invoke<void>("open_code_diff_pull_request", { id }),
  getGitGraph: (id: string, limit: number) => invoke<GitGraph>("get_git_graph", { id, limit }),
  getCommitDiff: (id: string, base: string | null, target: string) => invoke<DiffOverview>("get_commit_diff", { id, base, target }),
  revertCommit: (id: string, commit: string) => invoke<string>("revert_commit", { id, commit }),
  cherryPickCommit: (id: string, commit: string) => invoke<string>("cherry_pick_commit", { id, commit }),
  resetLastCommit: (id: string) => invoke<string>("reset_last_commit", { id }),
  createBranchAt: (id: string, name: string, commit: string | null) => invoke<string>("create_branch_at", { id, name, commit }),
  explainGitChange: (id: string, kind: "pr" | "commit" | "range", base: string | null, target: string) => invoke<string>("explain_git_change", { id, kind, base, target }),
  openHelper: (targetId: string, purpose: "diff" | "git", request: "explain" | "review" | null, mode: Exclude<DiffMode, "commits"> | null) => invoke<HelperAdmission>("open_helper", { targetId, purpose, request, mode }),
  scanDependencies: (folder: string) => invoke<DependencyGraph>("scan_dependencies", { folder }),
  getProjectRequirements: (folder: string) => invoke<ProjectRequirements>("get_project_requirements", { folder }),
  getDependencyClassification: (folder: string) => invoke<Classification>("get_dependency_classification", { folder }),
  classifyDependencies: (folder: string) => invoke<ClassifyAdmission>("classify_dependencies", { folder }),
  getPackageInsights: (folder: string, packageId: string, github: GithubMode = "skip", refresh = false) => invoke<PackageInsights>("get_package_insights", { folder, packageId, github, refresh }),
  getPackageInsightsStatus: () => invoke<PackageInsightsStatus>("get_package_insights_status"),
  recheckGithubCredential: () => invoke<PackageInsightsStatus>("recheck_github_credential"),
  getInhouseStatus: (folder: string, rescan: boolean, gate: boolean) => invoke<InhouseStatus>("get_inhouse_status", { folder, rescan, gate }),
  startInhouseSweep: (folder: string) => invoke<InhouseAdmission>("start_inhouse_sweep", { folder }),
  getProjectKind: (folder: string) => invoke<ProjectKindStatus>("get_project_kind", { folder }),
  startProjectKind: (folder: string) => invoke<KindAdmission>("start_project_kind", { folder }),
  getKnownStandard: () => invoke<KnownStandard>("get_known_standard"),
  clearKnownStandard: (language: string | null) => invoke<number>("clear_known_standard", { language }),
  startDependencySecurityReview: (folder: string, packageId: string, rerun = false) => invoke<DependencyJobAdmission>("start_dependency_security_review", { folder, packageId, rerun }),
  readDependencySecurityReview: (requestId: string) => invoke<SecurityReviewState>("read_dependency_security_review", { requestId }),
  startDependencyUsageExplanation: (folder: string, packageId: string, rerun = false) => invoke<DependencyJobAdmission>("start_dependency_usage_explanation", { folder, packageId, rerun }),
  readDependencyUsageExplanation: (requestId: string) => invoke<UsageExplanationState>("read_dependency_usage_explanation", { requestId }),
  showDependencyResultInArchitect: (requestId: string) => invoke<void>("show_dependency_result_in_architect", { requestId }),
  dependencyManifestStatus: (folder: string) => invoke<ManifestStatus>("dependency_manifest_status", { folder }),
  requestProjectInsight: (folder: string, kind: ProjectInsightKind, rerun = false) => invoke<ProjectInsightStatus>("request_project_insight", { folder, kind, rerun }),
  getProjectInsight: (folder: string, kind: ProjectInsightKind) => invoke<ProjectInsightStatus | null>("get_project_insight", { folder, kind }),
  readProjectInsight: (requestId: string) => invoke<ProjectInsightStatus>("read_project_insight", { requestId }),
  requestSystemVisualization: (folder: string, kind: VisualizationKind, snapshotId: string, sourceId: string | null = null, baseRequestId: string | null = null, supersedesRequestId: string | null = null) => invoke<ProjectInsightStatus>("request_system_visualization", { folder, kind, snapshotId, sourceId, baseRequestId, supersedesRequestId }),
  listStructureSources: (architectId: string, expectedGeneration: number) => invoke<StructureSourceList>("list_structure_sources", { architectId, expectedGeneration }),
  readStructureSource: (architectId: string, expectedGeneration: number, sourceId: string, offset: number) => invoke<StructureSourcePage>("read_structure_source", { architectId, expectedGeneration, sourceId, offset }),
  selectLastStructureSource: (architectId: string, expectedGeneration: number) => invoke<StructureSourceRow>("select_last_structure_source", { architectId, expectedGeneration }),
  approveStructurePreview: (requestId: string, expectedResultHash: string, expectedSourceHash: string) => invoke<ProjectInsightStatus>("approve_structure_preview", { requestId, expectedResultHash, expectedSourceHash }),
  getSourceIndex: (folder: string) => invoke<SourceIndexSelection>("get_source_index", { folder }),
  startSourceIndex: (folder: string) => invoke<SourceIndexStatus>("start_source_index", { folder }),
  readSourceIndexJob: (jobId: string) => invoke<SourceIndexStatus>("read_source_index_job", { jobId }),
  cancelSourceIndex: (jobId: string) => invoke<SourceIndexStatus>("cancel_source_index", { jobId }),
  querySourceIndex: <T = unknown>(snapshotId: string, query: SourceIndexQuery) => invoke<SourceIndexPage<T>>("query_source_index", { snapshotId, query }),
  prepareManifestCreation: (requestId: string, ecosystem: string) => invoke<ManifestPreparation>("prepare_manifest_creation", { requestId, ecosystem }),
  confirmManifestCreation: (confirmationToken: string) => invoke<ManifestCreated>("confirm_manifest_creation", { confirmationToken }),
  startProjectDependencySecurityReview: (folder: string, rerun = false) => invoke<DependencyJobAdmission>("start_project_dependency_security_review", { folder, rerun }),
  readProjectDependencySecurityReview: (requestId: string) => invoke<ProjectReviewState>("read_project_dependency_security_review", { requestId }),
  startDependencyUpdatePlan: (folder: string, reviewRequestId: string, regenerate = false) => invoke<DependencyJobAdmission>("start_dependency_update_plan", { folder, reviewRequestId, regenerate }),
  readDependencyUpdatePlan: (requestId: string) => invoke<UpdatePlanState>("read_dependency_update_plan", { requestId }),
  prepareDependencyUpdateHandoff: (requestId: string, revisionId: string) => invoke<HandoffPrepared>("prepare_dependency_update_handoff", { requestId, revisionId }),
  confirmDependencyUpdateHandoff: (confirmationToken: string) => invoke<UpdatePlanHandoff>("confirm_dependency_update_handoff", { confirmationToken }),
  getDerivedStandard: (folder: string) => invoke<DerivedStandard>("get_derived_standard", { folder }),
  setBrowserView: (instanceId: string | null, generation: number | null, rect: BrowserRect | null) => invoke<void>("set_browser_view", { instanceId, generation, rect }),
  createInstance: (config: CreateInstanceInput) => invoke<Instance>("create_instance", { config }),
  ensureCoordinators: (preferredFolders: string[]) => invoke<CoordinatorBootstrap>("ensure_coordinators", { preferredFolders }),
  createManager: (config: CreateInstanceInput) => invoke<Instance>("create_manager", { config }),
  createArchitect: (config: CreateInstanceInput) => invoke<Instance>("create_architect", { config }),
  changeAgentDirectory: (id: string, folder: string, expectedGeneration: number) => invoke<WorkspaceChange>("change_agent_directory", { id, folder, expectedGeneration }),
  handOverToManager: (sourceId: string) => invoke<WorkspaceChange>("hand_over_to_manager", { sourceId }),
  takeBackControl: (id: string) => invoke<void>("take_back_control", { id }),
  returnToManager: (id: string, humanRequestId: string) => invoke<WorkspaceChange>("return_to_manager", { id, humanRequestId }),
  giveRepoToArchitect: (sourceId: string) => invoke<WorkspaceChange>("give_repo_to_architect", { sourceId }),
  listWorkspaceChanges: () => invoke<WorkspaceChange[]>("list_workspace_changes"),
  listComputedWorkspaceFolders: () => invoke<string[]>("list_computed_workspace_folders"),
  startInstance: (id: string, fresh: boolean) => invoke<void>("start_instance", { id, fresh }),
  stopInstance: (id: string) => invoke<void>("stop_instance", { id }),
  restartInstance: (id: string) => invoke<void>("restart_instance", { id }),
  openNativeAgentHub: (id: string, expectedGeneration: number) => invoke<void>("open_native_agent_hub", { id, expectedGeneration }),
  listHumanDecisions: () => invoke<HumanDecisionSnapshot>("list_human_decisions"),
  respondHumanDecision: (target: HumanDecisionTarget, response: HumanDecisionResponse) => invoke<{ accepted: true }>("respond_human_decision", { target, response }),
  removeInstance: (id: string) => invoke<void>("remove_instance", { id }),
  attachPty: (id: string, onData: Channel<PtyChunk>) => invoke<void>("attach_pty", { id, onData }),
  writePty: (id: string, data: string) => invoke<void>("write_pty", { id, data }),
  resizePty: (id: string, cols: number, rows: number) => invoke<void>("resize_pty", { id, cols, rows }),
  listSessions: (projectPath: string, profile: string) => invoke<SessionInfo[]>("list_sessions", { projectPath, profile }),
  readPlan: (id: string) => invoke<string | null>("read_plan", { id }),
  listPlanBackups: (id: string) => invoke<PlanBackup[]>("list_plan_backups", { id }),
  restorePlanBackup: (id: string, backupId: string) => invoke<void>("restore_plan_backup", { id, backupId }),
  renderTranscript: (id: string, subagentId: string) => invoke<string>("render_transcript", { id, subagentId }),
  getGlobalConfig: () => invoke<GlobalConfigInfo>("get_global_config"),
  previewGlobalConfig: (sourceId: string | null, importPath: string | null) => invoke<ConfigPreview>("preview_global_config", { sourceId, importPath }),
  saveGlobalConfig: (yaml: string) => invoke<{ path: string }>("save_global_config", { yaml }),
  listSavedConfigs: () => invoke<SavedConfigs>("list_saved_configs"),
  activateGlobalConfig: (path: string) => invoke<{ path: string }>("activate_global_config", { path }),
  listAgentFiles: (kind: Instance["kind"]) => invoke<AgentFiles>("list_agent_files", { kind }),
  listOmpProfiles: () => invoke<OmpProfiles>("list_omp_profiles"),
  getSetupPaths: () => invoke<SetupPaths>("get_setup_paths"),
  getRepositorySetupRoot: (projectInstanceId: string) => invoke<string>("get_repository_setup_root", { projectInstanceId }),
  startAgentSetup: (request: SetupRequest) => invoke<SetupAdmission>("start_agent_setup", { request }),
  sendAgentSetup: (id: string, request: SetupRequest) => invoke<SetupAdmission>("send_agent_setup", { id, request }),
  openSetupFolder: (scope: SetupScope, projectInstanceId: string | null) => invoke<void>("open_setup_folder", { scope, projectInstanceId }),
  openPlanSession: (targetId: string) => invoke<PlanSessionAdmission>("open_plan_session", { targetId }),
  listAllTerminals: () => invoke<TerminalOwner[]>("list_all_terminals"),
  listTerminalHistory: (id: string | null, cursor: string | null) => invoke<HistoryPage>("list_terminal_history", { id, cursor }),
  readTerminalScript: (id: string, sessionId: string, executionId: string) => invoke<string>("read_terminal_script", { id, sessionId, executionId }),
  readTerminalOutput: (id: string, sessionId: string, executionId: string) => invoke<string>("read_terminal_output", { id, sessionId, executionId }),
  attachCommandOutput: (id: string, generation: number, sessionId: string, executionId: string, onData: Channel<CommandOutput>) => invoke<void>("attach_command_output", { id, generation, sessionId, executionId, onData }),
  killTerminalCommand: (id: string, generation: number, sessionId: string, executionId: string) => invoke<void>("kill_terminal_command", { id, generation, sessionId, executionId }),
  openUserTerminal: (id: string) => invoke<UserShellInfo>("open_user_terminal", { id }),
  attachUserTerminal: (id: string, shellId: string, onData: Channel<PtyChunk>) => invoke<void>("attach_user_terminal", { id, shellId, onData }),
  writeUserTerminal: (id: string, shellId: string, data: string) => invoke<void>("write_user_terminal", { id, shellId, data }),
  resizeUserTerminal: (id: string, shellId: string, cols: number, rows: number) => invoke<void>("resize_user_terminal", { id, shellId, cols, rows }),
  closeUserTerminal: (id: string, shellId: string) => invoke<void>("close_user_terminal", { id, shellId }),
};
