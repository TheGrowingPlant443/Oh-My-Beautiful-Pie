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
  configs: string[];
  model: string;
  status: InstanceStatus;
  activity: string;
  task: string | null;
  subagents: Subagent[];
  planPath: string | null;
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
  bridgeReady: boolean;
  pendingDecisionCount: number;
  humanRequest: { reason: string; decisionId: string | null } | null;
}
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
}
export type PtyChunk = ArrayBuffer;
export interface GlobalConfigInfo { path: string | null; error: string | null; }
export interface ConfigPreview { yaml: string; modelRoles: Record<string, string | null>; omitted: string[]; }
export type SetupScope = "global" | "manager";
export type SetupKind = "agents" | "rules" | "skill" | "tool" | "workflow";
export type ScriptLanguage = "python" | "bash" | "typescript";
export interface SetupItem { kind: SetupKind; name: string; description: string; language?: ScriptLanguage; }
export interface SetupRequest { scope: SetupScope; items: SetupItem[]; }
export interface SetupDraft { scope: SetupScope | null; items: SetupItem[]; step: number; instanceId: string | null; }
export interface SetupPaths { globalOmpFolder: string; globalAgentFolder: string; managerFolder: string; }
export interface SetupAdmission { instance: Instance; promptSent: boolean | null; error: string | null; }

export const api = {
  listInstances: () => invoke<Instance[]>("list_instances"),
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
  renderTranscript: (id: string, subagentId: string) => invoke<string>("render_transcript", { id, subagentId }),
  getGlobalConfig: () => invoke<GlobalConfigInfo>("get_global_config"),
  previewGlobalConfig: (sourceId: string | null, importPath: string | null) => invoke<ConfigPreview>("preview_global_config", { sourceId, importPath }),
  saveGlobalConfig: (yaml: string) => invoke<{ path: string }>("save_global_config", { yaml }),
  getSetupPaths: () => invoke<SetupPaths>("get_setup_paths"),
  startAgentSetup: (request: SetupRequest) => invoke<SetupAdmission>("start_agent_setup", { request }),
  sendAgentSetup: (id: string, request: SetupRequest) => invoke<SetupAdmission>("send_agent_setup", { id, request }),
  openSetupFolder: (scope: SetupScope) => invoke<void>("open_setup_folder", { scope }),
};
