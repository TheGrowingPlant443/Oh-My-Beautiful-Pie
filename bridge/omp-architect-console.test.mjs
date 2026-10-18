import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { registerClassifyTool } from './omp-architect.mjs';
import { createResultConsole, escapeTerminal, REPLAY_HEADER, RESULT_TYPE } from './omp-architect-console.mjs';

// A fake native session: explicit idle/pending state, a branch transcript and a fake clock. Nothing here can call a model.
function harness(sessionId = 'session-fixture') {
  const state = { idle: true, pending: false, session: sessionId, now: 0, timers: [], branch: [], sent: [], forbidden: [], handlers: {} };
  state.pi = {
    // The native runner calls every registered handler for an event, in registration order.
    on: (name, handler) => { const previous = state.handlers[name]; state.handlers[name] = previous ? (...args) => { previous(...args); return handler(...args); } : handler; },
    sendMessage: (message, options) => {
      state.sent.push({ message, options });
      state.branch.push({ type: 'custom_message', customType: message.customType, details: message.details });
    },
    sendUserMessage: (...args) => state.forbidden.push(['sendUserMessage', args]),
    steer: (...args) => state.forbidden.push(['steer', args]),
    followUp: (...args) => state.forbidden.push(['followUp', args]),
  };
  state.ctx = {
    agent: { kind: 'main' },
    sessionManager: { getBranch: () => state.branch, getSessionId: () => state.session },
    isIdle: () => state.idle,
    hasPendingMessages: () => state.pending,
    setTimeout: (fn, ms) => { state.timers.push({ fn, at: state.now + ms }); return state.timers.length; },
    clearTimer: () => {},
  };
  state.advance = ms => {
    state.now += ms;
    for (;;) {
      const due = state.timers.filter(timer => timer.at <= state.now).sort((a, b) => a.at - b.at)[0];
      if (!due) return;
      state.timers.splice(state.timers.indexOf(due), 1);
      due.fn();
    }
  };
  return state;
}
const identity = ctx => ({ instanceId: 'architect-fixture', generation: 7, launchId: 'launch', sessionId: ctx.sessionManager.getSessionId() });

test('terminal control sequences are shown as visible text; only newline and tab pass through', () => {
  assert.equal(escapeTerminal('a\u001b[31mred\u001b[0m\u0007\r\nb\tc\u009b'), 'a\\u001b[31mred\\u001b[0m\\u0007\\u000d\nb\tc\\u009b');
});

test('a result queued while streaming prints once after agent_end quiescence, without any model-triggering call', () => {
  const h = harness();
  const out = createResultConsole(h.pi, { identity });
  h.idle = false;
  out.enqueue(h.ctx, { kind: 'security', requestId: 'r1', text: 'full text \u001b[31m' });
  assert.equal(h.sent.length, 0);
  h.handlers.agent_end({ willContinue: true }, h.ctx);
  h.advance(500);
  assert.equal(h.sent.length, 0);
  h.handlers.agent_end({}, h.ctx);
  h.advance(300);
  assert.equal(h.sent.length, 0); // still streaming
  h.idle = true; h.pending = true;
  h.advance(300);
  assert.equal(h.sent.length, 0); // a pending message could still steer: wait
  h.pending = false;
  h.advance(100);
  assert.equal(h.sent.length, 1);
  const { message, options } = h.sent[0];
  assert.deepEqual(options, { triggerTurn: false });
  assert.equal(message.customType, RESULT_TYPE);
  assert.equal(message.display, true);
  assert.equal(message.attribution, 'agent');
  assert.deepEqual(message.details, { requestId: 'r1', kind: 'security' });
  assert.equal(message.content, 'full text \\u001b[31m');
  h.advance(2000);
  assert.equal(h.sent.length, 1);
  assert.deepEqual(h.forbidden, []);
});

test('saved replay while already idle prints at once with no future agent_end needed', () => {
  const h = harness();
  const out = createResultConsole(h.pi, { identity });
  out.registerKind('usage', (_dir, id) => `saved ${id}`);
  out.showSaved(h.ctx, '/ignored', 'usage', 'r2', { replay: true });
  assert.equal(h.sent.length, 1);
  assert.equal(h.sent[0].message.content, `${REPLAY_HEADER}\nsaved r2`);
  assert.throws(() => out.showSaved(h.ctx, '/ignored', 'missing-kind', 'r2'), /No saved missing-kind result/);
  assert.deepEqual(h.forbidden, []);
});

test('automatic publication is once per branch; a replay repeats only after another message followed', () => {
  const h = harness();
  const out = createResultConsole(h.pi, { identity });
  out.enqueue(h.ctx, { kind: 'security', requestId: 'r3', text: 'first' });
  out.enqueue(h.ctx, { kind: 'security', requestId: 'r3', text: 'again' }); // resubmission: already on this branch
  assert.equal(h.sent.length, 1);
  out.enqueue(h.ctx, { kind: 'security', requestId: 'r3', text: 'reopened', replay: true }); // already the last visible message
  assert.equal(h.sent.length, 1);
  h.branch.push({ type: 'message', role: 'user' });
  out.enqueue(h.ctx, { kind: 'security', requestId: 'r3', text: 'reopened', replay: true });
  assert.equal(h.sent.length, 2);
  assert.equal(h.sent[1].message.content, `${REPLAY_HEADER}\nreopened`);
  // A restart that already contains the entry in its transcript does not print it again automatically.
  const restarted = harness();
  restarted.branch.push({ type: 'custom_message', customType: RESULT_TYPE, details: { requestId: 'r9', kind: 'usage' } });
  createResultConsole(restarted.pi, { identity }).enqueue(restarted.ctx, { kind: 'usage', requestId: 'r9', text: 'x' });
  assert.equal(restarted.sent.length, 0);
});

test('duplicate queued replays for one result merge into a single display', () => {
  const h = harness();
  const out = createResultConsole(h.pi, { identity });
  h.idle = false;
  for (let index = 0; index < 3; index++) out.enqueue(h.ctx, { kind: 'usage', requestId: 'r4', text: 'same', replay: true });
  out.enqueue(h.ctx, { kind: 'security', requestId: 'r4', text: 'different kind', replay: true });
  h.handlers.agent_end({}, h.ctx);
  h.idle = true;
  h.advance(100);
  assert.deepEqual(h.sent.map(item => item.message.details.kind).sort(), ['security', 'usage']);
});

test('a session switch, shutdown or a replaced native session discards queued results', () => {
  for (const event of ['session_switch', 'session_shutdown']) {
    const h = harness();
    const out = createResultConsole(h.pi, { identity });
    h.idle = false;
    out.enqueue(h.ctx, { kind: 'security', requestId: 'r5', text: 'x' });
    h.handlers[event]({}, h.ctx);
    h.idle = true;
    h.handlers.agent_end({}, h.ctx);
    h.advance(500);
    assert.equal(h.sent.length, 0, event);
  }
  const replaced = harness();
  const out = createResultConsole(replaced.pi, { identity });
  replaced.idle = false;
  out.enqueue(replaced.ctx, { kind: 'security', requestId: 'r6', text: 'x' });
  replaced.session = 'another-native-session'; // no event reached us, the identity tuple still differs
  replaced.idle = true;
  replaced.handlers.agent_end({}, replaced.ctx);
  replaced.advance(500);
  assert.equal(replaced.sent.length, 0);
});

// ---- real submission tools against an app-owned fixture directory ----
const TOKEN = 'fixture-bridge-token';
const actor = { instanceId: 'architect-fixture', generation: 7, launchId: createHash('sha256').update(TOKEN).digest('hex'), sessionId: 'session-fixture' };

function toolbox(dir, h) {
  const tools = new Map();
  const schema = { optional() { return this; }, nullable() { return this; } };
  const zod = new Proxy({}, { get: () => () => schema });
  const api = registerClassifyTool({ ...h.pi, registerTool: tool => tools.set(tool.name, tool), zod }, dir);
  return { tools, api };
}
function admit(dir, overrides = {}) {
  const request = {
    requestId: randomUUID(), folder: path.join(dir, 'project'), packageId: 'npm:example', name: 'example', ecosystem: 'npm',
    resolvedVersions: ['1.2.3'], declaredVersions: ['^1.0.0'], newestVersion: '4.0.0', newestStatus: 'available',
    registryUrl: null, repositoryUrl: null,
    architectInstanceId: actor.instanceId, architectGeneration: actor.generation,
    schemaVersion: 2, kind: 'security', createdAt: 1_760_000_000_000, contextId: 'b'.repeat(64),
    architectLaunchId: actor.launchId, architectSessionId: actor.sessionId, ...overrides,
  };
  mkdirSync(request.folder, { recursive: true });
  const folder = path.join(dir, 'security', request.requestId);
  mkdirSync(folder, { recursive: true });
  writeFileSync(path.join(folder, 'request.json'), JSON.stringify(request));
  return request;
}
const payload = requestId => ({
  requestId, summary: 'Sourced fixture \u001b[31mnot a verdict',
  findings: [{
    title: 'Parser DoS', severity: 'high', majorVersion: '3', affectedVersions: '<3.0.1', fixedVersion: '3.0.1', advisoryId: 'EX-1',
    evidence: [{ title: 'Advisory', url: 'https://example.org/EX-1' }],
    applicability: { status: 'relevant', reason: 'The parser receives request bodies.', conditions: ['Untrusted input'],
      evidence: [{ path: 'src/server.js', lineStart: 12, lineEnd: 20, explanation: 'Calls parse(body)' }] },
  }],
  limitations: ['Fixture only.'],
});

async function withEnvironment(run) {
  const keep = ['OMP_MANAGER_BRIDGE_TOKEN', 'OMP_MANAGER_BRIDGE_INSTANCE_ID', 'OMP_MANAGER_BRIDGE_GENERATION'].map(name => [name, process.env[name]]);
  Object.assign(process.env, { OMP_MANAGER_BRIDGE_TOKEN: TOKEN, OMP_MANAGER_BRIDGE_INSTANCE_ID: actor.instanceId, OMP_MANAGER_BRIDGE_GENERATION: String(actor.generation) });
  const dir = mkdtempSync(path.join(os.tmpdir(), 'omp-architect-console-'));
  try { await run(dir); }
  finally {
    for (const [name, value] of keep) { if (value === undefined) delete process.env[name]; else process.env[name] = value; }
    rmSync(dir, { recursive: true, force: true });
  }
}

test('a submission while streaming saves the result but only prints, once, after agent_end and idle', () => withEnvironment(async dir => {
  const h = harness();
  const { tools, api } = toolbox(dir, h);
  const request = admit(dir);
  h.idle = false;
  const result = await tools.get('submit_dependency_security_review').execute('id', payload(request.requestId), undefined, undefined, h.ctx);
  assert.equal(result.isError, undefined);
  assert.match(result.content[0].text, /^Saved dependency security review/);
  assert.equal(h.sent.length, 0);
  h.handlers.agent_end({}, h.ctx);
  h.advance(200);
  assert.equal(h.sent.length, 0);
  h.idle = true;
  h.advance(100);
  assert.equal(h.sent.length, 1);
  const text = h.sent[0].message.content;
  for (const expected of ['Parser DoS', 'relevant — The parser receives request bodies.', 'src/server.js:12-20', 'Untrusted input', 'Fixture only.', 'Sourced fixture \\u001b[31mnot a verdict']) {
    assert.ok(text.includes(expected), expected);
  }
  assert.equal(text.includes('\u001b'), false);
  assert.deepEqual(h.sent[0].options, { triggerTurn: false });
  // Reopening the saved result: the last visible message already shows it, then a replay once something followed.
  api.showSaved(h.ctx, 'security', request.requestId, true);
  assert.equal(h.sent.length, 1);
  h.branch.push({ type: 'message', role: 'assistant' });
  api.showSaved(h.ctx, 'security', request.requestId, true);
  assert.equal(h.sent.length, 2);
  assert.ok(h.sent[1].message.content.startsWith(REPLAY_HEADER));
  assert.deepEqual(h.forbidden, []);
}));

test('an idle submission prints immediately and a usage explanation uses the same path', () => withEnvironment(async dir => {
  const h = harness();
  const { tools } = toolbox(dir, h);
  const request = admit(dir, { kind: 'usage' });
  await tools.get('submit_dependency_usage_explanation').execute('id', {
    requestId: request.requestId, status: 'used', summary: 'Parses settings', purpose: 'Config parsing', projectRole: 'runtime', benefits: ['Reads settings'],
    paths: [], sourceEvidence: [{ path: 'src/config.js', lineStart: 1, lineEnd: 4, explanation: 'imports parser' }], limitations: [],
  }, undefined, undefined, h.ctx);
  assert.equal(h.sent.length, 1);
  assert.match(h.sent[0].message.content, /Dependency usage explanation[\s\S]*Parses settings[\s\S]*src\/config\.js:1-4/);
  assert.deepEqual(h.sent[0].message.details, { requestId: request.requestId, kind: 'usage' });
  assert.deepEqual(h.forbidden, []);
}));

test('a submission from another session or launch is refused and nothing is queued or saved', () => withEnvironment(async dir => {
  const h = harness('different-session');
  const { tools } = toolbox(dir, h);
  const request = admit(dir);
  const result = await tools.get('submit_dependency_security_review').execute('id', payload(request.requestId), undefined, undefined, h.ctx);
  assert.equal(result.isError, true);
  assert.match(result.content[0].text, /different Architect/);
  h.advance(500);
  assert.equal(h.sent.length, 0);
  process.env.OMP_MANAGER_BRIDGE_TOKEN = 'a-later-launch-token';
  h.session = actor.sessionId;
  const later = await tools.get('submit_dependency_security_review').execute('id', payload(request.requestId), undefined, undefined, h.ctx);
  assert.equal(later.isError, true);
  assert.equal(h.sent.length, 0);
}));
