import test from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createHash, randomUUID } from 'node:crypto';
import os from 'node:os';
import path from 'node:path';
import {
  TOOL_HELICOPTER, loadInsightResultText, registerProjectInsightTools, repoEvidence, saveHelicopter, saveRepositoryStructure, saveSystemOverview,
} from './omp-project-insight.mjs';
import { formatVisualizationResult, technicalMarker } from './omp-system-results.mjs';
import { readTargetStructure, systemOverviewInspect } from './omp-source-snapshot.mjs';
import { markdownLike } from './omp-structure-sources.mjs';

const actor = { instanceId: 'architect-fixture', generation: 7, launchId: 'launch-fixture', sessionId: 'session-fixture' };
const sha = value => createHash('sha256').update(value).digest('hex');
const APP = "import { db } from './db';\nexport function main() {\n  return db();\n}\n";
const DB = 'export function db() {\n  return 1;\n}\n';
const COMPOSE = 'services:\n  cache:\n    image: valkey\n';
const TARGET = '# Target layout\n\n- src/core\n- src/adapters\n';
const file = (id, relative, body, language) => ({ id, path: relative, sha256: sha(body), bytes: Buffer.byteLength(body), language, status: 'parsed', diagnostics: [] });
const span = { byteStart: 0, byteEnd: 10, lineStart: 1, lineEnd: 4, columnStart: 0, columnEnd: 1 };

// A full on-disk fixture: architect dir, request directory (request.json, index-ref.json, target.md), and the app-owned
// snapshot job below <config>/system-overview/<repo-key>/<snapshot-id>/{source,index.json}.
function setup(kind = 'systemOverview', overrides = {}, options = {}) {
  const root = realRoot();
  const dir = path.join(root, 'architect');
  mkdirSync(dir);
  const project = path.join(root, 'project');
  mkdirSync(project);
  writeFileSync(path.join(project, 'src.txt'), 'LIVE REPOSITORY CONTENT');
  const snapshotId = randomUUID();
  const job = path.join(root, 'system-overview', 'repo-key', snapshotId);
  const source = path.join(job, 'source');
  mkdirSync(path.join(source, 'src'), { recursive: true });
  writeFileSync(path.join(source, 'src', 'app.ts'), APP);
  writeFileSync(path.join(source, 'src', 'db.ts'), DB);
  writeFileSync(path.join(source, 'docker-compose.yml'), COMPOSE);
  const files = [file('f-app', 'src/app.ts', APP, 'typescript'), file('f-db', 'src/db.ts', DB, 'typescript'), file('f-compose', 'docker-compose.yml', COMPOSE, null)];
  files[2].status = 'unsupported';
  const index = {
    schemaVersion: 1, snapshotId, folder: project, sourceHash: 'index-source-hash', extractorVersions: {}, directories: ['empty-dir'], files,
    symbols: [
      { id: 'sym-main', fileId: 'f-app', path: 'src/app.ts', language: 'typescript', region: null, ownerId: null, kind: 'function', name: 'main', qualifiedName: 'main', span },
      { id: 'sym-db', fileId: 'f-db', path: 'src/db.ts', language: 'typescript', region: null, ownerId: null, kind: 'function', name: 'db', qualifiedName: 'db', span },
    ],
    sites: [
      { id: 'site-1', fileId: 'f-app', path: 'src/app.ts', language: 'typescript', region: null, callerId: 'sym-main', span, relation: 'calls', raw: 'db', targetId: 'sym-db', resolution: 'resolvedStatic', conditional: false, typeOnly: false, candidateIds: [] },
      { id: 'site-2', fileId: 'f-app', path: 'src/app.ts', language: 'typescript', region: null, callerId: 'sym-main', span, relation: 'calls', raw: 'fetch("https://user:pw@api.example.com/v1/items?token=abcdef123456")', targetId: null, resolution: 'unresolved', conditional: false, typeOnly: false, candidateIds: [] },
    ],
    edges: [{ id: 'edge-1', sourceId: 'sym-main', targetId: 'sym-db', relation: 'calls', resolution: 'resolvedStatic', evidenceIds: ['site-1'] }],
    coverage: { admittedFiles: 3, parsedFiles: 2, unsupportedFiles: 1, partial: false }, limitations: ['Compose files are not parsed.'],
  };
  if (options.index) options.index(index);
  writeFileSync(path.join(job, 'index.json'), JSON.stringify(index));
  const requestId = randomUUID();
  const request = {
    schemaVersion: 1, requestId, folder: project, key: '0123456789abcdef', kind, createdAt: 1_760_000_000_000, evidenceInventory: [],
    architectInstanceId: actor.instanceId, architectGeneration: actor.generation, architectLaunchId: actor.launchId, architectSessionId: actor.sessionId,
    snapshotId, snapshotSourceHash: 'index-source-hash', sourceId: null, sourceHash: null, baseRequestId: null, supersedesRequestId: null,
    ...overrides,
  };
  if (kind === 'structureProposed') Object.assign(request, { sourceId: 'source-1', sourceHash: sha(TARGET), ...overrides });
  const folder = path.join(root, 'project-insight', requestId);
  mkdirSync(folder, { recursive: true });
  writeFileSync(path.join(folder, 'request.json'), JSON.stringify(request));
  writeFileSync(path.join(folder, 'index-ref.json'), JSON.stringify({ snapshotId, snapshotRoot: source, indexPath: path.join(job, 'index.json') }));
  if (kind === 'structureProposed') writeFileSync(path.join(folder, 'target.md'), TARGET);
  return { root, dir, project, requestId, folder, request, snapshotId, source, job, index };
}
function realRoot() { return mkdtempSync(path.join(os.tmpdir(), 'omp-sysov-')); }
const cleanup = fixture => rmSync(fixture.root, { recursive: true, force: true });
const resultOf = fixture => JSON.parse(readFileSync(path.join(fixture.folder, 'result.json'), 'utf8'));
const evidenceRecord = (id, relative, body, lineStart, lineEnd) => ({ id, path: relative, lineStart, lineEnd, contentHash: sha(body) });

const overview = (fixture, overrides = {}) => ({
  requestId: fixture.requestId, schemaVersion: 1, snapshotId: fixture.snapshotId,
  nodes: [
    { id: 'app', kind: 'application', label: 'Application', parentId: null, description: 'Main application flow', evidenceIds: ['e-app'] },
    { id: 'logic', kind: 'logicGroup', label: 'Request handling', parentId: 'app', description: '', evidenceIds: [] },
    { id: 'cache', kind: 'cache', label: 'Valkey cache', parentId: null, description: 'Declared in compose', evidenceIds: ['e-compose'] },
  ],
  edges: [
    { id: 'edge-calls', sourceId: 'app', targetId: 'cache', relation: 'reads', label: 'session lookup', evidenceIds: ['e-compose'], certainty: 'inferred', staticFactId: null },
    { id: 'edge-static', sourceId: 'logic', targetId: 'app', relation: 'calls', label: 'main -> db', evidenceIds: ['e-app'], certainty: 'declared', staticFactId: 'edge-1' },
  ],
  evidence: [evidenceRecord('e-app', 'src/app.ts', APP, 1, 4), evidenceRecord('e-compose', 'docker-compose.yml', COMPOSE, 2, 3)],
  coverage: [{ area: 'src', status: 'inspected', evidenceIds: ['e-app'], notes: 'application code' }, { area: 'kubernetes', status: 'notFound', evidenceIds: [], notes: 'no manifests in the inventory' }],
  limitations: ['Static inspection does not prove live deployment.'],
  ...overrides,
});

const structure = (fixture, overrides = {}) => ({
  requestId: fixture.requestId, schemaVersion: 1, snapshotId: fixture.snapshotId, sourceId: null, sourceHash: null, baseRequestId: null, supersedesRequestId: null,
  entries: [
    { id: 'src', parentId: null, label: 'Source', path: 'src', pattern: null, members: ['src/app.ts', 'src/db.ts'], purpose: 'Application source', evidenceIds: ['e-app'] },
    { id: 'infra', parentId: null, label: 'Infrastructure files', path: null, pattern: '*.yml', members: ['docker-compose.yml'], purpose: 'Deployment config', evidenceIds: [] },
  ],
  conventions: [{ scope: 'file names', current: 'kebab-case lowercase', proposed: null, rationale: 'all inventoried files', evidenceIds: [] }],
  mappings: [], conflicts: [], evidence: [evidenceRecord('e-app', 'src/app.ts', APP, 1, 2)], limitations: [],
  ...overrides,
});
const proposed = (fixture, overrides = {}) => structure(fixture, {
  sourceId: 'source-1', sourceHash: sha(TARGET),
  entries: [
    { id: 'core', parentId: null, label: 'Core', path: 'src/core', pattern: null, members: ['src/core/app.ts'], purpose: 'Domain code', evidenceIds: [] },
    { id: 'adapters', parentId: 'core', label: 'Adapters', path: 'src/adapters', pattern: null, members: ['src/adapters/db.ts'], purpose: 'IO', evidenceIds: [] },
  ],
  conventions: [{ scope: 'folders', current: 'flat src', proposed: 'core/adapters split', rationale: 'target document', evidenceIds: [] }],
  mappings: [
    { fromPaths: ['src/app.ts'], toPaths: ['src/core/app.ts'], operation: 'move', rationale: 'domain' },
    { fromPaths: ['src/db.ts'], toPaths: ['src/adapters/db.ts'], operation: 'move', rationale: 'io' },
  ],
  conflicts: ['The document lists src/legacy, which does not exist.'], ...overrides,
});

const evidence = (fixture, params) => JSON.parse(repoEvidence(fixture.dir, { requestId: fixture.requestId, ...params }, actor));
const inspect = (fixture, params) => JSON.parse(systemOverviewInspect(fixture.dir, { requestId: fixture.requestId, ...params }, actor));
const rejects = (action, pattern) => assert.throws(action, pattern);

test('repo_evidence for a visualization request reads the immutable snapshot, reports citation facts and records receipts', () => {
  const fixture = setup();
  try {
    assert.deepEqual(evidence(fixture, { action: 'list' }).entries.map(entry => entry.name), ['docker-compose.yml', 'src']);
    assert.throws(() => evidence(fixture, { action: 'read', path: 'src.txt' }), /does not exist/);
    const read = evidence(fixture, { action: 'read', path: 'src/app.ts' });
    assert.equal(read.text, APP);
    assert.equal(read.sha256, sha(APP));
    assert.equal(read.lineStart, 1);
    assert.equal(read.lineEnd, 4);
    assert.equal(read.totalLines, 4);
    const receipts = readFileSync(path.join(fixture.folder, 'evidence-receipts.jsonl'), 'utf8').trim().split('\n').map(line => JSON.parse(line));
    assert.equal(receipts.length, 1);
    assert.deepEqual({ path: receipts[0].path, sha256: receipts[0].sha256, byteStart: receipts[0].byteStart, byteEnd: receipts[0].byteEnd, lineStart: receipts[0].lineStart, lineEnd: receipts[0].lineEnd },
      { path: 'src/app.ts', sha256: sha(APP), byteStart: 0, byteEnd: Buffer.byteLength(APP), lineStart: 1, lineEnd: 4 });
  } finally { cleanup(fixture); }
});

test('snapshot reads page by byte offset with exact line spans and redact credential lines without shifting lines', () => {
  const fixture = setup();
  try {
    const lines = Array.from({ length: 4000 }, (_, index) => (index === 3 ? 'password = "hunter2hunter2"' : `line ${index} ${'x'.repeat(40)}`));
    const body = `${lines.join('\n')}\n`;
    writeFileSync(path.join(fixture.source, 'big.txt'), body);
    const first = evidence(fixture, { action: 'read', path: 'big.txt' });
    assert.equal(first.lineStart, 1);
    assert.ok(first.nextOffset > 0);
    assert.equal(first.redactedLines, 1);
    assert.equal(first.text.split('\n')[3], '[redacted: possible credential]');
    const second = evidence(fixture, { action: 'read', path: 'big.txt', offset: first.nextOffset });
    const expectedStart = 1 + body.slice(0, first.nextOffset).split('\n').length - 1;
    assert.equal(second.lineStart, expectedStart);
    assert.equal(second.totalLines, 4000);
    assert.equal(second.sha256, sha(body));
  } finally { cleanup(fixture); }
});

test('snapshot listings page in 200 entries with a continuing offset', () => {
  const fixture = setup();
  try {
    mkdirSync(path.join(fixture.source, 'many'));
    for (let index = 0; index < 250; index++) writeFileSync(path.join(fixture.source, 'many', `f${String(index).padStart(3, '0')}.txt`), 'x');
    const first = evidence(fixture, { action: 'list', path: 'many' });
    assert.equal(first.entries.length, 200);
    assert.equal(first.total, 250);
    assert.equal(first.nextOffset, 200);
    const second = evidence(fixture, { action: 'list', path: 'many', offset: 200 });
    assert.equal(second.entries.length, 50);
    assert.equal(second.nextOffset, null);
    assert.equal(second.entries[0].name, 'f200.txt');
    assert.throws(() => evidence(fixture, { action: 'list', path: 'many', offset: -1 }), /offset/);
  } finally { cleanup(fixture); }
});

test('the snapshot reference must be an app-owned job below the system-overview root for this exact snapshot', () => {
  const fixture = setup();
  try {
    const refFile = path.join(fixture.folder, 'index-ref.json');
    const outside = path.join(fixture.root, 'elsewhere');
    mkdirSync(path.join(outside, 'source'), { recursive: true });
    writeFileSync(path.join(outside, 'index.json'), '{}');
    writeFileSync(refFile, JSON.stringify({ snapshotId: fixture.snapshotId, snapshotRoot: path.join(outside, 'source'), indexPath: path.join(outside, 'index.json') }));
    assert.throws(() => evidence(fixture, { action: 'list' }), /not an app-owned snapshot/);
    writeFileSync(refFile, JSON.stringify({ snapshotId: randomUUID(), snapshotRoot: fixture.source, indexPath: path.join(fixture.job, 'index.json') }));
    assert.throws(() => evidence(fixture, { action: 'list' }), /invalid source snapshot reference/);
    writeFileSync(refFile, JSON.stringify({ snapshotId: fixture.snapshotId, snapshotRoot: fixture.project, indexPath: path.join(fixture.job, 'index.json') }));
    assert.throws(() => evidence(fixture, { action: 'list' }), /not an app-owned snapshot/);
    rmSync(refFile);
    assert.throws(() => evidence(fixture, { action: 'list' }), /no source snapshot reference/);
  } finally { cleanup(fixture); }
});

test('evidence tools stay bound to the Architect producer and refuse an ended request', () => {
  const fixture = setup();
  try {
    assert.throws(() => repoEvidence(fixture.dir, { requestId: fixture.requestId, action: 'list' }, { ...actor, generation: 8 }), /different Architect/);
    assert.throws(() => systemOverviewInspect(fixture.dir, { requestId: fixture.requestId, section: 'files' }, { ...actor, sessionId: 'other' }), /different Architect/);
    writeFileSync(path.join(fixture.folder, 'terminal.json'), JSON.stringify({ status: 'error', message: 'ended' }));
    assert.throws(() => evidence(fixture, { action: 'list' }), /already ended/);
    assert.throws(() => inspect(fixture, { section: 'files' }), /already ended/);
  } finally { cleanup(fixture); }
});

test('a request id the app never admitted is refused with a plain error that does not reveal app storage paths', () => {
  const fixture = setup();
  try {
    const unknown = randomUUID();
    for (const call of [() => repoEvidence(fixture.dir, { requestId: unknown, action: 'list' }, actor), () => systemOverviewInspect(fixture.dir, { requestId: unknown, section: 'files' }, actor)]) {
      assert.throws(call, error => /unknown request/.test(error.message) && !error.message.includes(fixture.dir) && !/ENOENT/.test(error.message));
    }
  } finally { cleanup(fixture); }
});

test('system_overview_inspect answers bounded index queries and never reports raw URLs or credentials', () => {
  const fixture = setup();
  try {
    const root = inspect(fixture, { section: 'inventory' });
    assert.equal(root.summary.files, 3);
    assert.equal(root.summary.statuses.unsupported, 1);
    assert.deepEqual(root.items.map(item => `${item.type}:${item.name}`), ['directory:empty-dir', 'directory:src', 'file:docker-compose.yml']);
    assert.deepEqual(inspect(fixture, { section: 'inventory', id: 'src' }).items.map(item => item.path), ['src/app.ts', 'src/db.ts']);
    assert.throws(() => inspect(fixture, { section: 'inventory', id: 'missing' }), /not part of the inventory/);
    const coverage = inspect(fixture, { section: 'coverage' });
    assert.deepEqual(coverage.items.map(item => item.path), ['docker-compose.yml']);
    assert.equal(coverage.coverage.unsupportedFiles, 1);
    assert.deepEqual(coverage.limitations, ['Compose files are not parsed.']);
    assert.equal(inspect(fixture, { section: 'files', limit: 2 }).nextOffset, 2);
    assert.equal(inspect(fixture, { section: 'files', id: 'src/app.ts' }).items.length, 1);
    assert.equal(inspect(fixture, { section: 'symbols', id: 'sym-main' }).items.length, 2);
    const neighbors = inspect(fixture, { section: 'neighbors', id: 'sym-main' });
    assert.equal(neighbors.items[0].type, 'edge');
    assert.equal(neighbors.items[0].direction, 'out');
    assert.equal(neighbors.items[0].evidence[0].id, 'site-1');
    const unresolved = neighbors.items.find(item => item.type === 'unresolvedSite');
    assert.equal(unresolved.resolution, 'unresolved');
    assert.ok(!/user:pw|token=abcdef|\/v1\/items/.test(JSON.stringify(neighbors)));
    assert.equal(inspect(fixture, { section: 'neighbors', id: 'sym-db' }).items[0].direction, 'in');
    assert.throws(() => inspect(fixture, { section: 'neighbors' }), /needs the id/);
    assert.throws(() => inspect(fixture, { section: 'neighbors', id: 'nope' }), /needs the id/);
    assert.throws(() => inspect(fixture, { section: 'files', limit: 201 }), /limit/);
    assert.throws(() => inspect(fixture, { section: 'files', limit: 0 }), /limit/);
    assert.throws(() => inspect(fixture, { section: 'files', offset: -1 }), /offset/);
    assert.throws(() => inspect(fixture, { section: 'files', path: '/etc' }), /unknown argument/);
    assert.throws(() => inspect(fixture, { section: 'secrets' }), /section must be/);
  } finally { cleanup(fixture); }
});

test('system_overview_inspect pages stay under 1 MiB and fail explicitly when one item cannot fit', () => {
  const fixture = setup('systemOverview', {}, { index: index => {
    for (let at = 0; at < 150; at++) index.symbols.push({ ...index.symbols[0], id: `big-${at}`, name: `n${at}`, qualifiedName: `${'q'.repeat(20000)}${at}` });
  } });
  try {
    const page = systemOverviewInspect(fixture.dir, { requestId: fixture.requestId, section: 'symbols', limit: 200 }, actor);
    assert.ok(Buffer.byteLength(page) <= 1024 * 1024);
    const parsed = JSON.parse(page);
    assert.ok(parsed.items.length < 152);
    assert.equal(parsed.nextOffset, parsed.items.length);
    assert.equal(parsed.total, 152);
  } finally { cleanup(fixture); }
  const huge = setup('systemOverview', {}, { index: index => { index.symbols[0].qualifiedName = 'q'.repeat(2 * 1024 * 1024); } });
  try { assert.throws(() => inspect(huge, { section: 'symbols', limit: 1 }), /exceeds the 1 MiB/); } finally { cleanup(huge); }
});

test('the saved index must belong to the admitted snapshot and source hash', () => {
  const fixture = setup('systemOverview', { snapshotSourceHash: 'different' });
  try { assert.throws(() => inspect(fixture, { section: 'files' }), /admitted source hash/); } finally { cleanup(fixture); }
});

test('read_target_structure pages the frozen Markdown, only for a proposed request, and detects tampering', () => {
  const fixture = setup('structureProposed');
  try {
    const page = JSON.parse(readTargetStructure(fixture.dir, { requestId: fixture.requestId }, actor));
    assert.equal(page.text, TARGET);
    assert.equal(page.sourceId, 'source-1');
    assert.equal(page.nextOffset, null);
    assert.equal(page.byteLength, Buffer.byteLength(TARGET));
    assert.equal(JSON.parse(readTargetStructure(fixture.dir, { requestId: fixture.requestId, offset: 8 }, actor)).text, TARGET.slice(8));
    assert.throws(() => readTargetStructure(fixture.dir, { requestId: fixture.requestId, path: 'x' }, actor), /unknown argument/);
    writeFileSync(path.join(fixture.folder, 'target.md'), `${TARGET}edited`);
    assert.throws(() => readTargetStructure(fixture.dir, { requestId: fixture.requestId }, actor), /does not match its admitted hash/);
  } finally { cleanup(fixture); }
  const current = setup('structureCurrent');
  try { assert.throws(() => readTargetStructure(current.dir, { requestId: current.requestId }, actor), /not a structureProposed request/); } finally { cleanup(current); }
  const multibyte = setup('structureProposed');
  try {
    const body = `${'é'.repeat(40000)}\n`;
    writeFileSync(path.join(multibyte.folder, 'target.md'), body);
    const request = JSON.parse(readFileSync(path.join(multibyte.folder, 'request.json'), 'utf8'));
    writeFileSync(path.join(multibyte.folder, 'request.json'), JSON.stringify({ ...request, sourceHash: sha(body) }));
    const first = JSON.parse(readTargetStructure(multibyte.dir, { requestId: multibyte.requestId }, actor));
    const second = JSON.parse(readTargetStructure(multibyte.dir, { requestId: multibyte.requestId, offset: first.nextOffset }, actor));
    assert.equal(first.text + second.text, body);
  } finally { cleanup(multibyte); }
});

test('a system overview is validated against the snapshot, saved with the admitted producer and never overwritten', () => {
  const fixture = setup();
  try {
    assert.deepEqual(saveSystemOverview(fixture.dir, overview(fixture), actor), { requestId: fixture.requestId, saved: true });
    const saved = resultOf(fixture);
    assert.equal(saved.kind, 'systemOverview');
    assert.equal(saved.snapshotId, fixture.snapshotId);
    assert.equal(saved.architectSessionId, actor.sessionId);
    assert.equal(saved.edges.find(edge => edge.id === 'edge-static').staticFactId, 'edge-1');
    assert.equal(saved.nodes.length, 3);
    assert.equal(typeof saved.completedAt, 'number');
    assert.throws(() => saveSystemOverview(fixture.dir, overview(fixture), actor), /already completed/);
    assert.match(loadInsightResultText(fixture.dir, 'systemOverview', fixture.requestId), /System overview \(Architect analysis[\s\S]*Valkey cache[\s\S]*-reads->/);
    assert.throws(() => loadInsightResultText(fixture.dir, 'structureCurrent', fixture.requestId), /not a structureCurrent request/);
  } finally { cleanup(fixture); }
});

test('system overviews are rejected for the wrong producer, kind or snapshot', () => {
  const fixture = setup();
  try {
    rejects(() => saveSystemOverview(fixture.dir, overview(fixture), { ...actor, launchId: 'other' }), /different Architect/);
    rejects(() => saveSystemOverview(fixture.dir, overview(fixture, { snapshotId: randomUUID() }), actor), /snapshotId must be/);
    rejects(() => saveSystemOverview(fixture.dir, overview(fixture, { schemaVersion: 2 }), actor), /schemaVersion/);
    rejects(() => saveSystemOverview(fixture.dir, { ...overview(fixture), extra: true }, actor), /unknown field/);
    const { limitations, ...missing } = overview(fixture);
    rejects(() => saveSystemOverview(fixture.dir, missing, actor), /missing field/);
    assert.ok(limitations);
    assert.equal(existsSync(path.join(fixture.folder, 'result.json')), false);
  } finally { cleanup(fixture); }
  const wrongKind = setup('structureCurrent');
  try { rejects(() => saveSystemOverview(wrongKind.dir, overview(wrongKind), actor), /not a systemOverview request/); } finally { cleanup(wrongKind); }
});

test('system overview graph rules: duplicates, dangling endpoints, containment cycles, enums and citations', () => {
  const fixture = setup();
  try {
    const bad = (mutate, pattern) => { const payload = overview(fixture); mutate(payload); rejects(() => saveSystemOverview(fixture.dir, payload, actor), pattern); };
    bad(payload => { payload.nodes[1].id = 'app'; }, /duplicated/);
    bad(payload => { payload.edges[0].targetId = 'missing'; }, /dangling endpoint/);
    bad(payload => { payload.nodes[0].parentId = 'logic'; }, /containment cycle/);
    bad(payload => { payload.nodes[0].parentId = 'app'; }, /containment cycle/);
    bad(payload => { payload.nodes[1].parentId = 'ghost'; }, /unknown parent/);
    bad(payload => { payload.nodes[0].kind = 'mainframe'; }, /kind must be one of/);
    bad(payload => { payload.edges[0].relation = 'telepathy'; }, /relation must be one of/);
    bad(payload => { payload.edges[0].certainty = 'sure'; }, /certainty/);
    bad(payload => { payload.nodes[0].evidenceIds = ['nope']; }, /unknown evidence/);
    bad(payload => { payload.nodes[0].label = ''; }, /label must be non-empty/);
    bad(payload => { payload.nodes[0].label = 'x'.repeat(201); }, /at most 200/);
    bad(payload => { payload.nodes[0].description = 'x'.repeat(2001); }, /at most 2000/);
    bad(payload => { payload.coverage = []; }, /coverage must account/);
    bad(payload => { payload.coverage[0].evidenceIds = []; }, /without evidence/);
    bad(payload => { payload.coverage[0].status = 'maybe'; }, /status must be/);
    bad(payload => { payload.nodes = Array.from({ length: 2001 }, (_, index) => ({ id: `n${index}`, kind: 'service', label: 'n', parentId: null, description: '', evidenceIds: [] })); payload.edges = []; }, /at most 2000/);
  } finally { cleanup(fixture); }
});

test('only an indexed fact can make an edge "declared"; a cited path or prose cannot', () => {
  const fixture = setup();
  try {
    const bad = (mutate, pattern) => { const payload = overview(fixture); mutate(payload); rejects(() => saveSystemOverview(fixture.dir, payload, actor), pattern); };
    bad(payload => { payload.edges[0].certainty = 'declared'; }, /without a staticFactId/);
    bad(payload => { payload.edges[1].staticFactId = 'invented-id'; }, /does not match an indexed declaration or edge/);
    bad(payload => { payload.edges[1].staticFactId = 'f-app'; }, /does not match an indexed declaration or edge/);
    const payload = overview(fixture);
    payload.edges[0].staticFactId = 'sym-main';
    payload.edges[0].certainty = 'declared';
    saveSystemOverview(fixture.dir, payload, actor);
    assert.equal(resultOf(fixture).edges[0].certainty, 'declared');
  } finally { cleanup(fixture); }
});

test('evidence records must match inventoried paths, whole-file hashes and real line ranges', () => {
  const fixture = setup();
  try {
    const bad = (mutate, pattern) => { const payload = overview(fixture); mutate(payload); rejects(() => saveSystemOverview(fixture.dir, payload, actor), pattern); };
    bad(payload => { payload.evidence[0].contentHash = sha('other'); }, /does not match the snapshot file/);
    bad(payload => { payload.evidence[0].contentHash = 'ABC'; }, /lowercase SHA-256/);
    bad(payload => { payload.evidence[0].lineEnd = 99; }, /has 4 line/);
    bad(payload => { payload.evidence[0].lineStart = 0; }, /1-based/);
    bad(payload => { payload.evidence[0].lineStart = 3; payload.evidence[0].lineEnd = 2; }, /1-based lineStart <= lineEnd/);
    bad(payload => { payload.evidence[0].path = 'src/ghost.ts'; }, /not in the source inventory/);
    bad(payload => { payload.evidence[0].path = '../project/src.txt'; }, /clean relative path/);
    bad(payload => { payload.evidence[0].path = 'src\\app.ts'; }, /forward slashes/);
    bad(payload => { payload.evidence[1].id = 'e-app'; }, /duplicated/);
    bad(payload => { payload.evidence[0].extra = 1; }, /unknown field/);
    // A snapshot file that no longer matches its indexed hash is refused even if the model quotes the new hash.
    writeFileSync(path.join(fixture.source, 'src', 'app.ts'), `${APP}// edited\n`);
    const edited = overview(fixture);
    edited.evidence[0] = evidenceRecord('e-app', 'src/app.ts', `${APP}// edited\n`, 1, 4);
    rejects(() => saveSystemOverview(fixture.dir, edited, actor), /no longer matches its indexed hash/);
  } finally { cleanup(fixture); }
});

test('published text may not carry secrets or raw URLs', () => {
  const fixture = setup();
  try {
    const bad = (mutate, pattern) => { const payload = overview(fixture); mutate(payload); rejects(() => saveSystemOverview(fixture.dir, payload, actor), pattern); };
    bad(payload => { payload.nodes[0].description = 'connects with password = "hunter2hunter2"'; }, /secret or credential/);
    bad(payload => { payload.nodes[0].description = 'see https://example.com/admin/panel'; }, /raw URL/);
    bad(payload => { payload.nodes[0].label = 'https://user:pw@db.internal'; }, /secret or credential|raw URL/);
    bad(payload => { payload.edges[0].label = 'calls https://api.example.com?key=1'; }, /raw URL/);
    bad(payload => { payload.limitations = ['token: sk-abcdefghijklmnopqrstuvwxyz']; }, /secret or credential/);
    const payload = overview(fixture);
    payload.nodes[2].description = 'Reached at https://cache.internal:6379 inside the cluster';
    payload.nodes[0].label = 'Application\u0000';
    rejects(() => saveSystemOverview(fixture.dir, payload, actor), /invalid characters/);
    payload.nodes[0].label = 'Application';
    saveSystemOverview(fixture.dir, payload, actor);
  } finally { cleanup(fixture); }
});

test('a current structure is saved with derived fields and only inventoried members', () => {
  const fixture = setup('structureCurrent');
  try {
    saveRepositoryStructure(fixture.dir, structure(fixture), actor);
    const saved = resultOf(fixture);
    assert.equal(saved.kind, 'structureCurrent');
    assert.equal(saved.sourceId, null);
    assert.equal(saved.entries[0].members[0], 'src/app.ts');
    assert.match(loadInsightResultText(fixture.dir, 'structureCurrent', fixture.requestId), /Current repository structure[\s\S]*Source \(src\)/);
  } finally { cleanup(fixture); }
  const bad = (mutate, pattern) => {
    const fixture2 = setup('structureCurrent');
    try { const payload = structure(fixture2); mutate(payload); rejects(() => saveRepositoryStructure(fixture2.dir, payload, actor), pattern); } finally { cleanup(fixture2); }
  };
  bad(payload => { payload.entries[0].members = ['src/ghost.ts']; }, /not in the source inventory/);
  bad(payload => { payload.entries[0].path = 'nowhere'; }, /not in the source inventory/);
  bad(payload => { payload.entries[0].members = ['src/app.ts', 'src/app.ts']; }, /repeats an inventory path/);
  bad(payload => { payload.mappings = [{ fromPaths: [], toPaths: ['x'], operation: 'create', rationale: '' }]; }, /mappings must be empty/);
  bad(payload => { payload.conflicts = ['x']; }, /no conflicts|current-structure analysis has none/);
  bad(payload => { payload.conventions[0].proposed = 'something'; }, /proposed must be null/);
  bad(payload => { payload.sourceId = 'source-1'; }, /sourceId must be null/);
  bad(payload => { payload.baseRequestId = randomUUID(); }, /baseRequestId must be null/);
  bad(payload => { payload.entries[1].parentId = 'infra'; }, /parent cycle/);
  bad(payload => { payload.entries[0].parentId = 'ghost'; }, /unknown parent/);
  bad(payload => { payload.entries[0].pattern = 'x'.repeat(201); }, /at most 200/);
  bad(payload => { payload.entries[0].extra = 1; }, /unknown field/);
});

test('a proposed structure requires the admitted source identity and valid, collision-free destinations', () => {
  const fixture = setup('structureProposed', { baseRequestId: randomUUID(), supersedesRequestId: randomUUID() });
  const base = fixture.request;
  try {
    const good = proposed(fixture, { baseRequestId: base.baseRequestId, supersedesRequestId: base.supersedesRequestId });
    rejects(() => saveRepositoryStructure(fixture.dir, proposed(fixture), actor), /baseRequestId must be/);
    rejects(() => saveRepositoryStructure(fixture.dir, { ...good, sourceHash: sha('other') }, actor), /sourceHash must be/);
    rejects(() => saveRepositoryStructure(fixture.dir, { ...good, sourceId: null }, actor), /sourceId must be/);
    rejects(() => saveRepositoryStructure(fixture.dir, { ...good, supersedesRequestId: null }, actor), /supersedesRequestId must be/);
    rejects(() => saveRepositoryStructure(fixture.dir, { ...good, sourceId: 'other', sourceHash: sha('other'), entries: [] }, actor), /sourceId must be "source-1"/);
    saveRepositoryStructure(fixture.dir, good, actor);
    const saved = resultOf(fixture);
    assert.equal(saved.kind, 'structureProposed');
    assert.equal(saved.sourceId, 'source-1');
    assert.equal(saved.sourceHash, sha(TARGET));
    assert.equal(saved.baseRequestId, base.baseRequestId);
    assert.equal(saved.supersedesRequestId, base.supersedesRequestId);
    assert.deepEqual(saved.conflicts, ['The document lists src/legacy, which does not exist.']);
    assert.match(loadInsightResultText(fixture.dir, 'structureProposed', fixture.requestId), /Target document: source-1[\s\S]*move: src\/app\.ts -> src\/core\/app\.ts[\s\S]*Target document conflicts:[\s\S]*src\/legacy/);
  } finally { cleanup(fixture); }
  const bad = (mutate, pattern) => {
    const fixture2 = setup('structureProposed');
    try { const payload = proposed(fixture2); mutate(payload); rejects(() => saveRepositoryStructure(fixture2.dir, payload, actor), pattern); } finally { cleanup(fixture2); }
  };
  bad(payload => { payload.entries[1].path = 'src/core'; }, /duplicate destination/);
  bad(payload => { payload.entries[1].path = 'SRC/Core'; }, /collides with/);
  bad(payload => { payload.entries[1].members = ['src/core/app.ts']; }, /duplicate destination/);
  bad(payload => { payload.mappings[1].toPaths = ['SRC/CORE/APP.TS']; }, /collides with/);
  bad(payload => { payload.mappings[1].toPaths = ['src/core/app.ts']; }, /duplicate destination/);
  bad(payload => { payload.entries[0].path = 'src/aux'; }, /not a valid Windows path component/);
  bad(payload => { payload.entries[0].path = 'src/bad:name'; }, /not a valid Windows path component/);
  bad(payload => { payload.entries[0].path = 'src/trailing.'; }, /not a valid Windows path component/);
  bad(payload => { payload.entries[0].path = '../escape'; }, /clean relative path/);
  bad(payload => { payload.entries[0].path = '/abs'; }, /clean relative path/);
  bad(payload => { payload.entries[0].members = ['C:\\x']; }, /clean relative path/);
  bad(payload => { payload.entries[0].parentId = 'adapters'; }, /parent cycle/);
  bad(payload => { payload.mappings[0].fromPaths = ['src/ghost.ts']; }, /not in the source inventory/);
  bad(payload => { payload.mappings[1].fromPaths = ['src/app.ts']; }, /already mapped/);
  bad(payload => { payload.mappings[0].toPaths = ['src/core/a.ts', 'src/core/b.ts']; }, /does not fit operation "move"/);
  bad(payload => { payload.mappings[0].operation = 'bogus'; }, /operation must be one of/);
});

test('splits, merges, creates and removals must be explicit and shaped correctly', () => {
  const fixture = setup('structureProposed');
  try {
    const attempt = mappings => { const payload = proposed(fixture, { mappings }); return () => saveRepositoryStructure(fixture.dir, payload, actor); };
    rejects(attempt([{ fromPaths: ['src/app.ts'], toPaths: ['src/a.ts', 'src/b.ts'], operation: 'rename', rationale: '' }]), /does not fit operation "rename"/);
    rejects(attempt([{ fromPaths: ['src/app.ts', 'src/db.ts'], toPaths: ['src/x.ts'], operation: 'move', rationale: '' }]), /does not fit operation "move"/);
    rejects(attempt([{ fromPaths: ['src/app.ts'], toPaths: ['src/a.ts'], operation: 'split', rationale: '' }]), /does not fit operation "split"/);
    rejects(attempt([{ fromPaths: ['src/app.ts'], toPaths: ['src/a.ts'], operation: 'merge', rationale: '' }]), /does not fit operation "merge"/);
    rejects(attempt([{ fromPaths: ['src/app.ts'], toPaths: ['src/a.ts'], operation: 'create', rationale: '' }]), /does not fit operation "create"/);
    rejects(attempt([{ fromPaths: [], toPaths: ['src/a.ts'], operation: 'remove', rationale: '' }]), /does not fit operation "remove"/);
    rejects(attempt([{ fromPaths: ['src/app.ts'], toPaths: ['src/other.ts'], operation: 'unchanged', rationale: '' }]), /does not fit operation "unchanged"/);
    const valid = attempt([
      { fromPaths: ['src/app.ts'], toPaths: ['src/core/a.ts', 'src/core/b.ts'], operation: 'split', rationale: 'two concerns' },
      { fromPaths: ['src/db.ts', 'docker-compose.yml'], toPaths: ['src/adapters/db.ts'], operation: 'merge', rationale: 'merged' },
      { fromPaths: [], toPaths: ['docs/README.md'], operation: 'create', rationale: 'new' },
    ]);
    valid();
    assert.equal(resultOf(fixture).mappings.length, 3);
  } finally { cleanup(fixture); }
});

test('visualization tools register with strict schemas, resolve the producer from the session and report errors as tool errors', async () => {
  const fixture = setup('structureCurrent');
  try {
    const tools = [];
    const schema = { optional() { return this; }, nullable() { return this; } };
    const zod = Object.fromEntries(['object', 'array', 'string', 'number', 'enum'].map(name => [name, () => schema]));
    const enqueued = [];
    registerProjectInsightTools({ registerTool: tool => tools.push(tool), zod }, fixture.dir, ctx => { if (!ctx.ok) throw new Error('Architect identity unavailable'); return actor; },
      { registerKind() {}, enqueue: (_ctx, item) => enqueued.push(item) });
    const named = name => tools.find(tool => tool.name === name);
    const inspected = await named('system_overview_inspect').execute('id', { requestId: fixture.requestId, section: 'files' }, undefined, undefined, { ok: true });
    assert.equal(JSON.parse(inspected.content[0].text).total, 3);
    assert.equal((await named('system_overview_inspect').execute('id', { requestId: fixture.requestId, section: 'files' }, undefined, undefined, { ok: false })).isError, true);
    assert.equal((await named('submit_system_overview').execute('id', overview(fixture), undefined, undefined, { ok: true })).isError, true);
    assert.equal((await named('read_target_structure').execute('id', { requestId: fixture.requestId }, undefined, undefined, { ok: true })).isError, true);
    const saved = await named('submit_repository_structure').execute('id', structure(fixture), undefined, undefined, { ok: true });
    assert.equal(saved.isError, undefined);
    assert.equal(enqueued.length, 1);
    assert.equal(enqueued[0].kind, 'structureCurrent');
    const again = await named('submit_repository_structure').execute('id', structure(fixture), undefined, undefined, { ok: true });
    assert.equal(again.isError, true);
  } finally { cleanup(fixture); }
});

test('project_visualization_prompt admits one guarded native follow-up per request and never resends', async () => {
  const fixture = setup();
  try {
    const sent = [];
    const handlers = new Map();
    const pi = { registerTool() {}, zod: new Proxy({}, { get: () => () => ({ optional() { return this; }, nullable() { return this; } }) }), on: (name, handler) => handlers.set(name, handler) };
    const control = registerProjectInsightTools(pi, fixture.dir, ctx => { if (!ctx.ok) throw new Error('Architect identity unavailable'); return actor; }, undefined, {
      submitPrompt: (message, callbacks) => { sent.push(message); callbacks.onPromptAdmitted(); return new Promise(() => {}); },
    });
    const ctx = { ok: true, agent: { kind: 'main' }, sessionManager: { getSessionId: () => actor.sessionId } };
    const payload = { requestId: fixture.requestId, kind: 'systemOverview', message: 'Analyze the system.', source: 'human', generation: 7 };
    assert.deepEqual(await control.promptVisualization(ctx, payload), { accepted: true, duplicate: false });
    assert.equal(sent.length, 1);
    assert.match(sent[0], /^Analyze the system\.\n\n\[omp-visualization-request id=/);
    assert.equal(control.guard.states()[0].state, 'pending');
    assert.deepEqual(await control.promptVisualization(ctx, payload), { accepted: true, duplicate: true });
    assert.equal(sent.length, 1);
    // The turn that carries the prompt activates the guard through the native before_agent_start hook.
    handlers.get('before_agent_start')({ prompt: sent[0] }, ctx);
    assert.equal(control.guard.states()[0].state, 'active');
    assert.equal(handlers.get('tool_call')({ toolName: 'bash', input: { command: 'ls' } }, ctx).block, true);
    assert.equal(handlers.get('tool_call')({ toolName: 'repo_evidence', input: { requestId: fixture.requestId, action: 'list' } }, ctx), undefined);
  } finally { cleanup(fixture); }
});

test('project_visualization_prompt rejects invalid, mismatched, wrong-producer and unadmitted prompts', async () => {
  const fixture = setup();
  try {
    const sent = [];
    const pi = { registerTool() {}, zod: new Proxy({}, { get: () => () => ({ optional() { return this; }, nullable() { return this; } }) }), on() {} };
    const refuse = { value: false };
    const control = registerProjectInsightTools(pi, fixture.dir, ctx => { if (!ctx.ok) throw new Error('Architect identity unavailable'); return actor; }, undefined, {
      submitPrompt: (message, callbacks) => { sent.push(message); if (refuse.value) return Promise.reject(new Error('native refused')); callbacks.onPromptAdmitted(); return new Promise(() => {}); },
    });
    const ctx = { ok: true, agent: { kind: 'main' }, sessionManager: { getSessionId: () => actor.sessionId } };
    const good = { requestId: fixture.requestId, kind: 'systemOverview', message: 'Analyze.' };
    await assert.rejects(control.promptVisualization(ctx, { ...good, kind: 'security' }), /invalid visualization prompt/);
    await assert.rejects(control.promptVisualization(ctx, { ...good, kind: 'structureCurrent' }), /not a structureCurrent request/);
    await assert.rejects(control.promptVisualization(ctx, { ...good, requestId: 'not-a-uuid' }), /invalid request id/);
    await assert.rejects(control.promptVisualization(ctx, { ...good, message: '   ' }), /message must be/);
    await assert.rejects(control.promptVisualization(ctx, { ...good, message: 'x'.repeat(64 * 1024 + 1) }), /message must be/);
    await assert.rejects(control.promptVisualization({ ...ctx, ok: false }, good), /Architect identity unavailable/);
    assert.equal(sent.length, 0);
    refuse.value = true;
    await assert.rejects(control.promptVisualization(ctx, good), /native refused/);
    assert.equal(control.guard.states().length, 0, 'a refused prompt leaves no guard behind');
    refuse.value = false;
    assert.deepEqual(await control.promptVisualization(ctx, good), { accepted: true, duplicate: false });
    writeFileSync(path.join(fixture.folder, 'terminal.json'), JSON.stringify({ status: 'error', message: 'ended' }));
    await assert.rejects(control.promptVisualization(ctx, { ...good, requestId: fixture.requestId }), /already ended/);
    const bare = registerProjectInsightTools(pi, fixture.dir, () => actor);
    await assert.rejects(bare.promptVisualization(ctx, good), /native_prompt_unavailable/);
  } finally { cleanup(fixture); }
});

// ---- helicopter ------------------------------------------------------------------------------------------------

const helicopter = (fixture, overrides = {}) => ({
  requestId: fixture.requestId, schemaVersion: 1, snapshotId: fixture.snapshotId, purpose: 'Lets customers book, change and cancel clinic appointments.',
  nodes: [
    { id: 'customer', kind: 'actor', label: 'Customer', parentId: null, description: 'Someone who wants to see the clinic', evidenceIds: ['e-app'] },
    { id: 'asks', kind: 'trigger', label: 'Customer asks for an appointment', parentId: 'booking', description: '', evidenceIds: ['e-app'] },
    { id: 'booking', kind: 'businessGroup', label: 'Booking an appointment', parentId: null, description: '', evidenceIds: [] },
    { id: 'free', kind: 'decision', label: 'Is a slot free?', parentId: 'booking', description: '', evidenceIds: ['e-app'] },
    { id: 'confirm', kind: 'activity', label: 'Clinic confirms the visit', parentId: 'booking', description: '', evidenceIds: ['e-app'] },
    { id: 'cancelled', kind: 'outcome', label: 'Appointment is cancelled', parentId: null, description: 'Needs clarification: whether a fee applies', evidenceIds: ['e-compose'] },
    { id: 'pay', kind: 'externalParty', label: 'Payment provider', parentId: null, description: '', evidenceIds: ['e-compose'] },
  ],
  edges: [
    { id: 'h1', sourceId: 'customer', targetId: 'asks', label: '', evidenceIds: ['e-app'], certainty: 'inferred' },
    { id: 'h2', sourceId: 'asks', targetId: 'free', label: '', evidenceIds: ['e-app'], certainty: 'inferred' },
    { id: 'h3', sourceId: 'free', targetId: 'confirm', label: 'Yes', evidenceIds: ['e-app'], certainty: 'inferred' },
    { id: 'h4', sourceId: 'free', targetId: 'cancelled', label: 'No', evidenceIds: [], certainty: 'unknown' },
    { id: 'h5', sourceId: 'confirm', targetId: 'pay', label: 'Deposit is taken', evidenceIds: ['e-compose'], certainty: 'inferred' },
  ],
  capabilities: [
    { id: 'cap-book', label: 'Book a visit', description: '', nodeIds: ['asks', 'free', 'confirm'], evidenceIds: ['e-app'] },
    { id: 'cap-cancel', label: 'Cancel a visit', description: '', nodeIds: ['cancelled'], evidenceIds: ['e-compose'] },
  ],
  evidence: [evidenceRecord('e-app', 'src/app.ts', APP, 1, 4), evidenceRecord('e-compose', 'docker-compose.yml', COMPOSE, 2, 3)],
  coverage: [{ area: 'src', status: 'inspected', evidenceIds: ['e-app'], notes: '' }],
  limitations: [],
  ...overrides,
});

test('a helicopter view is validated against the snapshot, saved as its own kind with the admitted producer and never overwritten', () => {
  const fixture = setup('helicopter');
  try {
    assert.deepEqual(saveHelicopter(fixture.dir, helicopter(fixture), actor), { requestId: fixture.requestId, saved: true });
    const saved = resultOf(fixture);
    assert.equal(saved.kind, 'helicopter');
    assert.equal(saved.architectSessionId, actor.sessionId);
    assert.equal(saved.snapshotId, fixture.snapshotId);
    assert.equal(saved.capabilities.length, 2);
    assert.equal(saved.edges.find(edge => edge.id === 'h4').certainty, 'unknown');
    assert.equal(saved.nodes.find(node => node.id === 'booking').kind, 'businessGroup');
    assert.throws(() => saveHelicopter(fixture.dir, helicopter(fixture), actor), /already completed/);
    const replay = loadInsightResultText(fixture.dir, 'helicopter', fixture.requestId);
    assert.match(replay, /Helicopter view \(Architect analysis[\s\S]*Purpose: Lets customers[\s\S]*\[journey\] Booking an appointment[\s\S]*Is a slot free\? -> Appointment is cancelled \(No\) \[Needs clarification\][\s\S]*Business capabilities \(2\)[\s\S]*Cancel a visit/);
    // The business replay never leaks repository paths or hashes.
    assert.doesNotMatch(replay, /src\/app\.ts|docker-compose|[0-9a-f]{64}/);
    assert.throws(() => loadInsightResultText(fixture.dir, 'systemOverview', fixture.requestId), /not a systemOverview request/);
  } finally { cleanup(fixture); }
  // Each submitter only serves its own kind, so a System result can never be stored as a Helicopter result or the reverse.
  const wrongForHelicopter = setup('systemOverview');
  try { rejects(() => saveHelicopter(wrongForHelicopter.dir, helicopter(wrongForHelicopter), actor), /not a helicopter request/); } finally { cleanup(wrongForHelicopter); }
  const wrongForSystem = setup('helicopter');
  try { rejects(() => saveSystemOverview(wrongForSystem.dir, overview(wrongForSystem), actor), /not a systemOverview request/); } finally { cleanup(wrongForSystem); }
  const otherProducer = setup('helicopter');
  try { rejects(() => saveHelicopter(otherProducer.dir, helicopter(otherProducer), { ...actor, sessionId: 'another' }), /different Architect/); } finally { cleanup(otherProducer); }
});

test('helicopter rules: evidence, journeys, containment, capabilities, enums and uncertainty are enforced before anything is published', () => {
  const fixture = setup('helicopter');
  try {
    const bad = (mutate, pattern) => { const payload = helicopter(fixture); mutate(payload); rejects(() => saveHelicopter(fixture.dir, payload, actor), pattern); };
    bad(payload => { payload.nodes[1].evidenceIds = []; }, /cites no evidence/);
    bad(payload => { payload.edges[3].certainty = 'inferred'; }, /Needs clarification/);
    bad(payload => { payload.edges[0].certainty = 'declared'; }, /certainty must be inferred or unknown/);
    bad(payload => { payload.nodes[0].kind = 'application'; }, /kind must be one of actor, trigger/);
    bad(payload => { payload.nodes[1].id = 'customer'; }, /duplicated/);
    bad(payload => { payload.nodes[1].parentId = 'customer'; }, /not a journey grouping/);
    bad(payload => { payload.nodes[1].parentId = 'ghost'; }, /unknown parent/);
    bad(payload => { payload.nodes[2].parentId = 'booking'; }, /cannot contain itself/);
    bad(payload => { payload.nodes[2].parentId = 'inner'; payload.nodes.push({ id: 'inner', kind: 'businessGroup', label: 'Inner', parentId: 'booking', description: '', evidenceIds: [] }); }, /containment cycle/);
    bad(payload => {
      payload.nodes[2].parentId = 'inner';
      payload.nodes.push({ id: 'inner', kind: 'businessGroup', label: 'Inner', parentId: 'booking', description: '', evidenceIds: [] });
      payload.nodes[6].parentId = 'ghost';
    }, /node "pay" has unknown parent "ghost"/);
    bad(payload => { payload.edges[0].targetId = 'ghost'; }, /dangling target/);
    bad(payload => { payload.edges[0].targetId = 'booking'; }, /containment is never a flow/);
    bad(payload => { payload.edges[0].sourceId = 'booking'; }, /containment is never a flow/);
    bad(payload => { payload.edges[1].id = 'h1'; }, /duplicated/);
    bad(payload => { payload.edges[0].relation = 'calls'; }, /unknown field/);
    bad(payload => { payload.edges[0].evidenceIds = ['nope']; }, /unknown evidence/);
    bad(payload => { payload.capabilities = []; }, /must list the business capabilities/);
    bad(payload => { payload.capabilities[0].nodeIds = []; }, /between 1 and 500 nodes/);
    bad(payload => { payload.capabilities[0].nodeIds = ['ghost']; }, /unknown node/);
    bad(payload => { payload.capabilities[0].nodeIds = ['asks', 'asks']; }, /names a node twice/);
    bad(payload => { payload.capabilities[0].evidenceIds = []; }, /do not invent product capabilities/);
    bad(payload => { payload.capabilities[1].id = 'cap-book'; }, /duplicated/);
    bad(payload => { payload.purpose = ''; }, /purpose must be non-empty/);
    bad(payload => { payload.snapshotId = randomUUID(); }, /snapshotId must be/);
    bad(payload => { payload.schemaVersion = 2; }, /schemaVersion/);
    bad(payload => { payload.evidence[0].contentHash = sha('other'); }, /does not match the snapshot file/);
    bad(payload => { payload.coverage[0].evidenceIds = []; }, /without evidence/);
    bad(payload => { payload.coverage = []; }, /coverage must account/);
    bad(payload => { payload.nodes[0].label = 'x'.repeat(201); }, /at most 200/);
    bad(payload => { payload.nodes = Array.from({ length: 2001 }, (_, index) => ({ id: `n${index}`, kind: 'activity', label: 'n', parentId: null, description: '', evidenceIds: ['e-app'] })); payload.edges = []; payload.capabilities[0].nodeIds = ['n1']; payload.capabilities[1].nodeIds = ['n2']; }, /at most 2000/);
    // No node at all is acceptable only with a stated reason.
    const empty = payload => { payload.nodes = []; payload.edges = []; payload.capabilities = []; };
    bad(empty, /must state its limitations/);
    bad(payload => { payload.limitations = ['x'.repeat(2001)]; }, /at most 2000/);
    const none = helicopter(fixture, { limitations: ['Only configuration files were present.'] });
    empty(none);
    saveHelicopter(fixture.dir, none, actor);
    assert.deepEqual(resultOf(fixture).nodes, []);
  } finally { cleanup(fixture); }
});

test('helicopter business text stays plain language: code, paths, identifiers, URLs and secrets are refused everywhere it is shown', () => {
  const fixture = setup('helicopter');
  try {
    const bad = (mutate, pattern) => { const payload = helicopter(fixture); mutate(payload); rejects(() => saveHelicopter(fixture.dir, payload, actor), pattern); };
    bad(payload => { payload.purpose = 'Reads src/app.ts to find bookings'; }, /purpose looks like a file path/);
    bad(payload => { payload.nodes[4].label = 'Calls createOrder()'; }, /label looks like code syntax/);
    bad(payload => { payload.nodes[4].label = 'Stores order_id'; }, /code identifier/);
    bad(payload => { payload.nodes[4].description = 'see app.ts'; }, /source file name/);
    bad(payload => { payload.edges[2].label = '`yes`'; }, /inline code/);
    bad(payload => { payload.capabilities[0].description = 'lives in lib/booking'; }, /file path/);
    bad(payload => { payload.capabilities[0].label = 'C:\\data\\orders'; }, /file path/);
    bad(payload => { payload.nodes[6].description = 'https://pay.example.com/v1/charge'; }, /raw URL/);
    bad(payload => { payload.nodes[6].description = 'password = "hunter2hunter2"'; }, /secret or credential/);
    // Evidence records keep the technical proof; limitations and coverage may name areas.
    const ok = helicopter(fixture, { limitations: ['Only src/app.ts and docker-compose.yml were readable.'], coverage: [{ area: 'src/app.ts', status: 'inspected', evidenceIds: ['e-app'], notes: 'application code' }] });
    ok.nodes[4].description = 'Pay and/or refund, 24/7 support, (or the clinic) cancels the visit(s). e.g. a late cancellation';
    saveHelicopter(fixture.dir, ok, actor);
  } finally { cleanup(fixture); }
});

test('technicalMarker agrees with the backend on what counts as code or a path', () => {
  for (const ok of ['Pay and/or refund', '24/7 support', 'Customer (or clinic) cancels', 'Cancel the visit(s)', 'Version 4.5 is announced.', 'e.g. a late cancellation', "Wait for the clinic's reply, then continue.", '[24/7]', '"and/or,"', 'rate/4.5', '_optional', 'optional_']) {
    assert.equal(technicalMarker(ok), null, ok);
  }
  for (const [bad, kind] of [
    ['C:\\data\\orders.csv', 'a file path'], ['open ./config/app', 'a file path'], ['src/booking', 'a file path'], ['see notes/readme.md', 'a file path'],
    ['handlers/order.ts is used', 'a file path'], ['start main.py', 'a source file name'], ['uses a => b', 'code syntax'], ['Order::new', 'code syntax'],
    ['order_total', 'a code identifier'], ['call `run`', 'inline code'],
    ['[src/booking],', 'a file path'], ['"MAIN.PY!"', 'a source file name'], ['notes/data.a1', 'a file path'],
    ['résumé_total', 'a code identifier'], ['order_total => result', 'code syntax'], ['src/app.ts and `run`', 'inline code'],
  ]) assert.equal(technicalMarker(bad), kind, bad);
  assert.equal(technicalMarker('Business' + '.'.repeat(32768) + 'text'), null);
  assert.equal(technicalMarker('.'.repeat(32768) + 'src/app.ts' + '!'.repeat(32768)), 'a file path');
});

test('fenced structure discovery keeps delimiter lengths, comments and native tree markers', () => {
  for (const value of [
    '   ~~~~text\nsrc/a/ # core\nREADME.md\n   ~~~~~` \t\n',
    '```\n  |-- src\n```',
    '~~~\n|  branch\n~~~',
    '~~~\n|\n  branch\n~~~',
    '````\n```\nsrc/\nlib/\n',
    '```\n```\nsrc/\nlib/\n',
    '```\ntext\n```\n~~~\nREADME.md\nconfig.toml\n~~~',
    '\r```\nsrc/\nlib/\n```',
    '```\n./\n../\n```',
    '```\na//b // comment\n/a/b/ — comment\n```',
  ]) assert.equal(markdownLike(value), true, value);
  for (const value of [
    '    ```\nsrc/\nlib/\n    ```',
    '```\nsrc/\n```',
    '```',
    '```js\nconst a = 1;\nfoo(a);\n```',
    '```text\nfolder.abcdefghi\none/\n```',
    '```text\nREADME.md note\none/\n```',
    '~~~\nnotes\n~~~\nsrc/\nlib/\n',
  ]) assert.equal(markdownLike(value), false, value);
});

test('structure discovery scans long whitespace and rejects incomplete path lines without truncating them', () => {
  assert.equal(markdownLike('```\n' + ' \n'.repeat(32768)), false);
  assert.equal(markdownLike('```\n' + 'a/'.repeat(16384) + '!\nsecond/\n```'), false);
  assert.equal(markdownLike('`'.repeat(32768) + '\nsrc/\nlib/\n```'), true);
});

test('submit_helicopter is a registered strict tool bound to the producer and its own kind, and announces the saved view', async () => {
  const fixture = setup('helicopter');
  try {
    const tools = [];
    const schema = { optional() { return this; }, nullable() { return this; } };
    const zod = Object.fromEntries(['object', 'array', 'string', 'number', 'enum'].map(name => [name, () => schema]));
    const enqueued = [];
    registerProjectInsightTools({ registerTool: tool => tools.push(tool), zod }, fixture.dir, ctx => { if (!ctx.ok) throw new Error('Architect identity unavailable'); return actor; },
      { registerKind() {}, enqueue: (_ctx, item) => enqueued.push(item) });
    const named = name => tools.find(tool => tool.name === name);
    assert.equal(TOOL_HELICOPTER, 'submit_helicopter');
    assert.ok(named('submit_helicopter'));
    assert.equal(named('submit_helicopter').approval(), 'read');
    // Another kind's submitter cannot save into a helicopter request, an unresolved producer is a tool error.
    assert.equal((await named('submit_system_overview').execute('id', overview(fixture), undefined, undefined, { ok: true })).isError, true);
    assert.equal((await named('submit_repository_structure').execute('id', structure(fixture), undefined, undefined, { ok: true })).isError, true);
    assert.equal((await named('submit_helicopter').execute('id', helicopter(fixture), undefined, undefined, { ok: false })).isError, true);
    assert.equal(existsSync(path.join(fixture.folder, 'result.json')), false);
    // The shared evidence/index tools serve a helicopter request exactly like the other visualizations.
    assert.equal(JSON.parse((await named('system_overview_inspect').execute('id', { requestId: fixture.requestId, section: 'files' }, undefined, undefined, { ok: true })).content[0].text).total, 3);
    const invalid = await named('submit_helicopter').execute('id', helicopter(fixture, { purpose: 'Opens src/app.ts' }), undefined, undefined, { ok: true });
    assert.equal(invalid.isError, true);
    assert.match(invalid.content[0].text, /purpose looks like a file path/);
    const saved = await named('submit_helicopter').execute('id', helicopter(fixture), undefined, undefined, { ok: true });
    assert.equal(saved.isError, undefined);
    assert.equal(enqueued.length, 1);
    assert.equal(enqueued[0].kind, 'helicopter');
    assert.equal((await named('submit_helicopter').execute('id', helicopter(fixture), undefined, undefined, { ok: true })).isError, true);
  } finally { cleanup(fixture); }
  const reader = setup('helicopter');
  try {
    assert.equal(JSON.parse(systemOverviewInspect(reader.dir, { requestId: reader.requestId, section: 'files' }, actor)).total, 3);
    assert.deepEqual(JSON.parse(repoEvidence(reader.dir, { requestId: reader.requestId, action: 'list' }, actor)).entries.map(entry => entry.name), ['docker-compose.yml', 'src']);
    assert.throws(() => repoEvidence(reader.dir, { requestId: reader.requestId, action: 'list' }, { ...actor, generation: 8 }), /different Architect/);
  } finally { cleanup(reader); }
});

test('replay formatting is exhaustive per kind: an unknown kind fails closed instead of printing a wrong layout', () => {
  assert.throws(() => formatVisualizationResult({ kind: 'surprise', folder: 'C:/p' }, { snapshotId: 's', limitations: [] }), /no console replay for request kind "surprise"/);
  const withHelicopter = formatVisualizationResult({ kind: 'helicopter', folder: 'C:/p' }, {
    snapshotId: 's', purpose: 'P', nodes: [], edges: [], capabilities: [], coverage: [], limitations: ['Only configuration was readable.'],
  });
  assert.match(withHelicopter, /Helicopter view[\s\S]*Purpose: P[\s\S]*Limitations:\n {2}- Only configuration was readable\./);
});

test('system replay keeps section boundaries, label resolution and the 80-row limit', () => {
  const result = {
    snapshotId: 'snapshot', limitations: [],
    nodes: Array.from({ length: 81 }, (_, index) => ({ id: `node-${index}`, kind: 'service', label: `Service ${index}`, parentId: index === 0 ? 'node-80' : null })),
    edges: Array.from({ length: 81 }, (_, index) => ({ sourceId: `node-${index}`, targetId: 'node-80', relation: 'reads', certainty: ['declared', 'inferred', 'unknown'][index % 3], label: index === 0 ? '' : `lookup ${index}` })),
    coverage: Array.from({ length: 81 }, (_, index) => ({ area: `Area ${index}`, status: 'unresolved', notes: index === 0 ? '' : 'outside scope' })),
  };
  const replay = formatVisualizationResult({ kind: 'systemOverview', folder: '/repo' }, result);
  const [header, elements, relationships, coverage, limitations] = replay.split('\n\n');
  assert.match(header, /Source snapshot: snapshot$/);
  for (const section of [elements, relationships, coverage]) {
    assert.equal(section.split('\n').filter(line => line.startsWith('  - ')).length, 80);
    assert.ok(section.endsWith('  ... 1 more in the saved result'));
  }
  assert.match(elements, /\n  - \[service\] Service 0 \(in Service 80\)\n/);
  assert.match(elements, /\n  - \[service\] Service 79\n/);
  assert.doesNotMatch(elements, /\n  - \[service\] Service 80/);
  assert.match(relationships, /\n  - Service 0 -reads-> Service 80 \[declared\]\n/);
  assert.match(relationships, /\n  - Service 1 -reads-> Service 80 \[inferred\]: lookup 1\n/);
  assert.match(relationships, /\n  - Service 2 -reads-> Service 80 \[unknown\]: lookup 2\n/);
  assert.doesNotMatch(relationships, /\n  - Service 80 -reads/);
  assert.match(coverage, /\n  - Area 0: unresolved\n  - Area 1: unresolved - outside scope\n/);
  assert.doesNotMatch(coverage, /\n  - Area 80:/);
  assert.match(limitations, /^Limitations:\n  - /);
});

test('helicopter replay uses business kinds and bounds capability rows without losing descriptions', () => {
  const result = helicopter({ requestId: 'request', snapshotId: 'snapshot' });
  result.capabilities = Array.from({ length: 81 }, (_, index) => ({ label: `Capability ${index}`, description: index === 0 ? '' : 'Available' }));
  const replay = formatVisualizationResult({ kind: 'helicopter', folder: '/repo' }, result);
  assert.match(replay, /\n\nPurpose: Lets customers/);
  assert.match(replay, /\n  - \[who\] Customer\n/);
  assert.match(replay, /\n  - \[starts when\] Customer asks for an appointment \(in Booking an appointment\)\n/);
  assert.match(replay, /\n  - \[outside party\] Payment provider\n\nFlow/);
  assert.match(replay, /\n\nBusiness capabilities \(81\):\n  - Capability 0\n  - Capability 1: Available\n/);
  assert.match(replay, /\n  - Capability 79: Available\n  \.\.\. 1 more in the saved result\n\nCoverage/);
  assert.doesNotMatch(replay, /\n  - Capability 80:/);
});

test('structure replay keeps path precedence, proposal directions, conflicts and section limits', () => {
  const result = proposed({ requestId: 'request', snapshotId: 'snapshot' });
  result.entries = Array.from({ length: 81 }, (_, index) => ({ label: `Group ${index}`, path: index === 0 ? 'src/core' : null, pattern: index < 2 ? '*.toml' : null, purpose: `Purpose ${index}` }));
  result.conventions = Array.from({ length: 81 }, (_, index) => ({ scope: `Scope ${index}`, current: `Current ${index}`, proposed: index === 0 ? null : `Proposed ${index}` }));
  result.mappings = [
    { operation: 'create', fromPaths: [], toPaths: ['new'] },
    { operation: 'remove', fromPaths: ['old'], toPaths: [] },
    ...Array.from({ length: 79 }, (_, index) => ({ operation: 'move', fromPaths: [`old-${index}`], toPaths: [`new-${index}`] })),
  ];
  result.conflicts = Array.from({ length: 81 }, (_, index) => `Conflict ${index}`);
  const replay = formatVisualizationResult({ kind: 'structureProposed', folder: '/repo' }, result);
  const [header, groups, conventions, mappings, conflicts] = replay.split('\n\n');
  assert.ok(header.includes(`Target document: source-1 (sha256 ${sha(TARGET)})`));
  for (const section of [groups, conventions, mappings]) {
    assert.equal(section.split('\n').filter(line => line.startsWith('  - ')).length, 80);
    assert.ok(section.endsWith('  ... 1 more in the saved result'));
  }
  assert.match(groups, /\n  - Group 0 \(src\/core\): Purpose 0\n  - Group 1 \(\*\.toml\): Purpose 1\n/);
  assert.match(groups, /\n  - Group 79: Purpose 79\n/);
  assert.doesNotMatch(groups, /\n  - Group 80/);
  assert.match(conventions, /\n  - Scope 0: Current 0\n  - Scope 1: Current 1 -> Proposed 1\n/);
  assert.doesNotMatch(conventions, /\n  - Scope 80:/);
  assert.match(mappings, /\n  - create: \(none\) -> new\n  - remove: old -> \(none\)\n/);
  assert.match(mappings, /\n  - move: old-77 -> new-77\n/);
  assert.doesNotMatch(mappings, /old-78/);
  assert.match(conflicts, /^Target document conflicts:\n  - Conflict 0\n/);
  assert.ok(conflicts.endsWith('  - Conflict 80'));
  const current = formatVisualizationResult({ kind: 'structureCurrent', folder: '/repo' }, structure({ requestId: 'request', snapshotId: 'snapshot' }));
  assert.match(current, /\n  - Infrastructure files \(\*\.yml\): Deployment config\n\nConventions/);
  assert.doesNotMatch(current, /Target document:|Mappings \(|Target document conflicts:/);
});

test('project_visualization_prompt admits a helicopter request in the existing Architect session; its guard allows only its own submitter', async () => {
  const fixture = setup('helicopter');
  try {
    const sent = [];
    const handlers = new Map();
    const pi = { registerTool() {}, zod: new Proxy({}, { get: () => () => ({ optional() { return this; }, nullable() { return this; } }) }), on: (name, handler) => handlers.set(name, handler) };
    const control = registerProjectInsightTools(pi, fixture.dir, ctx => { if (!ctx.ok) throw new Error('Architect identity unavailable'); return actor; }, undefined, {
      submitPrompt: (message, callbacks) => { sent.push(message); callbacks.onPromptAdmitted(); return new Promise(() => {}); },
    });
    const ctx = { ok: true, agent: { kind: 'main' }, sessionManager: { getSessionId: () => actor.sessionId } };
    const payload = { requestId: fixture.requestId, kind: 'helicopter', message: 'Describe the business.', source: 'human', generation: 7 };
    // A System request id cannot be admitted as a helicopter prompt (and the reverse), so the kinds never share an admission.
    await assert.rejects(control.promptVisualization(ctx, { ...payload, kind: 'systemOverview' }), /not a systemOverview request/);
    assert.deepEqual(await control.promptVisualization(ctx, payload), { accepted: true, duplicate: false });
    assert.equal(sent.length, 1);
    assert.match(sent[0], /^Describe the business\.\n\n\[omp-visualization-request id=/);
    assert.deepEqual(await control.promptVisualization(ctx, payload), { accepted: true, duplicate: true });
    assert.equal(sent.length, 1);
    handlers.get('before_agent_start')({ prompt: sent[0] }, ctx);
    assert.equal(control.guard.states()[0].kind, 'helicopter');
    assert.equal(control.guard.states()[0].state, 'active');
    assert.equal(handlers.get('tool_call')({ toolName: 'bash', input: { command: 'ls' } }, ctx).block, true);
    assert.equal(handlers.get('tool_call')({ toolName: 'submit_system_overview', input: { requestId: fixture.requestId } }, ctx).block, true);
    assert.equal(handlers.get('tool_call')({ toolName: 'submit_repository_structure', input: { requestId: fixture.requestId } }, ctx).block, true);
    assert.equal(handlers.get('tool_call')({ toolName: 'repo_evidence', input: { requestId: fixture.requestId, action: 'list' } }, ctx), undefined);
    // The helicopter turn must be able to call ITS submitter (this fails if the guard still maps every non-System kind to the structure tool).
    assert.equal(handlers.get('tool_call')({ toolName: 'submit_helicopter', input: { requestId: fixture.requestId } }, ctx), undefined);
    handlers.get('tool_result')({ toolName: 'submit_helicopter', input: { requestId: fixture.requestId }, isError: false }, ctx);
    assert.equal(control.guard.states()[0].state, 'submitted');
  } finally { cleanup(fixture); }
});
