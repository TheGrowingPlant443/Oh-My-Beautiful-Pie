import { test } from 'node:test';
import assert from 'node:assert/strict';
import { get } from 'svelte/store';
import { askResponse, canAnswer, createHumanDecisionStore, decisionKey, hasDecisionAttention, initialDecisionDraft, nativeOption, readDecisionImage, renderedHelperAgentId, toggleQuestionOption } from './human-decisions.ts';

function decision(overrides = {}) {
  return { target: { instanceId: 'agent', requestId: 'public-id', generation: 3, sessionId: 'session', ownershipEpoch: 4, workspaceEpoch: 5 }, createdAt: 100, instanceName: 'Worker', instanceKind: 'instance', agentOrigin: { kind: 'unattributed' }, repository: { folder: 'C:/repo', label: 'repo', scope: 'repository' }, method: 'input', payload: { title: 'Exact question', placeholder: 'Answer' }, answerable: true, humanOnly: false, ...overrides };
}
function harness(initial = { revision: 1, items: [decision()] }, respond = async () => ({ accepted: true })) {
  const store = createHumanDecisionStore();
  let receive;
  let snapshot = initial;
  const answers = [];
  const calls = [];
  const transport = {
    listen: async callback => { calls.push('listen'); receive = callback; return () => calls.push('unlisten'); },
    list: async () => { calls.push('list'); return snapshot; },
    respond: async (target, response) => { answers.push({ target, response }); return respond(target, response); },
  };
  return { store, transport, answers, calls, emit: next => receive(next), setSnapshot: next => { snapshot = next; }, state: () => get(store) };
}

test('every request identity field fences drafts even when prompts are identical', () => {
  const original = decision().target;
  for (const [field, value] of Object.entries({ instanceId: 'other', requestId: 'other', generation: 9, sessionId: null, ownershipEpoch: 9, workspaceEpoch: 9 })) {
    assert.notEqual(decisionKey(original), decisionKey({ ...original, [field]: value }), field);
  }
});

test('subscribe precedes list; an older list cannot revive a natively retired request', async () => {
  const pending = Promise.withResolvers();
  const h = harness();
  h.transport.list = () => { h.calls.push('list'); return pending.promise; };
  const connection = h.store.connect(h.transport);
  await Promise.resolve();
  assert.deepEqual(h.calls, ['listen', 'list']);
  h.emit({ revision: 3, items: [] });
  pending.resolve({ revision: 2, items: [decision()] });
  const disconnect = await connection;
  assert.equal(h.state().revision, 3);
  assert.deepEqual(h.state().items, []);
  assert.equal(h.state().drafts.size, 0);
  disconnect();
  assert.equal(h.calls.at(-1), 'unlisten');
});

test('a failed subscription never falls back to an unobserved live inbox', async () => {
  const h = harness();
  h.transport.listen = async () => { throw new Error('cannot subscribe'); };
  await assert.rejects(h.store.connect(h.transport), /cannot subscribe/);
  await h.store.refresh();
  assert.deepEqual(h.calls, []);
  assert.match(h.state().error, /cannot subscribe/);
});

test('drafts live outside tab mounts, preserve native prefill, and retire with their full target', async () => {
  const item = decision({ method: 'editor', payload: { title: 'Edit', prefill: 'original\ntext' } });
  const h = harness({ revision: 1, items: [item] });
  await h.store.connect(h.transport);
  const key = decisionKey(item.target);
  assert.equal(h.state().drafts.get(key).text, 'original\ntext');
  h.store.updateDraft(item.target, draft => ({ ...draft, text: 'human draft' }));
  // A keyed inbox can unsubscribe (Follow selects a command), then mount again without owning this data.
  const unmount = h.store.subscribe(() => {});
  unmount();
  h.emit({ revision: 2, items: [{ ...item, instanceName: 'Renamed' }] });
  assert.equal(h.state().drafts.get(key).text, 'human draft');
  h.emit({ revision: 3, items: [{ ...item, target: { ...item.target, workspaceEpoch: 6 } }] });
  assert.equal(h.state().drafts.has(key), false);
  assert.equal([...h.state().drafts.values()][0].text, 'original\ntext');
  h.emit({ revision: 4, items: [] });
  assert.equal(h.state().drafts.size, 0);
});

test('submission disables just its target, is single-flight, and waits for authoritative retirement', async () => {
  const response = Promise.withResolvers();
  const first = decision(), second = decision({ target: { ...first.target, requestId: 'second' } });
  const h = harness({ revision: 1, items: [first, second] }, () => response.promise);
  await h.store.connect(h.transport);
  const submission = h.store.submit(first.target, 'answer');
  assert.equal(h.state().drafts.get(decisionKey(first.target)).submitting, true);
  assert.equal(h.state().drafts.get(decisionKey(second.target)).submitting, false);
  await h.store.submit(first.target, 'duplicate');
  assert.equal(h.answers.length, 1);
  assert.equal(h.state().items.length, 2, 'no optimistic removal');
  response.resolve({ accepted: true });
  await submission;
  assert.equal(h.state().drafts.get(decisionKey(first.target)).awaitingResolution, true);
  await h.store.submit(first.target, 'duplicate after acceptance');
  assert.equal(h.answers.length, 1);
  h.emit({ revision: 2, items: [second] });
  assert.equal(h.state().drafts.has(decisionKey(first.target)), false);
});

test('a native answer racing a remote answer cannot re-create a card or its draft', async () => {
  const response = Promise.withResolvers();
  const h = harness(undefined, () => response.promise);
  await h.store.connect(h.transport);
  const submit = h.store.submit(decision().target, 'answer');
  h.emit({ revision: 3, items: [] });
  response.resolve({ accepted: true });
  await submit;
  assert.deepEqual(h.state().items, []);
  assert.equal(h.state().drafts.size, 0, 'the older refresh cannot revive a resolved request');
});

test('uncertain/failed answers retain draft and visible error without automatic retry', async () => {
  const h = harness(undefined, async () => { throw new Error('transport disconnected'); });
  await h.store.connect(h.transport);
  h.store.updateDraft(decision().target, draft => ({ ...draft, text: 'keep me' }));
  await h.store.submit(decision().target, 'keep me');
  const draft = h.state().drafts.get(decisionKey(decision().target));
  assert.equal(draft.text, 'keep me');
  assert.match(draft.error, /Answer not confirmed: transport disconnected/);
  await h.store.refresh();
  assert.equal(h.answers.length, 1);
  assert.equal(h.state().items.length, 1);
  h.setSnapshot({ revision: 2, items: [] });
  await h.store.refresh();
  assert.equal(h.state().drafts.size, 0);
});

test('all app-managed roles remain present, sorted; native-only requests never send fake responses', async () => {
  const items = ['manager', 'architect', 'instance', 'helper', 'plan'].map((role, i) => decision({ instanceKind: role, target: { ...decision().target, instanceId: role }, createdAt: 100 - i }));
  const native = decision({ method: 'custom', answerable: false, target: { ...decision().target, requestId: 'native' } });
  const h = harness({ revision: 1, items: [...items, native] });
  await h.store.connect(h.transport);
  assert.equal(h.state().items.length, 6);
  assert.equal(h.state().items[0].instanceKind, 'plan');
  await h.store.submit(native.target, true);
  assert.equal(h.answers.length, 0);
  assert.equal(canAnswer(native), false);
  assert.equal(canAnswer(decision({ method: 'terminal' })), false);
  assert.equal(canAnswer(decision({ method: 'approve', humanOnly: true })), true);
});

test('all rich native Ask groups preserve IDs, labels, custom text, notes, and empty selections', () => {
  const questions = [
    { id: 'single', question: 'One?', options: [{ label: 'Exact label', description: 'Description', preview: 'Preview' }, { label: 'Disabled' }], recommended: 0, disabledIndices: [1] },
    { id: 'multi', question: 'Several?', multi: true, options: ['A', 'B'] },
    { id: 'empty', question: 'None?', options: [] },
  ];
  const item = decision({ method: 'askDialog', payload: { questions, acceptImages: true, supportsChat: true } });
  const draft = initialDecisionDraft(item);
  assert.deepEqual(draft.questions.single.selectedOptions, [], 'recommended is guidance, not an automatic human answer');
  draft.questions.single = toggleQuestionOption(questions[0], draft.questions.single, 1);
  assert.deepEqual(draft.questions.single.selectedOptions, [], 'disabled options stay disabled');
  draft.questions.single = toggleQuestionOption(questions[0], draft.questions.single, 0);
  draft.questions.single.customInput = 'custom\nanswer';
  draft.questions.single.note = 'human note';
  draft.questions.multi = toggleQuestionOption(questions[1], draft.questions.multi, 0);
  draft.questions.multi = toggleQuestionOption(questions[1], draft.questions.multi, 1);
  draft.questions.multi = toggleQuestionOption(questions[1], draft.questions.multi, 0);
  assert.deepEqual(askResponse(item, draft), { kind: 'submit', results: [
    { id: 'single', selectedOptions: ['Exact label'], customInput: 'custom\nanswer', note: 'human note' },
    { id: 'multi', selectedOptions: ['B'] }, { id: 'empty', selectedOptions: [] },
  ] });
  draft.questions.single.selectedOptions = ['Disabled'];
  assert.throws(() => askResponse(item, draft), /Invalid native question selection/);
  assert.deepEqual(nativeOption(questions[0].options[0]), questions[0].options[0]);
});

test('images are read from explicitly selected files and included only when native Ask accepts them', async () => {
  const file = new File([new Uint8Array([1, 2, 3])], 'selected.png', { type: 'image/png' });
  const image = await readDecisionImage(file);
  assert.deepEqual(image, { type: 'image', data: 'AQID', mimeType: 'image/png' });
  await assert.rejects(readDecisionImage(new File(['text'], 'not-image.txt', { type: 'text/plain' })), /Select an image/);
  const item = decision({ method: 'askDialog', payload: { questions: [{ id: 'q', question: '?', options: [] }], acceptImages: true } });
  const draft = initialDecisionDraft(item);
  assert.deepEqual(askResponse(item, draft).results[0], { id: 'q', selectedOptions: [] }, 'no unsolicited attachment keys');
  draft.questions.q.customInputImages.push(image);
  draft.questions.q.noteImages.push(image);
  assert.deepEqual(askResponse(item, draft).results[0].customInputImages, [image]);
  assert.deepEqual(askResponse(item, draft).results[0].noteImages, [image]);
  assert.deepEqual(askResponse({ ...item, payload: { ...item.payload, acceptImages: false } }, draft).results[0], { id: 'q', selectedOptions: [] });
});

test('human answer union is sent unchanged with the exact fenced target', async () => {
  for (const [method, response] of [['approve', true], ['approve', false], ['confirm', false], ['select', 'Exact label'], ['input', ''], ['editor', 'full\ntext'], ['askDialog', { kind: 'chat' }], ['askDialog', { cancelled: true }]]) {
    const item = decision({ method });
    const h = harness({ revision: 1, items: [item] });
    await h.store.connect(h.transport);
    await h.store.submit(item.target, response);
    assert.deepEqual(h.answers, [{ target: item.target, response }]);
  }
});

test('attention lights only for pending decisions outside the actually visible conversation', () => {
  const a = decision(), b = decision({ target: { ...decision().target, instanceId: 'other', requestId: 'r2' } });
  assert.equal(hasDecisionAttention([], null), false);
  assert.equal(hasDecisionAttention([], 'agent'), false);
  assert.equal(hasDecisionAttention([a], 'agent'), false, 'only the visible agent is pending');
  assert.equal(hasDecisionAttention([a, b], 'agent'), true, 'another agent is pending');
  assert.equal(hasDecisionAttention([b], 'agent'), true);
  assert.equal(hasDecisionAttention([a], null), true, 'no visible conversation: any pending decision counts');
  assert.equal(hasDecisionAttention([a, { ...a, target: { ...a.target, requestId: 'second' } }], 'agent'), false, 'several for the visible agent stay quiet');
});

test('a helper conversation is visible only while shown and backed by a live generation', () => {
  assert.equal(renderedHelperAgentId(true, { id: 'helper', generation: 2 }), 'helper');
  assert.equal(renderedHelperAgentId(false, { id: 'helper', generation: 2 }), null);
  assert.equal(renderedHelperAgentId(true, { id: 'helper', generation: 0 }), null);
  assert.equal(renderedHelperAgentId(true, undefined), null);
  assert.equal(renderedHelperAgentId(true, null), null);
});
