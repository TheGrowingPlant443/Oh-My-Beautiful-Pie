import { clipText, node, revealPath, type GraphIndex, type GraphModel, type ModelEdge, type Role } from "./system-graph";
import type { EvidenceRef, HelicopterCapabilityPayload, HelicopterCertainty, HelicopterEdgePayload, HelicopterKind, HelicopterNodePayload, HelicopterPayload } from "./api";

/** Plain words for what a business node is. These (not the machine kinds) are what a card shows. */
export const HELICOPTER_KIND_WORDS: Record<HelicopterKind, string> = {
  actor: "Who", trigger: "Starts when", activity: "Step", decision: "Decision", outcome: "Result", externalParty: "Outside party", businessGroup: "Journey",
};
const GROUP_WORD = HELICOPTER_KIND_WORDS.businessGroup;
/** At most this many steps are shown opened up by default; beyond it a few journey cards are shown and each opens on demand. */
export const HELICOPTER_OPEN_STEPS = 12;
export const NEEDS_CLARIFICATION = "Needs clarification";

/** The Architect marks an uncertain node or capability by starting its description with this phrase. */
export const needsClarification = (text: string): boolean => /^needs clarification\b/i.test(text.trim());

/** The words on an arrow: a decision's answer, or "then". An unsettled arrow says so on the arrow itself. */
const edgeWord = (edge: HelicopterEdgePayload): string => {
  const base = edge.label.trim() ? clipText(edge.label.trim(), 60) : "then";
  return edge.certainty === "unknown" ? `${base} (${NEEDS_CLARIFICATION.toLowerCase()})` : base;
};

export const certaintyText = (certainty: HelicopterCertainty): string =>
  certainty === "unknown" ? `${NEEDS_CLARIFICATION}: the evidence does not settle this step` : "Read from the evidence by the Architect (not verified runtime behaviour)";

/**
 * The Architect's business view as a graph model. Every flow arrow is directed. A journey grouping only contains steps (it is never
 * an arrow endpoint and does not count as an item). Arrow words show the business label; the structured label stays on the model edge.
 */
export function modelFromHelicopter(payload: HelicopterPayload): GraphModel {
  const kindOf = new Map<string, HelicopterKind>(payload.nodes.map(item => [item.id, item.kind]));
  const startsOutside = new Set<string>(), receivesFromInside = new Set<string>();
  const outside = (id: string): boolean => kindOf.get(id) === "externalParty";
  for (const edge of payload.edges) {
    if (outside(edge.sourceId) && !outside(edge.targetId)) startsOutside.add(edge.sourceId);
    if (!outside(edge.sourceId) && outside(edge.targetId)) receivesFromInside.add(edge.targetId);
  }
  const roleOf = (item: HelicopterNodePayload): Role => {
    switch (item.kind) {
      case "actor": case "trigger": return "input";
      case "outcome": return "output";
      case "externalParty": return startsOutside.has(item.id) && !receivesFromInside.has(item.id) ? "input" : "output";
      default: return "app";
    }
  };
  const nodes = payload.nodes.map(item => node({
    id: item.id, kind: HELICOPTER_KIND_WORDS[item.kind], label: item.label, description: item.description, parentId: item.parentId,
    subtitle: needsClarification(item.description) ? NEEDS_CLARIFICATION : "", evidence: "analysis", role: roleOf(item), evidenceIds: item.evidenceIds,
    // A journey is only a container: it is not itself an item, so counts and "items inside" mean real people, steps and results.
    memberCount: item.kind === "businessGroup" ? 0 : 1,
  }));
  const edges: ModelEdge[] = payload.edges.map(item => ({
    id: item.id, source: item.sourceId, target: item.targetId, direction: "directed", relation: edgeWord(item), grade: item.certainty, derived: false,
    label: item.label, count: 1, evidenceIds: item.evidenceIds, truncated: false,
  }));
  return { nodes, edges };
}

/**
 * The opened journeys of the first picture: with a few steps everything is open; with many steps and several journeys only the
 * journeys are shown (a handful of cards) and every step stays one click away.
 */
export function helicopterDefaultExpanded(model: GraphModel): string[] {
  const groups = model.nodes.filter(item => item.kind === GROUP_WORD);
  const steps = model.nodes.length - groups.length;
  return groups.length >= 2 && steps > HELICOPTER_OPEN_STEPS ? [] : groups.map(item => item.id);
}

/** Opens every ancestor journey of each node so that all of them become visible cards. Unknown ids are ignored. */
export function revealNodes(index: GraphIndex, ids: readonly string[], expanded: ReadonlySet<string>): Set<string> {
  let open = new Set(expanded);
  for (const id of ids) if (index.nodes.has(id)) open = revealPath(index, id, open, "system");
  return open;
}

export interface JourneyRow { id: string; label: string; depth: number; open: boolean; steps: number }

/**
 * Every journey in reading order with its nesting depth, whether it is currently opened and how many people, steps and results it
 * holds (nested journeys included). An opened journey is not a card of its own, so this list is where it is opened or grouped again.
 */
export function journeyRows(payload: HelicopterPayload, expanded: readonly string[]): JourneyRow[] {
  const open = new Set(expanded);
  const byParent = new Map<string | null, HelicopterNodePayload[]>();
  for (const item of payload.nodes) {
    const list = byParent.get(item.parentId);
    if (list) list.push(item); else byParent.set(item.parentId, [item]);
  }
  const held = (id: string): number => (byParent.get(id) ?? []).reduce((sum, child) => sum + (child.kind === "businessGroup" ? held(child.id) : 1), 0);
  const rows: JourneyRow[] = [];
  const walk = (parent: string | null, depth: number): void => {
    for (const item of byParent.get(parent) ?? []) {
      if (item.kind !== "businessGroup") continue;
      rows.push({ id: item.id, label: item.label, depth, open: open.has(item.id), steps: held(item.id) });
      walk(item.id, depth + 1);
    }
  };
  walk(null, 0);
  return rows;
}

export interface CapabilityRow {
  id: string;
  label: string;
  description: string;
  needsClarification: boolean;
  /** The nodes that realize the capability, in the order the Architect listed them. Unknown ids never appear. */
  steps: { id: string; label: string }[];
  evidenceIds: string[];
}

/** Every capability the Architect identified, none dropped; a capability whose nodes are missing still lists with no steps. */
export function capabilityRows(payload: HelicopterPayload): CapabilityRow[] {
  const labels = new Map(payload.nodes.map(item => [item.id, item.label]));
  return payload.capabilities.map((item: HelicopterCapabilityPayload) => ({
    id: item.id, label: item.label, description: item.description, needsClarification: needsClarification(item.description),
    steps: item.nodeIds.filter(id => labels.has(id)).map(id => ({ id, label: labels.get(id)! })), evidenceIds: item.evidenceIds,
  }));
}

/** Technical proof, shown only on request: the evidence records behind a set of evidence ids. */
export function evidenceFor(payload: HelicopterPayload, ids: readonly string[]): EvidenceRef[] {
  const known = new Map(payload.evidence.map(item => [item.id, item]));
  return ids.map(id => known.get(id)).filter((item): item is EvidenceRef => !!item);
}

/** Business counts for the header strip. */
export function helicopterSummary(payload: HelicopterPayload): { journeys: number; steps: number; capabilities: number; unclear: number } {
  const journeys = payload.nodes.filter(item => item.kind === "businessGroup").length;
  const unclearNodes = payload.nodes.filter(item => needsClarification(item.description)).length;
  const unclearEdges = payload.edges.filter(item => item.certainty === "unknown").length;
  const unclearCapabilities = payload.capabilities.filter(item => needsClarification(item.description)).length;
  return { journeys, steps: payload.nodes.length - journeys, capabilities: payload.capabilities.length, unclear: unclearNodes + unclearEdges + unclearCapabilities };
}
