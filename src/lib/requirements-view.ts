import type { RequirementCategory, RequirementEvidence, RequirementItem } from "./api";

/** Fixed display order; mirrors the backend category order. */
export const CATEGORY_ORDER: readonly RequirementCategory[] = ["runtime", "buildTool", "packageManager", "containerEngine", "osFeature", "systemSetting", "executable"];
export const CATEGORY_LABEL: Record<RequirementCategory, string> = {
  runtime: "Runtimes", buildTool: "Build tools", packageManager: "Package managers", containerEngine: "Container engines",
  osFeature: "Operating system features", systemSetting: "System settings", executable: "Executables and scripts",
};
export interface RequirementGroup { category: RequirementCategory; label: string; items: RequirementItem[] }

/** Items from the repository's own files (static analysis). Architect findings are never part of these groups. */
export function staticItems(items: readonly RequirementItem[]): RequirementItem[] {
  return items.filter(item => item.source === "static");
}
export function architectItems(items: readonly RequirementItem[]): RequirementItem[] {
  return items.filter(item => item.source === "architect");
}
/** Static items grouped by category in the fixed order; empty categories are omitted and each group keeps the backend's item order. */
export function groupRequirements(items: readonly RequirementItem[]): RequirementGroup[] {
  const groups: RequirementGroup[] = [];
  for (const category of CATEGORY_ORDER) {
    const members = staticItems(items).filter(item => item.category === category);
    if (members.length) groups.push({ category, label: CATEGORY_LABEL[category], items: members });
  }
  return groups;
}

const NEED_LABEL: Record<RequirementItem["need"], string> = { required: "Required", optional: "Optional", unknown: "Need unknown" };
const ENVIRONMENT_LABEL: Record<RequirementItem["environment"], string> = { host: "This computer", wsl: "WSL", container: "Container" };
export function needLabel(need: RequirementItem["need"]): string {
  return NEED_LABEL[need] ?? "Need unknown";
}
export function environmentLabel(environment: RequirementItem["environment"]): string {
  return ENVIRONMENT_LABEL[environment] ?? "Container";
}
export function detectedLabel(detected: RequirementItem["detected"]): string {
  if (detected.state === "found") return detected.version ? `Found ${detected.version}` : "Found";
  return detected.state === "missing" ? "Not found" : "Unknown";
}
export function installLabel(install: RequirementItem["install"]): string {
  if (install.availability === "automatic") return install.via ? `Installable via ${install.via}` : "Installable automatically";
  return install.availability === "manual" ? "Manual install" : "Install unknown";
}
export function sourceLabel(source: RequirementItem["source"]): string {
  return source === "architect" ? "Architect finding (unverified)" : "From project files";
}
export function evidenceLocation(evidence: RequirementEvidence): string {
  return evidence.line === undefined ? evidence.path : `${evidence.path}:${evidence.line}`;
}
/** True while an Architect request can still deliver a result (an uncertain delivery may still produce one). */
export function insightInFlight(state: "pending" | "submitted" | "completed" | "uncertain" | "failed"): boolean {
  return state === "pending" || state === "submitted" || state === "uncertain";
}
