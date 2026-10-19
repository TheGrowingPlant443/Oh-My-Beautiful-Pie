import { invoke } from "@tauri-apps/api/core";
import { writable } from "svelte/store";
import type { SetupItem } from "./api";

export type AgentMarkdownTarget = { kind: "global" | "manager" | "architect" } | { kind: "repository"; projectInstanceId: string } | { kind: "profile"; profileName: string };
export type AgentMarkdownItemState = "nativeOnly" | "new" | "changed" | "deleted" | "unchanged" | "nativeChanged" | "conflict";
export interface AgentMarkdownItem {
  id: string; nativePath: string; vaultPath: string;
  sourceHash: string | null; nativeHash: string | null; appliedHash: string | null;
  state: AgentMarkdownItemState; diff: string; nativeDependencies: string[];
}
export interface AgentMarkdownStatus {
  target: AgentMarkdownTarget; vaultId: string | null; namespace: string; nativeRoot: string; sourceRoot: string | null;
  enabled: boolean; sourceRevision: string; appliedRevision: string | null; items: AgentMarkdownItem[]; warnings: string[];
}
export interface AgentMarkdownImportPreparation {
  token: string; target: AgentMarkdownTarget; items: AgentMarkdownItem[]; selectedIds: string[]; warnings: string[];
}
export interface AgentMarkdownApplyPreparation extends AgentMarkdownImportPreparation { sourceRevision: string }
export const agentMarkdownApi = {
  status: (target: AgentMarkdownTarget) => invoke<AgentMarkdownStatus>("get_agent_markdown_status", { target }),
  prepareImport: (target: AgentMarkdownTarget, selectedNativeIds: string[] | null) => invoke<AgentMarkdownImportPreparation>("prepare_agent_markdown_import", { target, selectedNativeIds }),
  confirmImport: (token: string) => invoke<AgentMarkdownStatus>("confirm_agent_markdown_import", { token }),
  prepareApply: (target: AgentMarkdownTarget, sourceRevision: string) => invoke<AgentMarkdownApplyPreparation>("prepare_agent_markdown_apply", { target, sourceRevision }),
  apply: (token: string, selectedIds: string[]) => invoke<AgentMarkdownStatus>("apply_agent_markdown", { token, selectedIds }),
};
export type AgentMarkdownApi = typeof agentMarkdownApi;
export const agentMarkdownTargetKey = (target: AgentMarkdownTarget): string => JSON.stringify(target);
export function canApplyMarkdown(item: AgentMarkdownItem): boolean { return item.state === "new" || item.state === "changed" || item.state === "deleted"; }
export function unappliedMarkdown(status: AgentMarkdownStatus): AgentMarkdownItem[] {
  return status.items.filter(item => canApplyMarkdown(item) || item.state === "conflict");
}
const join = (root: string, path: string): string => `${root.replace(/[\\/]+$/, "")}/${path}`;
/** Only known Markdown assets move. Settings, extensions and executable skill dependencies retain native destinations. */
export function agentSetupPreview(item: SetupItem, nativeRoot: string, status: AgentMarkdownStatus | null, globalOmpFolder: string): { nativePath: string; sourcePath: string | null }[] {
  const name = item.name.trim().toLowerCase().replace(/\s+/g, "-");
  let files: string[];
  switch (item.kind) {
    case "agents": files = [name ? `agent-instructions/${name}/AGENTS.md` : "AGENTS.md"]; break;
    case "rules": files = ["RULES.md"]; break;
    case "skill": files = [`skills/${name}/SKILL.md`, "SKILLS.md"]; break;
    case "workflow": files = [`rules/workflow-${name}.md`]; break;
    case "plugin": files = [`extensions/${name}/package.json`, `extensions/${name}/index.ts`]; break;
    case "profile": return [{ nativePath: join(globalOmpFolder, `profiles/${name}/agent/config.yml`), sourcePath: null }];
    case "tool": files = [`tools/${name}/index.ts`, ...(item.language === "python" ? [`tools/${name}/run.py`] : item.language === "bash" ? [`tools/${name}/run.sh`] : [])]; break;
  }
  return files.map(file => ({ nativePath: join(nativeRoot, file), sourcePath: status?.enabled && status.sourceRoot && file.toLowerCase().endsWith(".md") ? join(status.sourceRoot, file) : null }));
}

export type AgentMarkdownReview = { kind: "import" | "apply"; preparation: AgentMarkdownImportPreparation | AgentMarkdownApplyPreparation; expiresAt: number; selectedIds: string[] };
export interface AgentMarkdownState { target: AgentMarkdownTarget | null; status: AgentMarkdownStatus | null; review: AgentMarkdownReview | null; busy: boolean; error: string | null }
/** Target changes and newer reads fence every pending result, including failed preparations and consumed writes. */
export function createAgentMarkdownStore(api: AgentMarkdownApi = agentMarkdownApi, now: () => number = Date.now) {
  let state: AgentMarkdownState = { target: null, status: null, review: null, busy: false, error: null };
  const store = writable(state);
  let epoch = 0;
  let disposed = false;
  const publish = (patch: Partial<AgentMarkdownState>): void => { state = { ...state, ...patch }; store.set(state); };
  async function run(operation: () => Promise<Partial<AgentMarkdownState>>): Promise<void> {
    const mine = ++epoch;
    publish({ busy: true, error: null });
    try { const patch = await operation(); if (!disposed && mine === epoch) publish(patch); }
    catch (failure) { if (!disposed && mine === epoch) publish({ error: String(failure), review: null }); }
    finally { if (!disposed && mine === epoch) publish({ busy: false }); }
  }
  async function refresh(): Promise<void> {
    if (!state.target || disposed) return;
    const target = state.target;
    publish({ review: null });
    await run(async () => ({ status: await api.status(target) }));
  }
  async function selectTarget(target: AgentMarkdownTarget): Promise<void> {
    if (disposed) return;
    ++epoch;
    publish({ target, status: null, review: null, error: null, busy: false });
    await refresh();
  }
  async function prepare(kind: "import" | "apply", selectedNativeIds: string[] | null = null): Promise<void> {
    if (!state.target || !state.status || state.busy || disposed) return;
    const target = state.target, revision = state.status.sourceRevision;
    publish({ review: null });
    await run(async () => {
      const preparation = kind === "import" ? await api.prepareImport(target, selectedNativeIds) : await api.prepareApply(target, revision);
      const allowed = new Set(preparation.items.filter(canApplyMarkdown).map(item => item.id));
      return { review: { kind, preparation, expiresAt: now() + 10 * 60_000, selectedIds: kind === "apply" ? preparation.selectedIds.filter(id => allowed.has(id)) : preparation.selectedIds } };
    });
  }
  function selectApply(ids: string[]): void {
    if (state.review?.kind !== "apply" || state.busy || disposed) return;
    const allowed = new Set(state.review.preparation.items.filter(canApplyMarkdown).map(item => item.id));
    publish({ review: { ...state.review, selectedIds: [...new Set(ids)].filter(id => allowed.has(id)) } });
  }
  async function confirm(): Promise<void> {
    const review = state.review;
    if (!review || state.busy || disposed) return;
    if (now() >= review.expiresAt) { publish({ review: null, error: "Review expired after ten minutes. Prepare and review again." }); return; }
    // A token can be consumed even when the write fails: never offer a replay.
    publish({ review: null });
    await run(async () => ({ status: review.kind === "import" ? await api.confirmImport(review.preparation.token) : await api.apply(review.preparation.token, review.selectedIds) }));
  }
  return { subscribe: store.subscribe, selectTarget, refresh, prepare, selectApply, confirm,
    cancel: (): void => { if (disposed) return; ++epoch; publish({ review: null, busy: false, error: null }); },
    dispose: (): void => { disposed = true; ++epoch; },
  };
}
