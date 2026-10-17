import test from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, mkdirSync, readFileSync, readdirSync, rmdirSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { createHash, randomUUID } from 'node:crypto';
import { Worker } from 'node:worker_threads';
import os from 'node:os';
import path from 'node:path';
import { saveClassification, saveSecurityReview, saveUsageExplanation, saveInhouse, saveProjectKind, rememberStandard, registerClassifyTool, architectIdentity, loadSavedResultText } from './omp-architect.mjs';

const KEY = '0123456789abcdef';
const fresh = () => mkdtempSync(path.join(os.tmpdir(), 'omp-architect-'));
const read = dir => JSON.parse(readFileSync(path.join(dir, 'classifications', `${KEY}.json`), 'utf8'));

test('classifications merge by package id across calls', () => {
  const dir = fresh();
  try {
    assert.equal(saveClassification(dir, { key: KEY, items: [{ id: 'npm:a', basic: true, reason: 'core' }, { id: 'npm:b', basic: false }] }), 2);
    assert.equal(saveClassification(dir, { key: KEY, items: [{ id: 'npm:b', basic: true, reason: 'now basic' }, { id: 'npm:c', basic: false, reason: 'x'.repeat(500) }] }), 2);
    const saved = read(dir);
    assert.deepEqual(saved.items['npm:a'], { basic: true, reason: 'core' });
    assert.deepEqual(saved.items['npm:b'], { basic: true, reason: 'now basic' });
    assert.equal(saved.items['npm:c'].reason.length, 200);
    assert.equal(typeof saved.updatedAt, 'number');
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('invalid requests write nothing', () => {
  const dir = fresh();
  try {
    for (const bad of [
      { key: '../x', items: [{ id: 'a', basic: true }] },
      { key: KEY, items: [] },
      { key: KEY, items: [{ id: 'a', basic: 'yes' }] },
      { key: KEY, items: [{ id: 'a'.repeat(301), basic: true }] },
    ]) assert.throws(() => saveClassification(dir, bad));
    assert.equal(existsSync(path.join(dir, 'classifications')), false);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('a corrupt saved file is replaced instead of crashing', () => {
  const dir = fresh();
  try {
    mkdirSync(path.join(dir, 'classifications'));
    writeFileSync(path.join(dir, 'classifications', `${KEY}.json`), '{not json');
    saveClassification(dir, { key: KEY, items: [{ id: 'npm:a', basic: false }] });
    assert.deepEqual(Object.keys(read(dir).items), ['npm:a']);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('in-house judgements merge and reject invalid input before writing', () => {
  const dir = fresh();
  try {
    saveInhouse(dir, { key: KEY, items: [{ id: 'npm:a', inhouse: true, reason: 'private' }] });
    saveInhouse(dir, { key: KEY, items: [{ id: 'npm:b', inhouse: false }] });
    const saved = JSON.parse(readFileSync(path.join(dir, 'inhouse', `${KEY}.json`), 'utf8'));
    assert.equal(saved.items['npm:a'].inhouse, true);
    assert.equal(saved.items['npm:b'].inhouse, false);
    assert.equal(typeof saved.updatedAt, 'number');
    for (const bad of [
      { key: '../x', items: [{ id: 'a', inhouse: true }] },
      { key: KEY, items: [] }, { key: KEY, items: Array(2001).fill({ id: 'a', inhouse: true }) },
      { key: KEY, items: [{ id: 'a', inhouse: 'yes' }] },
      { key: KEY, items: [{ id: 'a'.repeat(301), inhouse: true }] },
      { key: KEY, items: [{ id: 'a', inhouse: true, reason: 'x'.repeat(201) }] },
    ]) assert.throws(() => saveInhouse(dir, bad));
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('project kind overwrites and validates key kind and reason', () => {
  const dir = fresh();
  try {
    saveProjectKind(dir, { key: KEY, kind: 'backend', reason: 'API' });
    assert.equal(saveProjectKind(dir, { key: KEY, kind: 'frontend' }), 'frontend');
    const saved = JSON.parse(readFileSync(path.join(dir, 'project-kind', `${KEY}.json`), 'utf8'));
    assert.equal(saved.kind, 'frontend');
    assert.equal(saved.reason, '');
    assert.equal(typeof saved.updatedAt, 'number');
    assert.throws(() => saveProjectKind(dir, { key: '../x', kind: 'backend' }), /invalid key/);
    assert.throws(() => saveProjectKind(dir, { key: KEY, kind: 'server' }), /invalid kind/);
    assert.throws(() => saveProjectKind(dir, { key: KEY, kind: 'backend', reason: 'x'.repeat(201) }), /invalid reason/);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('known standard memory only grows and preserves first timestamp', () => {
  const dir = fresh();
  try {
    assert.equal(rememberStandard(dir, [
      { id: 'npm:a', basic: true, reason: 'x'.repeat(300) }, { id: 'cargo:b', basic: true },
      { id: 'pypi:no', basic: false }, { id: 'java:no', basic: true }, { id: `npm:${'x'.repeat(301)}`, basic: true },
    ]), 2);
    const file = path.join(dir, 'known-standard', 'npm.json');
    const first = JSON.parse(readFileSync(file, 'utf8')).items['npm:a'];
    assert.equal(first.reason.length, 200);
    rememberStandard(dir, [{ id: 'npm:a', basic: false }]);
    assert.deepEqual(JSON.parse(readFileSync(file, 'utf8')).items['npm:a'], first);
    rememberStandard(dir, [{ id: 'npm:a', basic: true, reason: 'core' }]);
    assert.deepEqual(JSON.parse(readFileSync(file, 'utf8')).items['npm:a'], { reason: 'core', addedAt: first.addedAt });
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('classification tool reports saved classification even when learning fails', async () => {
  const dir = fresh();
  try {
    const tools = [];
    const schema = { optional() { return this; }, nullable() { return this; } };
    const zod = Object.fromEntries(['object', 'array', 'string', 'boolean', 'enum', 'number', 'any', 'literal', 'union', 'record', 'unknown'].map(name => [name, () => schema]));
    registerClassifyTool({ registerTool: tool => tools.push(tool), zod }, dir);
    const tool = tools.find(tool => tool.name === 'classify_dependencies');
    const params = { key: KEY, items: [{ id: 'npm:a', basic: true }] };
    assert.equal((await tool.execute('id', params)).content[0].text, 'Saved 1 classifications.');
    rmSync(path.join(dir, 'known-standard'), { recursive: true });
    writeFileSync(path.join(dir, 'known-standard'), 'not a directory');
    const result = await tool.execute('id', params);
    assert.match(result.content[0].text, /^Saved 1 classifications\. Known-standard list not updated:/);
    assert.notEqual(result.isError, true);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

const actor = { instanceId: 'architect-fixture', generation: 7, launchId: 'launch-fixture', sessionId: 'session-fixture' };
const CONTEXT = 'a'.repeat(64);
function admitted(dir, overrides = {}) {
  const request = {
    requestId: randomUUID(), folder: path.resolve(dir, 'project'), packageId: 'npm:example', name: 'example', ecosystem: 'npm',
    resolvedVersions: ['1.2.3', '2.0.1'], declaredVersions: ['^1.2.0'], newestVersion: '4.0.0', newestStatus: 'available',
    registryUrl: 'https://www.npmjs.com/package/example', repositoryUrl: 'https://github.com/example/example',
    architectInstanceId: actor.instanceId, architectGeneration: actor.generation,
    schemaVersion: 2, kind: 'security', createdAt: 1_760_000_000_000, contextId: CONTEXT,
    architectLaunchId: actor.launchId, architectSessionId: actor.sessionId,
    ...overrides,
  };
  mkdirSync(request.folder, { recursive: true });
  const folder = path.join(dir, 'security', request.requestId);
  mkdirSync(folder, { recursive: true });
  writeFileSync(path.join(folder, 'request.json'), JSON.stringify(request));
  return request;
}
const evidence = (overrides = {}) => ({ path: 'src/index.js', lineStart: 3, lineEnd: 9, explanation: 'Calls the affected parser with request bodies.', ...overrides });
const applicability = (status = 'relevant') => ({
  status, reason: 'Synthetic applicability reasoning.', conditions: ['Only for untrusted input'], evidence: status === 'unknown' ? [] : [evidence()],
});
function report(requestId) {
  return {
    requestId, summary: 'Synthetic fixture: sourced fix, not a vulnerability verdict.',
    findings: [{
      title: 'Synthetic advisory', severity: 'high', majorVersion: '3', affectedVersions: '<3.0.1',
      fixedVersion: '3.0.1', advisoryId: 'EXAMPLE-1',
      evidence: [{ title: 'Synthetic source', url: 'https://example.org/advisory/EXAMPLE-1' }],
      applicability: applicability(),
    }],
    limitations: ['Fixture report only; no real model or vulnerability investigation.'],
  };
}
function usage(requestId) {
  return {
    requestId, status: 'used', summary: 'Parses configuration at start-up.', purpose: 'Configuration parsing', projectRole: 'runtime',
    benefits: ['Reads user settings'], paths: [{ packageIds: ['npm:app-dep', 'npm:example'], explanation: 'app-dep re-exports the parser', evidence: [evidence()] }],
    sourceEvidence: [evidence()], limitations: [],
  };
}
const resultPath = (dir, id) => path.join(dir, 'security', id, 'result.json');
const savedReport = (dir, id) => JSON.parse(readFileSync(resultPath(dir, id), 'utf8'));
const requestDir = (dir, id) => path.dirname(resultPath(dir, id));

test('security review round-trips the admitted target and complete validated report', () => {
  const dir = fresh();
  try {
    const request = admitted(dir);
    const payload = report(request.requestId);
    payload.findings[0].title = '<script>untrusted source text</script>';
    const before = Date.now();
    assert.deepEqual(saveSecurityReview(dir, payload, actor), { requestId: request.requestId, saved: true });
    const saved = savedReport(dir, request.requestId);
    const { requestId, ...fields } = payload;
    assert.deepEqual(saved, {
      requestId, architectInstanceId: actor.instanceId, architectGeneration: actor.generation,
      architectLaunchId: actor.launchId, architectSessionId: actor.sessionId,
      ...fields, completedAt: saved.completedAt,
    });
    assert.ok(saved.completedAt >= before && saved.completedAt <= Date.now());
    assert.deepEqual(readdirSync(requestDir(dir, requestId)).sort(), ['request.json', 'result.json']);
    const original = readFileSync(resultPath(dir, requestId), 'utf8');
    assert.throws(() => saveSecurityReview(dir, { ...payload, summary: 'must not overwrite' }, actor), /already completed/);
    assert.equal(readFileSync(resultPath(dir, requestId), 'utf8'), original);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('positive, negative and unknown applicability all round-trip with their local evidence', () => {
  const dir = fresh();
  try {
    for (const status of ['relevant', 'notRelevant', 'unknown']) {
      const request = admitted(dir);
      const payload = report(request.requestId);
      payload.findings[0].applicability = applicability(status);
      saveSecurityReview(dir, payload, actor);
      assert.deepEqual(savedReport(dir, request.requestId).findings[0].applicability, applicability(status));
    }
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('security review requires an admitted UUID and the exact Architect instance, generation, launch and session', () => {
  const dir = fresh();
  try {
    const request = admitted(dir);
    const payload = report(request.requestId);
    for (const id of ['../escape', `${request.requestId}/../escape`, request.requestId.toUpperCase(), randomUUID(), '', null]) {
      assert.throws(() => saveSecurityReview(dir, { ...payload, requestId: id }, actor));
    }
    for (const wrong of [null, {}, { ...actor, instanceId: 'other' }, { ...actor, generation: 6 },
      { ...actor, generation: 8 }, { ...actor, generation: '7' }, { ...actor, generation: NaN },
      { ...actor, generation: Number.MAX_SAFE_INTEGER + 1 },
      // Equal numeric generation on a later app launch or another native session must not submit to an old request.
      { ...actor, launchId: 'later-launch' }, { ...actor, sessionId: 'other-session' },
      { instanceId: actor.instanceId, generation: actor.generation }, { ...actor, launchId: '' }]) {
      assert.throws(() => saveSecurityReview(dir, payload, wrong));
    }
    assert.equal(existsSync(resultPath(dir, request.requestId)), false);
    assert.deepEqual(readdirSync(requestDir(dir, request.requestId)), ['request.json']);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('legacy, wrong-kind, ended and completed requests never accept a new submission', () => {
  const dir = fresh();
  try {
    const legacy = admitted(dir, { schemaVersion: undefined, kind: undefined, architectLaunchId: undefined, architectSessionId: undefined, contextId: undefined });
    assert.throws(() => saveSecurityReview(dir, report(legacy.requestId), actor), /predates/);
    const usageRequest = admitted(dir, { kind: 'usage' });
    assert.throws(() => saveSecurityReview(dir, report(usageRequest.requestId), actor), /not a security request/);
    const securityRequest = admitted(dir);
    assert.throws(() => saveUsageExplanation(dir, usage(securityRequest.requestId), actor), /not a usage request/);
    for (const [marker, expected] of [[{ status: 'error', code: 'producerEnded', message: 'Architect session ended', at: 1 }, /already ended/], [{ status: 'complete', resultHash: 'x', at: 1 }, /already completed/]]) {
      const request = admitted(dir);
      writeFileSync(path.join(requestDir(dir, request.requestId), 'terminal.json'), JSON.stringify(marker));
      assert.throws(() => saveSecurityReview(dir, report(request.requestId), actor), expected);
      assert.equal(existsSync(resultPath(dir, request.requestId)), false);
    }
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('all report limits and HTTPS evidence validation reject before any partial result write', () => {
  const dir = fresh();
  try {
    const request = admitted(dir);
    const baseline = report(request.requestId);
    const mutations = [
      value => { value.summary = 'x'.repeat(20001); },
      value => { value.summary = '😀'.repeat(10001); },
      value => { value.findings = Array(101).fill(value.findings[0]); },
      value => { value.limitations = Array(51).fill('limitation'); },
      value => { value.limitations = ['x'.repeat(2001)]; },
      value => { value.findings[0].title = ''; },
      value => { value.findings[0].title = ' '.repeat(4); },
      value => { value.findings[0].title = 'x'.repeat(1001); },
      value => { value.findings[0].affectedVersions = 'x'.repeat(1001); },
      value => { value.findings[0].majorVersion = 'x'.repeat(1001); },
      value => { value.findings[0].fixedVersion = 'x'.repeat(1001); },
      value => { value.findings[0].advisoryId = 'x'.repeat(1001); },
      value => { value.findings[0].severity = 'safe'; },
      value => { delete value.findings[0].majorVersion; },
      value => { value.findings[0].evidence = []; },
      value => { value.findings[0].evidence = Array(21).fill(value.findings[0].evidence[0]); },
      value => { value.findings[0].evidence[0].title = 'x'.repeat(1001); },
      value => { value.findings[0].evidence[0].url = `https://example.org/${'x'.repeat(2048)}`; },
      ...['javascript:alert(1)', 'data:text/html,unsafe', 'file:///tmp/source', 'http://example.org/source',
        'https://user:secret@example.org/source', 'https://example.org\\evil', 'https://example.org/\nunsafe',
        'https://', 'https://example.org/ space'].map(url => value => { value.findings[0].evidence[0].url = url; }),
      // Applicability and its source evidence.
      value => { delete value.findings[0].applicability; },
      value => { value.findings[0].applicability.status = 'safe'; },
      value => { value.findings[0].applicability.reason = ' '; },
      value => { value.findings[0].applicability.reason = 'x'.repeat(4001); },
      value => { value.findings[0].applicability.conditions = Array(21).fill('c'); },
      value => { value.findings[0].applicability.conditions = ['x'.repeat(2001)]; },
      value => { value.findings[0].applicability.evidence = []; },
      value => { value.findings[0].applicability.status = 'notRelevant'; value.findings[0].applicability.evidence = []; },
      value => { value.findings[0].applicability.evidence = Array(21).fill(evidence()); },
      ...['', '/etc/passwd', '\\windows\\x', 'C:/Windows/win.ini', 'c:file', '../x', 'a/../b', 'a//b', './a', 'a/.', 'a\u0000b', 'a\nb', 'a/\u001b[31m', 'p'.repeat(1001)]
        .map(file => value => { value.findings[0].applicability.evidence[0].path = file; }),
      value => { value.findings[0].applicability.evidence[0].lineStart = 0; },
      value => { value.findings[0].applicability.evidence[0].lineStart = 1.5; },
      value => { value.findings[0].applicability.evidence[0].lineStart = 10; value.findings[0].applicability.evidence[0].lineEnd = 9; },
      value => { value.findings[0].applicability.evidence[0].lineEnd = Number.MAX_SAFE_INTEGER + 1; },
      value => { value.findings[0].applicability.evidence[0].explanation = ''; },
      value => { value.findings[0].applicability.evidence[0].explanation = 'x'.repeat(4001); },
    ];
    for (const mutate of mutations) {
      const payload = structuredClone(baseline);
      mutate(payload);
      assert.throws(() => saveSecurityReview(dir, payload, actor));
      assert.deepEqual(readdirSync(requestDir(dir, request.requestId)), ['request.json']);
    }
    // The complete payload is validated, not only its first finding.
    const lateInvalid = structuredClone(baseline);
    lateInvalid.findings.push({ ...lateInvalid.findings[0], evidence: [] });
    assert.throws(() => saveSecurityReview(dir, lateInvalid, actor));
    assert.equal(existsSync(resultPath(dir, request.requestId)), false);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('a citation through a link that leaves the project is rejected while a missing file is just a citation', () => {
  const dir = fresh();
  const outside = fresh();
  const link = path.join(dir, 'project', 'out');
  try {
    const request = admitted(dir);
    writeFileSync(path.join(outside, 'secret.txt'), 'x');
    symlinkSync(outside, link, process.platform === 'win32' ? 'junction' : 'dir');
    const escaping = report(request.requestId);
    escaping.findings[0].applicability.evidence[0].path = 'out/secret.txt';
    assert.throws(() => saveSecurityReview(dir, escaping, actor), /applicability/);
    assert.equal(existsSync(resultPath(dir, request.requestId)), false);
    const missing = report(request.requestId);
    missing.findings[0].applicability.evidence[0].path = 'deleted/file.js';
    assert.equal(saveSecurityReview(dir, missing, actor).saved, true);
  } finally {
    try { if (process.platform === 'win32') rmdirSync(link); else rmSync(link); } catch { /* not created */ }
    rmSync(dir, { recursive: true, force: true });
    rmSync(outside, { recursive: true, force: true });
  }
});

test('security report field limits are inclusive and empty findings remains an honest report', () => {
  const dir = fresh();
  try {
    const request = admitted(dir);
    const payload = report(request.requestId);
    payload.summary = '😀'.repeat(10000);
    payload.findings[0].title = 'x'.repeat(1000);
    payload.findings[0].affectedVersions = 'x'.repeat(1000);
    payload.findings[0].majorVersion = null;
    payload.findings[0].fixedVersion = null;
    payload.findings[0].advisoryId = null;
    payload.findings[0].evidence = Array.from({ length: 20 }, () => ({
      title: 'x'.repeat(1000), url: `https://example.org/${'x'.repeat(2048 - 'https://example.org/'.length)}`,
    }));
    payload.findings[0].applicability = {
      status: 'relevant', reason: '😀'.repeat(2000), conditions: Array(20).fill('x'.repeat(2000)),
      evidence: Array.from({ length: 20 }, () => evidence({ path: Array(10).fill('a'.repeat(99)).join('/'), explanation: 'x'.repeat(4000) })),
    };
    payload.limitations = Array(50).fill('x'.repeat(2000));
    assert.equal(saveSecurityReview(dir, payload, actor).saved, true);
    assert.deepEqual(savedReport(dir, request.requestId).findings, payload.findings);
    const maximum = admitted(dir);
    const many = report(maximum.requestId);
    many.findings = Array(100).fill(many.findings[0]);
    saveSecurityReview(dir, many, actor);
    assert.equal(savedReport(dir, maximum.requestId).findings.length, 100);
    const another = admitted(dir);
    const none = { requestId: another.requestId, summary: 'No verified findings is not a clean bill of health.', findings: [], limitations: ['Browsing unavailable.'] };
    saveSecurityReview(dir, none, actor);
    assert.deepEqual(savedReport(dir, another.requestId).findings, []);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('usage explanations round-trip concrete and honestly unknown results and enforce their contract', () => {
  const dir = fresh();
  try {
    const request = admitted(dir, { kind: 'usage' });
    const payload = usage(request.requestId);
    assert.deepEqual(saveUsageExplanation(dir, payload, actor), { requestId: request.requestId, saved: true });
    const { requestId, ...fields } = payload;
    const saved = savedReport(dir, requestId);
    assert.deepEqual(saved, { requestId, architectInstanceId: actor.instanceId, architectGeneration: actor.generation,
      architectLaunchId: actor.launchId, architectSessionId: actor.sessionId, ...fields, completedAt: saved.completedAt });
    assert.throws(() => saveUsageExplanation(dir, usage(requestId), actor), /already completed/);
    const unknown = admitted(dir, { kind: 'usage' });
    saveUsageExplanation(dir, { ...usage(unknown.requestId), status: 'unknown', paths: [], sourceEvidence: [], limitations: ['Searched src/ and config; nothing imports it.'] }, actor);
    assert.equal(savedReport(dir, unknown.requestId).status, 'unknown');
    const rejected = admitted(dir, { kind: 'usage' });
    const base = usage(rejected.requestId);
    const mutations = [
      value => { value.status = 'removable'; },
      value => { value.projectRole = 'ui'; },
      value => { value.summary = ' '; },
      value => { value.purpose = 'x'.repeat(4001); },
      // A dependency-chain label alone is not a source-based benefit.
      value => { value.sourceEvidence = []; value.paths[0].evidence = []; },
      value => { value.status = 'notObserved'; value.limitations = []; },
      value => { value.benefits = Array(21).fill('b'); },
      value => { value.benefits = ['x'.repeat(2001)]; },
      value => { value.paths = Array(21).fill(value.paths[0]); },
      value => { value.paths[0].packageIds = []; },
      value => { value.paths[0].packageIds = Array(101).fill('npm:a'); },
      value => { value.paths[0].packageIds = ['no-ecosystem']; },
      value => { value.paths[0].explanation = ''; },
      value => { value.paths[0].evidence = Array(21).fill(evidence()); },
      value => { value.sourceEvidence = Array(51).fill(evidence()); },
      value => { value.sourceEvidence[0].path = '../escape'; },
      value => { value.limitations = Array(51).fill('l'); },
    ];
    for (const mutate of mutations) {
      const payload = structuredClone(base);
      mutate(payload);
      assert.throws(() => saveUsageExplanation(dir, payload, actor));
      assert.deepEqual(readdirSync(requestDir(dir, rejected.requestId)), ['request.json']);
    }
    const limit = admitted(dir, { kind: 'usage' });
    saveUsageExplanation(dir, { ...usage(limit.requestId), benefits: Array(20).fill('b'.repeat(2000)), sourceEvidence: Array(50).fill(evidence()),
      paths: Array(20).fill({ packageIds: Array(100).fill('npm:a'), explanation: 'x'.repeat(4000), evidence: Array(20).fill(evidence()) }), limitations: Array(50).fill('l'.repeat(2000)) }, actor);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('malformed or oversized admitted requests and symlinked directories are rejected', () => {
  const dir = fresh();
  const outside = fresh();
  try {
    for (const patch of [{ packageId: 'project' }, { folder: '../project' }, { newestStatus: 'guessed' },
      { resolvedVersions: ['x'.repeat(1001)] }, { repositoryUrl: 'file:///project' }]) {
      const request = admitted(dir, patch);
      assert.throws(() => saveSecurityReview(dir, report(request.requestId), actor));
      assert.equal(existsSync(resultPath(dir, request.requestId)), false);
    }
    const malformed = admitted(dir);
    const requestFile = path.join(dir, 'security', malformed.requestId, 'request.json');
    writeFileSync(requestFile, '{broken json');
    assert.throws(() => saveSecurityReview(dir, report(malformed.requestId), actor));
    writeFileSync(requestFile, ' '.repeat(1024 * 1024 + 1));
    assert.throws(() => saveSecurityReview(dir, report(malformed.requestId), actor), /too large/);
    const target = admitted(outside);
    symlinkSync(path.join(outside, 'security', target.requestId), path.join(dir, 'security', target.requestId), process.platform === 'win32' ? 'junction' : 'dir');
    assert.throws(() => saveSecurityReview(dir, report(target.requestId), actor), /directory/);
    assert.equal(existsSync(resultPath(outside, target.requestId)), false);
  } finally {
    // Remove the junction itself before recursive fixture cleanup.
    for (const entry of readdirSync(path.join(dir, 'security'), { withFileTypes: true })) {
      if (entry.isSymbolicLink()) {
        const link = path.join(dir, 'security', entry.name);
        if (process.platform === 'win32') rmdirSync(link); else rmSync(link);
      }
    }
    rmSync(dir, { recursive: true, force: true });
    rmSync(outside, { recursive: true, force: true });
  }
});

test('aggregate byte limit is enforced even when individual report fields fit', () => {
  const dir = fresh();
  try {
    const request = admitted(dir);
    const payload = report(request.requestId);
    const finding = payload.findings[0];
    finding.evidence = Array.from({ length: 20 }, () => ({ title: '\u0001'.repeat(1000), url: 'https://example.org/source' }));
    payload.findings = Array(100).fill(finding);
    assert.throws(() => saveSecurityReview(dir, payload, actor), /too large/);
    assert.deepEqual(readdirSync(requestDir(dir, request.requestId)), ['request.json']);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('simultaneous report publishers admit exactly one complete result without overwrite or partial files', async () => {
  const dir = fresh();
  const workers = [];
  try {
    const request = admitted(dir);
    const barrier = new SharedArrayBuffer(4);
    const ready = [];
    const done = [];
    for (let index = 0; index < 4; index++) {
      const payload = { ...report(request.requestId), summary: `publisher ${index}` };
      const worker = new Worker(`
        const { parentPort, workerData } = require('node:worker_threads');
        import(workerData.module).then(({ saveSecurityReview }) => {
          parentPort.postMessage({ ready: true });
          Atomics.wait(new Int32Array(workerData.barrier), 0, 0);
          try { parentPort.postMessage({ saved: saveSecurityReview(workerData.dir, workerData.payload, workerData.actor), summary: workerData.payload.summary }); }
          catch (error) { parentPort.postMessage({ error: String(error.message) }); }
        }).catch(error => { throw error; });
      `, { eval: true, workerData: { dir, payload, actor, barrier, module: new URL('./omp-architect.mjs', import.meta.url).href } });
      workers.push(worker);
      ready.push(new Promise((resolve, reject) => { worker.on('message', message => { if (message.ready) resolve(); }); worker.on('error', reject); }));
      done.push(new Promise((resolve, reject) => { worker.on('message', message => { if (!message.ready) resolve(message); }); worker.on('error', reject); }));
    }
    await Promise.all(ready);
    Atomics.store(new Int32Array(barrier), 0, 1);
    Atomics.notify(new Int32Array(barrier), 0);
    const results = await Promise.all(done);
    const winners = results.filter(result => result.saved);
    assert.equal(winners.length, 1);
    assert.equal(results.filter(result => result.error).length, 3);
    assert.equal(savedReport(dir, request.requestId).summary, winners[0].summary);
    assert.deepEqual(readdirSync(requestDir(dir, request.requestId)).sort(), ['request.json', 'result.json']);
  } finally {
    await Promise.all(workers.map(worker => worker.terminate()));
    rmSync(dir, { recursive: true, force: true });
  }
});

test('model-supplied extra fields cannot choose a file, target actor, generation, launch, session or completion time', () => {
  const dir = fresh();
  try {
    const request = admitted(dir);
    const payload = Object.assign(report(request.requestId), {
      folder: '../outside', file: '../outside/result.json', architectInstanceId: 'intruder',
      architectGeneration: 999, architectLaunchId: 'intruder', architectSessionId: 'intruder', completedAt: 0,
    });
    payload.findings[0].unexpected = '<untrusted>';
    payload.findings[0].applicability.unexpected = '<untrusted>';
    payload.findings[0].applicability.evidence[0].unexpected = '<untrusted>';
    saveSecurityReview(dir, payload, actor);
    const saved = savedReport(dir, request.requestId);
    assert.equal(saved.architectInstanceId, actor.instanceId);
    assert.equal(saved.architectGeneration, actor.generation);
    assert.equal(saved.architectLaunchId, actor.launchId);
    assert.equal(saved.architectSessionId, actor.sessionId);
    assert.ok(saved.completedAt > 0);
    assert.equal('folder' in saved, false);
    assert.equal('file' in saved, false);
    assert.equal('unexpected' in saved.findings[0], false);
    assert.equal('unexpected' in saved.findings[0].applicability, false);
    assert.equal('unexpected' in saved.findings[0].applicability.evidence[0], false);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('the Architect identity hashes the launch token and takes the session from the tool context, never from parameters', () => {
  const env = { OMP_MANAGER_BRIDGE_TOKEN: 'secret-token', OMP_MANAGER_BRIDGE_INSTANCE_ID: 'architect-1', OMP_MANAGER_BRIDGE_GENERATION: '3' };
  const ctx = { agent: { kind: 'main' }, sessionManager: { getSessionId: () => 'native-session' } };
  const identity = architectIdentity(ctx, env);
  assert.deepEqual(identity, { instanceId: 'architect-1', generation: 3, launchId: createHash('sha256').update('secret-token').digest('hex'), sessionId: 'native-session' });
  assert.equal(JSON.stringify(identity).includes('secret-token'), false);
  assert.throws(() => architectIdentity({ ...ctx, agent: { kind: 'sub' } }, env), /main session/);
  assert.throws(() => architectIdentity({ agent: { kind: 'main' }, sessionManager: { getSessionId: () => undefined } }, env), /unavailable/);
  assert.throws(() => architectIdentity(ctx, { ...env, OMP_MANAGER_BRIDGE_TOKEN: '' }), /unavailable/);
  assert.throws(() => architectIdentity(ctx, { ...env, OMP_MANAGER_BRIDGE_GENERATION: 'x' }), /unavailable/);
});

test('saved results replay from the app-owned directory, with legacy reports shown as unknown applicability', () => {
  const dir = fresh();
  try {
    const request = admitted(dir);
    saveSecurityReview(dir, report(request.requestId), actor);
    const text = loadSavedResultText(dir, 'security', request.requestId);
    for (const expected of ['Synthetic advisory', 'relevant — Synthetic applicability reasoning.', 'src/index.js:3-9', 'Only for untrusted input', 'Reviewed versions: resolved 1.2.3, 2.0.1', 'Saved analysis — source code and dependencies may have changed', CONTEXT]) {
      assert.ok(text.includes(expected), expected);
    }
    const used = admitted(dir, { kind: 'usage' });
    saveUsageExplanation(dir, usage(used.requestId), actor);
    assert.match(loadSavedResultText(dir, 'usage', used.requestId), /Configuration parsing[\s\S]*npm:app-dep -> npm:example/);
    assert.throws(() => loadSavedResultText(dir, 'usage', request.requestId), /not a usage request/);
    assert.throws(() => loadSavedResultText(dir, 'security', randomUUID()));
    assert.throws(() => loadSavedResultText(dir, 'security', '../escape'), /invalid request id/);
    assert.throws(() => loadSavedResultText(dir, 'project', request.requestId), /invalid result kind/);
    const pending = admitted(dir);
    assert.throws(() => loadSavedResultText(dir, 'security', pending.requestId), /no saved result/);
    // A legacy report from before applicability and producer binding is still readable.
    const legacy = admitted(dir, { schemaVersion: undefined, kind: undefined, architectLaunchId: undefined, architectSessionId: undefined, contextId: undefined });
    const old = report(legacy.requestId);
    delete old.findings[0].applicability;
    writeFileSync(resultPath(dir, legacy.requestId), JSON.stringify({ ...old, architectInstanceId: actor.instanceId, architectGeneration: actor.generation, completedAt: 1_760_000_000_000 }));
    assert.match(loadSavedResultText(dir, 'security', legacy.requestId), /unknown — Saved before project-specific applicability/);
    // Tampered results are not displayed.
    const tampered = admitted(dir);
    saveSecurityReview(dir, report(tampered.requestId), actor);
    const saved = savedReport(dir, tampered.requestId);
    saved.findings[0].applicability.evidence[0].path = '../escape';
    writeFileSync(resultPath(dir, tampered.requestId), JSON.stringify(saved));
    assert.throws(() => loadSavedResultText(dir, 'security', tampered.requestId));
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
