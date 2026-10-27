import type {
  EvidenceRef, ProjectInsightStatus, SourceFileRow, StructureApproval, StructureConventionPayload, StructureEntryPayload,
  StructureMappingPayload, StructureOperation, StructurePayload, StructureSourceRow,
} from "./api";

// Pure data for the repository-structure area. Nothing here renders HTML; every string is plain text.

const compare = (a: string, b: string): number => {
  if (a < b) return -1;
  return a > b ? 1 : 0;
};

/** Sibling directories that share a child shape are grouped from this many. */
export const MIN_DIR_GROUP = 4;
/** Sibling files that share an extension/suffix are grouped from this many. */
export const MIN_FILE_GROUP = 5;

// ---------- Current tree: complete inventory ----------

export interface TreeFile { name: string; path: string; bytes: number; language: string | null }
export interface TreeDir {
  name: string; path: string; dirs: TreeDir[]; files: TreeFile[];
  /** Recursive file count. */
  fileCount: number;
  /** Recursive count of directories below this one (this one excluded). */
  dirCount: number;
}

/** Builds the exact directory tree of the complete inventory: every file and every directory, including empty ones. */
export function buildCurrentTree(files: readonly SourceFileRow[], directories: readonly string[]): TreeDir {
  const root: TreeDir = { name: "", path: "", dirs: [], files: [], fileCount: 0, dirCount: 0 };
  const dirMap = new Map<string, TreeDir>([["", root]]);
  const ensure = (path: string): TreeDir => {
    const hit = dirMap.get(path);
    if (hit) return hit;
    const cut = path.lastIndexOf("/");
    const parent = ensure(cut < 0 ? "" : path.slice(0, cut));
    const dir: TreeDir = { name: path.slice(cut + 1), path, dirs: [], files: [], fileCount: 0, dirCount: 0 };
    parent.dirs.push(dir);
    dirMap.set(path, dir);
    return dir;
  };
  for (const directory of directories) ensure(directory);
  const seen = new Set<string>();
  for (const file of files) {
    if (seen.has(file.path)) continue;
    seen.add(file.path);
    const cut = file.path.lastIndexOf("/");
    ensure(cut < 0 ? "" : file.path.slice(0, cut)).files.push({ name: file.path.slice(cut + 1), path: file.path, bytes: file.bytes, language: file.language });
  }
  const finish = (dir: TreeDir): void => {
    dir.dirs.sort((a, b) => compare(a.name, b.name));
    dir.files.sort((a, b) => compare(a.name, b.name));
    let fileCount = dir.files.length;
    let dirCount = dir.dirs.length;
    for (const child of dir.dirs) { finish(child); fileCount += child.fileCount; dirCount += child.dirCount; }
    dir.fileCount = fileCount;
    dir.dirCount = dirCount;
  };
  finish(root);
  return root;
}

export interface GeneralDir { kind: "dir"; name: string; path: string; children: GeneralNode[]; fileCount: number; dirCount: number }
export interface GeneralFile { kind: "file"; name: string; path: string; bytes: number; language: string | null }
export interface GeneralGroup {
  kind: "group"; id: string; label: string;
  /** Display-only description of what the members share. */
  pattern: string;
  memberKind: "dir" | "file";
  parentPath: string;
  /** Exact member paths; nothing is dropped by generalization. */
  members: string[];
  nodes: (GeneralDir | GeneralFile)[];
  fileCount: number; dirCount: number;
}
export type GeneralNode = GeneralDir | GeneralFile | GeneralGroup;

const ownName = (child: string, own: string): string => {
  if (child === own) return "*";
  return child.startsWith(`${own}.`) ? `*${child.slice(own.length)}` : child;
};
function shapePattern(items: readonly string[]): string {
  if (items.length === 0) return "*/ (empty)";
  return items.length === 1 ? `*/${items[0]}` : `*/{${items.join(", ")}}`;
}
function dirShape(dir: TreeDir): { key: string; pattern: string } {
  const items = [...dir.dirs.map(child => `${ownName(child.name, dir.name)}/`), ...dir.files.map(child => ownName(child.name, dir.name))].sort(compare);
  return { key: JSON.stringify(items), pattern: shapePattern(items) };
}
const fileSuffix = (name: string): string | null => {
  const dot = name.indexOf(".", 1);
  return dot < 0 || dot === name.length - 1 ? null : name.slice(dot);
};
const prefixOf = (path: string): string => path ? `${path}/` : "";

function generalizeSubdirs(dir: TreeDir): GeneralNode[] {
  const dirNodes = new Map<TreeDir, GeneralDir>(dir.dirs.map(child => [child, generalizeDir(child)]));
  const dirBuckets = new Map<string, { pattern: string; dirs: TreeDir[] }>();
  for (const child of dir.dirs) {
    const shape = dirShape(child);
    const bucket = dirBuckets.get(shape.key);
    if (bucket) bucket.dirs.push(child); else dirBuckets.set(shape.key, { pattern: shape.pattern, dirs: [child] });
  }
  const singles: GeneralDir[] = [];
  const dirGroups: GeneralGroup[] = [];
  for (const [key, bucket] of dirBuckets) {
    const nodes = bucket.dirs.map(child => dirNodes.get(child)!);
    if (bucket.dirs.length < MIN_DIR_GROUP) {
      singles.push(...nodes);
      continue;
    }
    dirGroups.push({
      kind: "group", id: `dir:${dir.path}:${key}`, label: `${prefixOf(dir.path)}*/ (${nodes.length} similar)`, pattern: bucket.pattern,
      memberKind: "dir", parentPath: dir.path, members: nodes.map(node => node.path), nodes,
      fileCount: nodes.reduce((sum, node) => sum + node.fileCount, 0), dirCount: nodes.length + nodes.reduce((sum, node) => sum + node.dirCount, 0),
    });
  }
  singles.sort((a, b) => compare(a.name, b.name));
  dirGroups.sort((a, b) => compare(a.label, b.label) || compare(a.id, b.id));
  return [...singles, ...dirGroups];
}

const generalFile = (file: TreeFile): GeneralFile => ({ kind: "file", name: file.name, path: file.path, bytes: file.bytes, language: file.language });

function generalizeFiles(dir: TreeDir): GeneralNode[] {
  const suffixBuckets = new Map<string, TreeFile[]>();
  const loose: TreeFile[] = [];
  for (const file of dir.files) {
    const suffix = fileSuffix(file.name);
    if (suffix === null) { loose.push(file); continue; }
    const bucket = suffixBuckets.get(suffix);
    if (bucket) bucket.push(file); else suffixBuckets.set(suffix, [file]);
  }
  const fileGroups: GeneralGroup[] = [];
  for (const [suffix, bucket] of suffixBuckets) {
    if (bucket.length < MIN_FILE_GROUP) {
      loose.push(...bucket);
      continue;
    }
    fileGroups.push({
      kind: "group", id: `file:${dir.path}:${suffix}`, label: `${prefixOf(dir.path)}*${suffix} (${bucket.length} similar)`, pattern: `*${suffix}`,
      memberKind: "file", parentPath: dir.path, members: bucket.map(file => file.path),
      nodes: bucket.map(generalFile),
      fileCount: bucket.length, dirCount: 0,
    });
  }
  loose.sort((a, b) => compare(a.name, b.name));
  fileGroups.sort((a, b) => compare(a.label, b.label) || compare(a.id, b.id));
  return [...fileGroups, ...loose.map(generalFile)];
}

function generalizeDir(dir: TreeDir): GeneralDir {
  const children = [...generalizeSubdirs(dir), ...generalizeFiles(dir)];
  return { kind: "dir", name: dir.name, path: dir.path, children, fileCount: dir.fileCount, dirCount: dir.dirCount };
}

/** Collapses repeated component/feature patterns into expandable groups that keep every exact member path. */
export function generalize(tree: TreeDir): GeneralDir { return generalizeDir(tree); }

export interface TreePaths { files: string[]; directories: string[] }
export function treePaths(root: TreeDir): TreePaths {
  const out: TreePaths = { files: [], directories: [] };
  const stack = [root];
  for (let dir = stack.pop(); dir; dir = stack.pop()) {
    if (dir.path !== "") out.directories.push(dir.path);
    for (const file of dir.files) out.files.push(file.path);
    stack.push(...dir.dirs);
  }
  out.files.sort(compare);
  out.directories.sort(compare);
  return out;
}
export function generalPaths(root: GeneralDir): TreePaths {
  const out: TreePaths = { files: [], directories: [] };
  const stack: GeneralNode[] = [root];
  for (let node = stack.pop(); node; node = stack.pop()) {
    if (node.kind === "file") out.files.push(node.path);
    else if (node.kind === "dir") {
      if (node.path !== "") out.directories.push(node.path);
      stack.push(...node.children);
    } else stack.push(...node.nodes);
  }
  out.files.sort(compare);
  out.directories.sort(compare);
  return out;
}
const sameList = (a: readonly string[], b: readonly string[]): boolean => a.length === b.length && a.every((value, index) => value === b[index]);
function groupsConsistent(root: GeneralDir): boolean {
  const stack: GeneralNode[] = [root];
  for (let node = stack.pop(); node; node = stack.pop()) {
    if (node.kind === "dir") stack.push(...node.children);
    else if (node.kind === "group") {
      if (!sameList(node.members, node.nodes.map(member => member.path))) return false;
      stack.push(...node.nodes.filter(member => member.kind === "dir"));
    }
  }
  return true;
}
/** True only when the generalized view contains exactly the same files and directories (and counts) as the source tree. */
export function reconcileGeneral(tree: TreeDir, general: GeneralDir): boolean {
  const a = treePaths(tree);
  const b = generalPaths(general);
  return sameList(a.files, b.files) && sameList(a.directories, b.directories) && groupsConsistent(general)
    && general.fileCount === tree.fileCount && general.dirCount === tree.dirCount && b.files.length === tree.fileCount && b.directories.length === tree.dirCount;
}

// ---------- Architect analysis overlay ----------

export const evidenceText = (ref: EvidenceRef): string => `${ref.path}:${ref.lineStart}-${ref.lineEnd}`;

export interface EntryLookup {
  byId: Map<string, StructureEntryPayload>; byPath: Map<string, StructureEntryPayload>; byMember: Map<string, StructureEntryPayload>;
  evidence: Map<string, EvidenceRef>;
}
export function entryLookup(payload: StructurePayload | null): EntryLookup {
  const lookup: EntryLookup = { byId: new Map(), byPath: new Map(), byMember: new Map(), evidence: new Map() };
  if (!payload) return lookup;
  for (const entry of payload.entries) {
    if (!lookup.byId.has(entry.id)) lookup.byId.set(entry.id, entry);
    if (entry.path !== null && !lookup.byPath.has(entry.path)) lookup.byPath.set(entry.path, entry);
    for (const member of entry.members) if (!lookup.byMember.has(member)) lookup.byMember.set(member, entry);
  }
  for (const ref of payload.evidence) if (!lookup.evidence.has(ref.id)) lookup.evidence.set(ref.id, ref);
  return lookup;
}
export const entryForPath = (lookup: EntryLookup, path: string): StructureEntryPayload | null => lookup.byPath.get(path) ?? lookup.byMember.get(path) ?? null;
/** The Architect entry covering the most members of a group. */
export function entryForGroup(lookup: EntryLookup, group: Pick<GeneralGroup, "members">): StructureEntryPayload | null {
  const hits = new Map<StructureEntryPayload, number>();
  for (const member of group.members) {
    const entry = entryForPath(lookup, member);
    if (entry) hits.set(entry, (hits.get(entry) ?? 0) + 1);
  }
  let best: StructureEntryPayload | null = null;
  let bestCount = 0;
  for (const [entry, count] of hits) if (count > bestCount) { best = entry; bestCount = count; }
  return best;
}
/** Citation text for each evidence id; an id the payload does not define is reported as such, never hidden. */
export const citationTexts = (lookup: EntryLookup, ids: readonly string[]): string[] =>
  ids.map(id => { const ref = lookup.evidence.get(id); return ref ? evidenceText(ref) : `${id} (evidence not included)`; });

export interface ConventionRow { scope: string; current: string; proposed: string | null; rationale: string; citations: string[] }
export const conventionRows = (payload: StructurePayload | null): ConventionRow[] => {
  if (!payload) return [];
  const lookup = entryLookup(payload);
  return payload.conventions.map((convention: StructureConventionPayload) => ({
    scope: convention.scope, current: convention.current, proposed: convention.proposed, rationale: convention.rationale, citations: citationTexts(lookup, convention.evidenceIds),
  }));
};
export const conflictList = (payload: StructurePayload | null): string[] => payload ? payload.conflicts.map(text => text.trim()).filter(Boolean) : [];

// ---------- Proposed tree ----------

export interface ProposedNode {
  id: string; label: string; path: string | null; pattern: string | null; members: string[]; purpose: string; evidenceIds: string[];
  children: ProposedNode[];
  /** A parent cycle was cut here; the entry is shown as a root. */
  cycleBroken: boolean;
}
export interface ProposedTree { roots: ProposedNode[]; issues: string[] }

/** Entries by id with their parent links; a repeated id keeps the first entry and is reported. */
function proposedNodes(payload: StructurePayload, issues: string[]): { nodes: Map<string, ProposedNode>; parentOf: Map<string, string | null> } {
  const nodes = new Map<string, ProposedNode>();
  const parentOf = new Map<string, string | null>();
  for (const entry of payload.entries) {
    if (nodes.has(entry.id)) { issues.push(`Duplicate entry id ${entry.id} ignored`); continue; }
    nodes.set(entry.id, { id: entry.id, label: entry.label, path: entry.path, pattern: entry.pattern, members: [...entry.members], purpose: entry.purpose, evidenceIds: [...entry.evidenceIds], children: [], cycleBroken: false });
    parentOf.set(entry.id, entry.parentId);
  }
  return { nodes, parentOf };
}

/** Builds the proposed tree from entry parent links without ever recursing on untrusted links. */
export function proposedTree(payload: StructurePayload | null): ProposedTree {
  const issues: string[] = [];
  if (!payload) return { roots: [], issues };
  const { nodes, parentOf } = proposedNodes(payload, issues);
  const roots: ProposedNode[] = [];
  for (const [id, node] of nodes) {
    const parentId = parentOf.get(id) ?? null;
    const linked = parentId !== null && parentId !== id;
    const parent = linked ? nodes.get(parentId) : undefined;
    if (parent) parent.children.push(node);
    else {
      roots.push(node);
      if (linked) issues.push(`Entry ${id} names a missing parent ${parentId}`);
    }
  }
  const reached = new Set<string>();
  const reach = (start: ProposedNode): void => {
    const stack = [start];
    for (let node = stack.pop(); node; node = stack.pop()) {
      if (reached.has(node.id)) continue;
      reached.add(node.id);
      stack.push(...node.children);
    }
  };
  for (const root of roots) reach(root);
  // Anything still unreached sits in or below a parent cycle: cut it open at the first such entry, repeatedly.
  for (const [id, node] of nodes) {
    if (reached.has(id)) continue;
    const parent = nodes.get(parentOf.get(id) ?? "");
    if (parent) parent.children = parent.children.filter(child => child !== node);
    node.cycleBroken = true;
    roots.push(node);
    issues.push(`Entry ${id} is part of a parent cycle and is shown as a root`);
    reach(node);
  }
  return { roots, issues };
}

// ---------- Mappings ----------

export const OPERATION_LABEL: Record<StructureOperation, string> = {
  unchanged: "Unchanged", rename: "Rename", move: "Move", split: "Split", merge: "Merge", create: "Create", remove: "Remove",
};
export interface MappingIndex { from: Map<string, StructureMappingPayload[]>; to: Map<string, StructureMappingPayload[]> }
export function mappingIndex(payload: StructurePayload | null): MappingIndex {
  const index: MappingIndex = { from: new Map(), to: new Map() };
  if (!payload) return index;
  const add = (map: Map<string, StructureMappingPayload[]>, path: string, mapping: StructureMappingPayload) => {
    const list = map.get(path);
    if (!list) map.set(path, [mapping]); else if (!list.includes(mapping)) list.push(mapping);
  };
  for (const mapping of payload.mappings) {
    for (const path of mapping.fromPaths) add(index.from, path, mapping);
    for (const path of mapping.toPaths) add(index.to, path, mapping);
  }
  return index;
}
/** Where a path of the current structure goes (one mapping each for a split or merge, possibly several for overlapping ones). */
export const mappingsFromCurrent = (index: MappingIndex, path: string): StructureMappingPayload[] => index.from.get(path) ?? [];
/** Where a path of the proposed structure comes from; an entry's own path and its member paths are both consulted. */
export function mappingsForProposed(index: MappingIndex, node: Pick<ProposedNode, "path" | "members">): StructureMappingPayload[] {
  const found: StructureMappingPayload[] = [];
  const paths = node.path === null ? node.members : [node.path, ...node.members];
  for (const path of paths) for (const mapping of index.to.get(path) ?? []) if (!found.includes(mapping)) found.push(mapping);
  return found;
}
/** Explicit, never abbreviated text of one mapping; splits and merges list every path. */
export function mappingText(mapping: StructureMappingPayload): string {
  const from = mapping.fromPaths.join(", ");
  const to = mapping.toPaths.join(", ");
  const label = OPERATION_LABEL[mapping.operation];
  if (mapping.operation === "create") return `${label}: ${to}`;
  if (mapping.operation === "remove" || mapping.operation === "unchanged") return `${label}: ${from}`;
  return `${label}: ${from || "(none)"} → ${to || "(none)"}`;
}

// ---------- Approval gate ----------

export interface ApprovalSource { sourceId: string; sha256: string }
export interface ApprovalContext { snapshotId: string | null; source: ApprovalSource | null }
export interface ApprovalCheck { ok: boolean; reason: string | null }
const deny = (reason: string): ApprovalCheck => ({ ok: false, reason });

/**
 * The only gate for approving a visualization for discussion. `revision.supersededBy` must already include any supersession
 * the caller knows about; the backend re-checks every hash again.
 */
export function canApprove(revision: ProjectInsightStatus | null, context: ApprovalContext): ApprovalCheck {
  if (!revision) return deny("No target structure preview is selected.");
  if (revision.kind !== "structureProposed") return deny("Only a target structure preview can be approved.");
  if (revision.state !== "completed" || revision.result?.kind !== "structureProposed") return deny("The preview has not completed.");
  if (!revision.resultHash) return deny("The preview has no result hash.");
  if (!revision.sourceHash) return deny("The preview has no source hash.");
  if (revision.result.sourceHash !== null && revision.result.sourceHash !== revision.sourceHash) return deny("The preview's source hash is inconsistent.");
  if (revision.supersededBy) return deny("A newer preview replaced this one.");
  if (revision.approval) return deny("This preview was already approved.");
  if (!context.snapshotId) return deny("No source index snapshot is selected.");
  if (revision.snapshotId !== context.snapshotId) return deny("Index changed since this preview.");
  if (!context.source) return deny("No target Markdown is selected.");
  if (revision.sourceId !== context.source.sourceId || revision.sourceHash !== context.source.sha256) return deny("This preview is from a different document than the selected Markdown.");
  return { ok: true, reason: null };
}

export const indexChanged = (revision: Pick<ProjectInsightStatus, "snapshotId">, snapshotId: string | null): boolean => !!revision.snapshotId && revision.snapshotId !== snapshotId;
export const differentDocument = (revision: Pick<ProjectInsightStatus, "sourceId" | "sourceHash">, source: ApprovalSource | null): boolean =>
  !!revision.sourceId && (revision.sourceId !== source?.sourceId || (!!revision.sourceHash && revision.sourceHash !== source.sha256));

export interface ApprovalNotice { tone: "ok" | "warn" | "err"; text: string; openArchitect: boolean }
/** Delivery outcome of the one automatic discussion message; uncertain/failed/unavailable are never resent automatically. */
export function approvalNotice(approval: StructureApproval): ApprovalNotice {
  const detail = approval.deliveryError ? ` (${approval.deliveryError})` : "";
  switch (approval.delivery) {
    case "pending": return { tone: "warn", text: "Approved. Delivery to the Architect is pending; the message is not resent automatically.", openArchitect: false };
    case "claimed": return { tone: "warn", text: "Approved. Sending the discussion message to the Architect…", openArchitect: false };
    case "sent": return { tone: "ok", text: "Approved. The discussion message was sent to the Architect.", openArchitect: false };
    case "uncertain": return { tone: "warn", text: `Approved, but delivery to the Architect is uncertain${detail}. It will not be resent automatically; open the Architect to check.`, openArchitect: true };
    case "failed": return { tone: "err", text: `Approved, but delivery to the Architect failed${detail}. It will not be resent automatically; open the Architect to continue.`, openArchitect: true };
    case "unavailable": return { tone: "err", text: `Approved, but the Architect was unavailable${detail}. It will not be resent automatically; open the Architect to continue.`, openArchitect: true };
  }
}

// ---------- Display ----------

/** First non-empty line, whitespace collapsed and cut to `max` characters. */
export function excerptLine(text: string, max = 120): string {
  const line = text.split(/\r?\n/).map(part => part.replace(/\s+/g, " ").trim()).find(Boolean) ?? "";
  const chars = [...line];
  return chars.length > max ? `${chars.slice(0, Math.max(1, max - 1)).join("")}…` : line;
}
export const shortHash = (hash: string | null | undefined, length = 8): string => hash ? hash.slice(0, length) : "—";
export const sourceKindLabel = (kind: StructureSourceRow["kind"]): string => kind === "interactiveMessage" ? "Inline message" : "Long paste file";
export function sizeText(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(bytes < 10 * 1024 ? 1 : 0)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}
export const capturedText = (millis: number): string => new Date(millis).toLocaleString();
