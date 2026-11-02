import { appendFileSync, closeSync, fstatSync, lstatSync, openSync, readFileSync, realpathSync } from 'node:fs';
import { createHash } from 'node:crypto';
import path from 'node:path';
import {
  MAX_READ_BYTES, assertOpen, containsSecret, inside, insightRoot, isHash, listEvidence, normalizeRelative, nonNegative, object, openRequest, readBoundedJson,
  readEvidence, resolveEvidence, stat, utf8Window, validRelativePath, VISUALIZATION_KINDS,
} from './omp-insight-base.mjs';

// Architect-side access to the immutable source-index snapshot of one admitted visualization request. The app writes
// `<request dir>/index-ref.json` at admission; no tool argument ever supplies a folder, a snapshot path or an index path.
export const MAX_INDEX_BYTES = 192 * 1024 * 1024;
export const MAX_PAGE_BYTES = 1024 * 1024;
const MAX_REF_BYTES = 64 * 1024;
const MAX_WHOLE_FILE_BYTES = 64 * 1024 * 1024;
const MAX_TARGET_BYTES = 1024 * 1024;
const MAX_RECEIPT_BYTES = 8 * 1024 * 1024;
const RECEIPTS = 'evidence-receipts.jsonl';
const SECTIONS = ['inventory', 'coverage', 'files', 'symbols', 'neighbors'];

// ---- text safety -----------------------------------------------------------------------------------------------

// A raw URL may carry credentials, tokens or private paths: tool results keep only scheme and host.
const URL_PATTERN = /\b([a-z][a-z0-9+.-]{1,20}):\/\/([^\s'"<>/?#]*)[^\s'"<>]*/gi;
export function sanitizeText(value) {
  const reduced = value.replace(URL_PATTERN, (_all, scheme, authority) => `${scheme}://${authority.slice(authority.lastIndexOf('@') + 1)}`);
  return containsSecret(reduced) ? '[redacted: possible credential]' : reduced;
}
export function scrub(value) {
  if (typeof value === 'string') return sanitizeText(value);
  if (Array.isArray(value)) return value.map(scrub);
  if (object(value)) return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, scrub(item)]));
  return value;
}

// File text shown to the model: credential-looking lines are replaced, line numbers stay exact.
const REDACTED = '[redacted: possible credential]';
const KEY_BEGIN = /-----BEGIN [A-Z ]*PRIVATE KEY-----/;
const KEY_END = /-----END [A-Z ]*PRIVATE KEY-----/;
export function redactLines(body) {
  let inKey = false;
  let redacted = 0;
  const lines = body.split('\n').map(line => {
    if (KEY_BEGIN.test(line) || inKey) inKey = !KEY_END.test(line);
    else if (!containsSecret(line)) return line;
    redacted++;
    return REDACTED;
  });
  return { text: lines.join('\n'), redacted };
}

// ---- snapshot reference and saved index ------------------------------------------------------------------------

function realDirectory(candidate) {
  const found = lstatSync(candidate);
  if (!found.isDirectory() || found.isSymbolicLink()) throw new Error('invalid source snapshot');
  return realpathSync(candidate);
}

/** Reads and validates `index-ref.json`: the snapshot must be an app-owned job directory below `<config>/system-overview`. */
export function snapshotReference(dir, requestDir, request) {
  let ref;
  try { ref = readBoundedJson(path.join(requestDir, 'index-ref.json'), MAX_REF_BYTES); }
  catch { throw new Error('This request has no source snapshot reference.'); }
  if (!object(ref) || ref.snapshotId !== request.snapshotId || ![ref.snapshotRoot, ref.indexPath].every(value => typeof value === 'string' && path.isAbsolute(value))) throw new Error('invalid source snapshot reference');
  const systemRoot = realDirectory(path.join(path.dirname(insightRoot(dir)), 'system-overview'));
  const root = realDirectory(ref.snapshotRoot);
  const indexFound = lstatSync(ref.indexPath);
  if (!indexFound.isFile() || indexFound.isSymbolicLink()) throw new Error('invalid source snapshot');
  const indexPath = realpathSync(ref.indexPath);
  const job = path.dirname(root);
  const parts = path.relative(systemRoot, job).split(path.sep);
  if (!inside(systemRoot, root) || parts.length !== 2 || parts.some(part => part === '' || part === '..') || path.basename(root) !== 'source' ||
      path.basename(job) !== request.snapshotId || indexPath !== path.join(job, 'index.json')) throw new Error('The source snapshot is not an app-owned snapshot.');
  return { root, indexPath, job };
}

let indexCache = null;
const arrayOf = value => (Array.isArray(value) ? value : null);

function buildTables(raw, request) {
  const files = arrayOf(raw.files); const symbols = arrayOf(raw.symbols); const sites = arrayOf(raw.sites); const edges = arrayOf(raw.edges);
  if (!object(raw) || raw.schemaVersion !== 1 || !files || !symbols || !sites || !edges) throw new Error('The saved source index is damaged.');
  if (raw.snapshotId !== request.snapshotId) throw new Error('The saved source index belongs to a different snapshot.');
  if (typeof request.snapshotSourceHash === 'string' && raw.sourceHash !== request.snapshotSourceHash) throw new Error('The saved source index does not match the admitted source hash.');
  const tables = {
    raw, files, symbols, sites, edges,
    filesByPath: new Map(), filesById: new Map(), symbolsById: new Map(), edgesById: new Map(), sitesById: new Map(),
    edgesOut: new Map(), edgesIn: new Map(), symbolsByFile: new Map(), sitesByCaller: new Map(), sitesByFile: new Map(), directories: new Set(arrayOf(raw.directories) ?? []), tree: null,
  };
  const push = (map, key, value) => { const found = map.get(key); if (found) found.push(value); else map.set(key, [value]); };
  files.forEach(file => {
    tables.filesByPath.set(file.path, file); tables.filesById.set(file.id, file);
    const parts = file.path.split('/'); parts.pop();
    for (let count = 1; count <= parts.length; count++) tables.directories.add(parts.slice(0, count).join('/'));
  });
  symbols.forEach(symbol => { tables.symbolsById.set(symbol.id, symbol); push(tables.symbolsByFile, symbol.fileId, symbol); });
  sites.forEach(site => { tables.sitesById.set(site.id, site); push(tables.sitesByFile, site.fileId, site); if (site.callerId) push(tables.sitesByCaller, site.callerId, site); });
  edges.forEach(edge => { tables.edgesById.set(edge.id, edge); push(tables.edgesOut, edge.sourceId, edge); push(tables.edgesIn, edge.targetId, edge); });
  return tables;
}

/** The validated saved index of this request's snapshot (cached by file identity; read-only, bounded). */
export function loadIndex(dir, requestDir, request) {
  const ref = snapshotReference(dir, requestDir, request);
  const found = lstatSync(ref.indexPath);
  const key = `${ref.indexPath}:${found.size}:${found.mtimeMs}:${request.snapshotId}:${request.snapshotSourceHash ?? ''}`;
  if (indexCache?.key === key) return { ...ref, tables: indexCache.tables };
  if (found.size > MAX_INDEX_BYTES) throw new Error('The saved source index is too large to inspect.');
  const fd = openSync(ref.indexPath, 'r');
  let raw;
  try {
    const opened = fstatSync(fd);
    if (!opened.isFile() || opened.size > MAX_INDEX_BYTES) throw new Error('The saved source index is too large to inspect.');
    raw = JSON.parse(readFileSync(fd).toString('utf8'));
  } finally { closeSync(fd); }
  const tables = buildTables(raw, request);
  indexCache = { key, tables };
  return { ...ref, tables };
}

// ---- file hashes and line numbers ------------------------------------------------------------------------------

const hashes = new Map();
let held = null;

/** Whole-file SHA-256, byte length and line count of one snapshot file (the buffer of the latest file is kept for line spans). */
export function wholeFile(file, keepBuffer = false) {
  const fd = openSync(file, 'r');
  try {
    const opened = fstatSync(fd);
    if (!opened.isFile()) throw new Error('That path is not a file.');
    const key = `${file}:${opened.size}:${opened.mtimeMs}`;
    if (!keepBuffer && hashes.has(key)) return { ...hashes.get(key), key };
    if (held?.key === key) return { ...held.info, key, buffer: held.buffer };
    if (opened.size > MAX_WHOLE_FILE_BYTES) throw new Error('That file is too large to cite as evidence.');
    const buffer = readFileSync(fd);
    let lines = 0;
    for (let at = buffer.indexOf(10); at !== -1; at = buffer.indexOf(10, at + 1)) lines++;
    if (buffer.length > 0 && buffer[buffer.length - 1] !== 10) lines++;
    const info = { sha256: createHash('sha256').update(buffer).digest('hex'), size: buffer.length, lines };
    hashes.set(key, info);
    if (hashes.size > 1024) hashes.delete(hashes.keys().next().value);
    held = { key, info, buffer };
    return { ...info, key, buffer };
  } finally { closeSync(fd); }
}

function countNewlines(buffer, start, end) {
  let count = 0;
  for (let at = buffer.indexOf(10, start); at !== -1 && at < end; at = buffer.indexOf(10, at + 1)) count++;
  return count;
}

/** 1-based inclusive line span of the byte window [start, end) (an empty window reports its own line). */
export function lineSpan(buffer, start, end) {
  const first = 1 + countNewlines(buffer, 0, start);
  return { lineStart: first, lineEnd: end > start ? first + countNewlines(buffer, start, end - 1) : first };
}

// ---- repo_evidence on the immutable snapshot -------------------------------------------------------------------

function recordReceipt(requestDir, receipt) {
  const file = path.join(requestDir, RECEIPTS);
  const found = stat(file);
  if (found && (!found.isFile() || found.isSymbolicLink() || found.size > MAX_RECEIPT_BYTES)) return;
  appendFileSync(file, `${JSON.stringify(receipt)}\n`, { flag: 'a' });
}

/** repo_evidence for a visualization request: reads only the job's immutable snapshot and records a receipt per read. */
export function visualizationEvidence(dir, params, actor) {
  const { folder, request } = openRequest(dir, params.requestId, actor, VISUALIZATION_KINDS);
  assertOpen(folder);
  const ref = snapshotReference(dir, folder, request);
  if (params.action === 'list') return JSON.stringify(listEvidence(ref.root, normalizeRelative(params.path, true), params.offset ?? 0));
  const relative = normalizeRelative(params.path, false);
  nonNegative(params.offset ?? 0, 'offset');
  return JSON.stringify(readEvidence(ref.root, relative, params.offset ?? 0, (current, window, start, end) => {
    const whole = wholeFile(current, true);
    const span = lineSpan(whole.buffer, start, end);
    const shown = redactLines(window.text);
    recordReceipt(folder, { path: relative, sha256: whole.sha256, byteStart: start, byteEnd: end, ...span, at: Date.now() });
    return { ...window, text: shown.text, sha256: whole.sha256, lineStart: span.lineStart, lineEnd: span.lineEnd, totalLines: whole.lines, redactedLines: shown.redacted };
  }));
}

// ---- system_overview_inspect -----------------------------------------------------------------------------------

function directoryTree(tables) {
  if (tables.tree) return tables.tree;
  const tree = new Map([['', new Map()]]);
  const child = (parent, name, info) => {
    const key = parent === '' ? name : `${parent}/${name}`;
    const map = tree.get(parent);
    const existing = map.get(name);
    if (!existing) map.set(name, info(key));
    else if (existing.type === 'directory') existing.files++;
    return key;
  };
  for (const file of tables.files) {
    const parts = file.path.split('/');
    let parent = '';
    for (let index = 0; index < parts.length - 1; index++) {
      parent = child(parent, parts[index], key => { tree.set(key, new Map()); return { name: parts[index], path: key, type: 'directory', files: 1 }; });
    }
    tree.get(parent).set(parts[parts.length - 1], { name: parts[parts.length - 1], path: file.path, type: 'file', bytes: file.bytes, language: file.language ?? null, status: file.status });
  }
  for (const directory of tables.directories) {
    const parts = directory.split('/'); let parent = '';
    for (const part of parts) {
      const key = parent === '' ? part : `${parent}/${part}`;
      if (!tree.get(parent).has(part)) { tree.set(key, new Map()); tree.get(parent).set(part, { name: part, path: key, type: 'directory', files: 0 }); }
      parent = key;
    }
  }
  tables.tree = tree;
  return tree;
}

const countBy = (items, pick) => items.reduce((counts, item) => { const key = pick(item) ?? 'none'; counts[key] = (counts[key] ?? 0) + 1; return counts; }, {});
const fileRow = file => ({ id: file.id, path: file.path, bytes: file.bytes, sha256: file.sha256, language: file.language ?? null, status: file.status, diagnostics: file.diagnostics ?? [] });
const spanRow = span => ({ byteStart: span.byteStart, byteEnd: span.byteEnd, lineStart: span.lineStart, lineEnd: span.lineEnd });
const symbolRow = symbol => ({ id: symbol.id, fileId: symbol.fileId, path: symbol.path, language: symbol.language, region: symbol.region ?? null, ownerId: symbol.ownerId ?? null, kind: symbol.kind, name: symbol.name, qualifiedName: symbol.qualifiedName, span: spanRow(symbol.span), exported: symbol.exported ?? null });
const siteRow = site => ({ id: site.id, fileId: site.fileId, path: site.path, callerId: site.callerId ?? null, relation: site.relation, raw: site.raw ?? null, targetId: site.targetId ?? null, resolution: site.resolution, conditional: !!site.conditional, typeOnly: !!site.typeOnly, candidateIds: site.candidateIds ?? [], span: spanRow(site.span) });

function describeNode(tables, id) {
  const file = tables.filesById.get(id);
  if (file) return { id, kind: 'file', path: file.path };
  const symbol = tables.symbolsById.get(id);
  return symbol ? { id, kind: symbol.kind, path: symbol.path, name: symbol.qualifiedName || symbol.name } : { id, kind: 'unknown' };
}

function edgeItem(tables, edge, direction) {
  const evidence = (edge.evidenceIds ?? []).slice(0, 5).map(id => tables.sitesById.get(id)).filter(Boolean).map(siteRow);
  return { type: 'edge', direction, id: edge.id, relation: edge.relation, resolution: edge.resolution, source: describeNode(tables, edge.sourceId), target: describeNode(tables, edge.targetId), evidenceTotal: (edge.evidenceIds ?? []).length, evidence };
}

function page(section, snapshotId, all, offset, limit, extra = {}) {
  let items = all.slice(offset, offset + limit);
  const build = () => JSON.stringify(scrub({ section, snapshotId, total: all.length, offset, nextOffset: offset + items.length < all.length ? offset + items.length : null, ...extra, items }));
  let bytes = build();
  while (Buffer.byteLength(bytes) > MAX_PAGE_BYTES && items.length > 1) { items = items.slice(0, Math.max(1, Math.floor(items.length / 2))); bytes = build(); }
  if (Buffer.byteLength(bytes) > MAX_PAGE_BYTES) throw new Error('A single result item exceeds the 1 MiB page limit.');
  return bytes;
}

const INSPECT_KEYS = new Set(['requestId', 'section', 'id', 'offset', 'limit']);
const compareItems = (a, b) => {
  if (a.type !== b.type) return a.type === 'directory' ? -1 : 1;
  if (a.name < b.name) return -1;
  return a.name > b.name ? 1 : 0;
};

function inspectArguments(params) {
  if (!object(params) || !SECTIONS.includes(params.section)) throw new Error(`section must be one of ${SECTIONS.join(', ')}`);
  if (Object.keys(params).some(key => !INSPECT_KEYS.has(key))) throw new Error('unknown argument; only requestId, section, id, offset and limit are accepted');
  const offset = nonNegative(params.offset ?? 0, 'offset');
  const limit = params.limit ?? 50;
  if (!Number.isSafeInteger(limit) || limit < 1 || limit > 200) throw new Error('limit must be an integer from 1 to 200');
  if (params.id !== undefined && params.id !== null && (typeof params.id !== 'string' || params.id.length > 1000)) throw new Error('id must be a string');
  return { offset, limit, id: params.id ?? undefined };
}

function inspectInventory({ tables, request, offset, limit, id }) {
  const target = id === undefined || id === '' ? '' : normalizeRelative(id, false);
  const children = directoryTree(tables).get(target);
  if (!children) throw new Error('That directory is not part of the inventory.');
  const items = [...children.values()].sort(compareItems);
  const summary = target === '' && offset === 0
    ? { summary: { files: tables.files.length, directories: tables.directories.size, symbols: tables.symbols.length, sites: tables.sites.length, edges: tables.edges.length, languages: countBy(tables.files, file => file.language), statuses: countBy(tables.files, file => file.status), sourceHash: tables.raw.sourceHash } }
    : {};
  return page('inventory', request.snapshotId, items, offset, limit, { path: target, ...summary });
}

function inspectCoverage({ tables, request, offset, limit }) {
  const { raw } = tables;
  const unparsed = tables.files.filter(file => file.status !== 'parsed').map(fileRow);
  const head = offset === 0 ? { coverage: raw.coverage ?? null, limitations: raw.limitations ?? [], extractorVersions: raw.extractorVersions ?? {} } : {};
  return page('coverage', request.snapshotId, unparsed, offset, limit, head);
}

function inspectFiles({ tables, request, offset, limit, id }) {
  if (id === undefined) return page('files', request.snapshotId, tables.files.map(fileRow), offset, limit);
  const file = tables.filesById.get(id) ?? tables.filesByPath.get(id);
  if (!file) throw new Error('No such file in the source index.');
  return page('files', request.snapshotId, (tables.symbolsByFile.get(file.id) ?? []).map(symbolRow), offset, limit, { file: fileRow(file), siteTotal: (tables.sitesByFile.get(file.id) ?? []).length });
}

function inspectSymbols({ tables, request, offset, limit, id }) {
  if (id === undefined) return page('symbols', request.snapshotId, tables.symbols.map(symbolRow), offset, limit);
  const symbol = tables.symbolsById.get(id);
  if (!symbol) throw new Error('No such symbol in the source index.');
  return page('symbols', request.snapshotId, (tables.sitesByCaller.get(symbol.id) ?? []).map(siteRow), offset, limit, { symbol: symbolRow(symbol) });
}

function inspectNeighbors({ tables, request, offset, limit, id }) {
  if (id === undefined || !(tables.filesById.has(id) || tables.symbolsById.has(id))) throw new Error('neighbors needs the id of an indexed file or symbol.');
  const items = [
    ...(tables.edgesOut.get(id) ?? []).map(edge => edgeItem(tables, edge, 'out')),
    ...(tables.edgesIn.get(id) ?? []).map(edge => edgeItem(tables, edge, 'in')),
    // Recognized call sites without a resolved target stay visible; an absent edge is not proof of "no connection".
    ...(tables.sitesByCaller.get(id) ?? tables.sitesByFile.get(id)?.filter(site => !site.callerId) ?? []).filter(site => !site.targetId).map(site => ({ type: 'unresolvedSite', ...siteRow(site) })),
  ];
  return page('neighbors', request.snapshotId, items, offset, limit, { node: describeNode(tables, id) });
}

const INSPECTORS = { inventory: inspectInventory, coverage: inspectCoverage, files: inspectFiles, symbols: inspectSymbols, neighbors: inspectNeighbors };

/** Bounded, read-only queries over the saved source index of this request's snapshot. Returns the JSON text. */
export function systemOverviewInspect(dir, params, actor) {
  const { offset, limit, id } = inspectArguments(params);
  const { folder, request } = openRequest(dir, params.requestId, actor, VISUALIZATION_KINDS);
  assertOpen(folder);
  const { tables } = loadIndex(dir, folder, request);
  return INSPECTORS[params.section]({ tables, request, offset, limit, id });
}

// ---- read_target_structure -------------------------------------------------------------------------------------

/** One 64 KiB page of the frozen target Markdown copied to this request at admission. Returns the JSON text. */
export function readTargetStructure(dir, params, actor) {
  if (!object(params) || Object.keys(params).some(key => !['requestId', 'offset'].includes(key))) throw new Error('unknown argument; only requestId and offset are accepted');
  const offset = nonNegative(params.offset ?? 0, 'offset');
  const { folder, request } = openRequest(dir, params.requestId, actor, 'structureProposed');
  assertOpen(folder);
  const file = path.join(folder, 'target.md');
  const found = stat(file);
  if (!found?.isFile() || found.isSymbolicLink()) throw new Error('This request has no target Markdown.');
  const fd = openSync(file, 'r');
  let bytes;
  try {
    const opened = fstatSync(fd);
    if (!opened.isFile() || opened.size > MAX_TARGET_BYTES) throw new Error('The target Markdown is invalid.');
    bytes = readFileSync(fd);
  } finally { closeSync(fd); }
  // The admitted source document is immutable: any difference from its recorded hash means it was tampered with.
  if (!isHash(request.sourceHash) || createHash('sha256').update(bytes).digest('hex') !== request.sourceHash) throw new Error('The target Markdown does not match its admitted hash.');
  const count = Math.max(0, Math.min(MAX_READ_BYTES, bytes.length - offset));
  const window = bytes.subarray(offset, offset + count);
  const { start, end } = utf8Window(window, count, offset + count >= bytes.length);
  const next = offset + end < bytes.length ? offset + end : null;
  if (next !== null && end === start && count > 0) throw new Error('The target Markdown is not readable as UTF-8 text.');
  return JSON.stringify({ requestId: request.requestId, sourceId: request.sourceId, sourceHash: request.sourceHash, byteLength: bytes.length, offset: offset + start, nextOffset: next, text: window.subarray(start, end).toString('utf8') });
}

// ---- citations -------------------------------------------------------------------------------------------------

/** What the result validators need: the indexed inventory and verified whole-file facts of one request's snapshot. */
export function citationContext(dir, requestDir, request) {
  const { root, tables } = loadIndex(dir, requestDir, request);
  return {
    tables,
    // Whole-file hash and line count of an inventoried file, verified against the immutable index and the snapshot copy.
    file(relative) {
      if (!validRelativePath(relative)) throw new Error('path must be a clean project-relative path');
      const entry = tables.filesByPath.get(relative);
      if (!entry) throw new Error(`"${relative}" is not in the source inventory`);
      const whole = wholeFile(resolveEvidence(root, relative));
      if (whole.sha256 !== entry.sha256) throw new Error(`The snapshot copy of "${relative}" no longer matches its indexed hash`);
      return { entry, ...whole };
    },
  };
}
