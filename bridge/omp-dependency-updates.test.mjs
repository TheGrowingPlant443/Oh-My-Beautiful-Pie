import test from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, mkdirSync, readFileSync, readdirSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import os from 'node:os';
import path from 'node:path';
import { formatDependencyUpdatePlan, loadDependencyUpdatePlanText, proposalHash, registerDependencyUpdateTool, saveDependencyUpdatePlan } from './omp-dependency-updates.mjs';

const fresh = () => mkdtempSync(path.join(os.tmpdir(), 'omp-updates-'));
const producer = { instanceId: 'architect-fixture', generation: 7, launchId: 'launch-fixture', sessionId: 'session-fixture' };
const payload = overrides => ({ title: 'Upgrade example', summary: 'Two upgrades, one breaking.', markdown: '# Plan\n1. Bump example\n2. Run tests\n', ...overrides });

function admitted(dir, overrides = {}) {
  const request = {
    schemaVersion: 1, kind: 'updatePlan', requestId: randomUUID(), folder: path.resolve(dir, 'project'), folderKey: '0123456789abcdef',
    sourceReviewId: randomUUID(), sourceKind: 'security', sourceReviewIds: [randomUUID()], sourceContextId: null, sourceCompletedAt: 1_760_000_000_000,
    contextId: 'c'.repeat(64), planningCapturedAt: 1_760_000_000_001, planningCheckedAt: 1_760_000_000_002, createdAt: 1_760_000_000_003,
    architectInstanceId: producer.instanceId, architectGeneration: producer.generation,
    architectLaunchId: producer.launchId, architectSessionId: producer.sessionId,
    ...overrides,
  };
  const folder = path.join(dir, 'dependency-update-plans', request.requestId);
  mkdirSync(folder, { recursive: true });
  writeFileSync(path.join(folder, 'request.json'), JSON.stringify(request));
  return request;
}
const resultFile = (dir, request) => path.join(dir, 'dependency-update-plans', request.requestId, 'result.json');
const folderOf = (dir, request) => path.join(dir, 'dependency-update-plans', request.requestId);

test('the proposal hash is a length-prefixed SHA-256 shared with the Rust reader', () => {
  assert.equal(proposalHash('Plan', 'Sum ü 😀', '# M\nx'), '0e9f21a4a8980361dd561bbe0c969bfc246e9f093ecf1b1571fec700f419a92b');
  assert.notEqual(proposalHash('ab', 'c', 'd'), proposalHash('a', 'bc', 'd'));
});

test('a valid proposal is saved once with only whitelisted, app-owned identity fields', () => {
  const dir = fresh();
  try {
    const request = admitted(dir);
    const saved = saveDependencyUpdatePlan(dir, { requestId: request.requestId, ...payload(), architectGeneration: 99, sourceReviewIds: ['x'], hash: 'forged', completedAt: 1 }, producer);
    assert.equal(saved.saved, true);
    const result = JSON.parse(readFileSync(resultFile(dir, request), 'utf8'));
    assert.deepEqual(Object.keys(result).sort(), ['architectGeneration', 'architectInstanceId', 'architectLaunchId', 'architectSessionId', 'completedAt', 'contextId',
      'hash', 'markdown', 'requestId', 'revisionId', 'sourceContextId', 'sourceReviewIds', 'summary', 'title'].sort());
    assert.equal(result.architectGeneration, producer.generation);
    assert.deepEqual(result.sourceReviewIds, request.sourceReviewIds);
    assert.equal(result.contextId, request.contextId);
    assert.equal(result.sourceContextId, null);
    assert.equal(result.hash, proposalHash(result.title, result.summary, result.markdown));
    assert.match(result.revisionId, /^[0-9a-f-]{36}$/);
    assert.equal(saved.revisionId, result.revisionId);
    assert.ok(Number.isSafeInteger(result.completedAt) && result.completedAt > 1_700_000_000_000);
    assert.deepEqual(readdirSync(folderOf(dir, request)).sort(), ['request.json', 'result.json']);
    // A saved proposal is final: a second submission, even an identical one, never overwrites it.
    assert.throws(() => saveDependencyUpdatePlan(dir, { requestId: request.requestId, ...payload({ title: 'Other' }) }, producer), /already submitted/);
    assert.equal(JSON.parse(readFileSync(resultFile(dir, request), 'utf8')).title, 'Upgrade example');
    assert.deepEqual(readdirSync(folderOf(dir, request)).sort(), ['request.json', 'result.json']);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('every part of the producer tuple must match the admitted request', () => {
  const dir = fresh();
  try {
    const request = admitted(dir);
    for (const wrong of [{ instanceId: 'replacement' }, { generation: 8 }, { launchId: 'another-launch' }, { sessionId: 'another-session' }]) {
      assert.throws(() => saveDependencyUpdatePlan(dir, { requestId: request.requestId, ...payload() }, { ...producer, ...wrong }), /different Architect/);
    }
    for (const bad of [null, {}, { ...producer, launchId: '' }, { ...producer, generation: -1 }, { ...producer, sessionId: 5 }]) {
      assert.throws(() => saveDependencyUpdatePlan(dir, { requestId: request.requestId, ...payload() }, bad), /invalid Architect producer/);
    }
    assert.equal(existsSync(resultFile(dir, request)), false);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('unknown, foreign-kind, traversal and malformed requests are rejected before any write', () => {
  const dir = fresh();
  try {
    const request = admitted(dir);
    for (const requestId of [randomUUID(), '../security', '..', '', 'not-a-uuid', `${request.requestId}/..`]) {
      assert.throws(() => saveDependencyUpdatePlan(dir, { requestId, ...payload() }, producer));
    }
    const foreign = admitted(dir, { kind: 'security' });
    assert.throws(() => saveDependencyUpdatePlan(dir, { requestId: foreign.requestId, ...payload() }, producer), /unknown dependency update request/);
    const badSources = [admitted(dir, { sourceReviewIds: [] }), admitted(dir, { sourceReviewIds: ['../x'] }), admitted(dir, { contextId: '' }), admitted(dir, { folder: 'relative/path' })];
    for (const bad of badSources) assert.throws(() => saveDependencyUpdatePlan(dir, { requestId: bad.requestId, ...payload() }, producer), /invalid admitted request/);
    for (const item of [foreign, ...badSources]) assert.equal(existsSync(resultFile(dir, item)), false);
    // A request id that names a directory without a request file cannot be used to create anything.
    mkdirSync(path.join(dir, 'dependency-update-plans', randomUUID()));
    assert.throws(() => saveDependencyUpdatePlan(dir, { requestId: readdirSync(path.join(dir, 'dependency-update-plans')).find(name => !existsSync(path.join(dir, 'dependency-update-plans', name, 'request.json'))), ...payload() }, producer));
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('title, summary and markdown limits and unsafe characters fail without leaving files', () => {
  const dir = fresh();
  try {
    const request = admitted(dir);
    const cases = [
      payload({ title: '' }), payload({ title: '   ' }), payload({ title: 'x'.repeat(201) }), payload({ title: 'two\nlines' }),
      payload({ summary: '' }), payload({ summary: 'x'.repeat(4001) }),
      payload({ markdown: '' }), payload({ markdown: ' \n ' }), payload({ markdown: 'x'.repeat(262145) }),
      payload({ markdown: 'escape \u001b[31m sequence' }), payload({ markdown: 'nul \u0000 byte' }), payload({ summary: 'bell \u0007' }),
      payload({ markdown: 'lone \ud800 surrogate' }), payload({ title: 'lone \udc00 surrogate' }),
      payload({ title: 7 }), payload({ markdown: null }),
    ];
    for (const item of cases) {
      assert.throws(() => saveDependencyUpdatePlan(dir, { requestId: request.requestId, ...item }, producer));
      assert.deepEqual(readdirSync(folderOf(dir, request)), ['request.json']);
    }
    assert.throws(() => saveDependencyUpdatePlan(dir, { ...payload() }, producer), /invalid request id/);
    // The bounds themselves, tabs/newlines in markdown and astral characters are fine.
    saveDependencyUpdatePlan(dir, { requestId: request.requestId, title: '😀'.repeat(100), summary: 's'.repeat(4000), markdown: `${'x'.repeat(262000)}\n\t\r\n` }, producer);
    assert.equal(existsSync(resultFile(dir, request)), true);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('a request the app already ended (producerEnded) accepts no late proposal', () => {
  const dir = fresh();
  try {
    const request = admitted(dir);
    writeFileSync(path.join(folderOf(dir, request), 'terminal.json'), JSON.stringify({ status: 'error', code: 'producerEnded', message: 'ended', at: 1 }));
    assert.throws(() => saveDependencyUpdatePlan(dir, { requestId: request.requestId, ...payload() }, producer), /already ended/);
    assert.equal(existsSync(resultFile(dir, request)), false);
    assert.equal(existsSync(path.join(folderOf(dir, request), 'result.lock')), false);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('oversized or non-file request records and linked directories are rejected', () => {
  const dir = fresh();
  try {
    const big = admitted(dir);
    writeFileSync(path.join(folderOf(dir, big), 'request.json'), ' '.repeat(1024 * 1024 + 1));
    assert.throws(() => saveDependencyUpdatePlan(dir, { requestId: big.requestId, ...payload() }, producer), /too large/);
    const broken = admitted(dir);
    writeFileSync(path.join(folderOf(dir, broken), 'request.json'), '{not json');
    assert.throws(() => saveDependencyUpdatePlan(dir, { requestId: broken.requestId, ...payload() }, producer));
    const original = admitted(dir);
    const linked = randomUUID();
    try {
      symlinkSync(folderOf(dir, original), path.join(dir, 'dependency-update-plans', linked), 'junction');
      assert.throws(() => saveDependencyUpdatePlan(dir, { requestId: linked, ...payload() }, producer), /invalid request directory/);
      assert.equal(existsSync(resultFile(dir, original)), false);
    } catch (error) {
      if (!['EPERM', 'EACCES', 'ENOSYS'].includes(error.code)) throw error;
    }
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('saved proposals replay as formatted text and a tampered file is refused', () => {
  const dir = fresh();
  try {
    const request = admitted(dir);
    saveDependencyUpdatePlan(dir, { requestId: request.requestId, ...payload({ markdown: '# Plan\n<script>x</script>' }) }, producer);
    const text = loadDependencyUpdatePlanText(dir, request.requestId);
    assert.match(text, /Upgrade example/);
    assert.match(text, /<script>x<\/script>/);
    assert.match(text, /nothing has been changed or handed to The Manager/);
    const saved = JSON.parse(readFileSync(resultFile(dir, request), 'utf8'));
    assert.equal(text, formatDependencyUpdatePlan(saved));
    for (const id of ['../x', randomUUID(), '']) assert.throws(() => loadDependencyUpdatePlanText(dir, id));
    writeFileSync(resultFile(dir, request), JSON.stringify({ ...saved, markdown: '# Edited' }));
    assert.throws(() => loadDependencyUpdatePlanText(dir, request.requestId), /invalid saved update plan/);
    writeFileSync(resultFile(dir, request), JSON.stringify({ ...saved, requestId: randomUUID() }));
    assert.throws(() => loadDependencyUpdatePlanText(dir, request.requestId), /invalid saved update plan/);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

function register(dir, { producerFor = () => producer, console: resultConsole } = {}) {
  const tools = [];
  const schema = { optional() { return this; }, nullable() { return this; } };
  const zod = Object.fromEntries(['object', 'array', 'string'].map(name => [name, () => schema]));
  registerDependencyUpdateTool({ registerTool: tool => tools.push(tool), zod }, dir, producerFor, resultConsole);
  return tools;
}

test('the Architect tool saves, queues one display-only console copy and never needs a Manager', async () => {
  const dir = fresh();
  try {
    const kinds = new Map();
    const queued = [];
    const resultConsole = { registerKind: (kind, load) => kinds.set(kind, load), enqueue: (ctx, item) => queued.push({ ctx, item }) };
    const tools = register(dir, { console: resultConsole });
    assert.deepEqual(tools.map(tool => tool.name), ['submit_dependency_update_plan']);
    assert.equal(tools[0].approval(), 'read');
    assert.equal(tools[0].loadMode, 'essential');
    const request = admitted(dir);
    const ctx = { agent: { kind: 'main' } };
    const result = await tools[0].execute('call', { requestId: request.requestId, ...payload() }, undefined, undefined, ctx);
    assert.notEqual(result.isError, true);
    assert.match(result.content[0].text, new RegExp(`Saved dependency update plan ${request.requestId}`));
    assert.equal(queued.length, 1);
    assert.equal(queued[0].ctx, ctx);
    assert.deepEqual({ kind: queued[0].item.kind, requestId: queued[0].item.requestId }, { kind: 'updatePlan', requestId: request.requestId });
    assert.match(queued[0].item.text, /1\. Bump example/);
    // The replay loader registered for the console reads the same saved file by id only.
    assert.equal(kinds.get('updatePlan')(dir, request.requestId), queued[0].item.text);
    // A second submission is an error result and queues nothing.
    const again = await tools[0].execute('call', { requestId: request.requestId, ...payload() }, undefined, undefined, ctx);
    assert.equal(again.isError, true);
    assert.equal(queued.length, 1);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('the tool refuses non-main sessions, unavailable identity and keeps the saved plan if the console fails', async () => {
  const dir = fresh();
  try {
    const request = admitted(dir);
    const params = { requestId: request.requestId, ...payload() };
    const refused = register(dir)[0];
    for (const ctx of [undefined, {}, { agent: { kind: 'subagent' } }]) {
      assert.equal((await refused.execute('call', params, undefined, undefined, ctx)).isError, true);
    }
    const unavailable = register(dir, { producerFor: () => { throw new Error('Architect identity unavailable'); } })[0];
    const missing = await unavailable.execute('call', params, undefined, undefined, { agent: { kind: 'main' } });
    assert.equal(missing.isError, true);
    assert.match(missing.content[0].text, /identity unavailable/);
    const wrong = register(dir, { producerFor: () => ({ ...producer, sessionId: 'switched' }) })[0];
    assert.equal((await wrong.execute('call', params, undefined, undefined, { agent: { kind: 'main' } })).isError, true);
    assert.equal(existsSync(resultFile(dir, request)), false);
    const failing = register(dir, { console: { registerKind() {}, enqueue() { throw new Error('console unavailable'); } } })[0];
    const saved = await failing.execute('call', params, undefined, undefined, { agent: { kind: 'main' } });
    assert.notEqual(saved.isError, true);
    assert.equal(existsSync(resultFile(dir, request)), true);
    const aborted = new AbortController();
    aborted.abort(new Error('stopped'));
    await assert.rejects(() => refused.execute('call', params, aborted.signal, undefined, { agent: { kind: 'main' } }), /stopped/);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
