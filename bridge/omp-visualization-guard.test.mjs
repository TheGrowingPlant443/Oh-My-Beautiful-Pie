import test from 'node:test';
import assert from 'node:assert/strict';
import { MARKER, TOOLS, createVisualizationGuard, registerVisualizationGuard } from './omp-visualization-guard.mjs';

const SYSTEM = '11111111-1111-4111-8111-111111111111';
const STRUCTURE = '22222222-2222-4222-8222-222222222222';

function fakeContext(sessionId = 'session-a', overrides = {}) {
  const timers = [];
  const state = { idle: false, pending: false, aborted: 0 };
  const ctx = {
    agent: { kind: 'main' },
    sessionManager: { getSessionId: () => sessionId },
    isIdle: () => state.idle,
    hasPendingMessages: () => state.pending,
    setTimeout: (run, ms) => timers.push({ run, ms }),
    abort: async () => { state.aborted++; },
    ...overrides,
  };
  const flush = () => { const batch = timers.splice(0); for (const timer of batch) timer.run(); return batch.length; };
  return { ctx, state, flush, timers };
}

function admit(guard, ctx, requestId = SYSTEM, kind = 'systemOverview') {
  const { text, duplicate } = guard.admit(ctx, { requestId, kind, message: 'Analyze this repository.' });
  assert.equal(duplicate, false);
  return text;
}
const call = (guard, ctx, toolName, input = {}) => guard.toolCall({ toolName, input }, ctx);

test('admission appends a random nonce marker and never resends a known request', () => {
  const guard = createVisualizationGuard();
  const { ctx } = fakeContext();
  const text = admit(guard, ctx);
  assert.match(text, new RegExp(`^Analyze this repository\\.\\n\\n\\[${MARKER} id=${SYSTEM} nonce=[0-9a-f]{32}\\]$`));
  assert.deepEqual(guard.admit(ctx, { requestId: SYSTEM, kind: 'systemOverview', message: 'again' }), { duplicate: true, text: null });
  assert.notEqual(text, admit(createVisualizationGuard(), ctx));
  assert.throws(() => guard.admit({ ...ctx, agent: { kind: 'sub' } }, { requestId: STRUCTURE, kind: 'structureCurrent', message: 'x' }), /main OMP context/);
});

test('a pending guard restricts nothing until the carrying turn starts', () => {
  const guard = createVisualizationGuard();
  const { ctx } = fakeContext();
  admit(guard, ctx);
  assert.equal(call(guard, ctx, 'bash', { command: 'ls' }), undefined);
  assert.equal(call(guard, ctx, 'edit', {}), undefined);
  guard.beforeAgentStart({ prompt: 'an unrelated turn' }, ctx);
  assert.equal(call(guard, ctx, 'bash', { command: 'ls' }), undefined);
  assert.deepEqual(guard.states(), [{ requestId: SYSTEM, kind: 'systemOverview', state: 'pending' }]);
});

test('an active visualization turn allows only request-bound reads, the matching submitter and native Ask', () => {
  const guard = createVisualizationGuard();
  const { ctx } = fakeContext();
  guard.beforeAgentStart({ prompt: admit(guard, ctx) }, ctx);
  assert.equal(guard.states()[0].state, 'active');
  for (const name of [TOOLS.evidence, TOOLS.inspect, TOOLS.target, TOOLS.systemSubmit]) assert.equal(call(guard, ctx, name, { requestId: SYSTEM }), undefined, name);
  assert.equal(call(guard, ctx, TOOLS.ask, { questions: [] }), undefined);
  // Another request, a missing request id, the other submitter, and everything that executes, writes or delegates are refused.
  for (const [name, input] of [
    [TOOLS.evidence, { requestId: STRUCTURE }], [TOOLS.evidence, {}], [TOOLS.inspect, { requestId: STRUCTURE }],
    [TOOLS.structureSubmit, { requestId: SYSTEM }], [TOOLS.systemSubmit, { requestId: STRUCTURE }],
    ['bash', { command: 'rm -rf /' }], ['write', { path: 'a' }], ['edit', {}], ['task', {}], ['eval', {}], ['read', { path: 'x' }],
    ['classify_dependencies', {}], ['manager', {}], ['plan_session', {}], ['submit_dependency_discovery', { requestId: SYSTEM }],
  ]) {
    const result = call(guard, ctx, name, input);
    assert.equal(result?.block, true, `${name} ${JSON.stringify(input)}`);
    assert.match(result.reason, /read-only visualization analysis/);
  }
});

test('structure kinds are bound to the structure submitter and the nested tool inputs cannot spoof another request', () => {
  const guard = createVisualizationGuard();
  const { ctx } = fakeContext();
  guard.beforeAgentStart({ prompt: admit(guard, ctx, STRUCTURE, 'structureProposed') }, ctx);
  assert.equal(call(guard, ctx, TOOLS.structureSubmit, { requestId: STRUCTURE }), undefined);
  assert.equal(call(guard, ctx, TOOLS.target, { requestId: STRUCTURE }), undefined);
  assert.equal(call(guard, ctx, TOOLS.systemSubmit, { requestId: STRUCTURE }).block, true);
  assert.equal(call(guard, ctx, TOOLS.evidence, { requestId: [STRUCTURE] }).block, true);
  assert.equal(call(guard, ctx, 'bash', { requestId: STRUCTURE }).block, true);
});

test('a successful submission ends the generation; a failed one does not', () => {
  const guard = createVisualizationGuard();
  const { ctx } = fakeContext();
  guard.beforeAgentStart({ prompt: admit(guard, ctx) }, ctx);
  guard.toolResult({ toolName: TOOLS.systemSubmit, input: { requestId: SYSTEM }, isError: true }, ctx);
  assert.equal(call(guard, ctx, TOOLS.evidence, { requestId: SYSTEM }), undefined);
  guard.toolResult({ toolName: TOOLS.evidence, input: { requestId: SYSTEM }, isError: false }, ctx);
  assert.equal(guard.states()[0].state, 'active');
  guard.toolResult({ toolName: TOOLS.systemSubmit, input: { requestId: STRUCTURE }, isError: false }, ctx);
  assert.equal(guard.states()[0].state, 'active');
  guard.toolResult({ toolName: TOOLS.systemSubmit, input: { requestId: SYSTEM }, isError: false }, ctx);
  assert.equal(guard.states()[0].state, 'submitted');
  for (const name of [TOOLS.evidence, TOOLS.systemSubmit, TOOLS.ask, 'bash']) {
    const result = call(guard, ctx, name, { requestId: SYSTEM });
    assert.equal(result.block, true, name);
    assert.match(result.reason, /complete/);
  }
});

test('the guard releases at native quiescence, not at an agent_end that still has queued work', () => {
  const guard = createVisualizationGuard();
  const { ctx, state, flush } = fakeContext();
  guard.beforeAgentStart({ prompt: admit(guard, ctx) }, ctx);
  guard.agentEnd({ willContinue: true }, ctx);
  assert.equal(flush(), 0, 'a continuing run schedules nothing');
  guard.agentEnd({}, ctx);
  state.idle = false;
  flush();
  assert.equal(guard.states().length, 1, 'not idle yet');
  state.idle = true; state.pending = true;
  flush();
  assert.equal(guard.states().length, 1, 'messages still pending');
  state.pending = false;
  flush();
  assert.equal(guard.states().length, 0);
  assert.equal(call(guard, ctx, 'bash', { command: 'ls' }), undefined, 'unrelated Architect turns are unrestricted again');
});

test('a queued visualization prompt survives the end of unrelated work and a dropped one is released at quiescence', () => {
  const guard = createVisualizationGuard();
  const { ctx, state, flush } = fakeContext();
  const text = admit(guard, ctx);
  // Unrelated running turn ends while the follow-up is still queued: nothing is released.
  guard.agentEnd({}, ctx);
  state.idle = true; state.pending = true;
  flush();
  assert.equal(guard.states()[0].state, 'pending');
  // The queued batch (joined with another user message) starts: the guard activates for that whole turn.
  state.idle = false;
  guard.beforeAgentStart({ prompt: `first\n\n${text}` }, ctx);
  assert.equal(guard.states()[0].state, 'active');
  state.pending = false;
  flush();
  assert.equal(guard.states().length, 1, 'the turn is still running');
  state.idle = true;
  flush();
  assert.equal(guard.states().length, 0);
  // A prompt the native queue dropped never starts: quiescence clears its pending guard.
  const second = createVisualizationGuard();
  const other = fakeContext();
  admit(second, other.ctx, STRUCTURE, 'structureCurrent');
  other.state.idle = true;
  second.agentEnd({}, other.ctx);
  other.flush();
  assert.equal(second.states().length, 0);
});

test('a new human turn without the nonce ends a finished visualization guard at the provider barrier; an unfinished sibling stays pending', async () => {
  const guard = createVisualizationGuard();
  const { ctx } = fakeContext();
  guard.beforeAgentStart({ prompt: admit(guard, ctx) }, ctx);
  const queued = admit(guard, ctx, STRUCTURE, 'structureCurrent');
  guard.beforeAgentStart({ prompt: 'the user asks something else' }, ctx);
  assert.equal(call(guard, ctx, 'bash', { command: 'ls' }).block, true, 'uncertain provenance is closed until the provider barrier decides');
  await guard.beforeModelCall(ctx, { messages: [{ role: 'user', content: 'the user asks something else', timestamp: 10 }] });
  assert.deepEqual(guard.states(), [{ requestId: STRUCTURE, kind: 'structureCurrent', state: 'pending' }]);
  assert.ok(queued);
  assert.equal(call(guard, ctx, 'bash', { command: 'ls' }), undefined);
});

test('a native automatic compaction continuation keeps the visualization restrictions and an unknown one fails closed', async () => {
  const guard = createVisualizationGuard();
  const { ctx } = fakeContext();
  guard.beforeAgentStart({ prompt: admit(guard, ctx) }, ctx);
  assert.match(guard.compacting({}, ctx).context[0], /read-only systemOverview request/);
  guard.agentEnd({ willContinue: true }, ctx);
  guard.beforeAgentStart({ prompt: 'Resume the user\'s latest intent.' }, ctx);
  assert.equal(call(guard, ctx, 'bash', {}).block, true);
  await guard.beforeModelCall(ctx, { messages: [{ role: 'user', content: 'summary', timestamp: 1 }, { role: 'developer', synthetic: true, attribution: 'agent', content: 'Resume' }] });
  assert.equal(guard.states()[0].state, 'active');
  assert.equal(call(guard, ctx, 'bash', { command: 'ls' }).block, true, 'compaction did not release the guard');
  assert.equal(call(guard, ctx, TOOLS.systemSubmit, { requestId: SYSTEM }), undefined);
  // An unlabelled continuation never becomes unrestricted: the provider call is refused.
  const unknown = createVisualizationGuard();
  unknown.beforeAgentStart({ prompt: admit(unknown, ctx) }, ctx);
  unknown.beforeAgentStart({ prompt: 'something unattributed' }, ctx);
  await assert.rejects(unknown.beforeModelCall(ctx, { messages: [{ role: 'assistant', content: 'x' }] }), /continuation_uncertain/);
  assert.equal(call(unknown, ctx, 'bash', {}).block, true);
});

test('two prompts batched into one turn share the turn and each keeps its own submitter', () => {
  const guard = createVisualizationGuard();
  const { ctx } = fakeContext();
  const first = admit(guard, ctx);
  const second = admit(guard, ctx, STRUCTURE, 'structureCurrent');
  guard.beforeAgentStart({ prompt: `${first}\n\n${second}` }, ctx);
  assert.deepEqual(guard.states().map(item => item.state), ['active', 'active']);
  assert.equal(call(guard, ctx, TOOLS.systemSubmit, { requestId: SYSTEM }), undefined);
  assert.equal(call(guard, ctx, TOOLS.structureSubmit, { requestId: STRUCTURE }), undefined);
  assert.equal(call(guard, ctx, TOOLS.structureSubmit, { requestId: SYSTEM }).block, true);
  guard.toolResult({ toolName: TOOLS.systemSubmit, input: { requestId: SYSTEM }, isError: false }, ctx);
  assert.equal(call(guard, ctx, TOOLS.evidence, { requestId: STRUCTURE }), undefined);
  assert.equal(call(guard, ctx, TOOLS.systemSubmit, { requestId: SYSTEM }).block, true);
});

test('the pre-model context hook fails closed when the activation hook never ran for the carried prompt', async () => {
  const guard = createVisualizationGuard();
  const { ctx, state } = fakeContext();
  const text = admit(guard, ctx);
  const messages = [{ role: 'user', content: 'earlier question' }, { role: 'user', content: [{ type: 'text', text }] }];
  const result = guard.context({ messages }, ctx);
  assert.deepEqual(result.messages, [messages[0]], 'the unguarded prompt never reaches the model');
  assert.equal(state.aborted, 1);
  assert.equal(guard.states().length, 0);
  assert.equal(guard.context({ messages }, ctx), undefined, 'nothing pending: nothing to enforce');
  // A guard that activated normally is never aborted, and history that contains an old prompt is harmless afterwards.
  const active = createVisualizationGuard();
  const fresh = fakeContext();
  const activeText = admit(active, fresh.ctx);
  active.beforeAgentStart({ prompt: activeText }, fresh.ctx);
  assert.equal(active.context({ messages: [{ role: 'user', content: activeText }] }, fresh.ctx), undefined);
  assert.equal(fresh.state.aborted, 0);
});

test('guards never cross a session change and ignore subagent contexts', () => {
  const guard = createVisualizationGuard();
  const first = fakeContext('session-a');
  guard.beforeAgentStart({ prompt: admit(guard, first.ctx) }, first.ctx);
  const subagent = { ...first.ctx, agent: { kind: 'sub' } };
  assert.equal(call(guard, subagent, 'bash', {}), undefined);
  const replaced = fakeContext('session-b');
  assert.equal(call(guard, replaced.ctx, 'bash', { command: 'ls' }), undefined, 'a replaced session inherits no guard');
  assert.equal(guard.states().length, 0);
  const again = createVisualizationGuard();
  again.beforeAgentStart({ prompt: admit(again, first.ctx) }, first.ctx);
  again.sessionChanged(first.ctx);
  assert.equal(again.states().length, 0);
  assert.equal(call(again, first.ctx, 'bash', {}), undefined);
});

test('discard removes a refused prompt guard so a retry is a fresh admission', () => {
  const guard = createVisualizationGuard();
  const { ctx } = fakeContext();
  admit(guard, ctx);
  guard.discard(SYSTEM);
  assert.equal(guard.has(SYSTEM), false);
  assert.equal(admit(guard, ctx).length > 0, true);
});

test('registration wires the native hooks and enforcement flows through them', async () => {
  const handlers = new Map();
  const guard = registerVisualizationGuard({ on: (name, handler) => handlers.set(name, handler) });
  assert.deepEqual([...handlers.keys()].sort(), ['agent_end', 'before_agent_start', 'context', 'session.compacting', 'session_compact', 'session_shutdown', 'session_switch', 'tool_call', 'tool_result']);
  const { ctx } = fakeContext();
  const text = admit(guard, ctx);
  await handlers.get('before_agent_start')({ prompt: text }, ctx);
  assert.equal((await handlers.get('tool_call')({ toolName: 'bash', input: {} }, ctx)).block, true);
  assert.equal(await handlers.get('tool_call')({ toolName: TOOLS.systemSubmit, input: { requestId: SYSTEM } }, ctx), undefined);
  await handlers.get('tool_result')({ toolName: TOOLS.systemSubmit, input: { requestId: SYSTEM }, isError: false }, ctx);
  assert.equal(guard.states()[0].state, 'submitted');
  await handlers.get('session_switch')({}, ctx);
  assert.equal(guard.states().length, 0);
});
