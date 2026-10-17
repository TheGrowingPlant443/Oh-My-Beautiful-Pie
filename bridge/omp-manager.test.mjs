import test from 'node:test';
import assert from 'node:assert/strict';
import { nativeAnswer, controlAllowed, planPayload, confirmedPlanHandoff } from './omp-manager.mjs';

test('approval answers must be actual native labels, not truthy values', () => {
  const request = { method: 'select', options: ['Approve', 'Deny'] };
  assert.equal(nativeAnswer(request, 'Deny'), 'Deny');
  assert.equal(nativeAnswer(request, 'Approve'), 'Approve');
  assert.throws(() => nativeAnswer(request, true));
  assert.throws(() => nativeAnswer(request, 'approve'));
  assert.equal(nativeAnswer(request, { cancelled: true }), undefined);
  assert.equal(nativeAnswer({ method: 'confirm' }, { cancelled: true }), false);
});
test('rich answers retain native question identity and enforce selection rules', () => {
  const request = { method: 'askDialog', questions: [{ id: 'design', question: 'Design?', options: [{label:'Simple'},{label:'Complex'}] }] };
  const answer = nativeAnswer(request, { kind:'submit', results:[{id:'design',question:'forged',options:['forged'],selectedOptions:['Simple'],note:'Keep it auditable'}] });
  assert.equal(answer.results[0].question, 'Design?');
  assert.deepEqual(answer.results[0].options, ['Simple','Complex']);
  assert.equal(answer.results[0].note, 'Keep it auditable');
  assert.throws(() => nativeAnswer(request, {kind:'submit',results:[{id:'different',selectedOptions:['Simple']}]}));
  assert.throws(() => nativeAnswer(request, {kind:'submit',results:[{id:'design',selectedOptions:['Simple','Complex']}]}));
  assert.throws(() => nativeAnswer(request, {kind:'submit',results:[{id:'design',selectedOptions:['Simple','Simple']}]}));
});
test('disabled options and opaque native overlays cannot be auto-approved', () => {
  assert.throws(() => nativeAnswer({method:'select',options:['Run','Keep'],disabledIndices:[0]}, 'Run'));
  assert.throws(() => nativeAnswer({method:'terminal'}, 'Approve and execute'), /native_terminal_decision/);
  assert.throws(() => nativeAnswer({method:'custom'}, true), /native_terminal_decision/);
});
test('app-originated abort and prompt reach any session; manager-originated control still needs an owned instance', () => {
  assert.equal(controlAllowed({ source: 'human', action: 'abort', role: 'manager', owner: null }), true);
  assert.equal(controlAllowed({ source: 'human', action: 'prompt', role: 'instance', owner: null }), true);
  assert.equal(controlAllowed({ source: 'human', action: 'reply_ui', role: 'instance', owner: null }), false);
  assert.equal(controlAllowed({ source: 'manager', action: 'abort', role: 'instance', owner: null }), false);
  assert.equal(controlAllowed({ source: 'manager', action: 'abort', role: 'manager', owner: 'M' }), false);
  assert.equal(controlAllowed({ source: 'manager', action: 'abort', role: 'instance', owner: 'M' }), true);
});
test('dependency_result is a human-origin, display-only action never reachable by a Manager or model', () => {
  assert.equal(controlAllowed({ source: 'human', action: 'dependency_result', role: 'instance', owner: null }), true);
  assert.equal(controlAllowed({ source: 'manager', action: 'dependency_result', role: 'instance', owner: 'M' }), false);
  assert.equal(controlAllowed({ source: 'manager', action: 'dependency_result', role: 'manager', owner: 'M' }), false);
});
test('plan payloads may be objects or JSON strings but never anything else', () => {
  assert.deepEqual(planPayload({ text: 'a' }), { text: 'a' });
  assert.deepEqual(planPayload('{"text": "# test\\n- [ ] one\\n"}'), { text: '# test\n- [ ] one\n' });
  assert.deepEqual(planPayload(undefined), {});
  assert.deepEqual(planPayload(null), {});
  for (const bad of ['not json', '"a string"', '[1]', 5, ['x']]) assert.throws(() => planPayload(bad), /payload must be a JSON object/);
});

test('a pending or declined confirmation cannot start or hand off; every retry asks again', async () => {
  let answer;
  let confirmations = 0;
  let handoffs = 0;
  const request = async action => {
    if (action === 'plan_handoff_info') return { name: 'Worker', revision: 'r1', needsStart: true };
    handoffs++;
    return { accepted: true };
  };
  const ctx = { hasUI: true, ui: { confirm: () => {
    confirmations++;
    return new Promise(resolve => { answer = resolve; });
  } } };
  const pending = confirmedPlanHandoff(request, ctx, { summary: 'shorter delays', confirmed: true });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(handoffs, 0);
  answer(false);
  assert.deepEqual(await pending, { accepted: false, cancelled: true });
  assert.equal(handoffs, 0);
  const retry = confirmedPlanHandoff(request, ctx, { summary: 'shorter delays' });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(confirmations, 2);
  answer(true);
  assert.equal((await retry).accepted, true);
  assert.equal(handoffs, 1);
});

test('headless and aborted handoffs cannot perform target mutations', async () => {
  let mutations = 0;
  const request = async action => {
    if (action === 'plan_handoff_info') return { name: 'Worker', revision: 'r1', needsStart: false };
    mutations++;
  };
  await assert.rejects(confirmedPlanHandoff(request, { hasUI: false }, { summary: 'change' }), /human confirmation/);
  const controller = new AbortController();
  const ctx = { hasUI: true, ui: { confirm: async () => { controller.abort(new Error('cancelled')); return true; } } };
  await assert.rejects(confirmedPlanHandoff(request, ctx, { summary: 'change' }, controller.signal), /cancelled/);
  assert.equal(mutations, 0);
});
