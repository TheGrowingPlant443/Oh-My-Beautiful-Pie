import type { CommandOutput, CommandSummary, TerminalOwner } from "./api";

// Per-view UI state keyed by instance id, or this key for the Dashboard's aggregate page.
export const GLOBAL_UI_KEY = "*";
export type RightPane = "browser" | "terminal";
export interface TerminalUi { pane: RightPane | null; tab: string; follow: boolean; }
export const defaultTerminalUi = (): TerminalUi => ({ pane: null, tab: "", follow: false });

export interface TabDescriptor {
  key: string;
  kind: "shell" | "history" | "decisions" | "command" | "monitor";
  label: string;
  title: string;
  instanceId: string | null;
  command?: CommandSummary;
}

export const HISTORY_KEY = "history";
export const DECISIONS_KEY = "decisions";
export const MONITOR_KEY = "outbound-processes";
export const shellKey = (instanceId: string): string => `shell:${instanceId}`;
export const commandKey = (instanceId: string, executionId: string): string => `command:${instanceId}:${executionId}`;

export function commandLabel(command: CommandSummary): string {
  return command.description ?? command.preview;
}

// Session pane: 0 user shell, 1 history, the permanent Outbound & Processes monitor (unnumbered), then live commands by slot.
// Aggregate: history, decisions, commands, then shells (never the monitor: it belongs to one agent).
export function terminalTabs(instanceId: string | null, owners: readonly TerminalOwner[]): TabDescriptor[] {
  const history: TabDescriptor = {
    key: HISTORY_KEY, kind: "history", label: instanceId === null ? "History" : "1 · History",
    title: "Every script the AI ran", instanceId: null,
  };
  const commands: TabDescriptor[] = owners.flatMap(owner => owner.active.map<TabDescriptor>(command => {
    const text = commandLabel(command);
    return {
      key: commandKey(owner.instanceId, command.executionId), kind: "command", instanceId: owner.instanceId, command,
      label: instanceId === null ? `${owner.name} · Tab ${command.tab ?? "?"} · ${text}` : `${command.tab ?? "?"} · ${text}`,
      title: `${owner.name}: ${command.preview}`,
    };
  })).sort((a, b) => instanceId === null
    ? a.command!.startedAt - b.command!.startedAt || a.key.localeCompare(b.key)
    : (a.command!.tab ?? 0) - (b.command!.tab ?? 0));
  const shells = owners.filter(owner => owner.userShell).map<TabDescriptor>(owner => ({
    key: shellKey(owner.instanceId), kind: "shell", instanceId: owner.instanceId,
    label: instanceId === null ? `${owner.name} · User terminal` : "0 · Terminal", title: "Your own shell",
  }));
  if (instanceId === null) return [history, {
    key: DECISIONS_KEY, kind: "decisions", label: "Decisions", title: "Human answers across all agents", instanceId: null,
  }, ...commands, ...shells];
  const monitor: TabDescriptor = {
    key: MONITOR_KEY, kind: "monitor", label: "Outbound & Processes", instanceId,
    title: "Running processes and external network connections on this computer, sampled only while this tab is shown",
  };
  return [...shells, history, monitor, ...commands];
}

export function initialTab(tabs: readonly TabDescriptor[]): string {
  return tabs.find(tab => tab.kind === "shell")?.key ?? HISTORY_KEY;
}

// A newly started command is selected only when Follow is on; losing the selected tab never leaves a stale one.
export function nextSelection(tabs: readonly TabDescriptor[], previousKeys: ReadonlySet<string>, selected: string, follow: boolean): string {
  const commands = tabs.filter(tab => tab.kind === "command");
  const newest = (list: readonly TabDescriptor[]) => list.reduce((best, tab) => tab.command!.startedAt >= best.command!.startedAt ? tab : best);
  const fresh = commands.filter(tab => !previousKeys.has(tab.key));
  if (follow && fresh.length) return newest(fresh).key;
  if (selected === "") return initialTab(tabs);
  if (tabs.some(tab => tab.key === selected)) return selected;
  if (follow && selected.startsWith("command:") && commands.length) return newest(commands).key;
  return HISTORY_KEY;
}

// Output frames carry the complete tail: append only when it extends what is already shown.
export function outputWrite(previous: string, next: CommandOutput): { reset: boolean; data: string } {
  if (next.text.startsWith(previous)) return { reset: false, data: next.text.slice(previous.length) };
  return { reset: true, data: next.text };
}

export function ownerStatus(owner: TerminalOwner | undefined): string | null {
  if (!owner) return null;
  if (owner.held) return "Agent interrupted — send a new prompt to resume commands";
  return owner.error;
}

// Saved failure output may contain colour escapes from the command; the History view shows plain text.
export function stripAnsi(text: string): string {
  return text.replace(/\u001b\[[0-9;?]*[ -/]*[@-~]/g, "");
}
