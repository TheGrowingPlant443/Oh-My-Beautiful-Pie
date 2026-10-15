import { invoke, Channel } from "@tauri-apps/api/core";

export type InstanceStatus = "stopped" | "starting" | "running" | "error";
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
  kind: "instance" | "manager";
  managerId: string | null;
  planTarget: string | null;
  bridgeReady: boolean;
  pendingDecisionCount: number;
  humanRequest: { reason: string; decisionId: string | null } | null;
}
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
export type SetupScope = "global" | "manager" | "repository";
export type SetupKind = "agents" | "rules" | "skill" | "tool" | "workflow" | "plugin" | "profile";
export type ScriptLanguage = "python" | "bash" | "typescript";
export interface SetupItem { kind: SetupKind; name: string; description: string; language?: ScriptLanguage; }
export interface SetupRequest { scope: SetupScope; items: SetupItem[]; projectInstanceId: string | null; }
export interface SetupDraft { scope: SetupScope | null; kinds: SetupKind[]; items: SetupItem[]; step: number; instanceId: string | null; projectInstanceId: string | null; }
export interface PlanSessionAdmission { planner: Instance; promptSent: boolean | null; error: string | null; }
export interface SetupPaths { globalOmpFolder: string; globalAgentFolder: string; managerFolder: string; }
export interface SetupAdmission { instance: Instance; promptSent: boolean | null; error: string | null; }
export interface SavedConfig { path: string; modifiedMs: number; }
export interface SavedConfigs { current: string | null; snapshots: SavedConfig[]; nativeGlobal: string | null; error: string | null; }
export interface AgentFile { name: string; scope: "global" | "manager"; path: string; }
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
export interface GitGraph { root: string; head: string | null; currentBranch: string | null; commits: GraphCommit[]; truncated: boolean; }
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

export const api = {
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
  setBrowserView: (instanceId: string | null, generation: number | null, rect: BrowserRect | null) => invoke<void>("set_browser_view", { instanceId, generation, rect }),
  createInstance: (config: CreateInstanceInput) => invoke<Instance>("create_instance", { config }),
  createManager: (config: CreateInstanceInput) => invoke<Instance>("create_manager", { config }),
  setManagerOwner: (id: string, managerId: string | null) => invoke<void>("set_manager_owner", { id, managerId }),
  returnToManager: (id: string) => invoke<void>("return_to_manager", { id }),
  startInstance: (id: string) => invoke<void>("start_instance", { id }),
  stopInstance: (id: string) => invoke<void>("stop_instance", { id }),
  restartInstance: (id: string) => invoke<void>("restart_instance", { id }),
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
  listAgentFiles: () => invoke<AgentFiles>("list_agent_files"),
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
