import { SCHEMA, clean, containsSecret, list, object, openRequest, producerFields, publishResult, text, validRelativePath } from './omp-insight-base.mjs';
import { citationContext } from './omp-source-snapshot.mjs';

// Validation and storage of the three visualization results (`systemOverview`, `structureCurrent`, `structureProposed`).
// The Rust service is authoritative and re-validates the stored file; these checks keep a bad submission from being
// published at all. Nothing is silently trimmed: every violation is a precise, actionable error.
export const SYSTEM_NODE_KINDS = ['application', 'logicGroup', 'actor', 'service', 'sidecar', 'cache', 'database', 'queue', 'storage', 'external', 'entrypoint'];
export const SYSTEM_RELATIONS = ['calls', 'imports', 'includes', 'reexports', 'references', 'renders', 'requests', 'responds', 'publishes', 'subscribes', 'reads', 'writes', 'authorizes'];
export const CERTAINTIES = ['declared', 'inferred', 'unknown'];
export const COVERAGE_STATUSES = ['inspected', 'notFound', 'unresolved', 'excluded'];
export const MAPPING_OPERATIONS = ['unchanged', 'rename', 'move', 'split', 'merge', 'create', 'remove'];
export const LIMITS = { nodes: 2000, edges: 5000, evidence: 2000, entries: 2000, conventions: 500, mappings: 5000, paths: 5000, conflicts: 200, limitations: 100, coverage: 2000, label: 200, description: 2000, id: 300 };
const HEX64 = /^[0-9a-f]{64}$/;

function exactKeys(value, keys, label) {
  if (!object(value)) throw new Error(`${label} must be an object`);
  const unknown = Object.keys(value).filter(key => !keys.includes(key));
  const missing = keys.filter(key => !(key in value));
  if (unknown.length) throw new Error(`${label} has unknown field(s): ${unknown.join(', ')}`);
  if (missing.length) throw new Error(`${label} is missing field(s): ${missing.join(', ')}`);
}

// A raw URL can carry credentials, tokens or private paths: only `scheme://host[:port]` is acceptable in published text.
const URL_PATTERN = /\b[a-z][a-z0-9+.-]{1,20}:\/\/([^\s'"<>]*)/gi;
function words(value, max, label, { required = false, nullable = false } = {}) {
  if (value === null && nullable) return null;
  if (!text(value, max, required)) throw new Error(`${label} must be ${required ? 'non-empty ' : ''}text of at most ${max} characters`);
  if (!clean(value)) throw new Error(`${label} contains invalid characters`);
  if (containsSecret(value)) throw new Error(`${label} looks like a secret or credential. Use names only, never values.`);
  for (const match of value.matchAll(URL_PATTERN)) {
    const rest = match[1];
    const authority = rest.split(/[/?#]/)[0];
    const tail = rest.slice(authority.length);
    if (authority.includes('@') || (tail !== '' && tail !== '/')) throw new Error(`${label} contains a raw URL. Use the service name, config key or a plain hostname instead.`);
  }
  return value;
}

function identifier(value, label, seen) {
  words(value, LIMITS.id, label, { required: true });
  if (seen.has(value)) throw new Error(`${label} "${value}" is duplicated`);
  seen.add(value);
}

function boundedList(value, max, label) {
  if (!list(value, max)) throw new Error(`${label} must be an array of at most ${max} items`);
  return value;
}

function citations(ids, evidenceIds, label) {
  boundedList(ids, 200, `${label} evidenceIds`).forEach(id => {
    if (typeof id !== 'string' || !evidenceIds.has(id)) throw new Error(`${label} cites unknown evidence "${String(id).slice(0, 80)}"`);
  });
  if (new Set(ids).size !== ids.length) throw new Error(`${label} repeats an evidence id`);
}

/** Evidence records are checked against the immutable snapshot: inventoried path, whole-file hash and a line range inside the file. */
function validateEvidence(payload, cite) {
  const seen = new Set();
  boundedList(payload.evidence, LIMITS.evidence, 'evidence').forEach((item, index) => {
    const label = `evidence[${index}]`;
    exactKeys(item, ['id', 'path', 'lineStart', 'lineEnd', 'contentHash'], label);
    identifier(item.id, `${label}.id`, seen);
    if (!validRelativePath(item.path) || item.path.includes('\\')) throw new Error(`${label}.path must be a clean relative path with forward slashes`);
    if (!Number.isSafeInteger(item.lineStart) || !Number.isSafeInteger(item.lineEnd) || item.lineStart < 1 || item.lineEnd < item.lineStart) throw new Error(`${label} needs 1-based lineStart <= lineEnd`);
    if (typeof item.contentHash !== 'string' || !HEX64.test(item.contentHash)) throw new Error(`${label}.contentHash must be the lowercase SHA-256 of the whole file`);
    const file = cite.file(item.path);
    if (file.sha256 !== item.contentHash) throw new Error(`${label}.contentHash does not match the snapshot file ${item.path} (sha256 ${file.sha256})`);
    if (item.lineEnd > file.lines) throw new Error(`${label} cites lines ${item.lineStart}-${item.lineEnd}, but ${item.path} has ${file.lines} line(s)`);
  });
  return seen;
}

function common(payload, request) {
  if (payload.schemaVersion !== SCHEMA) throw new Error('schemaVersion must be 1');
  if (payload.snapshotId !== request.snapshotId) throw new Error(`snapshotId must be "${request.snapshotId}" (the app supplies it)`);
  boundedList(payload.limitations, LIMITS.limitations, 'limitations').forEach((item, index) => words(item, LIMITS.description, `limitations[${index}]`, { required: true }));
}

// ---- systemOverview --------------------------------------------------------------------------------------------

const SYSTEM_KEYS = ['requestId', 'schemaVersion', 'snapshotId', 'nodes', 'edges', 'evidence', 'coverage', 'limitations'];

export function validateSystemOverview(payload, _folder, { request, requestDir, dir }) {
  exactKeys(payload, SYSTEM_KEYS, 'the system overview');
  common(payload, request);
  const cite = citationContext(dir, requestDir, request);
  const evidenceIds = validateEvidence(payload, cite);
  const nodeIds = new Set();
  boundedList(payload.nodes, LIMITS.nodes, 'nodes').forEach((node, index) => {
    const label = `nodes[${index}]`;
    exactKeys(node, ['id', 'kind', 'label', 'parentId', 'description', 'evidenceIds'], label);
    identifier(node.id, `${label}.id`, nodeIds);
    if (!SYSTEM_NODE_KINDS.includes(node.kind)) throw new Error(`${label}.kind must be one of ${SYSTEM_NODE_KINDS.join(', ')}`);
    words(node.label, LIMITS.label, `${label}.label`, { required: true });
    words(node.description, LIMITS.description, `${label}.description`);
    if (node.parentId !== null && typeof node.parentId !== 'string') throw new Error(`${label}.parentId must be null or a node id`);
    citations(node.evidenceIds, evidenceIds, label);
  });
  const parents = new Map(payload.nodes.map(node => [node.id, node.parentId]));
  for (const node of payload.nodes) {
    if (node.parentId !== null && !parents.has(node.parentId)) throw new Error(`node "${node.id}" has unknown parent "${node.parentId}"`);
    for (let step = node.parentId, count = 0; step !== null; step = parents.get(step), count++) {
      if (step === node.id || count > parents.size) throw new Error(`node "${node.id}" is part of a containment cycle`);
    }
  }
  const edgeIds = new Set();
  boundedList(payload.edges, LIMITS.edges, 'edges').forEach((edge, index) => {
    const label = `edges[${index}]`;
    exactKeys(edge, ['id', 'sourceId', 'targetId', 'relation', 'label', 'evidenceIds', 'certainty', 'staticFactId'], label);
    identifier(edge.id, `${label}.id`, edgeIds);
    if (!nodeIds.has(edge.sourceId) || !nodeIds.has(edge.targetId)) throw new Error(`${label} has a dangling endpoint (${String(edge.sourceId).slice(0, 80)} -> ${String(edge.targetId).slice(0, 80)})`);
    if (!SYSTEM_RELATIONS.includes(edge.relation)) throw new Error(`${label}.relation must be one of ${SYSTEM_RELATIONS.join(', ')}`);
    words(edge.label, LIMITS.label, `${label}.label`);
    citations(edge.evidenceIds, evidenceIds, label);
    if (!CERTAINTIES.includes(edge.certainty)) throw new Error(`${label}.certainty must be declared, inferred or unknown`);
    if (edge.staticFactId !== null && !text(edge.staticFactId, LIMITS.id, true)) throw new Error(`${label}.staticFactId must be null or an indexed fact id`);
    const known = edge.staticFactId !== null && (cite.tables.symbolsById.has(edge.staticFactId) || cite.tables.edgesById.has(edge.staticFactId));
    if (edge.staticFactId !== null && !known) throw new Error(`${label}.staticFactId does not match an indexed declaration or edge`);
    // "declared" is only earned by an indexed fact; a cited path or a description never promotes an edge.
    if (edge.certainty === 'declared' && !known) throw new Error(`${label} is "declared" without a staticFactId; mark it inferred or unknown unless an indexed declaration or edge proves it`);
  });
  boundedList(payload.coverage, LIMITS.coverage, 'coverage').forEach((entry, index) => {
    const label = `coverage[${index}]`;
    exactKeys(entry, ['area', 'status', 'evidenceIds', 'notes'], label);
    words(entry.area, LIMITS.label, `${label}.area`, { required: true });
    if (!COVERAGE_STATUSES.includes(entry.status)) throw new Error(`${label}.status must be one of ${COVERAGE_STATUSES.join(', ')}`);
    words(entry.notes, LIMITS.description, `${label}.notes`);
    citations(entry.evidenceIds, evidenceIds, label);
    if (entry.status === 'inspected' && entry.evidenceIds.length === 0) throw new Error(`${label} is "inspected" without evidence`);
  });
  if (payload.coverage.length === 0) throw new Error('coverage must account for the inventoried areas (inspected, notFound, unresolved or excluded)');
  if (payload.nodes.length === 0 && payload.limitations.length === 0) throw new Error('an empty system overview must state its limitations');
}

const systemResult = (payload, actor, { request }) => ({
  requestId: payload.requestId, kind: 'systemOverview', ...producerFields(actor),
  schemaVersion: SCHEMA, snapshotId: request.snapshotId,
  nodes: payload.nodes.map(({ id, kind, label, parentId, description, evidenceIds }) => ({ id, kind, label, parentId, description, evidenceIds: [...evidenceIds] })),
  edges: payload.edges.map(({ id, sourceId, targetId, relation, label, evidenceIds, certainty, staticFactId }) => ({ id, sourceId, targetId, relation, label, evidenceIds: [...evidenceIds], certainty, staticFactId })),
  evidence: payload.evidence.map(({ id, path: file, lineStart, lineEnd, contentHash }) => ({ id, path: file, lineStart, lineEnd, contentHash })),
  coverage: payload.coverage.map(({ area, status, evidenceIds, notes }) => ({ area, status, evidenceIds: [...evidenceIds], notes })),
  limitations: [...payload.limitations], completedAt: Date.now(),
});

export const saveSystemOverview = (dir, payload, actor) => publishResult(dir, payload, actor, 'systemOverview', validateSystemOverview, systemResult);

// ---- repository structure --------------------------------------------------------------------------------------

const STRUCTURE_KEYS = ['requestId', 'schemaVersion', 'snapshotId', 'sourceId', 'sourceHash', 'baseRequestId', 'supersedesRequestId', 'entries', 'conventions', 'mappings', 'conflicts', 'evidence', 'limitations'];
const RESERVED = /^(?:con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\..*)?$/i;

// A proposed destination must be creatable on Windows as well: no device names, trailing dots/spaces or reserved characters.
function destination(value, label) {
  if (typeof value !== 'string' || !validRelativePath(value) || value.includes('\\')) throw new Error(`${label} must be a clean relative path with forward slashes`);
  if (!clean(value) || containsSecret(value)) throw new Error(`${label} contains invalid characters`);
  for (const part of value.split('/')) {
    if (/[<>:"|?*]/.test(part) || /[. ]$/.test(part) || RESERVED.test(part)) throw new Error(`${label} "${value}" is not a valid Windows path component ("${part}")`);
  }
  return value;
}

const fold = value => value.normalize('NFC').toLowerCase();

function distinctDestinations(values, label) {
  const exact = new Set();
  const folded = new Map();
  for (const value of values) {
    if (exact.has(value)) throw new Error(`${label}: duplicate destination "${value}"`);
    exact.add(value);
    const key = fold(value);
    if (folded.has(key)) throw new Error(`${label}: "${value}" collides with "${folded.get(key)}" on a case-insensitive filesystem`);
    folded.set(key, value);
  }
}

function inventoryPath(tables, value, label) {
  if (typeof value !== 'string' || !(tables.filesByPath.has(value) || tables.directories.has(value))) throw new Error(`${label} "${String(value).slice(0, 120)}" is not in the source inventory`);
  return value;
}

const SHAPE = {
  unchanged: (from, to) => from.length >= 1 && from.length === to.length && from.every((item, index) => item === to[index]),
  rename: (from, to) => from.length === 1 && to.length === 1 && from[0] !== to[0],
  move: (from, to) => from.length === 1 && to.length === 1 && from[0] !== to[0],
  split: (from, to) => from.length === 1 && to.length >= 2,
  merge: (from, to) => from.length >= 2 && to.length === 1,
  create: (from, to) => from.length === 0 && to.length >= 1,
  remove: (from, to) => from.length >= 1 && to.length === 0,
};

export function validateRepositoryStructure(payload, _folder, { request, requestDir, dir }) {
  exactKeys(payload, STRUCTURE_KEYS, 'the repository structure');
  common(payload, request);
  const proposed = request.kind === 'structureProposed';
  const expected = { sourceId: request.sourceId ?? null, sourceHash: request.sourceHash ?? null, baseRequestId: request.baseRequestId ?? null, supersedesRequestId: request.supersedesRequestId ?? null };
  for (const [key, value] of Object.entries(expected)) {
    if (payload[key] !== value) throw new Error(`${key} must be ${value === null ? 'null' : `"${value}"`} (the app derives it from the request)`);
  }
  const cite = citationContext(dir, requestDir, request);
  const evidenceIds = validateEvidence(payload, cite);
  const ids = new Set();
  boundedList(payload.entries, LIMITS.entries, 'entries').forEach((entry, index) => {
    const label = `entries[${index}]`;
    exactKeys(entry, ['id', 'parentId', 'label', 'path', 'pattern', 'members', 'purpose', 'evidenceIds'], label);
    identifier(entry.id, `${label}.id`, ids);
    words(entry.label, LIMITS.label, `${label}.label`, { required: true });
    words(entry.purpose, LIMITS.description, `${label}.purpose`);
    words(entry.pattern, LIMITS.label, `${label}.pattern`, { nullable: true });
    if (entry.parentId !== null && typeof entry.parentId !== 'string') throw new Error(`${label}.parentId must be null or an entry id`);
    if (entry.path !== null) { if (proposed) destination(entry.path, `${label}.path`); else inventoryPath(cite.tables, entry.path, `${label}.path`); }
    boundedList(entry.members, LIMITS.paths, `${label}.members`).forEach((member, at) => {
      if (proposed) destination(member, `${label}.members[${at}]`); else inventoryPath(cite.tables, member, `${label}.members[${at}]`);
    });
    if (!proposed && new Set(entry.members).size !== entry.members.length) throw new Error(`${label}.members repeats an inventory path`);
    citations(entry.evidenceIds, evidenceIds, label);
  });
  const parents = new Map(payload.entries.map(entry => [entry.id, entry.parentId]));
  for (const entry of payload.entries) {
    if (entry.parentId !== null && !parents.has(entry.parentId)) throw new Error(`entry "${entry.id}" has unknown parent "${entry.parentId}"`);
    for (let step = entry.parentId, count = 0; step !== null; step = parents.get(step), count++) {
      if (step === entry.id || count > parents.size) throw new Error(`entry "${entry.id}" is part of a parent cycle`);
    }
  }
  // A "current" revision describes the repository as it is; the target-versus-current fields belong to a proposal.
  if (proposed) distinctDestinations(payload.entries.flatMap(entry => [...(entry.path === null ? [] : [entry.path]), ...entry.members]), 'entries');
  boundedList(payload.conventions, LIMITS.conventions, 'conventions').forEach((item, index) => {
    const label = `conventions[${index}]`;
    exactKeys(item, ['scope', 'current', 'proposed', 'rationale', 'evidenceIds'], label);
    words(item.scope, LIMITS.label, `${label}.scope`, { required: true });
    words(item.current, LIMITS.description, `${label}.current`, { required: true });
    words(item.proposed, LIMITS.description, `${label}.proposed`, { nullable: true });
    words(item.rationale, LIMITS.description, `${label}.rationale`);
    if (!proposed && item.proposed !== null) throw new Error(`${label}.proposed must be null in a current-structure analysis`);
    citations(item.evidenceIds, evidenceIds, label);
  });
  if (!proposed && payload.mappings.length) throw new Error('mappings must be empty in a current-structure analysis');
  const sources = new Set();
  const targets = [];
  boundedList(payload.mappings, LIMITS.mappings, 'mappings').forEach((mapping, index) => {
    const label = `mappings[${index}]`;
    exactKeys(mapping, ['fromPaths', 'toPaths', 'operation', 'rationale'], label);
    if (!MAPPING_OPERATIONS.includes(mapping.operation)) throw new Error(`${label}.operation must be one of ${MAPPING_OPERATIONS.join(', ')}`);
    words(mapping.rationale, LIMITS.description, `${label}.rationale`);
    boundedList(mapping.fromPaths, LIMITS.paths, `${label}.fromPaths`).forEach((item, at) => {
      inventoryPath(cite.tables, item, `${label}.fromPaths[${at}]`);
      if (sources.has(item)) throw new Error(`${label}.fromPaths[${at}] "${item}" is already mapped by another entry`);
      sources.add(item);
    });
    boundedList(mapping.toPaths, LIMITS.paths, `${label}.toPaths`).forEach((item, at) => { destination(item, `${label}.toPaths[${at}]`); targets.push(item); });
    if (!SHAPE[mapping.operation](mapping.fromPaths, mapping.toPaths)) throw new Error(`${label} does not fit operation "${mapping.operation}" (splits and merges must be explicit, not one-to-one)`);
  });
  distinctDestinations(targets, 'mappings');
  boundedList(payload.conflicts, LIMITS.conflicts, 'conflicts').forEach((item, index) => words(item, LIMITS.description, `conflicts[${index}]`, { required: true }));
  if (!proposed && payload.conflicts.length) throw new Error('conflicts describe the target document; a current-structure analysis has none');
  if (payload.entries.length === 0 && payload.limitations.length === 0) throw new Error('an empty structure must state its limitations');
}

const structureResult = (payload, actor, { request }) => ({
  requestId: payload.requestId, kind: request.kind, ...producerFields(actor),
  schemaVersion: SCHEMA, snapshotId: request.snapshotId,
  sourceId: request.sourceId ?? null, sourceHash: request.sourceHash ?? null, baseRequestId: request.baseRequestId ?? null, supersedesRequestId: request.supersedesRequestId ?? null,
  entries: payload.entries.map(({ id, parentId, label, path: file, pattern, members, purpose, evidenceIds }) => ({ id, parentId, label, path: file, pattern, members: [...members], purpose, evidenceIds: [...evidenceIds] })),
  conventions: payload.conventions.map(({ scope, current, proposed, rationale, evidenceIds }) => ({ scope, current, proposed, rationale, evidenceIds: [...evidenceIds] })),
  mappings: payload.mappings.map(({ fromPaths, toPaths, operation, rationale }) => ({ fromPaths: [...fromPaths], toPaths: [...toPaths], operation, rationale })),
  conflicts: [...payload.conflicts],
  evidence: payload.evidence.map(({ id, path: file, lineStart, lineEnd, contentHash }) => ({ id, path: file, lineStart, lineEnd, contentHash })),
  limitations: [...payload.limitations], completedAt: Date.now(),
});

/** One submission tool serves both structure kinds; the kind comes from the admitted request, never from the model. */
export function saveRepositoryStructure(dir, payload, actor) {
  if (!object(payload) || typeof payload.requestId !== 'string') throw new Error('invalid request id');
  const { request } = openRequest(dir, payload.requestId, actor, ['structureCurrent', 'structureProposed']);
  return publishResult(dir, payload, actor, request.kind, validateRepositoryStructure, structureResult);
}

// ---- console replay --------------------------------------------------------------------------------------------

const LISTED = 80;
const more = (items, shown) => (items.length > shown ? [`  ... ${items.length - shown} more in the saved result`] : []);

/** Replay text of a saved visualization result (a summary: the diagram itself is shown in the app). */
export function formatVisualizationResult(request, result) {
  const out = [];
  if (request.kind === 'systemOverview') {
    out.push('System overview (Architect analysis of the indexed source; not verified runtime traffic)', `Project: ${request.folder}`, `Source snapshot: ${result.snapshotId}`, '');
    const label = new Map(result.nodes.map(node => [node.id, node.label]));
    out.push(`Elements (${result.nodes.length}):`, ...result.nodes.slice(0, LISTED).map(node => `  - [${node.kind}] ${node.label}${node.parentId ? ` (in ${label.get(node.parentId) ?? node.parentId})` : ''}`), ...more(result.nodes, LISTED));
    out.push('', `Relationships (${result.edges.length}):`, ...result.edges.slice(0, LISTED).map(edge => `  - ${label.get(edge.sourceId) ?? edge.sourceId} -${edge.relation}-> ${label.get(edge.targetId) ?? edge.targetId} [${edge.certainty}]${edge.label ? `: ${edge.label}` : ''}`), ...more(result.edges, LISTED));
    out.push('', `Coverage (${result.coverage.length}):`, ...result.coverage.slice(0, LISTED).map(item => `  - ${item.area}: ${item.status}${item.notes ? ` - ${item.notes}` : ''}`), ...more(result.coverage, LISTED));
  } else {
    out.push(request.kind === 'structureProposed'
      ? 'Proposed repository structure (a visualization of the target Markdown; the Markdown remains the source of truth)'
      : 'Current repository structure and naming (Architect analysis of the indexed source)', `Project: ${request.folder}`, `Source snapshot: ${result.snapshotId}`);
    if (result.sourceId) out.push(`Target document: ${result.sourceId} (sha256 ${result.sourceHash})`);
    out.push('', `Groups (${result.entries.length}):`, ...result.entries.slice(0, LISTED).map(entry => `  - ${entry.label}${entry.path ? ` (${entry.path})` : entry.pattern ? ` (${entry.pattern})` : ''}: ${entry.purpose}`), ...more(result.entries, LISTED));
    out.push('', `Conventions (${result.conventions.length}):`, ...result.conventions.slice(0, LISTED).map(item => `  - ${item.scope}: ${item.current}${item.proposed ? ` -> ${item.proposed}` : ''}`), ...more(result.conventions, LISTED));
    if (request.kind === 'structureProposed') {
      out.push('', `Mappings (${result.mappings.length}):`, ...result.mappings.slice(0, LISTED).map(item => `  - ${item.operation}: ${item.fromPaths.join(', ') || '(none)'} -> ${item.toPaths.join(', ') || '(none)'}`), ...more(result.mappings, LISTED));
      out.push('', 'Target document conflicts:', ...(result.conflicts.length ? result.conflicts.map(item => `  - ${item}`) : ['  - none reported']));
    }
  }
  out.push('', 'Limitations:', ...(result.limitations.length ? result.limitations.map(item => `  - ${item}`) : ['  - none stated']));
  return out.join('\n');
}
