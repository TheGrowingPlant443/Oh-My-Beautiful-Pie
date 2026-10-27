import { spawnSync } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
import { mkdirSync, lstatSync, readdirSync, readFileSync, renameSync, unlinkSync, writeFileSync } from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

// Target-structure Markdown the human pastes into the NATIVE Architect chat. The only way a source comes into being is the
// passive `input` listener below: it runs for the main TUI session's interactive submissions and never consumes or rewrites
// them. App prompts, RPC/extension input and model tool arguments cannot create or rewrite a source. The app reads sources
// through three human-only leased controls (`structure_sources`, `structure_source_read`, `structure_source_select_last`);
// none accepts a path. The captured text is what the native editor submitted (earlier extensions may already have
// transformed it), never a claim about original keystroke bytes.
export const MAX_SOURCE_BYTES = 1024 * 1024;
export const PAGE_BYTES = 64 * 1024;
export const MAX_MEMORY = 20;
export const MAX_STORED = 20;
export const MAX_STORED_BYTES = 20 * 1024 * 1024;
export const MAX_LOCAL_REFERENCES = 5;
export const ACTIONS = Object.freeze(['structure_sources', 'structure_source_read', 'structure_source_select_last']);
export const KINDS = Object.freeze(['interactiveMessage', 'interactiveLocalPaste']);
const MAX_RECORD_FILE = 9 * 1024 * 1024;
const MAX_EXCERPT = 200;
const SOURCE_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const SHA256 = /^[0-9a-f]{64}$/;
const PASTE_NAME = /^paste-\d+\.md$/;
const PASTE_URI = /(?<![\w/.:-])local:\/\/(paste-\d+\.md)(?![\w-]|\.\w)/g;
const LONE_SURROGATE = /[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/;
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
const EMPTY_HASH = sha256('');

const HEADING = /^ {0,3}#{1,6}[ \t]+\S/m;
const FENCE_START = /^ {0,3}([`~])/gm;
const TREE_PREFIXES = ['|--', '+--', '`--', '\\--'];
const PATH_COMMENTS = ['#', '//', '<-', '--', '—'];
const pathCharacter = value => value !== undefined && /[\w.@~-]/.test(value);

function fenceRun(value, start) {
  const marker = value[start];
  let end = start;
  while (value[end] === marker) end++;
  return { marker, length: end - start, end };
}

function closingFenceEnd(value, start, fence) {
  let at = start;
  while (at - start < 3 && value[at] === ' ') at++;
  if (value[at] !== fence.marker) return -1;
  const run = fenceRun(value, at);
  if (run.length < fence.length) return -1;
  at = run.end;
  while (value[at] === '`' || value[at] === '~') at++;
  while (value[at] === ' ' || value[at] === '\t') at++;
  if (value[at] === '\n') return at + 1;
  return at === value.length ? at : -1;
}

function fencedBodyEnd(value, start, fence) {
  // A closer needs a preceding body newline, as in the native discovery pattern.
  let newline = value.indexOf('\n', start);
  while (newline !== -1) {
    const next = closingFenceEnd(value, newline + 1, fence);
    if (next !== -1) return { end: newline, next };
    newline = value.indexOf('\n', newline + 1);
  }
  return { end: value.length, next: value.length };
}

function treePrefixAt(value, start) {
  if (TREE_PREFIXES.some(prefix => value.startsWith(prefix, start))) return true;
  if (value[start] !== '|') return false;
  let end = start + 1;
  while (end < value.length && /\s/.test(value[end])) end++;
  return end - start > 2 && end < value.length;
}

function containsTreeGlyph(value) {
  let lineStart = true;
  for (let at = 0; at < value.length; at++) {
    const character = value[at];
    if ('├└│'.includes(character)) return true;
    if (lineStart && !/\s/.test(character)) {
      if (treePrefixAt(value, at)) return true;
      lineStart = false;
    }
    if ('\n\r\u2028\u2029'.includes(character)) lineStart = true;
  }
  return false;
}

function pathLineTail(line, start) {
  let at = start;
  while (at < line.length && (line[at] === '/' || pathCharacter(line[at]))) at++;
  if (at === line.length) return true;
  const pathEnd = at;
  while (at < line.length && /\s/.test(line[at])) at++;
  if (at === pathEnd) return false;
  return PATH_COMMENTS.some(prefix => line.startsWith(prefix, at)) && !/[\r\u2028\u2029]/.test(line.slice(at));
}

function pathLine(value) {
  const line = value.trim();
  const start = line.startsWith('/') ? 1 : 0;
  let at = start;
  while (pathCharacter(line[at])) at++;
  if (at > start && line[at] === '/') return pathLineTail(line, at);
  const dot = line.lastIndexOf('.');
  return start === 0 && at === line.length && dot > 0 && /^[A-Za-z0-9]{1,8}$/.test(line.slice(dot + 1));
}

function structureBody(value) {
  if (containsTreeGlyph(value)) return true;
  let paths = 0;
  for (const line of value.split('\n')) {
    if (pathLine(line)) paths++;
    if (paths >= 2) return true;
  }
  return false;
}

function fencedStructure(value) {
  FENCE_START.lastIndex = 0;
  while (FENCE_START.exec(value)) {
    const fence = fenceRun(value, FENCE_START.lastIndex - 1);
    FENCE_START.lastIndex = fence.end;
    if (fence.length < 3) continue;
    const newline = value.indexOf('\n', fence.end);
    if (newline === -1) return false;
    const bodyStart = newline + 1;
    const bodyEnd = fencedBodyEnd(value, bodyStart, fence);
    if (structureBody(value.slice(bodyStart, bodyEnd.end))) return true;
    FENCE_START.lastIndex = bodyEnd.next;
  }
  return false;
}

/** A heading, or a fenced block that looks like a directory tree / structure listing. Local paste files are handled separately. */
export function markdownLike(value) {
  if (typeof value !== 'string' || !value) return false;
  if (HEADING.test(value)) return true;
  return fencedStructure(value);
}

export function localPasteReferences(value) {
  const names = new Set();
  for (const match of value.matchAll(PASTE_URI)) names.add(match[1]);
  return [...names];
}

function excerptOf(value) {
  const head = value.slice(0, MAX_EXCERPT * 3).replace(/[\u0000-\u001f\u007f-\u009f\u2028\u2029]+/g, ' ').replace(/\s+/g, ' ').trim();
  let cut = head.slice(0, MAX_EXCERPT);
  // Never end on half of a surrogate pair.
  if (/[\ud800-\udbff]$/.test(cut)) cut = cut.slice(0, -1);
  return cut;
}

const wellFormed = value => typeof value === 'string' && !LONE_SURROGATE.test(value) && !value.includes('\u0000');
const sameIdentity = (a, b) => !!a && !!b && a.instanceId === b.instanceId && a.generation === b.generation && a.launchId === b.launchId && a.sessionId === b.sessionId;
const samePath = (a, b) => (process.platform === 'win32' ? a.toLowerCase() === b.toLowerCase() : a === b);

function sessionKey(sessionId) {
  return /^[A-Za-z0-9_.-]{1,128}$/.test(sessionId) && sessionId !== '.' && sessionId !== '..' ? sessionId : `h-${sha256(sessionId).slice(0, 32)}`;
}

function rowOf(record) {
  return { sourceId: record.sourceId, capturedAt: record.capturedAt, kind: record.kind, excerpt: record.error ? (record.localUri ?? '') : excerptOf(record.text),
    sha256: record.sha256, byteLength: record.byteLength, error: record.error ?? null };
}

function recordShape(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value) && typeof value.sourceId === 'string' && SOURCE_ID.test(value.sourceId) &&
    typeof value.sessionId === 'string' && KINDS.includes(value.kind) && Number.isSafeInteger(value.capturedAt) && typeof value.text === 'string' &&
    typeof value.sha256 === 'string' && SHA256.test(value.sha256) && Number.isSafeInteger(value.byteLength) && value.byteLength >= 0 &&
    (value.error === undefined || value.error === null || typeof value.error === 'string') && (value.localUri === null || typeof value.localUri === 'string');
}

const RESOLVE_SCRIPT = `const input = JSON.parse(process.env.OMP_LOCAL_RESOLVE);
const m = await import(input.module);
const options = { getArtifactsDir: () => input.artifactsDir, getSessionId: () => input.sessionId };
process.stdout.write(JSON.stringify({ root: m.resolveLocalRoot(options), path: input.uri ? m.resolveLocalUrlToPath(input.uri, options) : null }));`;

/**
 * The compiled OMP host cannot import the SDK's TypeScript sources from disk (their bare `@oh-my-pi/*` imports do not
 * resolve there), so the launched SDK's own `local-protocol.ts` is evaluated by a short-lived Bun process instead. It is
 * handed plain values only (artifact directory, session id, one local:// URI), never anything from a repository, and its
 * answer is the same SDK mapping (including the shortened Windows root) the native editor used to write the paste file.
 */
export function bunLocalResolver(sdkDir, { run = spawnSync, bun = process.env.BUN_BIN || 'bun', importError = null } = {}) {
  const module = pathToFileURL(path.join(sdkDir, 'src', 'internal-urls', 'local-protocol.ts')).href;
  const roots = new Map();
  const evaluate = (uri, protocol) => {
    const input = { module, uri, artifactsDir: protocol.getArtifactsDir?.() ?? null, sessionId: protocol.getSessionId?.() ?? null };
    const result = run(bun, ['-e', RESOLVE_SCRIPT], { env: { ...process.env, OMP_LOCAL_RESOLVE: JSON.stringify(input) }, encoding: 'utf8', timeout: 20000, windowsHide: true, maxBuffer: 1 << 20 });
    if (result.error || result.status !== 0) {
      const reason = result.error?.message ?? String(result.stderr ?? '').trim().split('\n')[0];
      const detail = reason || `exit ${result.status}`;
      const fallback = importError ? `; direct import failed: ${String(importError.message ?? importError)}` : '';
      throw new Error(`OMP compatibility: the SDK local:// mapping could not be evaluated (${detail}${fallback})`);
    }
    const answer = JSON.parse(String(result.stdout));
    if (typeof answer?.root !== 'string' || (uri && typeof answer.path !== 'string')) throw new Error('OMP compatibility: the SDK local:// mapping returned an unexpected answer.');
    roots.set(`${input.artifactsDir}\0${input.sessionId}`, answer.root);
    return answer;
  };
  return {
    resolveLocalUrlToPath: (uri, protocol) => evaluate(uri, protocol).path,
    resolveLocalRoot: protocol => roots.get(`${protocol.getArtifactsDir?.() ?? null}\0${protocol.getSessionId?.() ?? null}`) ?? evaluate(null, protocol).root,
  };
}

async function loadLocalProtocol() {
  const sdkDir = process.env.OMP_MANAGER_SDK_DIR;
  if (!sdkDir) throw new Error('OMP compatibility: the launched OMP SDK location is unknown, so local:// paste files cannot be resolved.');
  let module;
  try { module = await import(pathToFileURL(path.join(sdkDir, 'src', 'internal-urls', 'local-protocol.ts')).href); }
  catch (error) { module = bunLocalResolver(sdkDir, { importError: error }); }
  if (typeof module.resolveLocalUrlToPath !== 'function' || typeof module.resolveLocalRoot !== 'function') throw new Error('OMP compatibility: the launched OMP SDK does not expose local:// resolution.');
  return module;
}

function readStoredSource(folder, name, sessionId) {
  const match = /^([0-9a-f-]{36})\.json$/.exec(name);
  if (!match || !SOURCE_ID.test(match[1])) return null;
  const file = path.join(folder, name);
  let size = 0;
  try {
    const stat = lstatSync(file);
    if (!stat.isFile()) return null;
    size = stat.size;
    if (size > MAX_RECORD_FILE) throw new Error('oversized record');
    const record = JSON.parse(readFileSync(file, 'utf8'));
    if (!recordShape(record) || record.sourceId !== match[1] || record.sessionId !== sessionId) throw new Error('record does not belong to this session');
    const bytes = Buffer.from(record.text, 'utf8');
    if (!record.error && (sha256(bytes) !== record.sha256 || bytes.length !== record.byteLength || bytes.length > MAX_SOURCE_BYTES)) throw new Error('integrity check failed');
    return { record, file, size };
  } catch (error) {
    // A damaged saved record is visible and unusable, never silently dropped or served.
    return { file, size, record: { sourceId: match[1], sessionId, kind: 'interactiveMessage', capturedAt: 0, localUri: null, text: '', sha256: EMPTY_HASH, byteLength: 0,
      error: `The saved source is unreadable: ${String(error.message ?? error)}`, damaged: true } };
  }
}

function usable(identity, entry, kind, localUri, text) {
  const bytes = Buffer.from(text, 'utf8');
  return { sourceId: randomUUID(), architectInstanceId: identity.instanceId, generation: identity.generation, launchId: identity.launchId, sessionId: identity.sessionId,
    capturedAt: entry.capturedAt, kind, submittedMessageHash: entry.hash, localUri, sha256: sha256(bytes), byteLength: bytes.length, text };
}

function unusable(identity, entry, kind, localUri, error) {
  return { sourceId: randomUUID(), architectInstanceId: identity.instanceId, generation: identity.generation, launchId: identity.launchId, sessionId: identity.sessionId,
    capturedAt: entry.capturedAt, kind, submittedMessageHash: entry.hash, localUri, sha256: EMPTY_HASH, byteLength: 0, text: '', error };
}

function inlineRecord(identity, entry) {
  if (entry.text === null) return unusable(identity, entry, 'interactiveMessage', null, entry.problem);
  return usable(identity, entry, 'interactiveMessage', null, entry.text);
}

function localPasteRecord(identity, entry, name, result) {
  const uri = `local://${name}`;
  if ('error' in result) return unusable(identity, entry, 'interactiveLocalPaste', uri, result.error);
  return usable(identity, entry, 'interactiveLocalPaste', uri, result.text);
}

function localPasteLocation(sdkModule, uri, ctx) {
  const protocol = ctx.localProtocolOptions ?? { getArtifactsDir: () => ctx.sessionManager?.getArtifactsDir?.() ?? null, getSessionId: () => ctx.sessionManager?.getSessionId?.() ?? null };
  return { file: path.resolve(sdkModule.resolveLocalUrlToPath(uri, protocol)), localRoot: path.resolve(sdkModule.resolveLocalRoot(protocol)) };
}

async function inspectLocalPaste(file, localRoot, name) {
  const rootStat = await fsp.lstat(localRoot);
  if (rootStat.isSymbolicLink() || !rootStat.isDirectory()) throw new Error('the native local:// root is a link or not a directory');
  const realRoot = await fsp.realpath(localRoot);
  const before = await fsp.lstat(file, { bigint: true });
  if (before.isSymbolicLink() || !before.isFile()) throw new Error('it is not a regular file (links and reparse points are refused)');
  if (before.nlink !== 1n) throw new Error('it has multiple hard links');
  if (!samePath(await fsp.realpath(file), path.join(realRoot, name))) throw new Error('it resolves through a link or reparse point');
  if (before.size > BigInt(MAX_SOURCE_BYTES)) throw new Error(`it is ${before.size} bytes, over the 1 MiB limit`);
  return before;
}

async function readLocalPasteBytes(file, before) {
  const handle = await fsp.open(file, 'r');
  try {
    const opened = await handle.stat({ bigint: true });
    // `dev` is not comparable between a path lstat and a handle fstat on Windows; the file id and size are.
    if (!opened.isFile() || opened.size !== before.size || (before.ino !== 0n && opened.ino !== 0n && opened.ino !== before.ino)) throw new Error('it changed while it was being opened');
    const buffer = Buffer.alloc(MAX_SOURCE_BYTES + 1);
    let length = 0;
    for (;;) {
      const { bytesRead } = await handle.read(buffer, length, buffer.length - length, length);
      if (bytesRead === 0) break;
      length += bytesRead;
      if (length > MAX_SOURCE_BYTES) throw new Error('it grew past the 1 MiB limit while it was being read');
    }
    const bytes = buffer.subarray(0, length);
    const after = await handle.stat({ bigint: true });
    if (after.size !== opened.size || after.mtimeNs !== opened.mtimeNs || BigInt(bytes.length) !== opened.size) throw new Error('it changed while it was being read');
    return bytes;
  } finally { await handle.close(); }
}

function decodeLocalPaste(bytes) {
  if (!bytes.length) throw new Error('it is empty');
  if (bytes.includes(0)) throw new Error('it is binary (contains NUL bytes)');
  let decoded;
  try { decoded = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(bytes); }
  catch { throw new Error('it is not valid UTF-8 text'); }
  if (!wellFormed(decoded) || Buffer.byteLength(decoded, 'utf8') !== bytes.length) throw new Error('it is not valid UTF-8 text');
  return decoded;
}

const eligible = (event, ctx) => ctx?.mode === 'tui' && ctx?.agent?.kind === 'main' && event?.source === 'interactive' && typeof event?.text === 'string';

/**
 * @param {object} options
 * @param {string} options.dir Architect app directory; sources live below `<dir>/structure-sources/<session>/`.
 * @param {(ctx:any)=>{instanceId:string,generation:number,launchId:string,sessionId:string}} options.identityFor Producer tuple of the current main session.
 * @param {()=>any} [options.currentContext] Live main context; compared again after every async read.
 * @param {()=>Promise<{resolveLocalUrlToPath:Function,resolveLocalRoot:Function}>} [options.loadSdk] SDK `internal-urls/local-protocol` exports.
 */
export function createStructureSources(options) {
  const { dir, identityFor, currentContext, now = Date.now } = options;
  const root = path.join(dir, 'structure-sources');
  const submissions = [];
  let lastStamp = 0;
  const stamp = () => (lastStamp = Math.max(now(), lastStamp + 1));
  let epoch = 0;
  let sdk;
  const loadSdk = () => {
    if (options.loadSdk) return options.loadSdk();
    sdk ??= loadLocalProtocol().catch(error => { sdk = undefined; throw error; });
    return sdk;
  };

  // ---- persistence -----------------------------------------------------------------------------------------------------
  const sessionDir = sessionId => path.join(root, sessionKey(sessionId));
  function loadStored(sessionId) {
    const folder = sessionDir(sessionId);
    let names;
    try { names = readdirSync(folder); }
    catch (error) {
      if (error.code === 'ENOENT') return [];
      throw error;
    }
    const found = [];
    for (const name of names) {
      const stored = readStoredSource(folder, name, sessionId);
      if (stored) found.push(stored);
    }
    return found.sort((a, b) => b.record.capturedAt - a.record.capturedAt || (a.record.sourceId < b.record.sourceId ? 1 : -1));
  }
  function evict(sessionId, keep) {
    const entries = loadStored(sessionId);
    let count = entries.length;
    let bytes = entries.reduce((sum, entry) => sum + entry.size, 0);
    for (const entry of [...entries].reverse()) {
      if (count <= MAX_STORED && bytes <= MAX_STORED_BYTES) break;
      if (entry.record.sourceId === keep) continue;
      try { unlinkSync(entry.file); count--; bytes -= entry.size; } catch { /* a locked file is retried by the next capture */ }
    }
  }
  function persist(record) {
    const folder = sessionDir(record.sessionId);
    mkdirSync(folder, { recursive: true });
    const file = path.join(folder, `${record.sourceId}.json`);
    const temporary = path.join(folder, `${record.sourceId}.${randomUUID()}.tmp`);
    try { writeFileSync(temporary, JSON.stringify(record), { flag: 'wx' }); renameSync(temporary, file); }
    catch (error) { try { unlinkSync(temporary); } catch { /* absent */ } throw error; }
    evict(record.sessionId, record.sourceId);
  }

  // ---- records ---------------------------------------------------------------------------------------------------------

  const live = (identity, snapshotEpoch, ctx) => {
    if (snapshotEpoch !== epoch) return false;
    try { return sameIdentity(identity, identityFor(currentContext?.() ?? ctx)); } catch { return false; }
  };

  /** Resolve one `local://paste-N.md` through the SDK mapping and snapshot its BODY bytes. */
  async function readLocalPaste(name, ctx) {
    const uri = `local://${name}`;
    const fail = reason => ({ error: `${uri} cannot be used as target Markdown: ${reason}` });
    let location;
    try { location = localPasteLocation(await loadSdk(), uri, ctx); }
    catch (error) { return fail(String(error.message ?? error)); }
    const { file, localRoot } = location;
    if (!PASTE_NAME.test(path.basename(file)) || path.basename(file) !== name || !samePath(path.dirname(file), localRoot)) return fail('it does not resolve to a direct file of the native local:// root');
    try {
      const before = await inspectLocalPaste(file, localRoot, name);
      const bytes = await readLocalPasteBytes(file, before);
      return { text: decodeLocalPaste(bytes) };
    } catch (error) { return fail(error.code === 'ENOENT' ? 'the file does not exist' : String(error.message ?? error)); }
  }

  // ---- capture ---------------------------------------------------------------------------------------------------------
  function persistRecords(entry, records) {
    for (const record of records) {
      try { persist(record); entry.recordIds.push(record.sourceId); }
      catch { /* storage failure leaves the submission selectable as a visible error below */ entry.storageError = true; }
    }
  }

  async function run(entry, ctx) {
    const { identity } = entry;
    const refs = localPasteReferences(entry.rawText);
    entry.localRefs = refs.length;
    const records = [];
    if (refs.length) {
      for (const name of refs.slice(0, MAX_LOCAL_REFERENCES)) {
        const result = await readLocalPaste(name, ctx);
        if (!live(identity, entry.epoch, ctx)) return;
        records.push(localPasteRecord(identity, entry, name, result));
      }
      if (refs.length > MAX_LOCAL_REFERENCES) records.push(unusable(identity, entry, 'interactiveLocalPaste', null, `The message references ${refs.length} local paste files; only the first ${MAX_LOCAL_REFERENCES} are considered.`));
    } else if (markdownLike(entry.rawText)) records.push(inlineRecord(identity, entry));
    if (!live(identity, entry.epoch, ctx)) return;
    persistRecords(entry, records);
  }

  /** Start capturing one native input event. Always resolves, never throws; the caller must not await it before returning from the native handler. */
  function capture(event, ctx) {
    try {
      if (!eligible(event, ctx) || !event.text.trim()) return Promise.resolve();
      const identity = identityFor(ctx);
      const rawText = event.text;
      const wf = wellFormed(rawText);
      const size = Buffer.byteLength(rawText, 'utf8');
      const entry = { identity, epoch, capturedAt: stamp(), rawText: wf ? rawText : '', recordIds: [], done: undefined, storageError: false, localRefs: 0,
        hash: sha256(Buffer.from(rawText, 'utf8')) };
      if (!wf) { entry.text = null; entry.problem = 'The submitted message is not valid Unicode text (it contains NUL or unpaired surrogates).'; }
      else if (size > MAX_SOURCE_BYTES) { entry.text = null; entry.problem = `The submitted message is ${size} bytes, over the 1 MiB limit.`; entry.rawText = rawText.slice(0, MAX_SOURCE_BYTES); }
      else entry.text = rawText;
      // An oversize message is still checked for Markdown shape so the visible unusable record appears for a real attempt.
      submissions.push(entry);
      while (submissions.length > MAX_MEMORY) submissions.shift();
      entry.done = run(entry, ctx).catch(() => {});
      return entry.done;
    } catch { return Promise.resolve(); }
  }

  // ---- human-only controls ---------------------------------------------------------------------------------------------
  function currentIdentity(ctx) {
    const identity = identityFor(ctx);
    const liveContext = currentContext?.();
    if (liveContext && !sameIdentity(identity, identityFor(liveContext))) throw new Error('stale_generation');
    return identity;
  }
  const sourcesOf = identity => loadStored(identity.sessionId);

  async function selectLast(ctx) {
    const identity = currentIdentity(ctx);
    const entry = [...submissions].reverse().find(item => sameIdentity(item.identity, identity) && item.epoch === epoch);
    if (!entry) throw new Error('No message from this Architect session has been captured yet. Paste the target Markdown in the Architect chat first.');
    await entry.done;
    if (!live(identity, entry.epoch, ctx)) throw new Error('stale_generation');
    if (entry.recordIds.length) {
      const stored = sourcesOf(identity).find(item => item.record.sourceId === entry.recordIds.at(-1));
      if (stored) return { source: rowOf(stored.record) };
    }
    // A local paste is selected by its body record only; the wrapper message is never substituted for a missing file.
    if (entry.localRefs) throw new Error('The local paste in the last message is not available as a source; paste the target Markdown in the Architect chat again.');
    if (entry.storageError) throw new Error('The captured message could not be saved in app storage.');
    const record = inlineRecord(identity, entry);
    persist(record);
    entry.recordIds.push(record.sourceId);
    return { source: rowOf(record) };
  }

  function readPage(identity, payload) {
    if (typeof payload?.sourceId !== 'string' || !SOURCE_ID.test(payload.sourceId)) throw new Error('invalid structure source id');
    const offset = payload.offset ?? 0;
    if (!Number.isSafeInteger(offset) || offset < 0) throw new Error('offset must be a non-negative integer');
    const stored = sourcesOf(identity).find(item => item.record.sourceId === payload.sourceId);
    if (!stored) throw new Error('That target Markdown source is no longer available; paste it in the Architect chat again.');
    const { record } = stored;
    if (record.error) throw new Error(`That target Markdown source is unusable: ${record.error}`);
    const bytes = Buffer.from(record.text, 'utf8');
    if (offset > bytes.length || (offset === bytes.length && bytes.length > 0)) throw new Error('offset is past the end of the source');
    if (offset < bytes.length && (bytes[offset] & 0xc0) === 0x80) throw new Error('offset is not on a character boundary');
    let end = Math.min(offset + PAGE_BYTES, bytes.length);
    while (end < bytes.length && (bytes[end] & 0xc0) === 0x80) end--;
    return { sourceId: record.sourceId, sha256: record.sha256, text: bytes.subarray(offset, end).toString('utf8'), nextOffset: end < bytes.length ? end : null };
  }

  async function control(ctx, action, payload) {
    if (!ACTIONS.includes(action)) throw new Error('invalid_action');
    const identity = currentIdentity(ctx);
    if (action === 'structure_sources') return { sessionId: identity.sessionId, sources: sourcesOf(identity).slice(0, MAX_STORED).map(item => rowOf(item.record)) };
    if (action === 'structure_source_read') return readPage(identity, payload);
    return selectLast(ctx);
  }

  return {
    capture, control,
    /** Called on every native session start/switch/shutdown: pending async captures and the in-memory submissions are dropped. */
    invalidate() { epoch++; submissions.length = 0; },
    memorySize: () => submissions.length,
  };
}

/** Passive Architect-main capture listener plus the human-only controls. Returns `{ structureSources(ctx, action, payload) }`. */
export function registerStructureSources(pi, dir, identityFor, options = {}) {
  const sources = createStructureSources({ dir, identityFor, ...options });
  // The listener starts capture and returns undefined at once: it never consumes, transforms or delays a native submission.
  pi.on?.('input', (event, ctx) => { sources.capture(event, ctx); return undefined; });
  for (const name of ['session_start', 'session_switch', 'session_shutdown']) pi.on?.(name, () => { sources.invalidate(); });
  return { structureSources: (ctx, action, payload) => sources.control(ctx, action, payload), sources };
}
