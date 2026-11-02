import test from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { cp, mkdir, mkdtemp, readFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { MAX_MONITOR_FRAME, MONITOR_TOOL, monitorParameters } from './omp-host-monitor.mjs';
import { installSrsBridge } from './omp-srs.mjs';
import { MONITOR_BLOCK, MONITOR_GUARD_ERROR } from './omp-visualization-guard.mjs';
import { controlAllowed } from './omp-manager.mjs';

const REQUEST = '11111111-1111-4111-8111-111111111111';
const OTHER_REQUEST = '22222222-2222-4222-8222-222222222222';
const SNAPSHOT = 'snap-A';
const HASH = 'ab'.repeat(32);
const STAGE_IDS = { jobId: '33333333-3333-4333-8333-333333333333', attemptId: '44444444-4444-4444-8444-444444444444', stageId: '55555555-5555-4555-8555-555555555555' };
const tick = () => new Promise(resolve => setImmediate(resolve));

// A small structural zod stand-in: strict objects, bounded strings/numbers. The opt-in real-SDK test below runs the very same
// schemas through the installed validator.
function miniZod() {
  const make = (parse, flags = {}) => {
    const schema = { ...flags, parse, safeParse(value) { try { return { success: true, data: parse(value) }; } catch (error) { return { success: false, error }; } },
      optional: () => make(value => value === undefined ? undefined : parse(value), { optional: true }), nullable: () => make(value => value === null ? null : parse(value)), strict: () => schema };
    return schema;
  };
  const fail = message => { throw new Error(message); };
  const string = (min = 0, max = Infinity, pattern) => {
    const schema = make(value => { if (typeof value !== 'string' || value.length < min || value.length > max || (pattern && !pattern.test(value))) fail('string'); return value; });
    schema.min = bound => string(bound, max, pattern); schema.max = bound => string(min, bound, pattern); schema.regex = expression => string(min, max, expression);
    return schema;
  };
  const number = (min = -Infinity, max = Infinity, integer = false) => {
    const schema = make(value => { if (typeof value !== 'number' || !Number.isFinite(value) || value < min || value > max || (integer && !Number.isInteger(value))) fail('number'); return value; });
    schema.int = () => number(min, max, true); schema.min = bound => number(bound, max, integer); schema.max = bound => number(min, bound, integer);
    return schema;
  };
  const array = item => {
    const schema = make(value => { if (!Array.isArray(value)) fail('expected array'); return value.map(entry => item.parse(entry)); });
    schema.max = bound => make(value => { if (!Array.isArray(value) || value.length > bound) fail('array length'); return value.map(entry => item.parse(entry)); });
    return schema;
  };
  return {
    string: () => string(), number: () => number(), boolean: () => make(value => typeof value === 'boolean' ? value : fail('boolean')),
    enum: values => make(value => values.includes(value) ? value : fail('enum')), array,
    object: shape => make(value => {
      if (!value || typeof value !== 'object' || Array.isArray(value)) fail('object');
      for (const key of Object.keys(value)) if (!Object.hasOwn(shape, key)) fail(`unknown field ${key}`);
      const out = {};
      for (const [key, field] of Object.entries(shape)) {
        if (value[key] === undefined) { if (!field.optional) fail(`missing ${key}`); continue; }
        out[key] = field.parse(value[key]);
      }
      return out;
    }),
  };
}

function harness({ quiescent = true, guardAvailable = true, handlers = {}, submit } = {}) {
  const state = { quiescent, guardAvailable, sessionId: 'native-session', aborted: 0, prompts: [], requests: [], order: [], idle: true };
  const gates = new Set(), tools = new Map(), events = new Map(), timers = [];
  const ctx = { agent: { kind: 'main' }, sessionManager: { getSessionId: () => state.sessionId }, isIdle: () => state.idle, hasPendingMessages: () => false,
    setTimeout: run => timers.push(run), abort: async () => { state.aborted++; } };
  const answers = {
    monitor_explanation_validate: () => ({ authorized: true }), monitor_explanation_retired: () => ({ retired: true }),
    monitor_snapshot_read: payload => ({ snapshotId: payload.snapshotId, section: payload.section, total: 1, offset: payload.offset, nextOffset: null, items: [{ name: 'node.exe' }] }),
    ...handlers,
  };
  const request = async (action, payload) => {
    state.requests.push({ action, payload }); state.order.push(action);
    if (!answers[action]) throw new Error(`unexpected ${action}`);
    return answers[action](payload);
  };
  const workspace = {
    nativeGuardAvailable: () => state.guardAvailable,
    nativeUiCapabilities: () => ({ nativeDecisions: true }),
    quiescence: () => ({ quiescent: state.quiescent, reasons: state.quiescent ? [] : ['streaming'] }),
    identity: () => ({ sessionId: state.sessionId }),
    addBeforeModelCallGate(gate) { gates.add(gate); return () => gates.delete(gate); },
    nativeSessionEntries: () => [],
    appendNativeEntry() {},
    submitPrompt: submit ?? ((message, { onPromptAdmitted }) => { state.prompts.push(message); onPromptAdmitted(); return Promise.resolve(); }),
  };
  const pi = { zod: miniZod(), registerTool: tool => tools.set(tool.name, tool), on: (name, handler) => events.set(name, handler) };
  const srs = installSrsBridge(pi, { request, workspace, currentContext: () => ctx });
  const payload = (overrides = {}) => ({ source: 'human', generation: 3, sessionId: 'native-session', requestId: REQUEST, snapshotId: SNAPSHOT, snapshotHash: HASH, message: 'Explain the frozen snapshot.', ...overrides });
  const callModelGates = async nativeContext => { for (const gate of gates) await gate(undefined, nativeContext); };
  const flush = () => { const batch = timers.splice(0); for (const run of batch) run(); return batch.length; };
  const read = (overrides = {}, context = ctx) => tools.get(MONITOR_TOOL).execute('1', { requestId: REQUEST, snapshotId: SNAPSHOT, section: 'processes', offset: 0, limit: 50, ...overrides }, undefined, undefined, context);
  return { state, gates, tools, events, timers, ctx, srs, pi, workspace, payload, callModelGates, flush, read,
    async admitActive(overrides) {
      const receipt = await srs.monitor.control(ctx, payload(overrides));
      await events.get('before_agent_start')({ prompt: state.prompts.at(-1) }, ctx);
      return receipt;
    } };
}

test('the explanation control is human-only and a Manager, owner or model source is refused', () => {
  assert.equal(controlAllowed({ role: 'instance', owner: null, action: 'monitor_explanation_prompt', source: 'human' }), true);
  for (const source of ['manager', 'model', undefined]) for (const [role, owner] of [['instance', 'M'], ['manager', 'M'], ['instance', null]]) {
    assert.equal(controlAllowed({ role, owner, action: 'monitor_explanation_prompt', source }), false, `${role}/${owner}/${source}`);
  }
});

test('one shared guard, one native before-model gate and the strict reader are installed exactly once', () => {
  const h = harness();
  assert.equal(h.gates.size, 1, 'the monitor never installs a second native gate');
  assert.deepEqual([...h.tools.keys()].filter(name => name === MONITOR_TOOL), [MONITOR_TOOL]);
  assert.equal(h.tools.get(MONITOR_TOOL).loadMode, 'essential');
  assert.equal(typeof h.srs.guard.admitMonitor, 'function');
});

test('admission validates with the app first, then submits one marked prompt and returns the native receipt', async () => {
  const h = harness();
  const receipt = await h.srs.monitor.control(h.ctx, h.payload());
  assert.deepEqual(receipt, { admitted: true, requestId: REQUEST, snapshotId: SNAPSHOT });
  assert.deepEqual(h.state.order, ['monitor_explanation_validate']);
  assert.equal(h.state.requests[0].payload.preAdmission, true);
  assert.equal(h.state.prompts.length, 1);
  assert.match(h.state.prompts[0], new RegExp(`^Explain the frozen snapshot\\.\\n\\n\\[omp-host-monitor request=${REQUEST} snapshot=${SNAPSHOT} nonce=[0-9a-f]{32}\\]$`));
  assert.equal(h.srs.guard.monitor(h.ctx).state, 'pending');
  assert.equal(h.srs.guard.toolCall({ toolName: 'bash', input: {} }, h.ctx), undefined, 'nothing is restricted until the carrying turn starts');
  // A replayed control for the same request is a duplicate: never sent twice.
  assert.deepEqual(await h.srs.monitor.control(h.ctx, h.payload()), { admitted: true, requestId: REQUEST, snapshotId: SNAPSHOT });
  assert.equal(h.state.prompts.length, 1);
});

test('every refusal before the native session admits the prompt is a structured known-unsent receipt and leaves no guard', async () => {
  const cases = [
    ['native_monitor_guard_unavailable', harness({ guardAvailable: false }), {}],
    ['native_monitor_not_quiescent', harness({ quiescent: false }), {}],
    ['monitor_not_authorized', harness({ handlers: { monitor_explanation_validate: () => ({ authorized: false }) } }), {}],
    ['monitor_validation_failed', harness({ handlers: { monitor_explanation_validate: () => { throw new Error('controller timeout'); } } }), {}],
    ['invalid_request', harness(), { command: 'rm -rf /' }],
    ['invalid_request', harness(), { snapshotHash: 'short' }],
    ['invalid_request', harness(), { snapshotId: '../escape' }],
    ['native_prompt_unavailable', harness({ submit: () => { throw new Error('native_prompt_unavailable: Reopen this session'); } }), {}],
    ['native_prompt_not_admitted', harness({ submit: () => Promise.resolve() }), {}],
  ];
  for (const [code, h, overrides] of cases) {
    const receipt = await h.srs.monitor.control(h.ctx, h.payload(overrides));
    assert.equal(receipt.admitted, false, code);
    assert.equal(receipt.error.code, code);
    assert.equal(h.state.prompts.length, 0);
    assert.equal(h.srs.guard.monitor(h.ctx), null, `${code}: a refused prompt leaves no guard, so a later click is a fresh admission`);
  }
  const busy = harness({ quiescent: false });
  assert.deepEqual(busy.state.requests, []);
  await busy.srs.monitor.control(busy.ctx, busy.payload());
  assert.deepEqual(busy.state.requests, [], 'a busy session never reaches the app validation either');
  await assert.rejects(busy.srs.monitor.control({ ...busy.ctx, agent: { kind: 'sub' } }, busy.payload()), /not_controlled/);
});

test('an admission that loses quiescence or the session during the validation await sends nothing', async () => {
  for (const mutate of [state => { state.quiescent = false; }, state => { state.sessionId = 'replaced-session'; }]) {
    const h = harness({ handlers: { monitor_explanation_validate: () => { mutate(h.state); return { authorized: true }; } } });
    const receipt = await h.srs.monitor.control(h.ctx, h.payload());
    assert.equal(receipt.admitted, false);
    assert.equal(h.state.prompts.length, 0);
    assert.equal(h.srs.guard.monitor(h.ctx), null);
  }
});

test('mutual exclusion: SRS or another analysis refuses the monitor, and a monitor refuses visualization while preserving duplicates', async () => {
  const srsOwned = harness();
  srsOwned.srs.guard.admitSrs(srsOwned.ctx, { ...STAGE_IDS, nonce: 'b'.repeat(32), phase: 'survey', stageNumber: 1, message: 'Survey.' });
  const refused = await srsOwned.srs.monitor.control(srsOwned.ctx, srsOwned.payload());
  assert.equal(refused.admitted, false);
  assert.equal(refused.error.code, 'srs_owns_architect');
  assert.equal(srsOwned.state.prompts.length, 0);

  const visual = harness();
  visual.srs.guard.admit(visual.ctx, { requestId: OTHER_REQUEST, kind: 'systemOverview', message: 'Analyze.' });
  const blocked = await visual.srs.monitor.control(visual.ctx, visual.payload());
  assert.equal(blocked.error.code, 'analysis_in_progress');
  assert.deepEqual(visual.srs.guard.admit(visual.ctx, { requestId: OTHER_REQUEST, kind: 'systemOverview', message: 'again' }), { duplicate: true, text: null }, 'a visualization duplicate is still a duplicate');

  const monitor = harness();
  await monitor.srs.monitor.control(monitor.ctx, monitor.payload());
  assert.throws(() => monitor.srs.guard.admit(monitor.ctx, { requestId: OTHER_REQUEST, kind: 'systemOverview', message: 'Analyze.' }), /analysis_in_progress/);
  assert.throws(() => monitor.srs.guard.admitSrs(monitor.ctx, { ...STAGE_IDS, nonce: 'c'.repeat(32), phase: 'survey', stageNumber: 1, message: 'Survey.' }), /analysis_in_progress/);
  // A second explanation request while one is outstanding is refused, not queued.
  const second = await monitor.srs.monitor.control(monitor.ctx, monitor.payload({ requestId: OTHER_REQUEST }));
  assert.equal(second.error.code, 'analysis_in_progress');
});

test('the active turn runs only the exact reader and native Ask; the reader forwards exactly the validated arguments', async () => {
  const h = harness();
  assert.equal((await h.read()).isError, true, 'no active turn yet');
  await h.admitActive();
  assert.equal(h.srs.guard.activeMonitor(h.ctx).requestId, REQUEST);
  const ok = await h.read({ offset: 40, limit: 200 });
  assert.notEqual(ok.isError, true);
  assert.deepEqual(h.state.requests.at(-1), { action: 'monitor_snapshot_read', payload: { requestId: REQUEST, snapshotId: SNAPSHOT, section: 'processes', offset: 40, limit: 200 } });
  assert.equal(JSON.parse(ok.content[0].text).items[0].name, 'node.exe');
  assert.equal(h.srs.guard.toolCall({ toolName: 'ask', input: {} }, h.ctx), undefined);
  assert.equal(h.srs.guard.toolCall({ toolName: MONITOR_TOOL, input: { requestId: REQUEST, snapshotId: SNAPSHOT } }, h.ctx), undefined);
  for (const [name, input] of [[MONITOR_TOOL, { requestId: OTHER_REQUEST, snapshotId: SNAPSHOT }], [MONITOR_TOOL, { requestId: REQUEST, snapshotId: 'snap-B' }], [MONITOR_TOOL, {}],
    ['bash', { command: 'netstat -ano' }], ['read', { path: 'C:/Windows/System32' }], ['write', { path: 'x' }], ['edit', {}], ['task', {}], ['eval', {}], ['submit_system_overview', { requestId: REQUEST }],
    ['srs_inspect', STAGE_IDS], ['repo_evidence', { requestId: REQUEST }]]) {
    const result = h.srs.guard.toolCall({ toolName: name, input }, h.ctx);
    assert.equal(result?.block, true, `${name} ${JSON.stringify(input)}`);
    assert.equal(result.reason, MONITOR_BLOCK);
  }
  for (const bad of [{ requestId: OTHER_REQUEST }, { snapshotId: 'snap-B' }, { limit: 0 }, { limit: 201 }, { offset: -1 }, { offset: 1.5 }, { section: 'all' }, { path: 'C:/x' }, { pid: 4 }, { refresh: true }]) {
    const before = h.state.requests.length;
    assert.equal((await h.read(bad)).isError, true, JSON.stringify(bad));
    assert.equal(h.state.requests.length, before, 'a refused read never reaches the app');
  }
  assert.equal((await h.read({}, { ...h.ctx, agent: { kind: 'sub' } })).isError, true);
  const big = harness({ handlers: { monitor_snapshot_read: () => ({ items: ['x'.repeat(MAX_MONITOR_FRAME)] }) } });
  await big.admitActive();
  const oversized = await big.read();
  assert.equal(oversized.isError, true);
  assert.match(oversized.content[0].text, /monitor_transport_limit/);
});

test('the provider barrier asks the app to validate the exact binding before every call and fails closed otherwise', async () => {
  const h = harness();
  await h.admitActive();
  await h.callModelGates({ messages: [{ role: 'user', content: h.state.prompts[0], timestamp: 1 }] });
  assert.deepEqual(h.state.requests.at(-1), { action: 'monitor_explanation_validate', payload: { requestId: REQUEST, snapshotId: SNAPSHOT } });
  const denied = harness({ handlers: { monitor_explanation_validate: payload => ({ authorized: payload.preAdmission === true }) } });
  await denied.admitActive();
  await assert.rejects(denied.callModelGates({ messages: [{ role: 'user', content: denied.state.prompts[0], timestamp: 1 }] }), /monitor_not_authorized/);
  const gone = harness();
  await gone.admitActive();
  gone.state.guardAvailable = false;
  await assert.rejects(gone.callModelGates({ messages: [{ role: 'user', content: gone.state.prompts[0], timestamp: 1 }] }), /native_monitor_guard_unavailable/);
  const offline = harness();
  await offline.admitActive();
  offline.srs.disconnected();
  await assert.rejects(offline.callModelGates({ messages: [{ role: 'user', content: offline.state.prompts[0], timestamp: 1 }] }), new RegExp(MONITOR_GUARD_ERROR.slice(0, 30)));
});

test('native quiescence, an unrelated prompt, a session replacement and an uncertain continuation each retire the exact guard once', async () => {
  const retired = async h => { await tick(); return h.state.requests.filter(item => item.action === 'monitor_explanation_retired').map(item => item.payload); };
  // Genuine quiescence, not agent_end alone.
  const settled = harness();
  await settled.admitActive();
  settled.srs.guard.agentEnd({}, settled.ctx);
  settled.state.idle = false;
  settled.flush();
  assert.deepEqual(await retired(settled), [], 'still running: nothing is retired');
  settled.state.idle = true; settled.state.quiescent = true;
  settled.flush();
  assert.deepEqual(await retired(settled), [{ requestId: REQUEST, snapshotId: SNAPSHOT, reason: 'settled' }]);
  assert.equal(settled.srs.guard.monitor(settled.ctx), null);
  assert.equal((await settled.read()).isError, true, 'later reads are refused');
  settled.srs.guard.agentEnd({}, settled.ctx);
  settled.flush();
  assert.equal((await retired(settled)).length, 1, 'retirement is reported once');

  // An unrelated human turn at the provider barrier.
  const unrelated = harness();
  await unrelated.admitActive();
  unrelated.events.get('before_agent_start')({ prompt: 'the user asks something else' }, unrelated.ctx);
  await unrelated.callModelGates({ messages: [{ role: 'user', content: 'the user asks something else', timestamp: 10 }] });
  assert.deepEqual(await retired(unrelated), [{ requestId: REQUEST, snapshotId: SNAPSHOT, reason: 'unrelatedPrompt' }]);
  assert.equal(unrelated.srs.guard.toolCall({ toolName: 'bash', input: {} }, unrelated.ctx), undefined, 'the unrelated turn is unrestricted');

  // A replaced native session.
  const replaced = harness();
  await replaced.admitActive();
  await replaced.events.get('session_switch')({}, replaced.ctx);
  assert.deepEqual(await retired(replaced), [{ requestId: REQUEST, snapshotId: SNAPSHOT, reason: 'sessionChanged' }]);
  const stale = harness();
  await stale.admitActive();
  stale.state.sessionId = 'next-session';
  assert.equal(stale.srs.guard.monitor(stale.ctx), null);
  assert.deepEqual(await retired(stale), [{ requestId: REQUEST, snapshotId: SNAPSHOT, reason: 'sessionChanged' }]);

  // An unlabelled continuation never becomes unrestricted: it retires as uncertain and every provider call is refused.
  const uncertain = harness();
  await uncertain.admitActive();
  uncertain.events.get('before_agent_start')({ prompt: 'something unattributed' }, uncertain.ctx);
  await assert.rejects(uncertain.callModelGates({ messages: [{ role: 'assistant', content: 'x' }] }), /analysis_continuation_uncertain/);
  assert.deepEqual(await retired(uncertain), [{ requestId: REQUEST, snapshotId: SNAPSHOT, reason: 'uncertainContinuation' }]);
  assert.equal(uncertain.srs.guard.toolCall({ toolName: MONITOR_TOOL, input: { requestId: REQUEST, snapshotId: SNAPSHOT } }, uncertain.ctx).block, true);
  assert.equal((await uncertain.read()).isError, true);
  await assert.rejects(uncertain.callModelGates({ messages: [{ role: 'assistant', content: 'x' }] }), /analysis_continuation_uncertain|paused/);
  assert.equal((await retired(uncertain)).length, 1);
});

test('a native automatic compaction continuation keeps the reader+Ask restriction and its summary context names only that request', async () => {
  const h = harness();
  await h.admitActive();
  const summary = h.events.get('session.compacting')({}, h.ctx);
  assert.match(summary.context[0], new RegExp(`frozen host-monitor snapshot ${SNAPSHOT}, request ${REQUEST}`));
  assert.deepEqual(summary.preserveData.ompAnalysis, [{ mode: 'monitor', requestId: REQUEST, snapshotId: SNAPSHOT, nonce: summary.preserveData.ompAnalysis[0].nonce, state: 'active' }]);
  h.events.get('agent_end')({ willContinue: true }, h.ctx);
  assert.equal(h.flush(), 0, 'a continuing turn schedules no settlement');
  h.events.get('before_agent_start')({ prompt: 'Resume the user\'s latest intent.' }, h.ctx);
  assert.equal(h.srs.guard.toolCall({ toolName: 'bash', input: {} }, h.ctx).block, true, 'the continuation is closed until the provider barrier decides');
  await h.callModelGates({ messages: [{ role: 'user', content: 'summary', timestamp: 1 }, { role: 'developer', synthetic: true, attribution: 'agent', content: 'Resume' }] });
  assert.equal(h.srs.guard.activeMonitor(h.ctx).requestId, REQUEST, 'compaction did not release the guard');
  assert.equal(h.srs.guard.toolCall({ toolName: 'bash', input: {} }, h.ctx).block, true);
  assert.equal(h.srs.guard.toolCall({ toolName: MONITOR_TOOL, input: { requestId: REQUEST, snapshotId: SNAPSHOT } }, h.ctx), undefined);
  assert.equal(h.state.requests.filter(item => item.action === 'monitor_explanation_retired').length, 0);
});

test('a pending carrier prompt that reaches the model without activation fails closed instead of running unrestricted', async () => {
  const h = harness();
  await h.srs.monitor.control(h.ctx, h.payload());
  const messages = [{ role: 'user', content: 'earlier question' }, { role: 'user', content: [{ type: 'text', text: h.state.prompts[0] }] }];
  const result = h.events.get('context')({ messages }, h.ctx);
  assert.deepEqual(result.messages, [messages[0]], 'the unguarded prompt never reaches the model');
  assert.equal(h.state.aborted, 1);
  await assert.rejects(h.callModelGates({ messages }), /analysis_authorization_missing/);
  await tick();
  assert.deepEqual(h.state.requests.filter(item => item.action === 'monitor_explanation_retired').map(item => item.payload.reason), ['uncertainContinuation']);
});

test('the strict schemas accept exact requests and reject model-supplied authority', () => {
  const schemas = monitorParameters(miniZod());
  const read = { requestId: REQUEST, snapshotId: SNAPSHOT, section: 'tcp', offset: 0, limit: 200 };
  assert.deepEqual(schemas.read.parse(read), read);
  for (const bad of [{ ...read, path: 'C:/x' }, { ...read, limit: 201 }, { ...read, limit: 0 }, { ...read, offset: -1 }, { ...read, section: 'everything' }, { ...read, requestId: 'x' }, { ...read, snapshotId: 'a b' }]) {
    assert.equal(schemas.read.safeParse(bad).success, false, JSON.stringify(bad));
  }
  assert.equal(schemas.prompt.safeParse({ requestId: REQUEST, snapshotId: SNAPSHOT, snapshotHash: HASH, message: 'x' }).success, true);
  assert.equal(schemas.prompt.safeParse({ requestId: REQUEST, snapshotId: SNAPSHOT, snapshotHash: HASH, message: '' }).success, false);
  assert.equal(schemas.prompt.safeParse({ requestId: REQUEST, snapshotId: SNAPSHOT, snapshotHash: HASH, message: 'x', command: 'rm' }).success, false);
});

const sdk = process.env.OMP_SDK_DIR;
test('installed SDK validator accepts the strict monitor schemas and rejects model-supplied authority', { skip: sdk ? false : 'Set OMP_SDK_DIR to exercise installed pi.zod' }, async () => {
  const zod = await import(pathToFileURL(path.join(sdk, '..', 'omptype', 'dist', 'js', 'zod.js')).href);
  const schemas = monitorParameters(zod);
  const read = { requestId: REQUEST, snapshotId: SNAPSHOT, section: 'udp', offset: 0, limit: 1 };
  assert.equal(schemas.read.safeParse(read).success, true);
  for (const bad of [{ ...read, path: 'C:/x' }, { ...read, limit: 201 }, { ...read, offset: -1 }, { ...read, section: 'all' }, { ...read, requestId: 'x' }]) assert.equal(schemas.read.safeParse(bad).success, false, JSON.stringify(bad));
  assert.equal(schemas.validate.safeParse({ requestId: REQUEST, snapshotId: SNAPSHOT }).success, true);
  assert.equal(schemas.validate.safeParse({ requestId: REQUEST, snapshotId: SNAPSHOT, preAdmission: true }).success, true);
});

test('installed SDK compaction, willContinue and automatic continuation keep monitor authorization (real AgentSession, offline mock provider)', {
  skip: sdk ? false : 'Set OMP_SDK_DIR for the isolated installed-SDK compaction proof', timeout: 150000,
}, async () => {
  const root = await mkdtemp(path.join(os.homedir(), '.omp-monitor-compaction-'));
  try {
    await mkdir(path.join(root, 'agent'), { recursive: true });
    const { version } = JSON.parse(await readFile(path.join(sdk, 'package.json'), 'utf8'));
    const nativeRoot = path.join(root, 'natives');
    await cp(path.join(process.env.PI_NATIVES_DIR || path.join(os.homedir(), '.omp', 'natives'), version), path.join(nativeRoot, version), { recursive: true });
    // PI_CONFIG_DIR is relative to the real home; never override USERPROFILE.
    const config = path.relative(os.homedir(), path.join(root, 'config'));
    assert.equal(config.split(path.sep).includes('..'), false);
    const { stdout } = await promisify(execFile)(process.env.BUN_BIN || 'bun', [fileURLToPath(new URL('./fixtures/monitor-compaction-sdk.mjs', import.meta.url))], {
      env: { ...process.env, MONITOR_COMPACTION_TEST_ROOT: root, PI_CODING_AGENT_DIR: path.join(root, 'agent'), PI_CONFIG_DIR: config, PI_NATIVES_DIR: nativeRoot, OMP_PROFILE: 'default', PI_COMPILED: '' },
      timeout: 140000, maxBuffer: 1024 * 1024,
    });
    assert.match(stdout, /Installed SDK compaction\/willContinue\/automatic-continuation host-monitor guard verified/);
  } finally { await rm(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 }); }
});
