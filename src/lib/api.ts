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
}
export type PtyChunk = ArrayBuffer;

export const api = {
  listInstances: () => invoke<Instance[]>("list_instances"),
  createInstance: (config: CreateInstanceInput) => invoke<Instance>("create_instance", { config }),
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
};
