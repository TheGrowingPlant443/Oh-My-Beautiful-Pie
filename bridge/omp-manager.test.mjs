import test from 'node:test';
import assert from 'node:assert/strict';
import { nativeAnswer } from './omp-manager.mjs';

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
