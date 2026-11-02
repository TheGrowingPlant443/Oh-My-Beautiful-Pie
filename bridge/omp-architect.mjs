import { closeSync, existsSync, fstatSync, lstatSync, mkdirSync, openSync, readFileSync, realpathSync, renameSync, unlinkSync, writeFileSync } from 'node:fs';
import { createHash, randomUUID } from 'node:crypto';
import path from 'node:path';
import { createResultConsole, formatSecurityResult, formatUsageResult } from './omp-architect-console.mjs';
import { registerDependencyContextTool } from './omp-architect-context.mjs';
import { registerDependencyUpdateTool } from './omp-dependency-updates.mjs';
import { registerProjectInsightTools } from './omp-project-insight.mjs';
import { registerStructureSources } from './omp-structure-sources.mjs';
import { publishExclusively } from './omp-insight-base.mjs';

// Only The Architect registers these app-owned dependency classification and security report tools.
export const KEY = /^[0-9a-f]{16}$/;
const MAX_ITEMS = 2000;
const MAX_ID = 300;
const MAX_REASON = 200;

export function saveClassification(dir, { key, items }) {
  if (typeof key !== 'string' || !KEY.test(key)) throw new Error('invalid key');
  if (!Array.isArray(items) || items.length < 1 || items.length > MAX_ITEMS) throw new Error('invalid item');
  for (const item of items) {
    if (!item || typeof item.id !== 'string' || item.id.length < 1 || item.id.length > MAX_ID || typeof item.basic !== 'boolean') throw new Error('invalid item');
    if (item.reason !== undefined && typeof item.reason !== 'string') throw new Error('invalid item');
  }
  const folder = path.join(dir, 'classifications');
  const file = path.join(folder, `${key}.json`);
  let saved = { items: {} };
  if (existsSync(file)) {
    try {
      const parsed = JSON.parse(readFileSync(file, 'utf8'));
      if (parsed && typeof parsed === 'object' && parsed.items && typeof parsed.items === 'object' && !Array.isArray(parsed.items)) saved = { items: parsed.items };
    } catch { /* a corrupt file is replaced */ }
  }
  for (const item of items) saved.items[item.id] = { basic: item.basic, reason: (item.reason ?? '').slice(0, MAX_REASON) };
  saved.updatedAt = Date.now();
  mkdirSync(folder, { recursive: true });
  const temporary = `${file}.${process.pid}.tmp`;
  writeFileSync(temporary, JSON.stringify(saved));
  renameSync(temporary, file);
  return items.length;
}

function readItems(file) {
  try { const parsed = JSON.parse(readFileSync(file, 'utf8')); return object(parsed?.items) ? parsed.items : {}; }
  catch { return {}; }
}
function writeJson(file, value) {
  mkdirSync(path.dirname(file), { recursive: true });
  const temporary = `${file}.${process.pid}.${randomUUID()}.tmp`;
  try { writeFileSync(temporary, JSON.stringify(value)); renameSync(temporary, file); }
  catch (error) {
    try { unlinkSync(temporary); }
    catch { /* the write failure is the one to report */ }
    throw error;
  }
}
export function saveInhouse(dir, { key, items }) {
  if (typeof key !== 'string' || !KEY.test(key)) throw new Error('invalid key');
  if (!Array.isArray(items) || items.length < 1 || items.length > MAX_ITEMS) throw new Error('invalid item');
  for (const item of items) {
    if (!item || typeof item.id !== 'string' || !item.id.length || item.id.length > MAX_ID || typeof item.inhouse !== 'boolean' ||
        (item.reason !== undefined && (typeof item.reason !== 'string' || item.reason.length > MAX_REASON))) throw new Error('invalid item');
  }
  const file = path.join(dir, 'inhouse', `${key}.json`);
  const saved = readItems(file);
  for (const item of items) Object.defineProperty(saved, item.id, { value: { inhouse: item.inhouse, reason: item.reason ?? '' }, enumerable: true, configurable: true, writable: true });
  writeJson(file, { items: saved, updatedAt: Date.now() });
  return items.length;
}
const KINDS = ['backend', 'frontend', 'fullstack', 'library', 'other'];
export function saveProjectKind(dir, { key, kind, reason }) {
  if (typeof key !== 'string' || !KEY.test(key)) throw new Error('invalid key');
  if (!KINDS.includes(kind)) throw new Error('invalid kind');
  if (reason !== undefined && (typeof reason !== 'string' || reason.length > MAX_REASON)) throw new Error('invalid reason');
  writeJson(path.join(dir, 'project-kind', `${key}.json`), { kind, reason: reason ?? '', updatedAt: Date.now() });
  return kind;
}
export function rememberStandard(dir, items) {
  const files = new Map();
  let count = 0;
  for (const item of items) {
    if (item?.basic !== true || typeof item.id !== 'string' || item.id.length > MAX_ID) continue;
    const language = item.id.slice(0, item.id.indexOf(':'));
    if (!['npm', 'pypi', 'maven', 'cargo'].includes(language)) continue;
    const file = path.join(dir, 'known-standard', `${language}.json`);
    if (!files.has(file)) files.set(file, readItems(file));
    const saved = files.get(file);
    saved[item.id] = { reason: (item.reason ?? '').slice(0, MAX_REASON), addedAt: saved[item.id]?.addedAt ?? Date.now() };
    count++;
  }
  for (const [file, saved] of files) writeJson(file, { items: saved });
  return count;
}

const REQUEST_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const MAX_REQUEST_BYTES = 1024 * 1024;
const MAX_REPORT_BYTES = 8 * 1024 * 1024;
const STATUSES = new Set(['available', 'unavailable', 'rateLimited', 'error']);
const SEVERITIES = new Set(['critical', 'high', 'moderate', 'low', 'unknown']);
const APPLICABILITY = new Set(['relevant', 'notRelevant', 'unknown']);
const USAGE_STATUSES = new Set(['used', 'notObserved', 'unknown']);
const ROLES = new Set(['runtime', 'build', 'test', 'configuration', 'transitiveFeature', 'unknown']);
const SCHEMA = 2;
const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
const text = (value, max, required = false) => typeof value === 'string' && value.length <= max && (!required || value.trim().length > 0);
const nullableText = (value, max) => value === null || text(value, max);
const generationValid = value => Number.isSafeInteger(value) && value >= 0;
const list = (value, max) => Array.isArray(value) && value.length <= max;
const textList = (value, max, each) => list(value, max) && value.every(item => text(item, each, true));

// The producer tuple of the CURRENT main session: instance, generation, a hash of this launch's private bridge token and
// the native session id. It comes from the extension's own environment and tool context, never from tool parameters.
export function architectIdentity(ctx, env = process.env) {
  const token = env.OMP_MANAGER_BRIDGE_TOKEN;
  const instanceId = env.OMP_MANAGER_BRIDGE_INSTANCE_ID;
  const generation = Number(env.OMP_MANAGER_BRIDGE_GENERATION);
  const sessionId = ctx?.sessionManager?.getSessionId?.();
  if (ctx?.agent?.kind !== 'main') throw new Error('Architect tools require the main session');
  if (!text(token, 1000, true) || !text(instanceId, MAX_ID, true) || !generationValid(generation) || !text(sessionId, MAX_ID, true)) throw new Error('Architect identity unavailable');
  return { instanceId, generation, launchId: createHash('sha256').update(token).digest('hex'), sessionId };
}

function httpsUrl(value) {
  if (!text(value, 2048, true) || /[\u0000-\u0020\u007f\\]/.test(value) || !value.startsWith('https://')) return false;
  try {
    const url = new URL(value);
    return url.protocol === 'https:' && !!url.hostname && !url.username && !url.password;
  } catch { return false; }
}

function fileExists(file) {
  try { lstatSync(file); return true; }
  catch (error) {
    if (error.code === 'ENOENT') return false;
    throw error;
  }
}

function readBoundedJson(file, maximum) {
  if (!lstatSync(file).isFile() || lstatSync(file).isSymbolicLink()) throw new Error('invalid request file');
  const fd = openSync(file, 'r');
  try {
    const stat = fstatSync(fd);
    if (!stat.isFile() || stat.size > maximum) throw new Error('request file too large');
    const bytes = readFileSync(fd);
    if (bytes.length > maximum) throw new Error('request file too large');
    return JSON.parse(bytes.toString('utf8'));
  } finally { closeSync(fd); }
}

function validateRequest(request, requestId, actor, kind) {
  if (!object(actor) || !text(actor.instanceId, MAX_ID, true) || !generationValid(actor.generation) || !text(actor.launchId, MAX_ID, true) || !text(actor.sessionId, MAX_ID, true)) throw new Error('invalid Architect actor');
  if (!object(request) || request.requestId !== requestId) throw new Error('invalid admitted request');
  // Requests from before restart-safe binding can still be read, but never receive a new submission.
  if (request.schemaVersion !== SCHEMA) throw new Error('request predates restart-safe Architect binding');
  if (request.kind !== kind) throw new Error(`request is not a ${kind} request`);
  if (request.architectInstanceId !== actor.instanceId || request.architectGeneration !== actor.generation ||
      request.architectLaunchId !== actor.launchId || request.architectSessionId !== actor.sessionId) throw new Error('request targets a different Architect, generation, launch or session');
  if (!text(request.folder, 32768, true) || !path.isAbsolute(request.folder) ||
      !text(request.packageId, 1000, true) || !text(request.name, 1000, true) ||
      !['npm', 'pypi', 'cargo', 'maven'].includes(request.ecosystem) ||
      request.packageId !== `${request.ecosystem}:${request.name}` ||
      !STATUSES.has(request.newestStatus) || !nullableText(request.newestVersion, 1000) ||
      !nullableText(request.contextId, 300)) throw new Error('invalid admitted request');
  for (const versions of [request.resolvedVersions, request.declaredVersions]) {
    if (!Array.isArray(versions) || versions.length > 30000 || !versions.every(value => text(value, 1000, true))) throw new Error('invalid version evidence');
  }
  for (const url of [request.registryUrl, request.repositoryUrl]) {
    if (url !== null && !httpsUrl(url)) throw new Error('invalid admitted request URL');
  }
}

// A relative citation, never a file-read URL: no drive/absolute prefix, control characters or empty/dot/dotdot components.
export function validSourcePath(value) {
  return text(value, 1000, true) && !/[\u0000-\u001f\u007f-\u009f]/.test(value) && !value.startsWith('/') && !value.startsWith('\\') &&
    !/^[A-Za-z]:/.test(value) && value.split(/[\\/]/).every(part => part !== '' && part !== '.' && part !== '..');
}

// A cited file that does not exist cannot be followed anywhere; an existing one must stay inside the canonical project.
function insideProject(folder, relative) {
  let real;
  try { real = realpathSync(path.join(folder, relative)); }
  catch (error) {
    if (['ENOENT', 'ENOTDIR', 'ENAMETOOLONG'].includes(error.code)) return true;
    throw error;
  }
  const offset = path.relative(realpathSync(folder), real);
  return offset === '' || (offset !== '..' && !offset.startsWith(`..${path.sep}`) && !path.isAbsolute(offset));
}

function validEvidence(item, folder) {
  return object(item) && validSourcePath(item.path) && Number.isSafeInteger(item.lineStart) && item.lineStart >= 1 &&
    Number.isSafeInteger(item.lineEnd) && item.lineEnd >= item.lineStart && text(item.explanation, 4000, true) && insideProject(folder, item.path);
}
const evidenceList = (value, max, folder) => list(value, max) && value.every(item => validEvidence(item, folder));
const cleanEvidence = items => items.map(({ path: file, lineStart, lineEnd, explanation }) => ({ path: file, lineStart, lineEnd, explanation }));

function validApplicability(value, folder) {
  return object(value) && APPLICABILITY.has(value.status) && text(value.reason, 4000, true) && textList(value.conditions, 20, 2000) &&
    evidenceList(value.evidence, 20, folder) && (value.status === 'unknown' || value.evidence.length > 0);
}

function validateReport(payload, folder) {
  if (!object(payload) || !text(payload.requestId, 36) || !REQUEST_ID.test(payload.requestId)) throw new Error('invalid request id');
  if (!text(payload.summary, 20000) || !Array.isArray(payload.findings) || payload.findings.length > 100 ||
      !Array.isArray(payload.limitations) || payload.limitations.length > 50 || !payload.limitations.every(item => text(item, 2000))) throw new Error('invalid report');
  for (const finding of payload.findings) {
    if (!object(finding) || !text(finding.title, 1000, true) || !SEVERITIES.has(finding.severity) ||
        !nullableText(finding.majorVersion, 1000) || !text(finding.affectedVersions, 1000) ||
        !nullableText(finding.fixedVersion, 1000) || !nullableText(finding.advisoryId, 1000) ||
        !Array.isArray(finding.evidence) || finding.evidence.length < 1 || finding.evidence.length > 20) throw new Error('invalid finding');
    for (const evidence of finding.evidence) {
      if (!object(evidence) || !text(evidence.title, 1000, true) || !httpsUrl(evidence.url)) throw new Error('invalid HTTPS evidence');
    }
    if (!validApplicability(finding.applicability, folder)) throw new Error('invalid project applicability or source evidence');
  }
}

function validateUsage(payload, folder) {
  if (!object(payload) || !text(payload.requestId, 36) || !REQUEST_ID.test(payload.requestId)) throw new Error('invalid request id');
  if (!USAGE_STATUSES.has(payload.status) || !text(payload.summary, 20000, true) || !text(payload.purpose, 4000, true) || !ROLES.has(payload.projectRole) ||
      !textList(payload.benefits, 20, 2000) || !textList(payload.limitations, 50, 2000) || !list(payload.paths, 20) ||
      !evidenceList(payload.sourceEvidence, 50, folder)) throw new Error('invalid usage report');
  for (const item of payload.paths) {
    if (!object(item) || !list(item.packageIds, 100) || item.packageIds.length < 1 || !item.packageIds.every(id => text(id, 1000, true) && id.includes(':')) ||
        !text(item.explanation, 4000, true) || !evidenceList(item.evidence, 20, folder)) throw new Error('invalid usage dependency path');
  }
  // A dependency-chain label alone is not a source-based benefit.
  const evidence = payload.sourceEvidence.length > 0 || payload.paths.some(item => item.evidence.length > 0);
  if (payload.status === 'used' && !evidence) throw new Error('a used package needs at least one local source evidence reference');
  if (payload.status !== 'used' && payload.limitations.length < 1) throw new Error('notObserved and unknown results must state what was searched and their limitations');
}

const producerFields = actor => ({ architectInstanceId: actor.instanceId, architectGeneration: actor.generation, architectLaunchId: actor.launchId, architectSessionId: actor.sessionId });

function securityResult(payload, actor) {
  // Persist only validated fields: extra model-supplied keys cannot replace target identity or timestamps.
  return {
    requestId: payload.requestId, ...producerFields(actor), summary: payload.summary,
    findings: payload.findings.map(finding => ({
      title: finding.title, severity: finding.severity, majorVersion: finding.majorVersion,
      affectedVersions: finding.affectedVersions, fixedVersion: finding.fixedVersion, advisoryId: finding.advisoryId,
      evidence: finding.evidence.map(({ title, url }) => ({ title, url })),
      applicability: {
        status: finding.applicability.status, reason: finding.applicability.reason,
        conditions: [...finding.applicability.conditions], evidence: cleanEvidence(finding.applicability.evidence),
      },
    })),
    limitations: payload.limitations, completedAt: Date.now(),
  };
}

function usageResult(payload, actor) {
  return {
    requestId: payload.requestId, ...producerFields(actor), status: payload.status, summary: payload.summary, purpose: payload.purpose,
    projectRole: payload.projectRole, benefits: [...payload.benefits],
    paths: payload.paths.map(item => ({ packageIds: [...item.packageIds], explanation: item.explanation, evidence: cleanEvidence(item.evidence) })),
    sourceEvidence: cleanEvidence(payload.sourceEvidence), limitations: [...payload.limitations], completedAt: Date.now(),
  };
}

// The request directory must be an app-owned, non-linked directory below the fixed architect root.
function requestFolder(dir, requestId) {
  if (typeof requestId !== 'string' || !REQUEST_ID.test(requestId)) throw new Error('invalid request id');
  const root = realpathSync(dir);
  const security = path.join(root, 'security');
  const folder = path.join(security, requestId);
  for (const candidate of [security, folder]) {
    const stat = lstatSync(candidate);
    if (!stat.isDirectory() || stat.isSymbolicLink() || realpathSync(candidate) !== candidate) throw new Error('invalid request directory');
  }
  return folder;
}

// terminal.json: an explicit end (producerEnded, ...) or a completion marker means no further submission is accepted.
function assertOpen(folder) {
  const file = path.join(folder, 'terminal.json');
  if (!fileExists(file)) return;
  const marker = readBoundedJson(file, MAX_REQUEST_BYTES);
  if (marker?.status === 'error') throw new Error(`request already ended: ${typeof marker.message === 'string' ? marker.message : marker.code ?? 'error'}`);
  throw new Error('report already completed');
}

// `dir` comes only from OMP_MANAGER_ARCHITECT_DIR, never from a tool argument or project content.
function publishResult(dir, payload, actor, kind, validate, build) {
  if (!object(payload) || !text(payload.requestId, 36) || !REQUEST_ID.test(payload.requestId)) throw new Error('invalid request id');
  const folder = requestFolder(dir, payload.requestId);
  const requestFile = path.join(folder, 'request.json');
  const request = readBoundedJson(requestFile, MAX_REQUEST_BYTES);
  validateRequest(request, payload.requestId, actor, kind);
  validate(payload, request.folder);
  const bytes = JSON.stringify(build(payload, actor));
  if (Buffer.byteLength(bytes) > MAX_REPORT_BYTES) throw new Error('report file too large');
  // Exclusive publication lock covers the exists check and rename across processes. Never overwrite a report.
  publishExclusively(folder, bytes, () => {
    if (fileExists(path.join(folder, 'result.json'))) throw new Error('report already completed');
    // Check again under the publication lock: a replaced or ended request must never receive a stale report.
    validateRequest(readBoundedJson(requestFile, MAX_REQUEST_BYTES), payload.requestId, actor, kind);
    assertOpen(folder);
  });
  return { requestId: payload.requestId, saved: true };
}

export const saveSecurityReview = (dir, payload, actor) => publishResult(dir, payload, actor, 'security', validateReport, securityResult);
export const saveUsageExplanation = (dir, payload, actor) => publishResult(dir, payload, actor, 'usage', validateUsage, usageResult);

const LEGACY_APPLICABILITY = { status: 'unknown', reason: 'Saved before project-specific applicability was recorded; no source evidence is available.', conditions: [], evidence: [] };

// Replay text for an already saved result. The historical author tuple is accepted for DISPLAY only; new submissions
// always require the current producer. Nothing here accepts a path, text or prompt.
export function loadSavedResultText(dir, kind, requestId) {
  if (!['security', 'usage'].includes(kind)) throw new Error('invalid result kind');
  const folder = requestFolder(dir, requestId);
  const request = readBoundedJson(path.join(folder, 'request.json'), MAX_REQUEST_BYTES);
  if (!object(request) || request.requestId !== requestId || (request.kind ?? 'security') !== kind) throw new Error(`request is not a ${kind} request`);
  const file = path.join(folder, 'result.json');
  if (!fileExists(file)) throw new Error('there is no saved result to show yet');
  const result = readBoundedJson(file, MAX_REPORT_BYTES);
  if (!object(result) || result.requestId !== requestId) throw new Error('saved result does not match its request');
  if (kind === 'security') {
    if (Array.isArray(result.findings)) for (const finding of result.findings) if (object(finding) && finding.applicability === undefined) finding.applicability = LEGACY_APPLICABILITY;
    validateReport(result, request.folder);
    return formatSecurityResult({ ...request, contextId: request.contextId ?? null }, result);
  }
  validateUsage(result, request.folder);
  return formatUsageResult({ ...request, contextId: request.contextId ?? null }, result);
}

export function registerClassifyTool(pi, dir, options = {}) {
  pi.registerTool({
    name: 'classify_dependencies',
    label: 'Classify Dependencies',
    loadMode: 'essential',
    description: 'Record which direct dependencies of a project are BASIC (built in: framework core, language/runtime foundations, type definitions, universally required packages that were not a real design choice) and which were CHOSEN for this project (a deliberate library choice such as a charting, state or UI library). Call it with the key given in the request and one item per package id you were asked about; you may call it several times with parts of the list. Only this tool saves your answer.',
    parameters: pi.zod.object({
      key: pi.zod.string(),
      items: pi.zod.array(pi.zod.object({ id: pi.zod.string(), basic: pi.zod.boolean(), reason: pi.zod.string().optional() })),
    }),
    approval: () => 'read',
    async execute(_id, params, signal) {
      if (signal?.aborted) throw signal.reason;
      try {
        const count = saveClassification(dir, params);
        let warning = '';
        try { rememberStandard(dir, params.items); } catch (error) { warning = ` Known-standard list not updated: ${error.message ?? error}.`; }
        return { content: [{ type: 'text', text: `Saved ${count} classifications.${warning}` }] };
      }
      catch (error) { return { content: [{ type: 'text', text: String(error.message ?? error) }], isError: true }; }
    },
  });
  const z = pi.zod;
  pi.registerTool({
    name: 'classify_inhouse_packages', label: 'Classify In-house Packages', loadMode: 'essential',
    description: 'Record in-house or public judgements for the exact package ids and project key in the request. Only this tool saves your answer.',
    parameters: z.object({ key: z.string(), items: z.array(z.object({ id: z.string(), inhouse: z.boolean(), reason: z.string().optional() })) }),
    approval: () => 'read',
    async execute(_id, params, signal) {
      if (signal?.aborted) throw signal.reason;
      try { return { content: [{ type: 'text', text: `Saved ${saveInhouse(dir, params)} in-house judgements.` }] }; }
      catch (error) { return { content: [{ type: 'text', text: String(error.message ?? error) }], isError: true }; }
    },
  });
  pi.registerTool({
    name: 'classify_project_kind', label: 'Classify Project Kind', loadMode: 'essential',
    description: 'Save the project kind for the key in the request after reading local project files.',
    parameters: z.object({ key: z.string(), kind: z.enum(KINDS), reason: z.string().optional() }),
    approval: () => 'read',
    async execute(_id, params, signal) {
      if (signal?.aborted) throw signal.reason;
      try { return { content: [{ type: 'text', text: `Saved project kind ${saveProjectKind(dir, params)}.` }] }; }
      catch (error) { return { content: [{ type: 'text', text: String(error.message ?? error) }], isError: true }; }
    },
  });
  const evidenceSchema = z.object({ path: z.string(), lineStart: z.number(), lineEnd: z.number(), explanation: z.string() });
  const applicabilitySchema = z.object({
    status: z.enum([...APPLICABILITY]), reason: z.string(), conditions: z.array(z.string()), evidence: z.array(evidenceSchema),
  });
  const resultConsole = createResultConsole(pi, { identity: architectIdentity });
  for (const kind of ['security', 'usage']) resultConsole.registerKind(kind, (consoleDir, requestId) => loadSavedResultText(consoleDir, kind, requestId));
  // Saved text is printed after the result is on disk; a console that cannot print never fails the submission.
  const announce = (ctx, kind, requestId) => {
    try { resultConsole.enqueue(ctx, { kind, requestId, text: loadSavedResultText(dir, kind, requestId) }); } catch { /* popup still shows it */ }
  };
  pi.registerTool({
    name: 'submit_dependency_security_review',
    label: 'Submit Dependency Security Review',
    loadMode: 'essential',
    description: 'Save the sourced dependency security review for the exact app requestId. Investigate every project version and intervening major, distinguish security fixes from breaking changes, and note backports and limitations. Findings require HTTPS sources and a project applicability judgement (relevant, notRelevant or unknown) grounded in this project\'s own source, with project-relative path and line evidence; notRelevant needs positive evidence and unknown must say what is missing. This only writes an app-owned report; it does not edit or upgrade the project. Terminal prose alone does not submit a report.',
    parameters: z.object({
      requestId: z.string(), summary: z.string(),
      findings: z.array(z.object({
        title: z.string(), severity: z.enum([...SEVERITIES]), majorVersion: z.string().nullable(),
        affectedVersions: z.string(), fixedVersion: z.string().nullable(), advisoryId: z.string().nullable(),
        evidence: z.array(z.object({ title: z.string(), url: z.string() })),
        applicability: applicabilitySchema,
      })),
      limitations: z.array(z.string()),
    }),
    approval: () => 'read',
    async execute(_id, params, signal, _update, ctx) {
      if (signal?.aborted) throw signal.reason;
      try {
        const saved = saveSecurityReview(dir, params, architectIdentity(ctx));
        announce(ctx, 'security', saved.requestId);
        return { content: [{ type: 'text', text: `Saved dependency security review ${saved.requestId}.` }] };
      } catch (error) { return { content: [{ type: 'text', text: String(error.message ?? error) }], isError: true }; }
    },
  });
  pi.registerTool({
    name: 'submit_dependency_usage_explanation',
    label: 'Submit Dependency Usage Explanation',
    loadMode: 'essential',
    description: 'Save the explanation of how a package is used by THIS project for the exact app requestId. Trace actual source, configuration and tests, and transitive chains to a concrete feature or build/test function; cite project-relative paths and 1-based lines. status used needs local evidence; notObserved and unknown need limitations that say what was searched. Never claim the package is removable. This only writes an app-owned report; it does not edit the project. Terminal prose alone does not submit it.',
    parameters: z.object({
      requestId: z.string(), status: z.enum([...USAGE_STATUSES]), summary: z.string(), purpose: z.string(), projectRole: z.enum([...ROLES]),
      benefits: z.array(z.string()),
      paths: z.array(z.object({ packageIds: z.array(z.string()), explanation: z.string(), evidence: z.array(evidenceSchema) })),
      sourceEvidence: z.array(evidenceSchema), limitations: z.array(z.string()),
    }),
    approval: () => 'read',
    async execute(_id, params, signal, _update, ctx) {
      if (signal?.aborted) throw signal.reason;
      try {
        const saved = saveUsageExplanation(dir, params, architectIdentity(ctx));
        announce(ctx, 'usage', saved.requestId);
        return { content: [{ type: 'text', text: `Saved dependency usage explanation ${saved.requestId}.` }] };
      } catch (error) { return { content: [{ type: 'text', text: String(error.message ?? error) }], isError: true }; }
    },
  });
  registerDependencyContextTool(pi, dir, architectIdentity, resultConsole);
  registerDependencyUpdateTool(pi, dir, architectIdentity, resultConsole);
  const visualization = registerProjectInsightTools(pi, dir, architectIdentity, resultConsole, options);
  // Passive native-input capture of the human's pasted target Markdown; only these human-only controls ever read it.
  const structure = registerStructureSources(pi, dir, architectIdentity, { currentContext: options.currentContext });
  return {
    // Display-only replay of an already saved result, by UUID and kind; never starts an analysis.
    showSaved: (ctx, kind, requestId, replay) => resultConsole.showSaved(ctx, dir, kind, requestId, { replay }),
    // Human-only: submit the app's analysis-only visualization prompt for an admitted request (see omp-visualization-guard.mjs).
    promptVisualization: (ctx, payload) => visualization.promptVisualization(ctx, payload),
    // Human-only: structure_sources / structure_source_read / structure_source_select_last (see omp-structure-sources.mjs).
    structureSources: (ctx, action, payload) => structure.structureSources(ctx, action, payload),
  };
}
