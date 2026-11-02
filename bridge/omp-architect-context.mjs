import { closeSync, fstatSync, lstatSync, openSync, readFileSync, readSync, realpathSync } from 'node:fs';
import path from 'node:path';

// Architect-only, read-only access to the immutable dependency-context snapshots the app publishes. `dir` comes only from
// OMP_MANAGER_ARCHITECT_DIR; no tool argument is ever a path, URL or directory name other than a validated id.
export const TOOL_NAME = 'inspect_dependency_context';
export const SECTIONS = ['summary', 'packages', 'edges', 'package', 'work'];
export const OUTPUT_CAP = 256 * 1024;
const CONTEXT_ID = /^[0-9a-f]{64}$/;
const REQUEST_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const MAX_JSON_BYTES = 1024 * 1024;
const MAX_LINE_BYTES = 4 * 1024 * 1024;
const REQUEST_ROOTS = ['security', 'project-security', 'dependency-update-plans'];
const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);

function fileStat(file) {
  try { return lstatSync(file); }
  catch (error) {
    if (error.code === 'ENOENT') return null;
    throw error;
  }
}

function readBoundedJson(file, maximum = MAX_JSON_BYTES) {
  const stat = fileStat(file);
  if (!stat) throw new Error('file not found');
  if (!stat.isFile() || stat.isSymbolicLink()) throw new Error('invalid file');
  const fd = openSync(file, 'r');
  try {
    const opened = fstatSync(fd);
    if (!opened.isFile() || opened.size > maximum) throw new Error('file too large');
    const bytes = readFileSync(fd);
    if (bytes.length > maximum) throw new Error('file too large');
    return JSON.parse(bytes.toString('utf8'));
  } finally { closeSync(fd); }
}

// Streams `\n`-separated lines without loading the file; a single oversized line is an explicit error.
export function* readLines(file) {
  const fd = openSync(file, 'r');
  try {
    const chunk = Buffer.allocUnsafe(64 * 1024);
    let pending = Buffer.alloc(0);
    for (;;) {
      const count = readSync(fd, chunk, 0, chunk.length, null);
      if (count === 0) break;
      pending = Buffer.concat([pending, chunk.subarray(0, count)]);
      let start = 0;
      for (let index = pending.indexOf(10, start); index !== -1; index = pending.indexOf(10, start)) {
        yield pending.subarray(start, index).toString('utf8');
        start = index + 1;
      }
      pending = pending.subarray(start);
      if (pending.length > MAX_LINE_BYTES) throw new Error('record exceeds the maximum line size');
    }
    if (pending.length > 0) yield pending.toString('utf8');
  } finally { closeSync(fd); }
}

function safeDirectory(parent, name) {
  const candidate = path.join(parent, name);
  const stat = fileStat(candidate);
  if (!stat?.isDirectory() || stat.isSymbolicLink() || realpathSync(candidate) !== path.join(realpathSync(parent), name)) throw new Error('unknown or invalid directory');
  return candidate;
}

function pagination(params) {
  const offset = params.offset ?? 0;
  const limit = params.limit ?? 50;
  if (!Number.isSafeInteger(offset) || offset < 0) throw new Error('offset must be a nonnegative safe integer');
  if (!Number.isSafeInteger(limit) || limit < 1 || limit > 100) throw new Error('limit must be an integer from 1 to 100');
  return { offset, limit };
}

function sameProducer(request, actor) {
  return object(actor) && request.architectInstanceId === actor.instanceId && request.architectGeneration === actor.generation &&
    request.architectLaunchId === actor.launchId && request.architectSessionId === actor.sessionId &&
    typeof actor.launchId === 'string' && typeof actor.sessionId === 'string';
}

// The request roots that hold a folder for this id; the id must select exactly one.
function requestMatches(root, requestId) {
  const matches = [];
  for (const name of REQUEST_ROOTS) {
    const parent = path.join(root, name);
    const stat = fileStat(parent);
    if (!stat?.isDirectory() || stat.isSymbolicLink()) continue;
    const folderStat = fileStat(path.join(parent, requestId));
    if (!folderStat) continue;
    if (!folderStat.isDirectory() || folderStat.isSymbolicLink()) throw new Error('invalid request directory');
    matches.push({ name, folder: safeDirectory(parent, requestId) });
  }
  return matches;
}

function requestKind(name, request) {
  if (name === 'project-security') {
    if (request.kind !== 'project') throw new Error('invalid project request');
    return 'project';
  }
  if (name === 'dependency-update-plans') {
    if (request.kind !== 'updatePlan') throw new Error('invalid update-plan request');
    return 'updatePlan';
  }
  const kind = request.kind ?? 'security';
  if (!['security', 'usage'].includes(kind)) throw new Error('invalid request kind');
  return kind;
}

// Resolve a request id to its app-owned, actor-bound request. The id selects exactly one of the fixed request roots.
export function resolveRequest(dir, requestId, actor) {
  if (typeof requestId !== 'string' || !REQUEST_ID.test(requestId)) throw new Error('invalid request id');
  const matches = requestMatches(realpathSync(dir), requestId);
  if (matches.length !== 1) throw new Error('unknown request id');
  const { name, folder } = matches[0];
  const request = readBoundedJson(path.join(folder, 'request.json'));
  if (!object(request) || request.requestId !== requestId) throw new Error('request file does not match its id');
  const kind = requestKind(name, request);
  if (typeof request.contextId !== 'string' || !CONTEXT_ID.test(request.contextId)) throw new Error('this request has no saved dependency context (legacy or empty review)');
  if (!sameProducer(request, actor)) throw new Error('request is bound to a different Architect launch, session or generation');
  return { kind, folder, request, contextId: request.contextId };
}

// The active pointer names the newest scan's context for the existing Architect; it is not a path.
function activeContextId(dir) {
  const pointer = readBoundedJson(path.join(realpathSync(dir), 'active-dependency-context.json'));
  if (!object(pointer) || typeof pointer.contextId !== 'string' || !CONTEXT_ID.test(pointer.contextId)) throw new Error('no active dependency context');
  return pointer.contextId;
}

export function openContext(dir, contextId) {
  if (typeof contextId !== 'string' || !CONTEXT_ID.test(contextId)) throw new Error('invalid context id');
  const root = realpathSync(dir);
  const base = safeDirectory(root, 'dependency-context');
  const folder = safeDirectory(base, contextId);
  const summary = readBoundedJson(path.join(folder, 'summary.json'));
  if (!object(summary) || summary.id !== contextId || summary.contentHash !== contextId) throw new Error('dependency context summary does not match its id');
  for (const [file, bytes] of [['packages.jsonl', summary.packagesBytes], ['edges.jsonl', summary.edgesBytes]]) {
    const stat = fileStat(path.join(folder, file));
    if (!stat?.isFile() || stat.isSymbolicLink() || stat.size !== bytes) throw new Error('dependency context is incomplete');
  }
  return { folder, summary };
}

// A page of JSONL records. Stops before the output cap and reports nextOffset so nothing is silently shortened.
function pageRecords(file, total, offset, limit, parse) {
  const records = [];
  let size = 0;
  let index = 0;
  for (const line of readLines(file)) {
    if (index++ < offset) continue;
    if (records.length >= limit) break;
    const record = parse(line);
    const bytes = Buffer.byteLength(JSON.stringify(record));
    if (bytes > OUTPUT_CAP - 4096) throw new Error(`record ${index - 1} is ${bytes} bytes, larger than the ${OUTPUT_CAP}-byte output cap; it cannot be returned whole`);
    if (records.length > 0 && size + bytes > OUTPUT_CAP - 4096) break;
    records.push(record);
    size += bytes;
  }
  const next = offset + records.length;
  return { total, offset, count: records.length, nextOffset: next < total ? next : null, items: records };
}

function findPackage(folder, packageId) {
  const prefix = `{"id":${JSON.stringify(packageId)},`;
  for (const line of readLines(path.join(folder, 'packages.jsonl'))) {
    if (line.startsWith(prefix)) return JSON.parse(line);
  }
  throw new Error(`unknown package id: ${packageId}`);
}

function packageSection(folder, params, offset, limit) {
  if (typeof params.packageId !== 'string' || !params.packageId) throw new Error('packageId is required for section package');
  const record = findPackage(folder, params.packageId);
  const outgoing = [];
  const incoming = [];
  for (const line of readLines(path.join(folder, 'edges.jsonl'))) {
    const [from, to] = JSON.parse(line);
    if (from === record.id) outgoing.push(to);
    if (to === record.id) incoming.push(from);
  }
  const neighbours = [...outgoing.map(id => ({ direction: 'dependsOn', id })), ...incoming.map(id => ({ direction: 'requiredBy', id }))];
  const page = neighbours.slice(offset, offset + limit);
  const next = offset + page.length;
  return { package: record, outgoingCount: outgoing.length, incomingCount: incoming.length, neighbours: page, total: neighbours.length, offset, nextOffset: next < neighbours.length ? next : null };
}

// Current status of one project child, computed from app-owned files, never from the stale initial manifest.
function childStatus(dir, childId) {
  if (typeof childId !== 'string' || !REQUEST_ID.test(childId)) return 'unknown';
  const folder = path.join(realpathSync(dir), 'security', childId);
  const stat = fileStat(folder);
  if (!stat?.isDirectory() || stat.isSymbolicLink()) return 'unknown';
  if (fileStat(path.join(folder, 'result.json'))) return 'complete';
  try {
    const terminal = readBoundedJson(path.join(folder, 'terminal.json'));
    if (object(terminal) && terminal.status === 'error') return 'error';
    if (object(terminal) && terminal.status === 'complete') return 'complete';
  } catch (error) { if (error.message !== 'file not found') return 'error'; }
  return 'pending';
}

function entryStatus(dir, entry) {
  if (entry.state === 'skipped') return 'skipped';
  if (entry.state === 'failed') return 'error';
  return childStatus(dir, entry.requestId);
}

function workSection(dir, resolved, offset, limit) {
  if (resolved?.kind !== 'project') throw new Error('section work is only valid for a project review request');
  const file = path.join(resolved.folder, 'work.jsonl');
  const stat = fileStat(file);
  if (!stat?.isFile() || stat.isSymbolicLink()) throw new Error('project work manifest is missing');
  let total = 0;
  for (const line of readLines(file)) if (line.trim()) total += 1;
  const page = pageRecords(file, total, offset, limit, line => {
    const entry = JSON.parse(line);
    const status = entryStatus(dir, entry);
    return { packageId: entry.packageId, requestId: entry.requestId, state: entry.state, status, reason: entry.reason ?? null };
  });
  return { ...page, projectRequestId: resolved.request.requestId };
}

// Pure and testable: validates everything, returns the JSON-serialisable result.
export function inspectDependencyContext(dir, params, actor) {
  if (!object(params) || !SECTIONS.includes(params.section)) throw new Error(`section must be one of ${SECTIONS.join(', ')}`);
  const { offset, limit } = pagination(params);
  let resolved = null;
  let contextId;
  if (params.requestId !== undefined) {
    resolved = resolveRequest(dir, params.requestId, actor);
    contextId = resolved.contextId;
  } else {
    if (params.section === 'work') throw new Error('section work requires the project requestId');
    contextId = activeContextId(dir);
  }
  const { folder, summary } = openContext(dir, contextId);
  let body;
  if (params.section === 'summary') body = { summary };
  else if (params.section === 'packages') body = pageRecords(path.join(folder, 'packages.jsonl'), summary.packageCount, offset, limit, JSON.parse);
  else if (params.section === 'edges') body = pageRecords(path.join(folder, 'edges.jsonl'), summary.edgeCount, offset, limit, JSON.parse);
  else if (params.section === 'package') body = packageSection(folder, params, offset, limit);
  else body = workSection(dir, resolved, offset, limit);
  const result = { contextId, section: params.section, ...body };
  const text = JSON.stringify(result);
  if (Buffer.byteLength(text) > OUTPUT_CAP) throw new Error(`result is larger than the ${OUTPUT_CAP}-byte output cap; request a smaller limit`);
  return { result, text };
}

const STATUS = new Set(['pending', 'complete', 'error']);
const COVERAGE = new Set(['pending', 'complete', 'error', 'skippedInhouse']);

// Plain text for the Architect console: complete, never shortened. The console escapes control sequences.
export function formatProjectResult(state, requestId) {
  if (!object(state) || state.requestId !== requestId || !STATUS.has(state.status) || !Array.isArray(state.coverage) ||
      !state.coverage.every(item => object(item) && typeof item.packageId === 'string' && COVERAGE.has(item.status)) ||
      !Array.isArray(state.limitations)) throw new Error('invalid project review result');
  const lines = [`Project dependency security review — ${state.folder}`, `Request: ${state.requestId}`, `Status: ${state.status}`];
  if (state.contextId) lines.push(`Dependency context: ${state.contextId}`);
  if (state.completedAt) lines.push(`Completed: ${new Date(state.completedAt).toISOString()}`);
  if (state.error) lines.push(`Error: ${state.error}`);
  if (state.summary) lines.push('', state.summary);
  lines.push('', `Coverage (${state.coverage.length} direct dependencies):`);
  for (const item of state.coverage) {
    const reason = item.reason ? `: ${item.reason}` : '';
    const review = item.requestId ? ` (review ${item.requestId})` : '';
    lines.push(`- ${item.packageId} — ${item.status}${reason}${review}`);
  }
  if (state.limitations.length > 0) lines.push('', 'Limitations:', ...state.limitations.map(item => `- ${item}`));
  return lines.join('\n');
}

// Loader for the console's saved-result replay: reads the fixed app-owned project result by UUID only.
export function loadProjectResultText(dir, requestId) {
  if (typeof requestId !== 'string' || !REQUEST_ID.test(requestId)) throw new Error('invalid request id');
  const root = realpathSync(dir);
  const folder = safeDirectory(safeDirectory(root, 'project-security'), requestId);
  return formatProjectResult(readBoundedJson(path.join(folder, 'result.json'), 8 * 1024 * 1024), requestId);
}

export function registerDependencyContextTool(pi, dir, identity, resultConsole) {
  resultConsole?.registerKind('project', loadProjectResultText);
  const z = pi.zod;
  pi.registerTool({
    name: TOOL_NAME,
    label: 'Inspect Dependency Context',
    loadMode: 'essential',
    description: 'Read-only access to the complete scanned dependency graph the app saved for an analysis. Sections: summary, packages (paged JSONL of every scanned package with versions, judgements and saved metadata), edges (paged [from,to] meaning package -> its dependency), package (one package by packageId with incoming/outgoing edge counts and paged neighbours), work (project review manifest with current per-package status). Pass the requestId named in the analysis request to resolve its context; without it the newest scan is used. offset/limit page the results (limit 1-100, default 50) and nextOffset continues. Local app-owned files only: no network.',
    parameters: z.object({
      requestId: z.string().optional(), section: z.enum(SECTIONS), packageId: z.string().optional(),
      offset: z.number().optional(), limit: z.number().optional(),
    }),
    approval: () => 'read',
    async execute(_id, params, signal, _update, ctx) {
      if (signal?.aborted) throw signal.reason;
      try { return { content: [{ type: 'text', text: inspectDependencyContext(dir, params, params.requestId === undefined ? null : identity(ctx)).text }] }; }
      catch (error) { return { content: [{ type: 'text', text: String(error.message ?? error) }], isError: true }; }
    },
  });
}
