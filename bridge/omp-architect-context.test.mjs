import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import test from 'node:test';
import {
  OUTPUT_CAP, formatProjectResult, inspectDependencyContext, loadProjectResultText, readLines, registerDependencyContextTool,
} from './omp-architect-context.mjs';

const CONTEXT = 'a'.repeat(64);
const OTHER = 'b'.repeat(64);
const PROJECT = '11111111-1111-4111-8111-111111111111';
const CHILD_DONE = '22222222-2222-4222-8222-222222222222';
const CHILD_ERROR = '33333333-3333-4333-8333-333333333333';
const CHILD_OPEN = '44444444-4444-4444-8444-444444444444';
const SECURITY = '55555555-5555-4555-8555-555555555555';
const actor = { instanceId: 'architect', generation: 3, launchId: 'launch-1', sessionId: 'session-1' };
const binding = { architectInstanceId: 'architect', architectGeneration: 3, architectLaunchId: 'launch-1', architectSessionId: 'session-1' };

function fixture({ packages, edges } = {}) {
  const dir = mkdtempSync(path.join(tmpdir(), 'omp-context-'));
  const records = packages ?? [
    { id: 'cargo:iso', name: 'iso', ecosystem: 'cargo', direct: false, resolvedVersions: ['1.0.0', '2.0.0'], insights: null, inhouse: null },
    { id: 'npm:a', name: 'a', ecosystem: 'npm', direct: true, resolvedVersions: ['1.0.0'], insights: null, inhouse: { inhouse: true, reason: 'private' } },
    { id: 'npm:b', name: 'b', ecosystem: 'npm', direct: false, resolvedVersions: ['1.0.0'], insights: null, inhouse: null },
    { id: 'npm:deep', name: 'deep', ecosystem: 'npm', direct: false, resolvedVersions: ['3.0.0'], insights: null, inhouse: null },
  ];
  const pairs = edges ?? [['npm:a', 'npm:b'], ['npm:b', 'npm:deep'], ['npm:deep', 'npm:b'], ['project', 'npm:a']];
  const packageText = records.map(record => JSON.stringify({ id: record.id, ...record })).join('\n') + '\n';
  const edgeText = pairs.map(pair => JSON.stringify(pair)).join('\n') + '\n';
  const folder = path.join(dir, 'dependency-context', CONTEXT);
  mkdirSync(folder, { recursive: true });
  writeFileSync(path.join(folder, 'packages.jsonl'), packageText);
  writeFileSync(path.join(folder, 'edges.jsonl'), edgeText);
  writeFileSync(path.join(folder, 'summary.json'), JSON.stringify({
    id: CONTEXT, contentHash: CONTEXT, folder: 'C:/project', folderKey: '0123456789abcdef', capturedAt: 1, packageCount: records.length,
    edgeCount: pairs.length, directCount: 1, rootId: 'project', notes: ['truncated'], truncated: true,
    packagesBytes: Buffer.byteLength(packageText), edgesBytes: Buffer.byteLength(edgeText),
  }));
  writeFileSync(path.join(dir, 'active-dependency-context.json'), JSON.stringify({ contextId: CONTEXT }));
  return dir;
}

function request(dir, root, id, body) {
  const folder = path.join(dir, root, id);
  mkdirSync(folder, { recursive: true });
  writeFileSync(path.join(folder, 'request.json'), JSON.stringify({ requestId: id, schemaVersion: 2, contextId: CONTEXT, ...binding, ...body }));
  return folder;
}

function project(dir) {
  const folder = request(dir, 'project-security', PROJECT, { kind: 'project' });
  const entries = [
    { packageId: 'npm:a', requestId: CHILD_DONE, state: 'reused', reason: null },
    { packageId: 'npm:b', requestId: CHILD_ERROR, state: 'new', reason: null },
    { packageId: 'npm:c', requestId: CHILD_OPEN, state: 'new', reason: null },
    { packageId: 'npm:in', requestId: null, state: 'skipped', reason: 'Marked in-house' },
  ];
  writeFileSync(path.join(folder, 'work.jsonl'), entries.map(entry => JSON.stringify(entry)).join('\n') + '\n');
  for (const [child, files] of [[CHILD_DONE, { 'result.json': '{}' }], [CHILD_ERROR, { 'terminal.json': '{"status":"error","code":"producerEnded"}' }], [CHILD_OPEN, {}]]) {
    const childFolder = path.join(dir, 'security', child);
    mkdirSync(childFolder, { recursive: true });
    for (const [name, text] of Object.entries(files)) writeFileSync(path.join(childFolder, name), text);
  }
  return folder;
}

const call = (dir, params, who = actor) => inspectDependencyContext(dir, params, who).result;
const fails = (dir, params, pattern, who = actor) => assert.throws(() => inspectDependencyContext(dir, params, who), pattern);

test('summary, packages and edges expose every record independent of page size', () => {
  const dir = fixture();
  try {
    assert.equal(call(dir, { section: 'summary' }).summary.truncated, true);
    const seen = [];
    for (let offset = 0, page; offset !== null; offset = page.nextOffset) {
      page = call(dir, { section: 'packages', offset, limit: 1 });
      seen.push(...page.items.map(item => item.id));
    }
    assert.deepEqual(seen, ['cargo:iso', 'npm:a', 'npm:b', 'npm:deep']);
    const all = call(dir, { section: 'packages', limit: 100 });
    assert.equal(all.nextOffset, null);
    assert.equal(all.items[0].insights, null);
    assert.deepEqual(all.items[1].inhouse, { inhouse: true, reason: 'private' });
    assert.equal(call(dir, { section: 'edges', limit: 100 }).items.length, 4);
    assert.deepEqual(call(dir, { section: 'edges', offset: 3, limit: 5 }).items, [['project', 'npm:a']]);
    assert.equal(call(dir, { section: 'edges', offset: 99 }).items.length, 0);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('package returns the full record, edge counts and paged neighbours, including cycles and the root', () => {
  const dir = fixture();
  try {
    const result = call(dir, { section: 'package', packageId: 'npm:b' });
    assert.equal(result.package.id, 'npm:b');
    assert.equal(result.outgoingCount, 1);
    assert.equal(result.incomingCount, 2);
    assert.deepEqual(result.neighbours, [{ direction: 'dependsOn', id: 'npm:deep' }, { direction: 'requiredBy', id: 'npm:a' }, { direction: 'requiredBy', id: 'npm:deep' }]);
    const page = call(dir, { section: 'package', packageId: 'npm:b', offset: 1, limit: 1 });
    assert.deepEqual(page.neighbours, [{ direction: 'requiredBy', id: 'npm:a' }]);
    assert.equal(page.nextOffset, 2);
    assert.deepEqual(call(dir, { section: 'package', packageId: 'npm:a' }).neighbours.map(item => item.id), ['npm:b', 'project']);
    fails(dir, { section: 'package', packageId: 'npm:missing' }, /unknown package id/);
    fails(dir, { section: 'package' }, /packageId is required/);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('invalid sections and pagination fail explicitly', () => {
  const dir = fixture();
  try {
    for (const params of [{ section: 'nope' }, {}, { section: 'packages', offset: -1 }, { section: 'packages', offset: 1.5 }, { section: 'packages', limit: 0 }, { section: 'packages', limit: 101 }, { section: 'packages', offset: Number.MAX_SAFE_INTEGER + 2 }]) {
      fails(dir, params, /section|offset|limit/);
    }
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('request resolution requires the full actor tuple and never accepts paths', () => {
  const dir = fixture();
  try {
    request(dir, 'security', SECURITY, { kind: 'security' });
    assert.equal(call(dir, { requestId: SECURITY, section: 'summary' }).contextId, CONTEXT);
    for (const wrong of [{ ...actor, instanceId: 'other' }, { ...actor, generation: 4 }, { ...actor, launchId: 'launch-2' }, { ...actor, sessionId: 'session-2' }, { ...actor, sessionId: undefined }]) {
      fails(dir, { requestId: SECURITY, section: 'summary' }, /different Architect/, wrong);
    }
    for (const bad of ['../security/' + SECURITY, SECURITY.replace(/5/g, 'A'), 'security', `${SECURITY}/../x`, '']) fails(dir, { requestId: bad, section: 'summary' }, /invalid request id/);
    fails(dir, { requestId: '66666666-6666-4666-8666-666666666666', section: 'summary' }, /unknown request id/);
    // A request that never saved a context (legacy) cannot reach any alternate context.
    request(dir, 'security', '77777777-7777-4777-8777-777777777777', { contextId: undefined });
    fails(dir, { requestId: '77777777-7777-4777-8777-777777777777', section: 'summary' }, /no saved dependency context/);
    request(dir, 'security', '88888888-8888-4888-8888-888888888888', { contextId: OTHER });
    fails(dir, { requestId: '88888888-8888-4888-8888-888888888888', section: 'summary' }, /invalid directory|unknown/);
    // The same id in two request roots is ambiguous and rejected.
    request(dir, 'project-security', SECURITY, { kind: 'project' });
    fails(dir, { requestId: SECURITY, section: 'summary' }, /unknown request id/);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('work computes current child status from app-owned files and is project-only', () => {
  const dir = fixture();
  try {
    project(dir);
    request(dir, 'security', SECURITY, { kind: 'security' });
    const work = call(dir, { requestId: PROJECT, section: 'work' });
    assert.equal(work.total, 4);
    assert.deepEqual(work.items.map(item => [item.packageId, item.state, item.status]), [
      ['npm:a', 'reused', 'complete'], ['npm:b', 'new', 'error'], ['npm:c', 'new', 'pending'], ['npm:in', 'skipped', 'skipped'],
    ]);
    // A reused child that completes later is reported from the file, not the stale manifest.
    writeFileSync(path.join(dir, 'security', CHILD_OPEN, 'result.json'), '{}');
    assert.equal(call(dir, { requestId: PROJECT, section: 'work', offset: 2, limit: 1 }).items[0].status, 'complete');
    assert.equal(call(dir, { requestId: PROJECT, section: 'work', offset: 2, limit: 1 }).nextOffset, 3);
    fails(dir, { requestId: SECURITY, section: 'work' }, /only valid for a project/);
    fails(dir, { section: 'work' }, /requires the project requestId/);
    // The project context section works for the same request.
    assert.equal(call(dir, { requestId: PROJECT, section: 'summary' }).summary.id, CONTEXT);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('update-plan drafts resolve their planning context but have no work list', () => {
  const dir = fixture();
  try {
    request(dir, 'dependency-update-plans', SECURITY, { kind: 'updatePlan' });
    assert.equal(call(dir, { requestId: SECURITY, section: 'summary' }).contextId, CONTEXT);
    fails(dir, { requestId: SECURITY, section: 'work' }, /only valid for a project/);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('output is capped with a continuation cursor and an oversized record is an explicit error', () => {
  const big = Array.from({ length: 5 }, (_, index) => ({ id: `npm:p${index}`, filler: 'x'.repeat(100_000) }));
  const dir = fixture({ packages: big, edges: [] });
  try {
    const first = call(dir, { section: 'packages', limit: 100 });
    assert.ok(first.items.length >= 1 && first.items.length < 5);
    assert.equal(first.nextOffset, first.items.length);
    assert.ok(Buffer.byteLength(JSON.stringify(first)) <= OUTPUT_CAP);
    const seen = [];
    for (let offset = 0, page; offset !== null; offset = page.nextOffset) { page = call(dir, { section: 'packages', offset, limit: 100 }); seen.push(...page.items.map(item => item.id)); }
    assert.deepEqual(seen, big.map(item => item.id));
  } finally { rmSync(dir, { recursive: true, force: true }); }
  const huge = fixture({ packages: [{ id: 'npm:huge', filler: 'x'.repeat(OUTPUT_CAP) }], edges: [] });
  try { fails(huge, { section: 'packages' }, /larger than the .* output cap/); }
  finally { rmSync(huge, { recursive: true, force: true }); }
});

test('an incomplete or mismatched snapshot is rejected', () => {
  const dir = fixture();
  try {
    writeFileSync(path.join(dir, 'dependency-context', CONTEXT, 'edges.jsonl'), '["x","y"]\n');
    fails(dir, { section: 'summary' }, /incomplete/);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('readLines streams records larger than one chunk and handles a missing final newline', () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'omp-lines-'));
  try {
    const file = path.join(dir, 'x.jsonl');
    writeFileSync(file, `${'a'.repeat(200_000)}\nb\nc`);
    assert.deepEqual([...readLines(file)].map(line => line.length), [200_000, 1, 1]);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('the registered tool resolves identity only for request-bound calls and reports errors as tool errors', async () => {
  const dir = fixture();
  try {
    project(dir);
    const tools = new Map();
    const kinds = new Map();
    const pi = { zod: new Proxy({}, { get: () => () => ({ optional: () => ({}) }) }), registerTool: tool => tools.set(tool.name, tool) };
    registerDependencyContextTool(pi, dir, ctx => { if (ctx === 'broken') throw new Error('identity unavailable'); return actor; }, { registerKind: (kind, loader) => kinds.set(kind, loader) });
    const tool = tools.get('inspect_dependency_context');
    assert.ok(kinds.has('project'));
    const ok = await tool.execute('id', { section: 'summary' }, undefined, undefined, 'broken');
    assert.ok(!ok.isError, 'the active-pointer path needs no session identity');
    const bound = await tool.execute('id', { requestId: PROJECT, section: 'work' }, undefined, undefined, 'ctx');
    assert.equal(JSON.parse(bound.content[0].text).total, 4);
    const broken = await tool.execute('id', { requestId: PROJECT, section: 'work' }, undefined, undefined, 'broken');
    assert.equal(broken.isError, true);
    assert.match(broken.content[0].text, /identity unavailable/);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('project results format completely and replay only from the fixed app-owned file', () => {
  const dir = fixture();
  try {
    const state = {
      requestId: PROJECT, folder: 'C:/project', contextId: CONTEXT, status: 'complete', completedAt: 1760000000000, summary: 'Compiled from saved package reviews.',
      coverage: [{ packageId: 'npm:a', requestId: CHILD_DONE, status: 'complete', reason: null }, { packageId: 'npm:in', requestId: null, status: 'skippedInhouse', reason: 'Marked in-house' }],
      limitations: ['Not a clean bill of health.'], error: null,
    };
    const folder = path.join(dir, 'project-security', PROJECT);
    mkdirSync(folder, { recursive: true });
    writeFileSync(path.join(folder, 'result.json'), JSON.stringify(state));
    const text = loadProjectResultText(dir, PROJECT);
    for (const needle of ['C:/project', 'npm:a — complete', `(review ${CHILD_DONE})`, 'npm:in — skippedInhouse: Marked in-house', 'Limitations:', '- Not a clean bill of health.', '2025-']) assert.ok(text.includes(needle), needle);
    assert.throws(() => loadProjectResultText(dir, '../x'), /invalid request id/);
    assert.throws(() => formatProjectResult({ ...state, status: 'weird' }, PROJECT), /invalid project review result/);
    assert.throws(() => formatProjectResult(state, SECURITY), /invalid project review result/);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
