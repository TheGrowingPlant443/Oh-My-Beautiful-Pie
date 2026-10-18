import { open, save } from "@tauri-apps/plugin-dialog";
import type { InstanceStatus } from "./api";

export function basename(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, "");
  return trimmed.slice(Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\")) + 1);
}
export function agentName(path: string): string {
  const parts = path.split(/[\\/]+/).filter(Boolean);
  return parts.length >= 2 ? parts[parts.length - 2] : path;
}
export function statusClass(status: InstanceStatus): string {
  if (status === "running") return "ok";
  if (status === "starting") return "warn";
  if (status === "error") return "err";
  return "";
}
export async function pickFolder(defaultPath?: string): Promise<string | null> {
  const result = await open({ directory: true, multiple: false, title: "Choose project folder", defaultPath });
  return typeof result === "string" ? result : null;
}
export async function pickReportPath(defaultName: string): Promise<string | null> {
  return await save({ title: "Save code quality report", defaultPath: defaultName, filters: [{ name: "HTML report", extensions: ["html"] }] });
}
export async function confirmDanger(message: string): Promise<boolean> {
  return window.confirm(message);
}
