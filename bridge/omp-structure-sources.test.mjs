import test from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { createHash } from 'node:crypto';
import { cp, mkdtemp, mkdir, readFile, rm } from 'node:fs/promises';
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, symlinkSync, writeFileSync, linkSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { architectIdentity, registerClassifyTool } from './omp-architect.mjs';
import { controlAllowed } from './omp-manager.mjs';
import { ACTIONS, MAX_MEMORY, MAX_SOURCE_BYTES, PAGE_BYTES, bunLocalResolver, createStructureSources, localPasteReferences, markdownLike, registerStructureSources } from './omp-structure-sources.mjs';

const ENV = { OMP_MANAGER_BRIDGE_TOKEN: 'token', OMP_MANAGER_BRIDGE_INSTANCE_ID: 'architect-1', OMP_MANAGER_BRIDGE_GENERATION: '4' };
const sha = value => createHash('sha256').update(value).digest('hex');
const identityFor = ctx => architectIdentity(ctx, ENV);

function fixture({ sdk = true, mutate = () => {} } = {}) {
  const root = mkdtempSync(path.join(os.tmpdir(), 'omp-structure-'));
  const dir = path.join(root, 'architect');
  const localRoot = path.join(root, 'local');
  mkdirSync(localRoot, { recursive: true });
  const state = { sessionId: 'session-a', gate: undefined };
  const ctx = {
    mode: 'tui', agent: { kind: 'main' },
    sessionManager: { getSessionId: () => state.sessionId },
    localProtocolOptions: { getArtifactsDir: () => root, getSessionId: () => state.sessionId },
  };
  const fakeSdk = {
    resolveLocalRoot: () => localRoot,
    resolveLocalUrlToPath: uri => (state.resolve ? state.resolve(uri) : path.join(localRoot, uri.slice('local://'.length))),
  };
  const sources = createStructureSources({
    dir, identityFor, currentContext: () => ctx,
    loadSdk: async () => { if (state.gate) await state.gate; if (!sdk) throw new Error('no sdk'); return fakeSdk; },
    now: () => 1_760_000_000_000,
  });
  mutate({ ctx, state });
  const submit = (text, overrides = {}, context = ctx) => sources.capture({ type: 'input', text, source: 'interactive', ...overrides }, context);
  const list = () => sources.control(ctx, 'structure_sources', {});
  const stored = () => { try { return readdirSync(path.join(dir, 'structure-sources', state.sessionId)); } catch { return []; } };
  return { root, dir, localRoot, state, ctx, sources, submit, list, stored, cleanup: () => rmSync(root, { recursive: true, force: true }) };
}

async function readAll(f, sourceId) {
  const parts = [];
  let offset = 0;
  let sha256;
  for (;;) {
    const page = await f.sources.control(f.ctx, 'structure_source_read', { sourceId, offset });
    assert.equal(page.sourceId, sourceId);
    sha256 ??= page.sha256;
    assert.equal(page.sha256, sha256);
    assert.ok(Buffer.byteLength(page.text) <= PAGE_BYTES);
    parts.push(Buffer.from(page.text));
    if (page.nextOffset === null) break;
    assert.ok(page.nextOffset > offset);
    offset = page.nextOffset;
  }
  return { bytes: Buffer.concat(parts), sha256 };
}

test('only the native interactive main TUI submission is captured; app, RPC, extension and child input never is', async () => {
  const f = fixture();
  try {
    const text = '# Target\n\n```\nsrc/\n  lib/\n```\n';
    for (const [overrides, context] of [
      [{ source: 'rpc' }, f.ctx], [{ source: 'extension' }, f.ctx], [{ source: undefined }, f.ctx],
      [{}, { ...f.ctx, mode: 'rpc' }], [{}, { ...f.ctx, mode: undefined }], [{}, { ...f.ctx, agent: { kind: 'sub' } }], [{}, { ...f.ctx, agent: undefined }],
      [{ text: '   \n' }, f.ctx], [{ text: 42 }, f.ctx],
    ]) await f.submit(text, overrides, context);
    assert.equal(f.sources.memorySize(), 0);
    assert.deepEqual((await f.list()).sources, []);
    await f.submit(text);
    assert.equal(f.sources.memorySize(), 1);
    const { sessionId, sources } = await f.list();
    assert.equal(sessionId, 'session-a');
    assert.equal(sources.length, 1);
    assert.equal(sources[0].kind, 'interactiveMessage');
  } finally { f.cleanup(); }
});

test('the registered listener is passive: it returns undefined, leaves the event untouched and survives hostile events', async () => {
  const handlers = new Map();
  const f = fixture();
  try {
    registerStructureSources({ on: (name, handler) => handlers.set(name, handler) }, f.dir, identityFor, { currentContext: () => f.ctx, loadSdk: async () => ({}) });
    assert.ok(handlers.has('input'));
    const event = Object.freeze({ type: 'input', text: '# Target\n', source: 'interactive' });
    assert.equal(handlers.get('input')(event, f.ctx), undefined);
    for (const hostile of [undefined, null, {}, { text: Symbol('x') }]) assert.equal(handlers.get('input')(hostile, f.ctx), undefined);
    assert.equal(handlers.get('input')(event, undefined), undefined);
    assert.deepEqual(event, { type: 'input', text: '# Target\n', source: 'interactive' });
  } finally { f.cleanup(); }
});

test('Markdown-like discovery needs a heading, a fenced structure/tree or a local paste reference', () => {
  assert.equal(markdownLike('# Layout\ntext'), true);
  assert.equal(markdownLike('intro\n\n```\nsrc/\n├── lib\n└── app\n```\n'), true);
  assert.equal(markdownLike('```text\nsrc/lib/\nsrc/app/\n```'), true);
  assert.equal(markdownLike('please restructure the repo'), false);
  assert.equal(markdownLike('```js\nconst a = 1;\nfoo(a);\n```'), false);
  assert.equal(markdownLike('#hashtag without heading space'), false);
  assert.deepEqual(localPasteReferences('see local://paste-3.md, local://paste-3.md and (local://paste-12.md).'), ['paste-3.md', 'paste-12.md']);
  assert.deepEqual(localPasteReferences('xlocal://paste-1.md local://sub/paste-1.md local://paste-1.mdx file:local://paste-2.md'), []);
});

test('inline Markdown keeps the full native text, exact record shape and byte-exact UTF-8 pages', async () => {
  const f = fixture();
  try {
    const text = `# Target ✓\n${'€ä😀'.repeat(15000)}\n`;
    assert.ok(Buffer.byteLength(text) > PAGE_BYTES * 2);
    await f.submit(text);
    const { sources } = await f.list();
    assert.equal(sources.length, 1);
    const [row] = sources;
    assert.deepEqual(Object.keys(row).sort(), ['byteLength', 'capturedAt', 'error', 'excerpt', 'kind', 'sha256', 'sourceId']);
    assert.equal(row.error, null);
    assert.equal(row.sha256, sha(text));
    assert.equal(row.byteLength, Buffer.byteLength(text));
    assert.ok(row.excerpt.startsWith('# Target ✓') && row.excerpt.length <= 200);
    const { bytes, sha256 } = await readAll(f, row.sourceId);
    assert.equal(bytes.toString('utf8'), text);
    assert.equal(sha256, row.sha256);
    const record = JSON.parse(readFileSync(path.join(f.dir, 'structure-sources', 'session-a', `${row.sourceId}.json`), 'utf8'));
    assert.deepEqual(Object.keys(record).sort(), ['architectInstanceId', 'byteLength', 'capturedAt', 'generation', 'kind', 'launchId', 'localUri', 'sessionId', 'sha256', 'sourceId', 'submittedMessageHash', 'text']);
    assert.equal(record.architectInstanceId, 'architect-1');
    assert.equal(record.generation, 4);
    assert.equal(record.launchId, sha('token'));
    assert.equal(record.sessionId, 'session-a');
    assert.equal(record.submittedMessageHash, sha(text));
    assert.equal(record.localUri, null);
    // Offsets are byte offsets on character boundaries.
    await assert.rejects(f.sources.control(f.ctx, 'structure_source_read', { sourceId: row.sourceId, offset: 10 }), /character boundary/);
  } finally { f.cleanup(); }
});

test('a local paste snapshots the file BODY, not the URI text', async () => {
  const f = fixture();
  try {
    const body = '# Pasted layout\n\nsrc/\n  routes/\n';
    writeFileSync(path.join(f.localRoot, 'paste-1.md'), body);
    await f.submit('here is the layout local://paste-1.md thanks');
    const { sources } = await f.list();
    assert.equal(sources.length, 1);
    assert.equal(sources[0].kind, 'interactiveLocalPaste');
    assert.equal(sources[0].error, null);
    assert.equal(sources[0].sha256, sha(body));
    const { bytes } = await readAll(f, sources[0].sourceId);
    assert.equal(bytes.toString('utf8'), body);
    const record = JSON.parse(readFileSync(path.join(f.dir, 'structure-sources', 'session-a', `${sources[0].sourceId}.json`), 'utf8'));
    assert.equal(record.localUri, 'local://paste-1.md');
    assert.equal(record.submittedMessageHash, sha('here is the layout local://paste-1.md thanks'));
    // Later artifact edits cannot change the frozen record.
    writeFileSync(path.join(f.localRoot, 'paste-1.md'), '# Changed\n');
    assert.equal((await readAll(f, sources[0].sourceId)).bytes.toString('utf8'), body);
    // A non-Markdown body is still a local paste candidate (the local paste md is itself the discovery signal).
    writeFileSync(path.join(f.localRoot, 'paste-2.md'), 'plain words only');
    await f.submit('local://paste-2.md');
    assert.equal((await f.list()).sources.length, 2);
  } finally { f.cleanup(); }
});

test('unusable local pastes produce visible error records and never truncated text', async () => {
  const f = fixture();
  try {
    const errors = async () => (await f.list()).sources.filter(row => row.error);
    writeFileSync(path.join(f.localRoot, 'paste-1.md'), Buffer.from([0x23, 0x20, 0xff, 0xfe, 0x0a]));
    await f.submit('local://paste-1.md');
    writeFileSync(path.join(f.localRoot, 'paste-2.md'), Buffer.from('# bin\u0000ary\n'));
    await f.submit('local://paste-2.md');
    writeFileSync(path.join(f.localRoot, 'paste-3.md'), Buffer.alloc(MAX_SOURCE_BYTES + 1, 0x61));
    await f.submit('local://paste-3.md');
    await f.submit('local://paste-4.md');
    writeFileSync(path.join(f.localRoot, 'paste-5.md'), '');
    await f.submit('local://paste-5.md');
    const rows = await errors();
    assert.equal(rows.length, 5);
    for (const row of rows) {
      assert.equal(row.kind, 'interactiveLocalPaste');
      assert.equal(row.byteLength, 0);
      assert.match(row.error, /^local:\/\/paste-\d\.md cannot be used as target Markdown: /);
      await assert.rejects(f.sources.control(f.ctx, 'structure_source_read', { sourceId: row.sourceId, offset: 0 }), /unusable/);
    }
    for (const expected of [/UTF-8/, /binary/, /1 MiB/, /does not exist/, /empty/]) assert.ok(rows.some(row => expected.test(row.error)), String(expected));
  } finally { f.cleanup(); }
});

test('escapes, nested names, links, junctions and hard links are refused', async () => {
  const f = fixture();
  try {
    const outside = path.join(f.root, 'outside');
    mkdirSync(outside);
    writeFileSync(path.join(outside, 'paste-1.md'), '# outside\n');
    f.state.resolve = () => path.join(outside, 'paste-1.md');
    await f.submit('local://paste-1.md');
    f.state.resolve = () => `${f.localRoot}${path.sep}..${path.sep}outside${path.sep}paste-1.md`;
    await f.submit('local://paste-1.md');
    f.state.resolve = uri => path.join(f.localRoot, 'nested', uri.slice('local://'.length));
    mkdirSync(path.join(f.localRoot, 'nested'));
    writeFileSync(path.join(f.localRoot, 'nested', 'paste-1.md'), '# nested\n');
    await f.submit('local://paste-1.md');
    f.state.resolve = undefined;
    let rows = (await f.list()).sources;
    assert.equal(rows.length, 3);
    assert.ok(rows.every(row => /native local:\/\/ root/.test(row.error)));
    // A hard link shares the body with another name.
    writeFileSync(path.join(f.root, 'original.md'), '# linked\n');
    linkSync(path.join(f.root, 'original.md'), path.join(f.localRoot, 'paste-7.md'));
    await f.submit('local://paste-7.md');
    // A symbolic link (when the platform allows creating one without privilege) and a junction as the root.
    let linked = false;
    try { symlinkSync(path.join(f.root, 'original.md'), path.join(f.localRoot, 'paste-8.md'), 'file'); linked = true; } catch { /* not permitted here */ }
    if (linked) await f.submit('local://paste-8.md');
    rows = (await f.list()).sources;
    assert.equal(rows.length, 4 + (linked ? 1 : 0));
    assert.ok(rows.every(row => row.error));
    assert.ok(rows.some(row => /hard links/.test(row.error)));
    if (linked) assert.ok(rows.some(row => /regular file|link/.test(row.error)));
    const realLocal = path.join(f.root, 'real-local');
    mkdirSync(realLocal);
    writeFileSync(path.join(realLocal, 'paste-9.md'), '# via junction\n');
    const junction = path.join(f.root, 'junction-local');
    let junctioned = false;
    try { symlinkSync(realLocal, junction, 'junction'); junctioned = true; } catch { /* not permitted here */ }
    if (junctioned) {
      f.state.resolve = undefined;
      const before = (await f.list()).sources.length;
      const fixtureSdk = createStructureSources({ dir: f.dir, identityFor, currentContext: () => f.ctx, loadSdk: async () => ({ resolveLocalRoot: () => junction, resolveLocalUrlToPath: uri => path.join(junction, uri.slice('local://'.length)) }) });
      await fixtureSdk.capture({ type: 'input', text: 'local://paste-9.md', source: 'interactive' }, f.ctx);
      const after = (await f.list()).sources;
      assert.equal(after.length, before + 1);
      assert.match(after.find(row => row.excerpt === 'local://paste-9.md').error, /link or not a directory/);
    }
  } finally { f.cleanup(); }
});

test('when the SDK sources cannot be imported the SDK mapping is evaluated by a Bun process that receives plain values only', () => {
  const calls = [];
  const run = (bun, args, options) => {
    calls.push({ bun, args, input: JSON.parse(options.env.OMP_LOCAL_RESOLVE) });
    return { status: 0, stdout: JSON.stringify({ root: 'C:\\short\\root', path: 'C:\\short\\root\\paste-1.md' }) };
  };
  const resolver = bunLocalResolver('C:\\sdk', { run, bun: 'bun-under-test', importError: new Error('cannot find package') });
  const protocol = { getArtifactsDir: () => 'C:\\artifacts\\session', getSessionId: () => 'sid' };
  assert.equal(resolver.resolveLocalUrlToPath('local://paste-1.md', protocol), 'C:\\short\\root\\paste-1.md');
  assert.equal(resolver.resolveLocalRoot(protocol), 'C:\\short\\root');
  assert.equal(calls.length, 1, 'the answer to the URI also primes the root');
  assert.equal(calls[0].bun, 'bun-under-test');
  assert.deepEqual(Object.keys(calls[0].input).sort(), ['artifactsDir', 'module', 'sessionId', 'uri']);
  assert.equal(calls[0].input.uri, 'local://paste-1.md');
  assert.equal(calls[0].input.artifactsDir, 'C:\\artifacts\\session');
});

test('a failing Bun evaluation is a visible compatibility error that keeps the import failure', () => {
  const protocol = { getArtifactsDir: () => 'C:\\a', getSessionId: () => 'sid' };
  const failing = bunLocalResolver('C:\\sdk', { run: () => ({ status: 1, stdout: '', stderr: 'boom\nmore' }), importError: new Error('cannot find package') });
  assert.throws(() => failing.resolveLocalUrlToPath('local://paste-1.md', protocol), /could not be evaluated \(boom; direct import failed: cannot find package\)/);
  const missing = bunLocalResolver('C:\\sdk', { run: () => ({ error: new Error('spawn bun ENOENT') }) });
  assert.throws(() => missing.resolveLocalRoot(protocol), /spawn bun ENOENT/);
  const odd = bunLocalResolver('C:\\sdk', { run: () => ({ status: 0, stdout: '{"root":1}' }) });
  assert.throws(() => odd.resolveLocalRoot(protocol), /unexpected answer/);
});

test('a missing SDK resolver is a visible compatibility error', async () => {
  const f = fixture({ sdk: false });
  try {
    await f.submit('local://paste-1.md');
    const [row] = (await f.list()).sources;
    assert.match(row.error, /no sdk/);
  } finally { f.cleanup(); }
});

test('a native session switch while a paste is being read invalidates the capture', async () => {
  const f = fixture();
  try {
    writeFileSync(path.join(f.localRoot, 'paste-1.md'), '# Old session\n');
    let release;
    f.state.gate = new Promise(resolve => { release = resolve; });
    const pending = f.submit('local://paste-1.md');
    f.state.sessionId = 'session-b';
    release();
    await pending;
    assert.equal((await f.list()).sources.length, 0);
    f.state.sessionId = 'session-a';
    assert.equal((await f.list()).sources.length, 0);
    // The same session continuing is still accepted.
    f.state.gate = new Promise(resolve => { release = resolve; });
    const kept = f.submit('local://paste-1.md');
    release();
    await kept;
    assert.equal((await f.list()).sources.length, 1);
    // An explicit invalidation (session_start/switch/shutdown) drops reads that finish afterwards and the memory ring.
    f.state.gate = new Promise(resolve => { release = resolve; });
    const dropped = f.submit('local://paste-1.md');
    f.sources.invalidate();
    release();
    await dropped;
    assert.equal(f.sources.memorySize(), 0);
    assert.equal((await f.list()).sources.length, 1);
    await assert.rejects(f.sources.control(f.ctx, 'structure_source_select_last', {}), /No message|captured/);
  } finally { f.cleanup(); }
});

test('only the newest 20 submissions stay in memory and only the newest 20 sources / 20 MiB persist', async () => {
  const f = fixture();
  try {
    for (let index = 0; index < 25; index++) await f.submit(`plain request ${index}`);
    assert.equal(f.sources.memorySize(), MAX_MEMORY);
    assert.deepEqual(f.stored(), []);
    const selected = await f.sources.control(f.ctx, 'structure_source_select_last', {});
    assert.equal(selected.source.excerpt, 'plain request 24');
    for (let index = 0; index < 22; index++) await f.submit(`# Target ${index}\n`);
    let rows = (await f.list()).sources;
    assert.equal(rows.length, 20);
    assert.equal(rows[0].excerpt, '# Target 21');
    assert.equal(rows.at(-1).excerpt, '# Target 2');
    assert.equal(f.stored().length, 20);
    assert.equal((await f.list()).sources.some(row => row.excerpt === 'plain request 24'), false);
    // Size cap: maximum-size sources evict older ones below 20 MiB.
    const big = `# Big\n${'a'.repeat(MAX_SOURCE_BYTES - 6)}`;
    assert.equal(Buffer.byteLength(big), MAX_SOURCE_BYTES);
    for (let index = 0; index < 21; index++) await f.submit(big.replace('# Big', `# B${String(index).padStart(2, '0')}`));
    rows = (await f.list()).sources;
    const folder = path.join(f.dir, 'structure-sources', 'session-a');
    const total = readdirSync(folder).reduce((sum, name) => sum + statSync(path.join(folder, name)).size, 0);
    assert.ok(total <= 20 * 1024 * 1024, String(total));
    assert.ok(rows.length >= 15 && rows.length < 20);
    assert.equal(rows[0].excerpt.slice(0, 5), '# B20');
  } finally { f.cleanup(); }
});

test('Use last submitted message persists the last bounded native submission; oversize and local pastes stay honest', async () => {
  const f = fixture();
  try {
    await assert.rejects(f.sources.control(f.ctx, 'structure_source_select_last', {}), /No message/);
    await f.submit('no heading here, just the layout: src/a, src/b');
    assert.deepEqual((await f.list()).sources, []);
    const first = await f.sources.control(f.ctx, 'structure_source_select_last', {});
    assert.deepEqual(Object.keys(first), ['source']);
    assert.equal(first.source.kind, 'interactiveMessage');
    assert.equal(first.source.error, null);
    assert.equal((await readAll(f, first.source.sourceId)).bytes.toString(), 'no heading here, just the layout: src/a, src/b');
    // Idempotent: selecting again does not create a second source.
    assert.equal((await f.sources.control(f.ctx, 'structure_source_select_last', {})).source.sourceId, first.source.sourceId);
    assert.equal((await f.list()).sources.length, 1);
    // An oversize message becomes a visible unusable record.
    await f.submit('x'.repeat(MAX_SOURCE_BYTES + 1));
    const oversize = (await f.sources.control(f.ctx, 'structure_source_select_last', {})).source;
    assert.match(oversize.error, /over the 1 MiB limit/);
    assert.equal(oversize.byteLength, 0);
    // The selected submission is the last one: a local paste returns its body record, not the wrapper message.
    writeFileSync(path.join(f.localRoot, 'paste-1.md'), 'body only');
    await f.submit('use local://paste-1.md');
    const paste = (await f.sources.control(f.ctx, 'structure_source_select_last', {})).source;
    assert.equal(paste.kind, 'interactiveLocalPaste');
    assert.equal(paste.sha256, sha('body only'));
    // Unpaired surrogates or NUL cannot be represented as UTF-8 text.
    await f.submit('bad \ud800 text');
    assert.match((await f.sources.control(f.ctx, 'structure_source_select_last', {})).source.error, /not valid Unicode/);
  } finally { f.cleanup(); }
});

test('controls are human-only and refuse malformed ids, offsets, stale identity and non-main contexts', async () => {
  for (const action of ACTIONS) {
    assert.equal(controlAllowed({ role: 'instance', owner: 'manager', action, source: 'human' }), true, action);
    assert.equal(controlAllowed({ role: 'instance', owner: 'manager', action, source: 'manager' }), false, action);
    assert.equal(controlAllowed({ role: 'manager', owner: null, action, source: 'manager' }), false, action);
    assert.equal(controlAllowed({ role: 'instance', owner: 'manager', action, source: undefined }), false, action);
  }
  const f = fixture();
  try {
    await f.submit('# Target\n');
    const [row] = (await f.list()).sources;
    for (const [action, payload] of [['structure_source_read', {}], ['structure_source_read', { sourceId: '../../etc/passwd' }], ['structure_source_read', { sourceId: row.sourceId, offset: -1 }],
      ['structure_source_read', { sourceId: row.sourceId, offset: 1.5 }], ['structure_source_read', { sourceId: row.sourceId, offset: '0' }],
      ['structure_source_read', { sourceId: row.sourceId, offset: 9999 }], ['structure_source_read', { sourceId: '00000000-0000-4000-8000-000000000000', offset: 0 }], ['unknown', {}]]) {
      await assert.rejects(f.sources.control(f.ctx, action, payload), Error, `${action} ${JSON.stringify(payload)}`);
    }
    await assert.rejects(f.sources.control({ ...f.ctx, agent: { kind: 'sub' } }, 'structure_sources', {}), /main session/);
    const other = { ...f.ctx, sessionManager: { getSessionId: () => 'session-other' } };
    await assert.rejects(f.sources.control(other, 'structure_sources', {}), /stale_generation/);
    f.state.sessionId = 'session-b';
    assert.deepEqual((await f.list()).sources, []);
    await assert.rejects(f.sources.control(f.ctx, 'structure_source_read', { sourceId: row.sourceId, offset: 0 }), /no longer available/);
  } finally { f.cleanup(); }
});

test('damaged saved records are visible and unusable instead of served', async () => {
  const f = fixture();
  try {
    await f.submit('# Target\n');
    const [row] = (await f.list()).sources;
    const file = path.join(f.dir, 'structure-sources', 'session-a', `${row.sourceId}.json`);
    const record = JSON.parse(readFileSync(file, 'utf8'));
    writeFileSync(file, JSON.stringify({ ...record, text: '# Tampered\n' }));
    const [damaged] = (await f.list()).sources;
    assert.equal(damaged.sourceId, row.sourceId);
    assert.match(damaged.error, /integrity/);
    await assert.rejects(f.sources.control(f.ctx, 'structure_source_read', { sourceId: row.sourceId, offset: 0 }), /unusable/);
  } finally { f.cleanup(); }
});

test('Architect registration owns the capture listener and the human-only control entry point', async () => {
  const f = fixture();
  const saved = Object.fromEntries(Object.keys(ENV).map(key => [key, process.env[key]]));
  Object.assign(process.env, ENV);
  try {
    const handlers = new Map();
    const tools = [];
    const schema = { optional() { return this; }, nullable() { return this; } };
    const zod = Object.fromEntries(['object', 'array', 'string', 'boolean', 'enum', 'number', 'any', 'literal', 'union', 'record', 'unknown'].map(name => [name, () => schema]));
    const api = registerClassifyTool({ registerTool: tool => tools.push(tool), on: (name, handler) => handlers.set(name, handler), zod }, f.dir, { currentContext: () => f.ctx });
    assert.equal(typeof api.structureSources, 'function');
    assert.ok(handlers.has('input'));
    // No model tool can create, rewrite or read a structure source.
    assert.equal(tools.some(tool => /structure_source|target_markdown/.test(tool.name) && tool.name !== 'read_target_structure'), false);
    assert.equal(handlers.get('input')({ type: 'input', text: '# Pasted\n', source: 'interactive' }, f.ctx), undefined);
    await new Promise(resolve => setImmediate(resolve));
    const listed = await api.structureSources(f.ctx, 'structure_sources', { generation: 4, source: 'human' });
    assert.equal(listed.sources.length, 1);
    assert.equal(listed.sessionId, 'session-a');
  } finally {
    for (const [key, value] of Object.entries(saved)) { if (value === undefined) delete process.env[key]; else process.env[key] = value; }
    f.cleanup();
  }
});

test('installed SDK local:// resolver backs a real session paste file and the capture reads its body', {
  skip: process.env.OMP_SDK_DIR ? false : 'Set OMP_SDK_DIR for the isolated installed-SDK local paste proof', timeout: 90000,
}, async () => {
  const root = await mkdtemp(path.join(os.homedir(), '.omp-structure-sdk-'));
  try {
    await mkdir(path.join(root, 'agent'), { recursive: true });
    const { version } = JSON.parse(await readFile(path.join(process.env.OMP_SDK_DIR, 'package.json'), 'utf8'));
    const nativeRoot = path.join(root, 'natives');
    await cp(path.join(process.env.PI_NATIVES_DIR || path.join(os.homedir(), '.omp', 'natives'), version), path.join(nativeRoot, version), { recursive: true });
    // PI_CONFIG_DIR is relative to the real home. Never override USERPROFILE: native addon lookup depends on it.
    const config = path.relative(os.homedir(), path.join(root, 'config'));
    assert.equal(config.split(path.sep).includes('..'), false, 'scratch config must remain under the real home');
    const { stdout } = await promisify(execFile)(process.env.BUN_BIN || 'bun', [fileURLToPath(new URL('./fixtures/structure-sources-sdk.mjs', import.meta.url))], {
      env: { ...process.env, STRUCTURE_SOURCES_TEST_ROOT: root, OMP_MANAGER_SDK_DIR: process.env.OMP_SDK_DIR, PI_CODING_AGENT_DIR: path.join(root, 'agent'), PI_CONFIG_DIR: config, PI_NATIVES_DIR: nativeRoot, OMP_PROFILE: 'default', PI_COMPILED: '' },
      timeout: 80000, maxBuffer: 1024 * 1024,
    });
    assert.match(stdout, /Structure source local paste capture verified/);
  } finally { await rm(root, { recursive: true, force: true }); }
});
