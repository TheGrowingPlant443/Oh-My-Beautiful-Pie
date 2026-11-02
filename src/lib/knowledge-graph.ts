import type { GraphifyCommunity, GraphifyHyperedge, GraphifyLink, GraphifyNode, GraphifyRef, VaultLinkRecord, VaultLinkStatus, VaultNoteRecord } from "./knowledge";
import { dirId, node, type GraphModel, type ModelEdge, type ModelNode } from "./system-graph";

// Pure builders of the graph models behind the note graph and the Graphify view. They run inside the link-index worker (and in
// tests), never touch the backend, and keep every record: nothing is merged by name, nothing is invented, containment is never an edge.

export const noteNodeId = (path: string): string => `note:${path}`;
export const communityId = (community: number): string => `community:${community}`;
/** Numeric and text ids are different identities even when they print alike. */
export const graphifyNodeId = (ref: GraphifyRef): string => `n:${ref.idKind}:${ref.id}`;

const baseNoteName = (path: string): string => path.slice(path.lastIndexOf("/") + 1).replace(/\.(md|markdown)$/i, "");
const parentOf = (path: string): string => { const cut = path.lastIndexOf("/"); return cut < 0 ? "" : path.slice(0, cut); };

/** Code-unit order, never locale order: the model order must not depend on the machine. */
function comparePaths(a: string, b: string): number {
  if (a === b) return 0;
  return a < b ? -1 : 1;
}

// ---------- note graph ----------

export interface VaultStats {
  notes: number;
  /** Link occurrences by status, as the index reported them. */
  links: Record<VaultLinkStatus, number>;
  /** Occurrences of a note linking to itself (also counted under `resolved`). */
  selfLinks: number;
  /** Occurrences whose source note is not in the note list (kept visible, never drawn). */
  orphanSources: number;
}
export const emptyVaultStats = (): VaultStats => ({ notes: 0, links: { resolved: 0, missing: 0, ambiguous: 0, nonDocument: 0, external: 0, anchor: 0 }, selfLinks: 0, orphanSources: 0 });

export class VaultModelBuilder {
  private readonly notes = new Map<string, VaultNoteRecord>();
  private readonly edges: ModelEdge[] = [];
  readonly stats = emptyVaultStats();

  addNotes(items: readonly VaultNoteRecord[]): void {
    for (const item of items) {
      if (!this.notes.has(item.path)) this.stats.notes++;
      this.notes.set(item.path, item);
    }
  }

  /** Only resolved occurrences between notes become edges; the rest stay countable and are listed from the index per note. */
  addLinks(items: readonly VaultLinkRecord[]): void {
    for (const link of items) {
      this.stats.links[link.status]++;
      if (link.status !== "resolved" || link.target === null) continue;
      if (link.source === link.target) this.stats.selfLinks++;
      this.edges.push({
        id: link.id, source: noteNodeId(link.source), target: noteNodeId(link.target), direction: "undirected", relation: link.kind === "embed" ? "embed" : "link",
        grade: "resolvedLink", derived: false, label: link.label ?? link.raw, count: 1, evidenceIds: [`${link.source}:${link.line}`], truncated: false,
      });
    }
  }

  build(): GraphModel {
    const nodes: ModelNode[] = [];
    const folders = new Map<string, ModelNode>();
    const ensureFolder = (path: string): string | null => {
      if (path === "") return null;
      const id = dirId(path);
      if (folders.has(id)) return id;
      folders.set(id, node({ id, kind: "folder", label: path.slice(path.lastIndexOf("/") + 1), subtitle: path, synthetic: true, path, parentId: ensureFolder(parentOf(path)) }));
      return id;
    };
    for (const note of [...this.notes.values()].sort((a, b) => comparePaths(a.path, b.path))) {
      nodes.push(node({
        id: noteNodeId(note.path), kind: "note", label: note.title || baseNoteName(note.path), subtitle: note.path, path: note.path, ref: note.path,
        parentId: ensureFolder(parentOf(note.path)), evidence: "none",
        description: `${note.outgoing.toLocaleString()} outgoing, ${note.incoming.toLocaleString()} incoming link${note.outgoing + note.incoming === 1 ? "" : "s"}`,
      }));
    }
    return { nodes: [...folders.values(), ...nodes], edges: this.edges };
  }
}

// ---------- Graphify ----------

export interface GraphifyStats {
  nodes: number;
  links: number;
  dangling: number;
  /** Links by confidence label, `none` when the record has no confidence. */
  confidence: Record<string, number>;
  hyperedges: number;
  communities: number;
}

const sourceText = (file: string | null, location: string | null): string => {
  if (!file) return "";
  return location ? `${file}:${location}` : file;
};

const graphifyModelNode = (item: GraphifyNode, id: string, parentId: string | null): ModelNode => node({
  id, kind: item.fileType || "node", label: item.label || item.ref.id, subtitle: sourceText(item.sourceFile, item.sourceLocation),
  path: item.sourceFile, ref: id, parentId, evidence: "none", description: item.origin ? `Origin: ${item.origin}` : "",
});

function graphifyModelEdge(link: GraphifyLink): ModelEdge {
  const evidence = sourceText(link.sourceFile, link.sourceLocation);
  return {
    id: link.id, source: graphifyNodeId(link.source), target: graphifyNodeId(link.target), direction: link.directed ? "directed" : "undirected", relation: link.relation,
    grade: link.confidence ?? "unknown", derived: false, label: link.relation, count: 1, evidenceIds: evidence ? [evidence] : [], truncated: false,
  };
}

export class GraphifyModelBuilder {
  private readonly nodes: GraphifyNode[] = [];
  private readonly links: GraphifyLink[] = [];
  private readonly communities = new Map<number, GraphifyCommunity>();
  private hyperedges = 0;

  addNodes(items: readonly GraphifyNode[]): void { this.nodes.push(...items); }
  addLinks(items: readonly GraphifyLink[]): void { this.links.push(...items); }
  addCommunities(items: readonly GraphifyCommunity[]): void { for (const item of items) this.communities.set(item.id, item); }
  addHyperedges(items: readonly GraphifyHyperedge[]): void { this.hyperedges += items.length; }

  private container(community: number, containers: Map<number, ModelNode>): string {
    const id = communityId(community);
    if (!containers.has(community)) {
      const known = this.communities.get(community);
      containers.set(community, node({ id, kind: "community", label: known?.label || `Community ${community}`, subtitle: known ? `${known.size.toLocaleString()} nodes` : "", synthetic: true }));
    }
    return id;
  }

  build(): { model: GraphModel; stats: GraphifyStats } {
    const nodes: ModelNode[] = [];
    const containers = new Map<number, ModelNode>();
    const seen = new Set<string>();
    for (const item of this.nodes) {
      const id = graphifyNodeId(item.ref);
      if (seen.has(id)) continue;
      seen.add(id);
      nodes.push(graphifyModelNode(item, id, item.community === null ? null : this.container(item.community, containers)));
    }
    const edges: ModelEdge[] = [];
    const confidence: Record<string, number> = {};
    let dangling = 0;
    for (const link of this.links) {
      const label = link.confidence ?? "none";
      confidence[label] = (confidence[label] ?? 0) + 1;
      if (link.dangling) dangling++;
      edges.push(graphifyModelEdge(link));
    }
    return {
      model: { nodes: [...containers.values(), ...nodes], edges },
      stats: { nodes: seen.size, links: this.links.length, dangling, confidence, hyperedges: this.hyperedges, communities: containers.size },
    };
  }
}
