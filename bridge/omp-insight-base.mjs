import { closeSync, fstatSync, lstatSync, openSync, readFileSync, readSync, readdirSync, realpathSync, renameSync, unlinkSync, writeFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import path from 'node:path';

// Shared primitives of the Architect's request-bound project-insight tools: request directories below the app-owned
// `project-insight/` root, bounded JSON reads, the evidence reader's path rules and the immutable result publisher.
// Nothing here takes a folder or an absolute path from a tool argument, executes, installs or reads the network.
export const KINDS = ['dependencyDiscovery', 'requirements', 'systemOverview', 'structureCurrent', 'structureProposed'];
export const VISUALIZATION_KINDS = ['systemOverview', 'structureCurrent', 'structureProposed'];
export const MAX_READ_BYTES = 64 * 1024;
export const MAX_LIST_ENTRIES = 200;
export const REQUEST_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
export const MAX_REQUEST_BYTES = 1024 * 1024;
export const MAX_RESULT_BYTES = 8 * 1024 * 1024;
export const SCHEMA = 1;
export const ID = 300;
export const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
export const text = (value, max, required = false) => typeof value === 'string' && value.length <= max && (!required || value.trim().length > 0);
export const nullableText = (value, max) => value === null || text(value, max);
export const list = (value, max) => Array.isArray(value) && value.length <= max;
export const textList = (value, max, each) => list(value, max) && value.every(item => text(item, each, true));
const LONE_SURROGATE = /[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/;
const CONTROL = /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/;
export const clean = value => !LONE_SURROGATE.test(value) && !CONTROL.test(value);
export const lineValid = value => value === null || (Number.isSafeInteger(value) && value >= 1);

// Directories that are build output, VCS state or dependency caches, and file names that commonly hold secrets.
const EXCLUDED_DIRECTORIES = new Set(['.git', 'node_modules', 'target', 'dist', 'build', 'out', '.venv', 'venv', 'env', '__pycache__', '.next', '.nuxt', '.svelte-kit', '.gradle', '.tox']);
const SECRET_NAME = /^(?:\.env.*|.*\.(?:pem|key|p12|pfx|jks|keystore)|id_.*|\.npmrc|\.pypirc|\.netrc)$/i;
const SAMPLE_NAME = /\.(?:example|sample)$/i;

/** True for a file or directory name the evidence reader never shows or opens. */
export function evidenceExcluded(name, directory) {
  if (directory) return EXCLUDED_DIRECTORIES.has(name.toLowerCase());
  if (/^\.env/i.test(name) && SAMPLE_NAME.test(name)) return false;
  return SECRET_NAME.test(name);
}

const SECRET_PATTERNS = [
  /-----BEGIN [A-Z ]*PRIVATE KEY-----/,
  /\b(?:ghp|gho|ghu|ghs|github_pat)_[A-Za-z0-9_]{20,}/,
  /\bAKIA[0-9A-Z]{16}\b/,
  /\bxox[baprs]-[A-Za-z0-9-]{10,}/,
  /\bsk-[A-Za-z0-9_-]{20,}/,
  /\b(?:api[_-]?key|secret|token|passw(?:or)?d|authorization|bearer|private[_-]?key)\b["']?\s*[:=]\s*["']?[^\s"'`]{8,}/i,
  /:\/\/[^\s/:@]+:[^\s/@]+@/,
];
/** Findings may name an environment variable, never carry a secret or its value. */
export const containsSecret = value => typeof value === 'string' && SECRET_PATTERNS.some(pattern => pattern.test(value));

// A relative citation, never a file-read URL: no drive/absolute prefix, control characters or empty/dot/dotdot components.
export function validRelativePath(value) {
  return text(value, 1000, true) && !/[\u0000-\u001f\u007f-\u009f]/.test(value) && !value.startsWith('/') && !value.startsWith('\\') &&
    !/^[A-Za-z]:/.test(value) && value.split(/[\\/]/).every(part => part !== '' && part !== '.' && part !== '..');
}

export function stat(file) {
  try { return lstatSync(file); }
  catch (error) { if (error.code === 'ENOENT') return null; throw error; }
}

export function readBoundedJson(file, maximum) {
  const found = stat(file);
  if (!found) throw new Error('file not found');
  if (!found.isFile() || found.isSymbolicLink()) throw new Error('invalid request file');
  const fd = openSync(file, 'r');
  try {
    const opened = fstatSync(fd);
    if (!opened.isFile() || opened.size > maximum) throw new Error('request file too large');
    const bytes = readFileSync(fd);
    if (bytes.length > maximum) throw new Error('request file too large');
    return JSON.parse(bytes.toString('utf8'));
  } finally { closeSync(fd); }
}

export const inside = (root, real) => {
  const offset = path.relative(root, real);
  return offset === '' || (offset !== '..' && !offset.startsWith(`..${path.sep}`) && !path.isAbsolute(offset));
};

/** `<config>/project-insight` next to the architect directory (which comes only from the app's environment). */
export function insightRoot(dir) { return path.join(realpathSync(path.dirname(path.resolve(dir))), 'project-insight'); }

// The request directory must be an app-owned, non-linked directory below the fixed project-insight root.
export function requestFolder(dir, requestId) {
  if (typeof requestId !== 'string' || !REQUEST_ID.test(requestId)) throw new Error('invalid request id');
  const root = insightRoot(dir);
  const folder = path.join(root, requestId);
  for (const candidate of [root, folder]) {
    let found;
    try { found = lstatSync(candidate); }
    catch (error) { throw new Error(error?.code === 'ENOENT' ? 'unknown request: the app has not admitted this request id' : 'invalid request directory'); }
    if (!found.isDirectory() || found.isSymbolicLink() || realpathSync(candidate) !== candidate) throw new Error('invalid request directory');
  }
  return folder;
}

// terminal.json or a published result means no further tool use for this request.
export function assertOpen(folder) {
  const marker = stat(path.join(folder, 'terminal.json'));
  if (marker) {
    const saved = readBoundedJson(path.join(folder, 'terminal.json'), MAX_REQUEST_BYTES);
    if (saved?.status === 'error') throw new Error(`request already ended: ${typeof saved.message === 'string' ? saved.message : saved.code ?? 'error'}`);
    throw new Error('result already completed');
  }
  if (stat(path.join(folder, 'result.json'))) throw new Error('result already completed');
}

const HEX64 = /^[0-9a-f]{64}$/;
export const isHash = value => typeof value === 'string' && HEX64.test(value);

// The source-index binding of a visualization request: the app wrote these fields, so any inconsistency means a damaged request.
function validateVisualizationRequest(request) {
  const nullable = value => value === undefined || value === null || text(value, ID, true);
  if (!text(request.snapshotId, ID, true)) throw new Error('visualization request has no source snapshot');
  if (!['sourceId', 'baseRequestId', 'supersedesRequestId'].every(key => nullable(request[key]))) throw new Error('invalid admitted request');
  if (!(request.sourceHash === undefined || request.sourceHash === null || isHash(request.sourceHash))) throw new Error('invalid admitted request');
  if (!(request.snapshotSourceHash === undefined || request.snapshotSourceHash === null || text(request.snapshotSourceHash, 128, true))) throw new Error('invalid admitted request');
  const proposed = request.kind === 'structureProposed';
  if (proposed && (!text(request.sourceId, ID, true) || !isHash(request.sourceHash))) throw new Error('a proposed structure request needs its selected source document');
  if (!proposed && (request.sourceId ?? null) !== null) throw new Error('this request kind takes no source document');
}

export function validateRequest(request, requestId, actor, kind) {
  if (!object(actor) || !text(actor.instanceId, ID, true) || !Number.isSafeInteger(actor.generation) || actor.generation < 0 || !text(actor.launchId, ID, true) || !text(actor.sessionId, ID, true)) throw new Error('invalid Architect actor');
  if (!object(request) || request.requestId !== requestId) throw new Error('invalid admitted request');
  if (request.schemaVersion !== SCHEMA) throw new Error('unsupported request schema');
  if (!KINDS.includes(request.kind)) throw new Error('invalid request kind');
  if (kind !== undefined && request.kind !== kind) throw new Error(`request is not a ${kind} request`);
  if (request.architectInstanceId !== actor.instanceId || request.architectGeneration !== actor.generation ||
      request.architectLaunchId !== actor.launchId || request.architectSessionId !== actor.sessionId) throw new Error('request targets a different Architect, generation, launch or session');
  if (!text(request.folder, 32768, true) || !path.isAbsolute(request.folder)) throw new Error('invalid admitted request');
  if (VISUALIZATION_KINDS.includes(request.kind)) validateVisualizationRequest(request);
}

/** Opens an admitted request bound to this producer. `kind` (or an array of kinds) narrows what it may be. */
export function openRequest(dir, requestId, actor, kind) {
  const folder = requestFolder(dir, requestId);
  const request = readBoundedJson(path.join(folder, 'request.json'), MAX_REQUEST_BYTES);
  const kinds = Array.isArray(kind) ? kind : [kind];
  validateRequest(request, requestId, actor, kinds.length === 1 ? kinds[0] : undefined);
  if (kinds.length > 1 && !kinds.includes(request.kind)) throw new Error(`request is not a ${kinds.join(' or ')} request`);
  return { folder, request };
}

export const producerFields = actor => ({ architectInstanceId: actor.instanceId, architectGeneration: actor.generation, architectLaunchId: actor.launchId, architectSessionId: actor.sessionId });

// ---- repo_evidence primitives ----------------------------------------------------------------------------------

// Walk a project-relative path one component at a time: a link or an excluded name anywhere on it is refused.
export function resolveEvidence(root, relative) {
  const parts = relative.split('/').filter(Boolean);
  let current = root;
  parts.forEach((part, index) => {
    const found = stat(path.join(current, part));
    if (!found) throw new Error('That path does not exist in this repository.');
    if (found.isSymbolicLink() || evidenceExcluded(part, found.isDirectory()) || (index < parts.length - 1 && !found.isDirectory())) throw new Error('That path is not available as evidence.');
    current = path.join(current, part);
  });
  if (!inside(root, realpathSync(current))) throw new Error('That path is not available as evidence.');
  return current;
}

export function normalizeRelative(value, allowRoot) {
  if (allowRoot && (value === undefined || value === '' || value === '.')) return '';
  if (!validRelativePath(value)) throw new Error('path must be a clean project-relative path');
  return value.split(/[\\/]/).join('/');
}

export function nonNegative(value, label) {
  if (!Number.isSafeInteger(value) || value < 0) throw new Error(`${label} must be a non-negative integer`);
  return value;
}

/** One page (MAX_LIST_ENTRIES) of a directory in stable name order; `nextOffset` continues the listing. */
export function listEvidence(root, relative, offset = 0) {
  nonNegative(offset, 'offset');
  const current = resolveEvidence(root, relative);
  if (!lstatSync(current).isDirectory()) throw new Error('That path is not a directory.');
  const visible = [];
  for (const entry of readdirSync(current, { withFileTypes: true }).sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0))) {
    // Links, excluded directories and secret-looking files are omitted without a trace.
    if (entry.isSymbolicLink() || (!entry.isDirectory() && !entry.isFile()) || evidenceExcluded(entry.name, entry.isDirectory())) continue;
    visible.push(entry);
  }
  const entries = visible.slice(offset, offset + MAX_LIST_ENTRIES).map(entry => {
    const item = { name: entry.name, type: entry.isDirectory() ? 'directory' : 'file' };
    if (entry.isFile()) item.size = lstatSync(path.join(current, entry.name)).size;
    return item;
  });
  const next = offset + entries.length < visible.length ? offset + entries.length : null;
  return { path: relative, offset, entries, total: visible.length, nextOffset: next, truncated: next !== null };
}

// Start on a character boundary and never cut a multi-byte sequence at the end.
export function utf8Window(buffer, length, final) {
  let start = 0;
  while (start < length && (buffer[start] & 0xc0) === 0x80) start++;
  let end = length;
  if (!final) {
    for (let back = 1; back <= 3 && end - back >= start; back++) {
      const byte = buffer[end - back];
      if ((byte & 0xc0) === 0x80) continue;
      const need = byte >= 0xf0 ? 4 : byte >= 0xe0 ? 3 : byte >= 0xc0 ? 2 : 1;
      if (need > back) end -= back;
      break;
    }
  }
  return { start, end };
}

/** 64 KiB UTF-8 window of one file. `inspect(file, absoluteStart, absoluteEnd)` may add fields (receipts, hashes). */
export function readEvidence(root, relative, offset, inspect) {
  const current = resolveEvidence(root, relative);
  const found = lstatSync(current);
  if (!found.isFile()) throw new Error('That path is not a file.');
  nonNegative(offset, 'offset');
  const fd = openSync(current, 'r');
  try {
    const opened = fstatSync(fd);
    if (!opened.isFile()) throw new Error('That path is not a file.');
    const buffer = Buffer.alloc(MAX_READ_BYTES);
    const count = offset >= opened.size ? 0 : readSync(fd, buffer, 0, MAX_READ_BYTES, offset);
    if (buffer.subarray(0, count).includes(0)) throw new Error('That file is binary and cannot be shown as text.');
    const final = offset + count >= opened.size;
    const { start, end } = utf8Window(buffer, count, final);
    const next = offset + end < opened.size ? offset + end : null;
    if (next !== null && end === start && count > 0) throw new Error('That file is not readable as UTF-8 text.');
    const result = { path: relative, size: opened.size, offset: offset + start, nextOffset: next, text: buffer.subarray(start, end).toString('utf8') };
    return inspect ? inspect(current, result, offset + start, offset + end) : result;
  } finally { closeSync(fd); }
}

// ---- immutable publication -------------------------------------------------------------------------------------

/**
 * `dir` comes only from OMP_MANAGER_ARCHITECT_DIR. Exclusive publication lock; a result is never overwritten.
 * `validate(payload, projectFolder, { request, requestDir, dir })` throws on a bad payload; `build(payload, actor, context)`
 * returns the stored result.
 */
export function publishResult(dir, payload, actor, kind, validate, build) {
  if (!object(payload) || !text(payload.requestId, 36) || !REQUEST_ID.test(payload.requestId)) throw new Error('invalid request id');
  const { folder, request } = openRequest(dir, payload.requestId, actor, kind);
  const context = { request, requestDir: folder, dir };
  validate(payload, request.folder, context);
  const bytes = JSON.stringify(build(payload, actor, context));
  if (Buffer.byteLength(bytes) > MAX_RESULT_BYTES) throw new Error('result file too large');
  const file = path.join(folder, 'result.json');
  const lock = path.join(folder, 'result.lock');
  const temporary = path.join(folder, `result.${process.pid}.${randomUUID()}.tmp`);
  const fd = openSync(lock, 'wx');
  try {
    // Checked again under the lock: an ended or replaced request must never receive a stale result.
    validateRequest(readBoundedJson(path.join(folder, 'request.json'), MAX_REQUEST_BYTES), payload.requestId, actor, kind);
    assertOpen(folder);
    writeFileSync(temporary, bytes, { flag: 'wx' });
    renameSync(temporary, file);
  } finally {
    try { unlinkSync(temporary); }
    catch (error) { if (error.code !== 'ENOENT') throw error; }
    finally {
      try { closeSync(fd); } finally { unlinkSync(lock); }
    }
  }
  return { requestId: payload.requestId, saved: true };
}
