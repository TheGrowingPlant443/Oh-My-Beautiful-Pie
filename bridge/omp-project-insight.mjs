import path from 'node:path';
import { realpathSync } from 'node:fs';
import {
  ID, KINDS, MAX_LIST_ENTRIES, MAX_READ_BYTES, MAX_REQUEST_BYTES, MAX_RESULT_BYTES, VISUALIZATION_KINDS, assertOpen, clean, containsSecret, evidenceExcluded, insightRoot, inside, lineValid, list, listEvidence, normalizeRelative,
  nullableText, object, openRequest, producerFields, publishResult, readBoundedJson, readEvidence, requestFolder, stat, text, textList, validRelativePath,
} from './omp-insight-base.mjs';
import { readTargetStructure, snapshotReference, systemOverviewInspect, visualizationEvidence } from './omp-source-snapshot.mjs';
import { CERTAINTIES, COVERAGE_STATUSES, MAPPING_OPERATIONS, SYSTEM_NODE_KINDS, SYSTEM_RELATIONS, formatVisualizationResult, saveRepositoryStructure, saveSystemOverview } from './omp-system-results.mjs';
import { MAX_PROMPT_BYTES, TOOLS, createVisualizationGuard, registerVisualizationGuard } from './omp-visualization-guard.mjs';

export { KINDS, MAX_LIST_ENTRIES, MAX_READ_BYTES, containsSecret, evidenceExcluded, insightRoot, validRelativePath };

// Architect-only project-insight tools: a read-only, request-bound repository evidence reader and the producer-bound
// result submissions (dependencyDiscovery, requirements, systemOverview, structureCurrent, structureProposed). `dir` is
// OMP_MANAGER_ARCHITECT_DIR; the request directories live in its sibling `project-insight/`. No tool argument is ever a
// folder, an absolute path or a directory name other than a validated request id, and nothing here executes, installs,
// writes into the project or reads the network. Shared primitives live in omp-insight-base.mjs; the visualization
// kinds read the job's immutable source snapshot (omp-source-snapshot.mjs) and validate in omp-system-results.mjs.
export const TOOL_EVIDENCE = TOOLS.evidence;
export const TOOL_DISCOVERY = 'submit_dependency_discovery';
export const TOOL_REQUIREMENTS = 'submit_requirements_findings';
export const TOOL_SYSTEM_INSPECT = TOOLS.inspect;
export const TOOL_SYSTEM_OVERVIEW = TOOLS.systemSubmit;
export const TOOL_STRUCTURE = TOOLS.structureSubmit;
export const TOOL_TARGET = TOOLS.target;

const CATEGORIES = ['runtime', 'buildTool', 'packageManager', 'containerEngine', 'osFeature', 'systemSetting', 'executable'];
const NEEDS = ['required', 'optional', 'unknown'];
const ENVIRONMENTS = ['host', 'wsl', 'container'];

// A cited file that does not exist cannot be followed anywhere; an existing one must stay inside the canonical project.
function insideProject(folder, relative) {
  let real;
  try { real = realpathSync(path.join(folder, relative)); }
  catch (error) { if (['ENOENT', 'ENOTDIR', 'ENAMETOOLONG'].includes(error.code)) return true; throw error; }
  return inside(realpathSync(folder), real);
}

// ---- repo_evidence ---------------------------------------------------------------------------------------------

/** Reads from the repository bound to the request (never from a tool-supplied folder). Returns the JSON text. */
export function repoEvidence(dir, params, actor) {
  if (!object(params) || !['list', 'read'].includes(params.action)) throw new Error('action must be list or read');
  const { folder, request } = openRequest(dir, params.requestId, actor);
  // Visualization requests read their immutable source snapshot, never the live repository.
  if (VISUALIZATION_KINDS.includes(request.kind)) return visualizationEvidence(dir, params, actor);
  assertOpen(folder);
  const root = realpathSync(request.folder);
  if (params.action === 'list') return JSON.stringify(listEvidence(root, normalizeRelative(params.path, true), params.offset ?? 0));
  return JSON.stringify(readEvidence(root, normalizeRelative(params.path, false), params.offset ?? 0));
}

// ---- result validation -----------------------------------------------------------------------------------------

const noSecret = (...values) => { if (values.some(value => containsSecret(value))) throw new Error('A finding contains something that looks like a secret or credential. Remove values; names only.'); };

function validCitation(item, folder) {
  return object(item) && validRelativePath(item.path) && lineValid(item.line ?? null) && insideProject(folder, item.path);
}

function validateCommon(payload) {
  if (!textList(payload.searchedAreas, 50, 500) || payload.searchedAreas.length < 1) throw new Error('searchedAreas must name what was searched');
  if (!textList(payload.limitations, 50, 2000)) throw new Error('invalid limitations');
  if (!list(payload.items, 500)) throw new Error('invalid items');
  if (payload.items.length === 0 && payload.limitations.length === 0) throw new Error('an empty result must state its limitations');
  if (![...payload.searchedAreas, ...payload.limitations].every(clean)) throw new Error('invalid text');
  noSecret(...payload.searchedAreas, ...payload.limitations);
}

function validateDiscovery(payload, folder) {
  validateCommon(payload);
  for (const item of payload.items) {
    if (!object(item) || !text(item.name, ID, true) || !clean(item.name) ||
        !(item.ecosystem === null || (typeof item.ecosystem === 'string' && /^[a-z0-9][a-z0-9._+-]{0,39}$/.test(item.ecosystem))) ||
        !nullableText(item.declaredVersion, 200) || !nullableText(item.relationship, 100) || !text(item.notes, 2000) ||
        !list(item.citations, 20) || item.citations.length < 1 || !item.citations.every(citation => validCitation(citation, folder))) throw new Error('invalid dependency item');
    if (![item.declaredVersion, item.relationship, item.notes].every(value => value === null || clean(value))) throw new Error('invalid text');
    noSecret(item.name, item.declaredVersion, item.relationship, item.notes);
  }
}

function validateRequirements(payload, folder) {
  validateCommon(payload);
  const seen = new Set();
  for (const item of payload.items) {
    if (!object(item) || typeof item.id !== 'string' || !/^[a-z0-9][a-z0-9._:-]{0,89}$/.test(item.id) || seen.has(item.id) ||
        !text(item.name, 200, true) || !CATEGORIES.includes(item.category) || !NEEDS.includes(item.need) || !ENVIRONMENTS.includes(item.environment) ||
        !text(item.explanation, 4000, true) || !list(item.evidence, 10) ||
        !item.evidence.every(evidence => validCitation(evidence, folder) && (evidence.line ?? 0) <= 4294967295 && text(evidence.text, 500))) throw new Error('invalid requirement item');
    seen.add(item.id);
    if (![item.name, item.explanation, ...item.evidence.map(evidence => evidence.text)].every(clean)) throw new Error('invalid text');
    noSecret(item.name, item.explanation, ...item.evidence.map(evidence => evidence.text));
  }
}

const discoveryResult = (payload, actor) => ({
  requestId: payload.requestId, kind: 'dependencyDiscovery', ...producerFields(actor),
  items: payload.items.map(item => ({
    name: item.name, ecosystem: item.ecosystem, declaredVersion: item.declaredVersion, resolvedVersion: null, relationship: item.relationship,
    citations: item.citations.map(({ path: file, line }) => ({ path: file, line: line ?? null })), notes: item.notes,
  })),
  searchedAreas: [...payload.searchedAreas], limitations: [...payload.limitations], completedAt: Date.now(),
});

// Whatever the model says about detection, installation or setup is replaced: an Architect finding is never a measured fact.
const requirementsResult = (payload, actor) => ({
  requestId: payload.requestId, kind: 'requirements', ...producerFields(actor),
  items: payload.items.map(item => ({
    id: `architect:${item.id}`, name: item.name, category: item.category, need: item.need, environment: item.environment,
    evidence: item.evidence.map(({ path: file, line, text: excerpt }) => (line === null || line === undefined ? { path: file, text: excerpt } : { path: file, line, text: excerpt })),
    detected: { state: 'unknown' },
    install: { availability: 'unknown', note: 'Reported by The Architect; availability was not checked.' },
    explanation: item.explanation, source: 'architect', setupLink: null,
  })),
  searchedAreas: [...payload.searchedAreas], limitations: [...payload.limitations], completedAt: Date.now(),
});

// `dir` comes only from OMP_MANAGER_ARCHITECT_DIR. Results are published exclusively and never overwritten (see publishResult).
export const saveDependencyDiscovery = (dir, payload, actor) => publishResult(dir, payload, actor, 'dependencyDiscovery', validateDiscovery, discoveryResult);
export const saveRequirementsFindings = (dir, payload, actor) => publishResult(dir, payload, actor, 'requirements', validateRequirements, requirementsResult);
export { saveRepositoryStructure, saveSystemOverview };

// ---- console replay --------------------------------------------------------------------------------------------

const bullets = items => items.map(item => `  - ${item}`);

export function formatInsightResult(request, result) {
  if (VISUALIZATION_KINDS.includes(request.kind)) return formatVisualizationResult(request, result);
  const header = request.kind === 'dependencyDiscovery'
    ? `Dependency discovery (Architect findings from repository files; nothing was installed or changed)\nProject: ${request.folder}`
    : `Project requirements findings (Architect findings, unverified; the app's own detections are separate)\nProject: ${request.folder}`;
  const out = [header, ''];
  if (request.kind === 'dependencyDiscovery') {
    out.push(`Dependencies (${result.items.length}):`);
    for (const item of result.items) {
      const where = item.citations.map(citation => (citation.line ? `${citation.path}:${citation.line}` : citation.path)).join(', ');
      out.push(`  - ${item.name} [${item.ecosystem ?? 'unknown ecosystem'}] declared ${item.declaredVersion ?? 'unknown'}${item.relationship ? `, ${item.relationship}` : ''} (${where})${item.notes ? `: ${item.notes}` : ''}`);
    }
  } else {
    out.push(`Requirements (${result.items.length}):`);
    for (const item of result.items) out.push(`  - ${item.name} [${item.category}, ${item.need}, ${item.environment}]: ${item.explanation}`);
  }
  out.push('', 'Searched:', ...bullets(result.searchedAreas), '', 'Limitations:', ...(result.limitations.length ? bullets(result.limitations) : ['  - none stated']));
  return out.join('\n');
}

/** Replay text for an already saved result, by UUID and kind. The historical author tuple is accepted for DISPLAY only. */
export function loadInsightResultText(dir, kind, requestId) {
  if (!KINDS.includes(kind)) throw new Error('invalid result kind');
  const folder = requestFolder(dir, requestId);
  const request = readBoundedJson(path.join(folder, 'request.json'), MAX_REQUEST_BYTES);
  if (!object(request) || request.requestId !== requestId || request.kind !== kind) throw new Error(`request is not a ${kind} request`);
  if (!stat(path.join(folder, 'result.json'))) throw new Error('there is no saved result to show yet');
  const result = readBoundedJson(path.join(folder, 'result.json'), MAX_RESULT_BYTES);
  if (!object(result) || result.requestId !== requestId || result.kind !== kind) throw new Error('saved result does not match its request');
  return formatInsightResult(request, result);
}

// ---- registration ----------------------------------------------------------------------------------------------

/**
 * `producerFor(ctx)` returns `{ instanceId, generation, launchId, sessionId }` for the current main session, or throws.
 * `resultConsole` queues the display-only console copy of a saved result and registers its replay loader.
 * `options.submitPrompt(message, { onPromptAdmitted })` submits one native follow-up prompt with template and command
 * expansion disabled (the captured session prompt API); without it `promptVisualization` is unavailable.
 * Returns `{ promptVisualization(ctx, payload) }`, the human-only `project_visualization_prompt` control.
 */
export function registerProjectInsightTools(pi, dir, producerFor, resultConsole, options = {}) {
  for (const kind of KINDS) resultConsole?.registerKind(kind, (consoleDir, requestId) => loadInsightResultText(consoleDir, kind, requestId));
  const announce = (ctx, kind, requestId) => {
    try { resultConsole?.enqueue(ctx, { kind, requestId, text: loadInsightResultText(dir, kind, requestId) }); } catch { /* the saved dialog still shows it */ }
  };
  const z = pi.zod;
  const failure = error => ({ content: [{ type: 'text', text: String(error.message ?? error) }], isError: true });
  const citation = z.object({ path: z.string(), line: z.number().nullable() });
  pi.registerTool({
    name: TOOL_EVIDENCE,
    label: 'Read Repository Evidence',
    loadMode: 'essential',
    description: 'Read-only access to the repository named by the app request (pass its requestId). action "list" shows one directory (path empty for the repository root; pages of 200 entries, continue with nextOffset passed as offset); action "read" returns up to 64 KiB of one text file from the byte offset, with nextOffset to continue. System overview and Repository structure requests read the immutable indexed snapshot (reads also report sha256, lineStart/lineEnd and totalLines for citations; credential-looking lines are redacted). Links, .git, dependency and build-output directories and secret files (.env*, keys, certificates) are never available. Nothing is executed. This is the only way to inspect the repository for a project-insight request.',
    parameters: z.object({ requestId: z.string(), action: z.enum(['list', 'read']), path: z.string().optional(), offset: z.number().optional() }),
    approval: () => 'read',
    async execute(_id, params, signal, _update, ctx) {
      if (signal?.aborted) throw signal.reason;
      try { return { content: [{ type: 'text', text: repoEvidence(dir, params, producerFor(ctx)) }] }; }
      catch (error) { return failure(error); }
    },
  });
  pi.registerTool({
    name: TOOL_DISCOVERY,
    label: 'Submit Dependency Discovery',
    loadMode: 'essential',
    description: 'Save the dependencies you found in the repository files for the exact app requestId. Every item needs at least one project-relative citation (path and optional 1-based line); resolvedVersion is never recorded because nothing was resolved. Use null for unknown ecosystem, version or relationship; never invent versions. State what you searched and your limitations. This only writes an app-owned result; it does not edit the project. Terminal prose alone does not submit it.',
    parameters: z.object({
      requestId: z.string(),
      items: z.array(z.object({ name: z.string(), ecosystem: z.string().nullable(), declaredVersion: z.string().nullable(), relationship: z.string().nullable(), citations: z.array(citation), notes: z.string() })),
      searchedAreas: z.array(z.string()), limitations: z.array(z.string()),
    }),
    approval: () => 'read',
    async execute(_id, params, signal, _update, ctx) {
      if (signal?.aborted) throw signal.reason;
      try {
        const saved = saveDependencyDiscovery(dir, params, producerFor(ctx));
        announce(ctx, 'dependencyDiscovery', saved.requestId);
        return { content: [{ type: 'text', text: `Saved dependency discovery ${saved.requestId}.` }] };
      } catch (error) { return failure(error); }
    },
  });
  pi.registerTool({
    name: TOOL_REQUIREMENTS,
    label: 'Submit Requirements Findings',
    loadMode: 'essential',
    description: 'Save additional programs, build tools, settings and executables this project needs, found in its documentation, scripts and configuration, for the exact app requestId. Each item needs category, need, environment, an explanation and project-relative evidence. Findings are unverified: the app marks them as Architect findings and never takes detection or installation state from you. Never include secrets, tokens or environment variable values; names only. State what you searched and your limitations. Terminal prose alone does not submit it.',
    parameters: z.object({
      requestId: z.string(),
      items: z.array(z.object({
        id: z.string(), name: z.string(), category: z.enum(CATEGORIES), need: z.enum(NEEDS), environment: z.enum(ENVIRONMENTS),
        evidence: z.array(z.object({ path: z.string(), line: z.number().nullable(), text: z.string() })), explanation: z.string(),
      })),
      searchedAreas: z.array(z.string()), limitations: z.array(z.string()),
    }),
    approval: () => 'read',
    async execute(_id, params, signal, _update, ctx) {
      if (signal?.aborted) throw signal.reason;
      try {
        const saved = saveRequirementsFindings(dir, params, producerFor(ctx));
        announce(ctx, 'requirements', saved.requestId);
        return { content: [{ type: 'text', text: `Saved requirements findings ${saved.requestId}.` }] };
      } catch (error) { return failure(error); }
    },
  });

  // ---- system overview / repository structure tools and the analysis-only prompt control ----------------------
  const guard = registerVisualizationGuard(pi, createVisualizationGuard());
  const ok = body => ({ content: [{ type: 'text', text: body }] });
  pi.registerTool({
    name: TOOL_SYSTEM_INSPECT,
    label: 'Inspect Source Index',
    loadMode: 'essential',
    description: 'Read-only, paged queries over the saved source index of the snapshot bound to a System overview or Repository structure request (pass its requestId; the app supplies the snapshot, never a path). section "inventory": directory children (id = directory path, empty for the root; the first page also carries totals by language and parse status); "coverage": files that are not fully parsed plus per-language coverage and limitations; "files": all files, or with id (file id or path) that file\'s declarations; "symbols": all declarations, or with id one declaration\'s call sites; "neighbors": id of a file or declaration -> in/out edges with their evidence sites plus its unresolved sites. offset/limit (1-200) page every result (at most 1 MiB). The index records static facts only; an unresolved or dynamic site is not proof that no connection exists, and nothing here verifies runtime behaviour.',
    parameters: z.object({ requestId: z.string(), section: z.enum(['inventory', 'coverage', 'files', 'symbols', 'neighbors']), id: z.string().optional(), offset: z.number().optional(), limit: z.number().optional() }),
    approval: () => 'read',
    async execute(_id, params, signal, _update, ctx) {
      if (signal?.aborted) throw signal.reason;
      try { return ok(systemOverviewInspect(dir, params, producerFor(ctx))); }
      catch (error) { return failure(error); }
    },
  });
  pi.registerTool({
    name: TOOL_TARGET,
    label: 'Read Target Structure',
    loadMode: 'essential',
    description: 'Read the frozen target-structure Markdown the user selected for this Preview target structure request (pass its requestId; pages of 64 KiB, continue with nextOffset). That Markdown is the authoritative source of truth: never rewrite, repair or echo it into a result field. Point out any contradiction in conflicts instead.',
    parameters: z.object({ requestId: z.string(), offset: z.number().optional() }),
    approval: () => 'read',
    async execute(_id, params, signal, _update, ctx) {
      if (signal?.aborted) throw signal.reason;
      try { return ok(readTargetStructure(dir, params, producerFor(ctx))); }
      catch (error) { return failure(error); }
    },
  });
  const evidenceRecord = z.object({ id: z.string(), path: z.string(), lineStart: z.number(), lineEnd: z.number(), contentHash: z.string() });
  pi.registerTool({
    name: TOOL_SYSTEM_OVERVIEW,
    label: 'Submit System Overview',
    loadMode: 'essential',
    description: 'Save the system-level diagram for the exact app requestId (schemaVersion 1; snapshotId exactly as in the request). nodes {id,kind,label<=200,parentId|null,description<=2000,evidenceIds}: kinds application, logicGroup, actor, service, sidecar, cache, database, queue, storage, external, entrypoint. edges {id,sourceId,targetId,relation,label,evidenceIds,certainty,staticFactId|null}: relations calls, imports, includes, reexports, references, renders, requests, responds, publishes, subscribes, reads, writes, authorizes; a response is "responds" from the responder, not a new request. certainty "declared" REQUIRES staticFactId = an id of an indexed declaration or edge (from system_overview_inspect); a cited path or prose never proves it, so otherwise use "inferred" or "unknown". evidence {id,path,lineStart,lineEnd,contentHash} where contentHash is the whole-file sha256 that repo_evidence returned as sha256 and the lines exist in that file. coverage {area,status inspected|notFound|unresolved|excluded,evidenceIds,notes} must account for every inventoried source, config and docs area. No raw URLs, credentials or environment values (service names, config keys, plain hostnames only). Repository inspection does not prove live deployment or every runtime connection: say so in limitations. Limits: 2000 nodes, 5000 edges, 2000 evidence records, 8 MiB. Nothing is trimmed; violations are rejected. Terminal prose alone does not submit it.',
    parameters: z.object({
      requestId: z.string(), schemaVersion: z.number(), snapshotId: z.string(),
      nodes: z.array(z.object({ id: z.string(), kind: z.enum(SYSTEM_NODE_KINDS), label: z.string(), parentId: z.string().nullable(), description: z.string(), evidenceIds: z.array(z.string()) })),
      edges: z.array(z.object({ id: z.string(), sourceId: z.string(), targetId: z.string(), relation: z.enum(SYSTEM_RELATIONS), label: z.string(), evidenceIds: z.array(z.string()), certainty: z.enum(CERTAINTIES), staticFactId: z.string().nullable() })),
      evidence: z.array(evidenceRecord),
      coverage: z.array(z.object({ area: z.string(), status: z.enum(COVERAGE_STATUSES), evidenceIds: z.array(z.string()), notes: z.string() })),
      limitations: z.array(z.string()),
    }),
    approval: () => 'read',
    async execute(_id, params, signal, _update, ctx) {
      if (signal?.aborted) throw signal.reason;
      try {
        const saved = saveSystemOverview(dir, params, producerFor(ctx));
        announce(ctx, 'systemOverview', saved.requestId);
        return ok(`Saved system overview ${saved.requestId}.`);
      } catch (error) { return failure(error); }
    },
  });
  pi.registerTool({
    name: TOOL_STRUCTURE,
    label: 'Submit Repository Structure',
    loadMode: 'essential',
    description: 'Save the repository-structure visualization for the exact app requestId (current analysis or proposed preview; the kind comes from the request). schemaVersion 1; snapshotId, sourceId, sourceHash, baseRequestId and supersedesRequestId must be exactly the values the app gave for the request (null when it gave none). entries {id,parentId|null,label,path|null,pattern|null,members,purpose,evidenceIds}: current paths and members must exist in the source inventory; proposed paths and members are clean relative destinations (forward slashes, no duplicates, no Windows case-collisions). "pattern" is a display-only generalization, never a path. conventions {scope,current,proposed|null,rationale,evidenceIds} (proposed only for a preview). mappings {fromPaths,toPaths,operation unchanged|rename|move|split|merge|create|remove,rationale} (preview only): splits and merges are explicit, never fabricated one-to-one. conflicts: contradictions in the target Markdown, described not repaired. evidence {id,path,lineStart,lineEnd,contentHash} with the whole-file sha256 repo_evidence returned. The Markdown remains the source of truth; this diagram is an aid and does not authorize any repository change. Terminal prose alone does not submit it.',
    parameters: z.object({
      requestId: z.string(), schemaVersion: z.number(), snapshotId: z.string(), sourceId: z.string().nullable(), sourceHash: z.string().nullable(), baseRequestId: z.string().nullable(), supersedesRequestId: z.string().nullable(),
      entries: z.array(z.object({ id: z.string(), parentId: z.string().nullable(), label: z.string(), path: z.string().nullable(), pattern: z.string().nullable(), members: z.array(z.string()), purpose: z.string(), evidenceIds: z.array(z.string()) })),
      conventions: z.array(z.object({ scope: z.string(), current: z.string(), proposed: z.string().nullable(), rationale: z.string(), evidenceIds: z.array(z.string()) })),
      mappings: z.array(z.object({ fromPaths: z.array(z.string()), toPaths: z.array(z.string()), operation: z.enum(MAPPING_OPERATIONS), rationale: z.string() })),
      conflicts: z.array(z.string()), evidence: z.array(evidenceRecord), limitations: z.array(z.string()),
    }),
    approval: () => 'read',
    async execute(_id, params, signal, _update, ctx) {
      if (signal?.aborted) throw signal.reason;
      try {
        const saved = saveRepositoryStructure(dir, params, producerFor(ctx));
        announce(ctx, openRequest(dir, saved.requestId, producerFor(ctx)).request.kind, saved.requestId);
        return ok(`Saved repository structure ${saved.requestId}.`);
      } catch (error) { return failure(error); }
    },
  });

  /**
   * Human-only control `project_visualization_prompt`: validates the admitted request, then submits the backend-built
   * prompt (plus a random nonce guard) as ONE native follow-up with template and command expansion disabled. Resolves
   * once the native session has admitted it. A repeated request id is a duplicate and is never sent again.
   */
  async function promptVisualization(ctx, payload) {
    if (typeof options.submitPrompt !== 'function') throw new Error('native_prompt_unavailable: Reopen this session with a compatible OMP version.');
    if (!object(payload) || typeof payload.requestId !== 'string' || !VISUALIZATION_KINDS.includes(payload.kind) || typeof payload.message !== 'string') throw new Error('invalid visualization prompt request');
    if (!payload.message.trim() || !clean(payload.message) || Buffer.byteLength(payload.message) > MAX_PROMPT_BYTES) throw new Error(`message must be 1..${MAX_PROMPT_BYTES} bytes of text`);
    const actor = producerFor(ctx);
    const { folder, request } = openRequest(dir, payload.requestId, actor, payload.kind);
    assertOpen(folder);
    snapshotReference(dir, folder, request);
    const admission = guard.admit(ctx, { requestId: payload.requestId, kind: payload.kind, message: payload.message });
    if (admission.duplicate) return { accepted: true, duplicate: true };
    const admitted = Promise.withResolvers();
    let accepted = false;
    try {
      const running = options.submitPrompt(admission.text, { onPromptAdmitted() { accepted = true; admitted.resolve(); } });
      Promise.resolve(running).then(() => { if (!accepted) admitted.reject(new Error('The native session did not admit the visualization prompt')); }, error => { if (!accepted) admitted.reject(error); });
      await admitted.promise;
    } catch (error) {
      // A refused prompt never ran: drop its guard so a later explicit retry is a fresh admission.
      if (!accepted) guard.discard(payload.requestId);
      throw error;
    }
    return { accepted: true, duplicate: false };
  }
  return { promptVisualization, guard };
}
