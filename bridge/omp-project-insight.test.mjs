import test from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import os from 'node:os';
import path from 'node:path';
import {
  MAX_LIST_ENTRIES, MAX_READ_BYTES, containsSecret, evidenceExcluded, loadInsightResultText, registerProjectInsightTools, repoEvidence,
  saveDependencyDiscovery, saveRequirementsFindings,
} from './omp-project-insight.mjs';

const actor = { instanceId: 'architect-fixture', generation: 7, launchId: 'launch-fixture', sessionId: 'session-fixture' };
const fresh = () => mkdtempSync(path.join(os.tmpdir(), 'omp-insight-'));

// `dir` is the architect directory; requests live in its sibling `project-insight/`.
function setup(kind = 'dependencyDiscovery', overrides = {}) {
  const root = fresh();
  const dir = path.join(root, 'architect');
  mkdirSync(dir);
  const project = path.join(root, 'project');
  mkdirSync(project);
  const requestId = randomUUID();
  const request = {
    schemaVersion: 1, requestId, folder: project, key: '0123456789abcdef', kind, createdAt: 1_760_000_000_000, evidenceInventory: [],
    architectInstanceId: actor.instanceId, architectGeneration: actor.generation, architectLaunchId: actor.launchId, architectSessionId: actor.sessionId,
    ...overrides,
  };
  const folder = path.join(root, 'project-insight', requestId);
  mkdirSync(folder, { recursive: true });
  writeFileSync(path.join(folder, 'request.json'), JSON.stringify(request));
  return { root, dir, project, requestId, folder, request };
}
const cleanup = fixture => rmSync(fixture.root, { recursive: true, force: true });
const evidence = (fixture, params) => JSON.parse(repoEvidence(fixture.dir, { requestId: fixture.requestId, ...params }, actor));
const resultOf = fixture => JSON.parse(readFileSync(path.join(fixture.folder, 'result.json'), 'utf8'));

const discovery = (requestId, overrides = {}) => ({
  requestId,
  items: [{ name: 'requests', ecosystem: 'pypi', declaredVersion: '2.31.0', relationship: 'runtime', citations: [{ path: 'app.py', line: 1 }], notes: 'Imported in app.py' }],
  searchedAreas: ['*.py imports'], limitations: ['No lock file was present'],
  ...overrides,
});
const requirement = (overrides = {}) => ({
  id: 'podman', name: 'Podman', category: 'containerEngine', need: 'optional', environment: 'host',
  evidence: [{ path: 'README.md', line: 12, text: 'Install Podman first' }], explanation: 'The README asks for Podman.', ...overrides,
});
const requirements = (requestId, items = [requirement()]) => ({ requestId, items, searchedAreas: ['README.md'], limitations: [] });

test('evidence names: build output, VCS state and secret files are excluded but samples are readable', () => {
  for (const name of ['.git', 'node_modules', 'target', 'dist', 'build', '.venv', '__pycache__']) assert.equal(evidenceExcluded(name, true), true, name);
  for (const name of ['.env', '.env.local', 'server.pem', 'KEY.KEY', 'id_rsa', 'id_ed25519.pub', '.npmrc', 'cert.pfx']) assert.equal(evidenceExcluded(name, false), true, name);
  for (const name of ['.env.example', '.env.sample', 'README.md', 'identity.md', 'package.json', 'key.txt']) assert.equal(evidenceExcluded(name, false), false, name);
  assert.equal(evidenceExcluded('src', true), false);
});

test('repo_evidence lists and reads only the request repository and omits secrets, links and excluded directories', () => {
  const fixture = setup();
  try {
    writeFileSync(path.join(fixture.project, 'app.py'), 'import requests\n');
    writeFileSync(path.join(fixture.project, '.env'), 'TOKEN=secret');
    writeFileSync(path.join(fixture.project, '.env.example'), 'TOKEN=');
    writeFileSync(path.join(fixture.project, 'server.pem'), 'x');
    writeFileSync(path.join(fixture.project, 'id_rsa'), 'x');
    mkdirSync(path.join(fixture.project, 'node_modules', 'x'), { recursive: true });
    mkdirSync(path.join(fixture.project, '.git'));
    mkdirSync(path.join(fixture.project, 'src'));
    writeFileSync(path.join(fixture.project, 'src', 'main.py'), 'print(1)\n');
    const listing = evidence(fixture, { action: 'list' });
    assert.deepEqual(listing.entries.map(entry => entry.name), ['.env.example', 'app.py', 'src']);
    assert.equal(listing.truncated, false);
    assert.deepEqual(evidence(fixture, { action: 'list', path: 'src' }).entries, [{ name: 'main.py', type: 'file', size: 9 }]);
    const read = evidence(fixture, { action: 'read', path: 'app.py' });
    assert.deepEqual({ ...read }, { path: 'app.py', size: 16, offset: 0, nextOffset: null, text: 'import requests\n' });
    assert.equal(evidence(fixture, { action: 'read', path: 'src\\main.py' }).path, 'src/main.py');
    for (const bad of ['.env', 'server.pem', 'id_rsa', 'node_modules/x', '.git/config', '../outside.txt', '/etc/passwd', 'C:\\Windows\\win.ini', 'src/../app.py', '', 'missing.txt']) {
      assert.throws(() => evidence(fixture, { action: 'read', path: bad }), undefined, bad);
    }
    assert.throws(() => evidence(fixture, { action: 'list', path: 'node_modules' }));
    assert.throws(() => evidence(fixture, { action: 'list', path: 'app.py' }), /not a directory/);
    assert.throws(() => evidence(fixture, { action: 'read', path: 'src' }), /not a file/);
    assert.throws(() => evidence(fixture, { action: 'delete', path: 'app.py' }), /action/);
  } finally { cleanup(fixture); }
});

test('repo_evidence refuses links that leave the repository', () => {
  const fixture = setup();
  try {
    const outside = path.join(fixture.root, 'outside');
    mkdirSync(outside);
    writeFileSync(path.join(outside, 'secret.txt'), 'outside');
    symlinkSync(outside, path.join(fixture.project, 'linked'), process.platform === 'win32' ? 'junction' : 'dir');
    assert.equal(evidence(fixture, { action: 'list' }).entries.some(entry => entry.name === 'linked'), false);
    assert.throws(() => evidence(fixture, { action: 'read', path: 'linked/secret.txt' }), /not available/);
    assert.throws(() => evidence(fixture, { action: 'list', path: 'linked' }), /not available/);
  } finally { cleanup(fixture); }
});

test('repo_evidence is bound to the Architect producer and to an open request', () => {
  const fixture = setup();
  try {
    writeFileSync(path.join(fixture.project, 'app.py'), 'x');
    const call = (id, who = actor) => repoEvidence(fixture.dir, { requestId: id, action: 'list' }, who);
    assert.doesNotThrow(() => call(fixture.requestId));
    for (const other of [{ ...actor, sessionId: 'other' }, { ...actor, launchId: 'other' }, { ...actor, generation: 8 }, { ...actor, instanceId: 'other' }]) {
      assert.throws(() => call(fixture.requestId, other), /different Architect/);
    }
    assert.throws(() => call('not-a-uuid'), /invalid request id/);
    assert.throws(() => call(randomUUID()));
    writeFileSync(path.join(fixture.folder, 'terminal.json'), JSON.stringify({ status: 'error', code: 'producerEnded', message: 'ended', at: 1 }));
    assert.throws(() => call(fixture.requestId), /already ended: ended/);
  } finally { cleanup(fixture); }
});

test('repo_evidence pages large files on UTF-8 boundaries and caps listings', () => {
  const fixture = setup();
  try {
    const text = `a${'é'.repeat(MAX_READ_BYTES)}`; // the first 64 KiB window ends in the middle of a two-byte character
    writeFileSync(path.join(fixture.project, 'big.txt'), `${text}tail`);
    let offset = 0;
    let rebuilt = '';
    for (let guard = 0; guard < 10; guard++) {
      const page = evidence(fixture, { action: 'read', path: 'big.txt', offset });
      assert.equal(page.text.includes('\ufffd'), false);
      rebuilt += page.text;
      if (page.nextOffset === null) break;
      assert.ok(page.nextOffset > offset);
      offset = page.nextOffset;
    }
    assert.equal(rebuilt, text + 'tail');
    writeFileSync(path.join(fixture.project, 'bin.dat'), Buffer.from([1, 0, 2]));
    assert.throws(() => evidence(fixture, { action: 'read', path: 'bin.dat' }), /binary/);
    assert.throws(() => evidence(fixture, { action: 'read', path: 'big.txt', offset: -1 }), /offset/);
    for (let index = 0; index < MAX_LIST_ENTRIES + 5; index++) writeFileSync(path.join(fixture.project, `f${String(index).padStart(4, '0')}.txt`), 'x');
    const listing = evidence(fixture, { action: 'list' });
    assert.equal(listing.entries.length, MAX_LIST_ENTRIES);
    assert.equal(listing.truncated, true);
    assert.equal(listing.total, MAX_LIST_ENTRIES + 7);
  } finally { cleanup(fixture); }
});

test('a discovery is saved with the admitted producer, forced nulls and never overwritten', () => {
  const fixture = setup();
  try {
    writeFileSync(path.join(fixture.project, 'app.py'), 'import requests');
    const payload = discovery(fixture.requestId);
    payload.items[0].resolvedVersion = '9.9.9';
    payload.items[0].extra = 'ignored';
    payload.completedAt = 1;
    assert.deepEqual(saveDependencyDiscovery(fixture.dir, payload, actor), { requestId: fixture.requestId, saved: true });
    const saved = resultOf(fixture);
    assert.equal(saved.kind, 'dependencyDiscovery');
    assert.equal(saved.architectInstanceId, actor.instanceId);
    assert.equal(saved.architectSessionId, actor.sessionId);
    assert.equal(saved.items[0].resolvedVersion, null);
    assert.equal(saved.items[0].extra, undefined);
    assert.ok(saved.completedAt > 1);
    assert.deepEqual(saved.items[0].citations, [{ path: 'app.py', line: 1 }]);
    assert.throws(() => saveDependencyDiscovery(fixture.dir, discovery(fixture.requestId), actor), /already completed/);
    assert.equal(existsSync(path.join(fixture.folder, 'result.lock')), false);
  } finally { cleanup(fixture); }
});

test('a discovery is rejected for the wrong producer, request kind, citations or secrets', () => {
  const fixture = setup();
  try {
    const attempt = (payload, who = actor) => saveDependencyDiscovery(fixture.dir, payload, who);
    assert.throws(() => attempt(discovery(fixture.requestId), { ...actor, sessionId: 'other' }), /different Architect/);
    assert.throws(() => attempt(discovery(fixture.requestId, { items: [{ ...discovery('x').items[0], citations: [] }] })), /invalid dependency item/);
    assert.throws(() => attempt(discovery(fixture.requestId, { items: [{ ...discovery('x').items[0], citations: [{ path: '../x', line: 1 }] }] })), /invalid dependency item/);
    assert.throws(() => attempt(discovery(fixture.requestId, { items: [{ ...discovery('x').items[0], citations: [{ path: 'app.py', line: 0 }] }] })), /invalid dependency item/);
    assert.throws(() => attempt(discovery(fixture.requestId, { items: [{ ...discovery('x').items[0], ecosystem: 'PyPI' }] })), /invalid dependency item/);
    assert.throws(() => attempt(discovery(fixture.requestId, { items: [{ ...discovery('x').items[0], notes: 'api_key = abcd1234efgh5678' }] })), /secret/);
    assert.throws(() => attempt(discovery(fixture.requestId, { searchedAreas: [] })), /searchedAreas/);
    assert.throws(() => attempt(discovery(fixture.requestId, { items: [], limitations: [] })), /limitations/);
    assert.throws(() => attempt(discovery('00000000-0000-4000-8000-000000000000')));
    assert.equal(existsSync(path.join(fixture.folder, 'result.json')), false);
    // A requirements submission cannot target a dependencyDiscovery request.
    assert.throws(() => saveRequirementsFindings(fixture.dir, requirements(fixture.requestId), actor), /not a requirements request/);
    // A request that has ended accepts nothing.
    writeFileSync(path.join(fixture.folder, 'terminal.json'), JSON.stringify({ status: 'error', code: 'producerEnded', message: 'stopped', at: 1 }));
    assert.throws(() => attempt(discovery(fixture.requestId)), /already ended: stopped/);
  } finally { cleanup(fixture); }
});

test('requirements findings are saved as unverified Architect items with neutral detection and install state', () => {
  const fixture = setup('requirements');
  try {
    writeFileSync(path.join(fixture.project, 'README.md'), 'Install Podman first');
    const hostile = requirement({ detected: { state: 'found', path: 'C:/podman.exe' }, install: { availability: 'automatic' }, source: 'static', setupLink: 'codeQuality' });
    saveRequirementsFindings(fixture.dir, requirements(fixture.requestId, [hostile]), actor);
    const saved = resultOf(fixture);
    assert.equal(saved.kind, 'requirements');
    assert.deepEqual(saved.items, [{
      id: 'architect:podman', name: 'Podman', category: 'containerEngine', need: 'optional', environment: 'host',
      evidence: [{ path: 'README.md', line: 12, text: 'Install Podman first' }],
      detected: { state: 'unknown' }, install: { availability: 'unknown', note: 'Reported by The Architect; availability was not checked.' },
      explanation: 'The README asks for Podman.', source: 'architect', setupLink: null,
    }]);
  } finally { cleanup(fixture); }
});

test('requirements findings reject secrets, bad enums, duplicates and requests of another kind', () => {
  const fixture = setup('requirements');
  try {
    const attempt = items => saveRequirementsFindings(fixture.dir, requirements(fixture.requestId, items), actor);
    assert.throws(() => attempt([requirement({ category: 'program' })]), /invalid requirement item/);
    assert.throws(() => attempt([requirement({ need: 'maybe' })]), /invalid requirement item/);
    assert.throws(() => attempt([requirement({ id: 'Bad Id' })]), /invalid requirement item/);
    assert.throws(() => attempt([requirement(), requirement()]), /invalid requirement item/);
    assert.throws(() => attempt([requirement({ evidence: [{ path: '/abs', line: null, text: 'x' }] })]), /invalid requirement item/);
    assert.throws(() => attempt([requirement({ explanation: 'Set PASSWORD=hunter2hunter2 before running' })]), /secret/);
    assert.throws(() => attempt([requirement({ evidence: [{ path: 'README.md', line: null, text: '-----BEGIN RSA PRIVATE KEY-----' }] })]), /secret/);
    assert.throws(() => attempt([requirement({ evidence: [{ path: 'README.md', line: 5_000_000_000, text: 'x' }] })]), /invalid requirement item/);
    assert.throws(() => saveDependencyDiscovery(fixture.dir, discovery(fixture.requestId), actor), /not a dependencyDiscovery request/);
    assert.equal(existsSync(path.join(fixture.folder, 'result.json')), false);
    // A line is optional, and an environment variable may be named but not valued.
    assert.doesNotThrow(() => attempt([requirement({ evidence: [{ path: 'README.md', line: null, text: 'Set the API_KEY environment variable' }] })]));
    assert.equal(resultOf(fixture).items[0].evidence[0].line, undefined);
  } finally { cleanup(fixture); }
});

test('secret detection catches credentials but not ordinary prose', () => {
  for (const value of ['-----BEGIN PRIVATE KEY-----', 'ghp_abcdefghijklmnopqrstuvwxyz0123', 'AKIAABCDEFGHIJKLMNOP', 'token: abcdef123456', 'https://user:pass@example.org/x', 'sk-abcdefghijklmnopqrstuvwx']) {
    assert.equal(containsSecret(value), true, value);
  }
  for (const value of ['Install Podman', 'Set the API_KEY environment variable', 'tokenizer package', 'password policy documentation', null]) assert.equal(containsSecret(value), false, String(value));
});

test('saved results replay as console text by id and kind only', () => {
  const fixture = setup();
  try {
    writeFileSync(path.join(fixture.project, 'app.py'), 'x');
    assert.throws(() => loadInsightResultText(fixture.dir, 'dependencyDiscovery', fixture.requestId), /no saved result/);
    saveDependencyDiscovery(fixture.dir, discovery(fixture.requestId), actor);
    const shown = loadInsightResultText(fixture.dir, 'dependencyDiscovery', fixture.requestId);
    assert.match(shown, /requests \[pypi\] declared 2\.31\.0, runtime \(app\.py:1\)/);
    assert.match(shown, /nothing was installed or changed/);
    assert.throws(() => loadInsightResultText(fixture.dir, 'requirements', fixture.requestId), /not a requirements request/);
    assert.throws(() => loadInsightResultText(fixture.dir, 'security', fixture.requestId), /invalid result kind/);
  } finally { cleanup(fixture); }
});

test('the three Architect tools register, resolve the producer from the session and report failures as tool errors', async () => {
  const fixture = setup();
  try {
    writeFileSync(path.join(fixture.project, 'app.py'), 'import requests');
    const tools = [];
    const schema = { optional() { return this; }, nullable() { return this; } };
    const zod = Object.fromEntries(['object', 'array', 'string', 'number', 'enum'].map(name => [name, () => schema]));
    const kinds = [];
    const enqueued = [];
    registerProjectInsightTools({ registerTool: tool => tools.push(tool), zod }, fixture.dir, ctx => { if (!ctx.ok) throw new Error('Architect identity unavailable'); return actor; },
      { registerKind: kind => kinds.push(kind), enqueue: (_ctx, item) => enqueued.push(item) });
    assert.deepEqual(tools.map(tool => tool.name), ['repo_evidence', 'submit_dependency_discovery', 'submit_requirements_findings', 'system_overview_inspect', 'read_target_structure', 'submit_system_overview', 'submit_repository_structure']);
    assert.deepEqual(kinds, ['dependencyDiscovery', 'requirements', 'systemOverview', 'structureCurrent', 'structureProposed']);
    const named = name => tools.find(tool => tool.name === name);
    const listed = await named('repo_evidence').execute('id', { requestId: fixture.requestId, action: 'list' }, undefined, undefined, { ok: true });
    assert.equal(JSON.parse(listed.content[0].text).entries[0].name, 'app.py');
    const denied = await named('repo_evidence').execute('id', { requestId: fixture.requestId, action: 'list' }, undefined, undefined, { ok: false });
    assert.equal(denied.isError, true);
    const saved = await named('submit_dependency_discovery').execute('id', discovery(fixture.requestId), undefined, undefined, { ok: true });
    assert.equal(saved.isError, undefined);
    assert.equal(enqueued.length, 1);
    assert.equal(enqueued[0].kind, 'dependencyDiscovery');
    const again = await named('submit_dependency_discovery').execute('id', discovery(fixture.requestId), undefined, undefined, { ok: true });
    assert.equal(again.isError, true);
    const wrongKind = await named('submit_requirements_findings').execute('id', requirements(fixture.requestId), undefined, undefined, { ok: true });
    assert.equal(wrongKind.isError, true);
    assert.equal(enqueued.length, 1);
  } finally { cleanup(fixture); }
});
