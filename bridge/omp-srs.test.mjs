import test from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { cp, mkdir, mkdtemp, readFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { promisify } from 'node:util';
import { SRS_ALLOWLISTS, SRS_MARKER, exactUtf8Chunk, installSrsBridge, srsParameters } from './omp-srs.mjs';
import { createVisualizationGuard } from './omp-visualization-guard.mjs';
import { installNativeDecisions } from './omp-native-ui.mjs';
import { installWorkspaceChanges } from './omp-workspace.mjs';
import { controlAllowed, nativeAnswer } from './omp-manager.mjs';

const JOB = '11111111-1111-4111-8111-111111111111';
const ATTEMPT = '22222222-2222-4222-8222-222222222222';
const STAGE = '33333333-3333-4333-8333-333333333333';
const NONCE = 'a1b2c3d4e5f60718293a4b5c6d7e8f90';
const HASH = 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855';
const ids = { jobId: JOB, attemptId: ATTEMPT, stageId: STAGE };
const tick = () => new Promise(resolve => setImmediate(resolve));

// A small structural zod stand-in: strict objects, bounded strings/numbers/arrays. The opt-in real-SDK test below runs the
// very same schemas through the installed validator.
function miniZod() {
  const make = (parse, flags = {}) => {
    const schema = { ...flags, parse, safeParse(value) { try { return { success: true, data: parse(value) }; } catch (error) { return { success: false, error }; } },
      optional: () => make(value => value === undefined ? undefined : parse(value), { optional: true }),
      nullable: () => make(value => value === null ? null : parse(value)),
      strict: () => schema };
    return schema;
  };
  const fail = message => { throw new Error(message); };
  const string = (min = 0, max = Infinity, pattern) => {
    const schema = make(value => {
      if (typeof value !== 'string') fail('expected string');
      if (value.length < min || value.length > max) fail('string length');
      if (pattern && !pattern.test(value)) fail('string pattern');
      return value;
    });
    schema.min = bound => string(bound, max, pattern); schema.max = bound => string(min, bound, pattern); schema.regex = expression => string(min, max, expression);
    return schema;
  };
  const number = (min = -Infinity, max = Infinity, integer = false) => {
    const schema = make(value => {
      if (typeof value !== 'number' || !Number.isFinite(value) || value < min || value > max || (integer && !Number.isInteger(value))) fail('number');
      return value;
    });
    schema.int = () => number(min, max, true); schema.min = bound => number(bound, max, integer); schema.max = bound => number(min, bound, integer);
    return schema;
  };
  const array = (item) => {
    const schema = make(value => { if (!Array.isArray(value)) fail('expected array'); return value.map(entry => item.parse(entry)); });
    schema.max = bound => { const bounded = make(value => { if (!Array.isArray(value) || value.length > bound) fail('array length'); return value.map(entry => item.parse(entry)); }); return bounded; };
    return schema;
  };
  return {
    string: () => string(), number: () => number(), boolean: () => make(value => typeof value === 'boolean' ? value : fail('expected boolean')),
    enum: values => make(value => values.includes(value) ? value : fail('enum')), array,
    object: shape => make(value => {
      if (!value || typeof value !== 'object' || Array.isArray(value)) fail('expected object');
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

function harness({ quiescent = true, guardAvailable = true, handlers = {} } = {}) {
  const state = { quiescent, guardAvailable, sessionId: 'native-session', aborted: 0, prompts: [], order: [], requests: [], idle: true };
  const entries = [], gates = new Set(), tools = new Map(), events = new Map(), timers = [];
  const ctx = { agent: { kind: 'main' }, sessionManager: { getSessionId: () => state.sessionId }, isIdle: () => state.idle, hasPendingMessages: () => false,
    setTimeout: run => timers.push(run), abort: async () => { state.aborted++; } };
  const answers = {
    srs_stage_validate: () => ({ authorized: true }), srs_stage_admitted: () => ({ persisted: true }), srs_stage_yielded: () => ({ paused: true }),
    srs_tool: payload => ({ ok: true, action: payload.action }), ...handlers,
  };
  const request = async (action, payload) => {
    state.requests.push({ action, payload });
    state.order.push(action);
    if (!answers[action]) throw new Error(`unexpected ${action}`);
    return answers[action](payload);
  };
  const workspace = {
    nativeGuardAvailable: () => state.guardAvailable,
    nativeUiCapabilities: () => ({ nativeDecisions: true }),
    quiescence: () => ({ quiescent: state.quiescent, reasons: state.quiescent ? [] : ['streaming'] }),
    identity: () => ({ sessionId: state.sessionId }),
    addBeforeModelCallGate(gate) { gates.add(gate); return () => gates.delete(gate); },
    nativeSessionEntries: () => entries,
    appendNativeEntry(customType, data) { state.order.push('entry'); entries.push({ type: 'custom', customType, data }); },
    submitPrompt(message, { onPromptAdmitted }) { state.prompts.push(message); onPromptAdmitted(); return Promise.resolve(); },
  };
  const pi = { zod: miniZod(), registerTool: tool => tools.set(tool.name, tool), on: (name, handler) => events.set(name, handler) };
  const srs = installSrsBridge(pi, { request, workspace, currentContext: () => ctx });
  const stagePayload = (overrides = {}) => ({ source: 'human', generation: 3, sessionId: 'native-session', ...ids, nonce: NONCE, phase: 'survey', stageNumber: 1, message: 'Survey every admitted area.', ...overrides });
  const callModelGates = async nativeContext => { for (const gate of gates) await gate(undefined, nativeContext); };
  return { state, entries, gates, tools, events, timers, ctx, srs, pi, workspace, stagePayload, callModelGates,
    async admitActive(overrides) {
      const result = await srs.control('srs_stage_prompt', stagePayload(overrides), ctx);
      await events.get('before_agent_start')({ prompt: state.prompts.at(-1) }, ctx);
      return result;
    } };
}

test('SRS controls are human-only and unauthorized Manager/model sources stay denied', () => {
  for (const action of ['srs_stage_prompt', 'srs_stage_status', 'srs_pause', 'setup_markdown_hold', 'setup_markdown_release']) {
    assert.equal(controlAllowed({ role: 'instance', owner: null, action, source: 'human' }), true, action);
    assert.equal(controlAllowed({ role: 'manager', owner: 'M', action, source: 'manager' }), false, action);
  }
});

test('strict schemas accept bounded ledger mutations and reject paths, foreign fields, oversized pages and forged hashes', () => {
  const schemas = srsParameters(miniZod());
  const checkpoint = { ...ids, expectedRevision: 4, updates: {
    addWork: [{ ref: 'a', phase: 'survey', kind: 'sourceArea', rationale: 'Area' }],
    addDocuments: [{ type: 'requirement', title: 'Login', requirementId: 'REQ-FUN-001' }],
    addRequirements: [{ area: 'FUN', title: 'Login', workIds: ['w1'] }],
    addQuestions: [{ prompt: 'Who approves?', options: [{ label: 'Manager' }] }], summary: 'ok' } };
  assert.deepEqual(schemas.srs_checkpoint.parse(checkpoint), checkpoint);
  const reject = (name, value) => assert.throws(() => schemas[name].parse(value), undefined, `${name} ${JSON.stringify(value).slice(0, 80)}`);
  reject('srs_checkpoint', { ...checkpoint, outputPath: 'C:/x' });
  reject('srs_checkpoint', { ...checkpoint, updates: { ...checkpoint.updates, addWork: [{ phase: 'survey', kind: 'sourceArea', rationale: 'x', status: 'excluded' }] } });
  reject('srs_checkpoint', { ...checkpoint, updates: { addRequirements: [{ area: 'XYZ', title: 'x' }] } });
  reject('srs_checkpoint', { ...checkpoint, updates: { addWork: Array.from({ length: 201 }, () => ({ phase: 'survey', kind: 'sourceArea', rationale: 'x' })) } });
  reject('srs_checkpoint', { ...checkpoint, updates: { raw: {} } });
  reject('srs_inspect', { ...ids, section: 'files' });
  reject('srs_inspect', { ...ids, section: 'work', limit: 201 });
  reject('srs_inspect', { ...ids, section: 'work', path: 'C:/secret' });
  reject('srs_read_evidence', { ...ids, evidenceId: 'e1', path: 'C:/secret' });
  reject('srs_write_document_chunk', { ...ids, documentId: 'd', revisionId: 'r', offset: 0, text: 'x', expectedPrefixHash: 'short' });
  reject('srs_write_document_chunk', { ...ids, documentId: 'd', revisionId: 'r', offset: -1, text: 'x', expectedPrefixHash: HASH });
  reject('srs_commit_document', { ...ids, documentId: 'd', revisionId: 'r', expectedHash: HASH, metadata: { type: 'requirement', specificity: 'portable', approvedByHuman: true } });
  reject('srs_commit_document', { ...ids, documentId: 'd', revisionId: 'r', expectedHash: HASH, metadata: { type: 'requirement', specificity: 'bogus' } });
  reject('srs_finish_stage', { ...ids, expectedRevision: 1, review: { phase: 'survey', summary: 's', finalDeclaration: 'done' } });
  reject('srs_inspect', { jobId: 'not-a-uuid', attemptId: ATTEMPT, stageId: STAGE, section: 'work' });
  assert.equal(exactUtf8Chunk('é'.repeat(32 * 1024)), true);
  assert.equal(exactUtf8Chunk('é'.repeat(32 * 1024 + 1)), false, 'a chunk is limited by bytes, not characters');
  assert.equal(exactUtf8Chunk('\ud800'), false, 'a lone surrogate is not exact UTF-8');
});

test('stage admission records the native entry before the backend marker and never resends an admitted stage', async () => {
  const h = harness();
  const first = await h.srs.control('srs_stage_prompt', h.stagePayload(), h.ctx);
  assert.deepEqual(first, { admitted: true, duplicate: false, state: 'queued', sessionId: 'native-session' });
  assert.deepEqual(h.state.order, ['srs_stage_validate', 'entry', 'srs_stage_admitted']);
  assert.equal(h.state.requests.find(item => item.action === 'srs_stage_validate').payload.preAdmission, true, 'before the native prompt exists the claim itself is validated');
  assert.equal(h.state.prompts.length, 1);
  assert.match(h.state.prompts[0], new RegExp(`^Survey every admitted area\\.\\n\\n\\[omp-srs-stage job=${JOB} attempt=${ATTEMPT} stage=${STAGE} nonce=${NONCE}\\]$`));
  assert.deepEqual(h.entries, [{ type: 'custom', customType: SRS_MARKER, data: { ...ids, nonce: NONCE } }]);
  assert.deepEqual(h.state.requests.at(-1).payload, { ...ids, nonce: NONCE });
  await h.events.get('before_agent_start')({ prompt: h.state.prompts[0] }, h.ctx);
  const again = await h.srs.control('srs_stage_prompt', h.stagePayload(), h.ctx);
  assert.deepEqual(again, { admitted: true, duplicate: true, state: 'active', sessionId: 'native-session' });
  assert.equal(h.state.prompts.length, 1, 'a lost acknowledgement is reconciled from the native entry, not resent');
  await assert.rejects(h.srs.control('srs_stage_prompt', h.stagePayload({ nonce: 'Z'.repeat(32) }), h.ctx), /stale_stage/);
});

test('admission is refused while the native session is not quiescent, without the native guard, or when the backend denies the stage', async () => {
  const busy = harness({ quiescent: false });
  await assert.rejects(busy.srs.control('srs_stage_prompt', busy.stagePayload(), busy.ctx), /native_srs_not_quiescent: streaming/);
  assert.equal(busy.state.prompts.length, 0);
  const unavailable = harness({ guardAvailable: false });
  await assert.rejects(unavailable.srs.control('srs_stage_prompt', unavailable.stagePayload(), unavailable.ctx), /native_srs_guard_unavailable/);
  assert.equal(unavailable.state.prompts.length, 0);
  const denied = harness({ handlers: { srs_stage_validate: () => ({ authorized: false }) } });
  await assert.rejects(denied.srs.control('srs_stage_prompt', denied.stagePayload(), denied.ctx), /srs_stage_not_authorized/);
  assert.equal(denied.state.prompts.length, 0);
  const wrongNonce = harness();
  await assert.rejects(wrongNonce.srs.control('srs_stage_prompt', wrongNonce.stagePayload({ nonce: 'short' }), wrongNonce.ctx));
  await assert.rejects(wrongNonce.srs.control('srs_stage_prompt', { ...wrongNonce.stagePayload(), command: 'rm' }, wrongNonce.ctx));
});

test('an admission the backend did not persist pauses the job and keeps the native stage fenced', async () => {
  const h = harness({ handlers: { srs_stage_admitted: () => ({ persisted: false }) } });
  await assert.rejects(h.srs.control('srs_stage_prompt', h.stagePayload(), h.ctx), /not_persisted/);
  assert.ok(h.state.requests.some(item => item.action === 'srs_stage_yielded' && item.payload.reason === 'guard_failure'));
  assert.equal(h.srs.guard.srs(h.ctx).state, 'pending', 'the already admitted native prompt stays restricted');
  await assert.rejects(h.callModelGates({ messages: [{ role: 'user', content: h.state.prompts[0], timestamp: 1 }] }));
  const refused = harness();
  refused.workspace.submitPrompt = () => Promise.reject(new Error('native_prompt_unavailable'));
  await assert.rejects(refused.srs.control('srs_stage_prompt', refused.stagePayload(), refused.ctx), /native_prompt_unavailable/);
  assert.equal(refused.srs.guard.srs(refused.ctx), null, 'a prompt the native session never admitted leaves no guard');
});

test('the stage prompt is answered only after backend persistence and the guard becomes active only for the carrying turn', async () => {
  const h = harness();
  await h.srs.control('srs_stage_prompt', h.stagePayload(), h.ctx);
  assert.equal(h.srs.guard.activeSrs(h.ctx), null, 'queued, not yet running');
  await h.events.get('before_agent_start')({ prompt: 'an unrelated human prompt' }, h.ctx);
  assert.equal(h.srs.guard.activeSrs(h.ctx), null);
  await h.events.get('before_agent_start')({ prompt: h.state.prompts[0] }, h.ctx);
  assert.deepEqual(h.srs.guard.activeSrs(h.ctx), { ...ids, nonce: NONCE, phase: 'survey', state: 'active' });
});

test('stage status is read-only and reports quiescence, guard availability and native admission state', async () => {
  const h = harness({ quiescent: false });
  assert.deepEqual(await h.srs.control('srs_stage_status', { source: 'human', ...ids }, h.ctx), {
    sessionId: 'native-session', quiescent: false, reasons: ['streaming'], guardAvailable: true, active: null,
    admission: { stageId: STAGE, found: false, nonce: null, state: 'absent' } });
  h.state.quiescent = true;
  await h.admitActive();
  const active = await h.srs.control('srs_stage_status', { source: 'human', jobId: JOB, attemptId: ATTEMPT }, h.ctx);
  assert.deepEqual(active.active, ids);
  assert.deepEqual(active.admission, { stageId: STAGE, found: true, nonce: NONCE, state: 'active' });
  assert.equal(h.state.prompts.length, 1, 'status never admits another prompt');
  assert.equal(h.state.aborted, 0, 'status never aborts');
  h.srs.guard.toolResult({ toolName: 'srs_finish_stage', input: { ...ids }, isError: false }, h.ctx);
  const committed = await h.srs.control('srs_stage_status', { source: 'human', ...ids }, h.ctx);
  assert.equal(committed.active, null, 'a committed stage must not report itself active or the scheduler never dispatches the next stage');
  h.srs.settled();
  const ended = await h.srs.control('srs_stage_status', { source: 'human', ...ids }, h.ctx);
  assert.equal(ended.admission.state, 'ended');
  h.state.guardAvailable = false;
  assert.equal((await h.srs.control('srs_stage_status', { source: 'human', ...ids }, h.ctx)).guardAvailable, false);
  await assert.rejects(h.srs.control('srs_stage_status', { source: 'human', ...ids, extra: 1 }, h.ctx));
});

test('pause aborts only the active app-created SRS stage and fences every further provider call', async () => {
  const h = harness();
  const unrelated = await h.srs.control('srs_pause', { source: 'human', ...ids, reason: 'pause' }, h.ctx);
  assert.deepEqual(unrelated, { aborted: false, state: 'paused' });
  assert.equal(h.state.aborted, 0);
  await h.admitActive();
  h.state.quiescent = false;
  assert.deepEqual(await h.srs.control('srs_pause', { source: 'human', ...ids, reason: 'close' }, h.ctx), { aborted: true, state: 'paused' });
  assert.equal(h.state.aborted, 1);
  await assert.rejects(h.callModelGates({ messages: [{ role: 'user', content: h.state.prompts[0] }] }), /srs_stage_paused/);
  assert.equal(h.srs.guard.toolCall({ toolName: 'srs_inspect', input: ids }, h.ctx).block, true);
  h.state.quiescent = true;
  assert.deepEqual(await h.srs.control('srs_pause', { source: 'human', ...ids, reason: 'close' }, h.ctx), { aborted: false, state: 'paused' });
  assert.equal(h.srs.guard.srs(h.ctx), null, 'the tombstone is released at proven quiescence');
});

test('tools forward only to srs_tool for the active stage and enforce the per-phase allowlist', async () => {
  const h = harness();
  assert.deepEqual([...h.tools.keys()], ['srs_inspect', 'srs_read_evidence', 'srs_write_document_chunk', 'srs_commit_document', 'srs_checkpoint', 'srs_finish_stage']);
  const inspect = { ...ids, section: 'work', limit: 50 };
  assert.equal((await h.tools.get('srs_inspect').execute('1', inspect, undefined, undefined, h.ctx)).isError, true, 'no active stage');
  await h.admitActive();
  const result = await h.tools.get('srs_inspect').execute('1', inspect, undefined, undefined, h.ctx);
  assert.equal(result.isError, undefined);
  assert.deepEqual(h.state.requests.at(-1), { action: 'srs_tool', payload: { action: 'srs_inspect', ...inspect } });
  const chunk = { ...ids, documentId: 'd', revisionId: 'r', offset: 0, text: '# Doc', expectedPrefixHash: HASH };
  const survey = await h.tools.get('srs_write_document_chunk').execute('2', chunk, undefined, undefined, h.ctx);
  assert.equal(survey.isError, true, 'survey cannot write documents');
  assert.equal(h.state.requests.filter(item => item.action === 'srs_tool').length, 1);
  const other = await h.tools.get('srs_inspect').execute('3', { ...inspect, stageId: '44444444-4444-4444-8444-444444444444' }, undefined, undefined, h.ctx);
  assert.equal(other.isError, true, 'another stage cannot borrow this authorization');
  for (const tool of ['bash', 'eval', 'read', 'write', 'edit', 'task', 'manage_sessions', 'repo_evidence', 'submit_system_overview']) {
    assert.equal(h.srs.guard.toolCall({ toolName: tool, input: { requestId: JOB } }, h.ctx).block, true, tool);
  }
  assert.equal(h.srs.guard.toolCall({ toolName: 'ask', input: {} }, h.ctx), undefined);
  assert.deepEqual(SRS_ALLOWLISTS.specify.slice(-2), ['srs_write_document_chunk', 'srs_commit_document']);
  assert.equal(SRS_ALLOWLISTS.survey.includes('srs_commit_document'), false);
  const specify = harness();
  await specify.admitActive({ phase: 'specify' });
  assert.equal((await specify.tools.get('srs_write_document_chunk').execute('4', chunk, undefined, undefined, specify.ctx)).isError, undefined);
  assert.equal((await specify.tools.get('srs_write_document_chunk').execute('5', { ...chunk, text: '\ud800' }, undefined, undefined, specify.ctx)).isError, true);
  assert.equal((await specify.tools.get('srs_checkpoint').execute('6', { ...ids, expectedRevision: 0, updates: { arbitrary: true } }, undefined, undefined, specify.ctx)).isError, true);
  const sub = await specify.tools.get('srs_inspect').execute('7', inspect, undefined, undefined, { ...specify.ctx, agent: { kind: 'sub' } });
  assert.equal(sub.isError, true);
});

test('image evidence is exposed as native image content and not as base64 text', async () => {
  const h = harness({ handlers: { srs_tool: payload => ({ evidenceId: payload.evidenceId, kind: 'image', image: { mimeType: 'image/png', data: 'AAAA' }, receiptId: 'r' }) } });
  await h.admitActive();
  const result = await h.tools.get('srs_read_evidence').execute('1', { ...ids, evidenceId: 'e1' }, undefined, undefined, h.ctx);
  assert.deepEqual(result.content[1], { type: 'image', mimeType: 'image/png', data: 'AAAA' });
  assert.equal(result.content[0].text.includes('AAAA'), false);
  const bad = harness({ handlers: { srs_tool: () => ({ kind: 'image', image: { mimeType: 'image/svg+xml', data: 'AAAA' } }) } });
  await bad.admitActive();
  assert.equal((await bad.tools.get('srs_read_evidence').execute('1', { ...ids, evidenceId: 'e1' }, undefined, undefined, bad.ctx)).isError, true);
});

test('the pre-model gate validates the exact persisted stage for every provider call and pauses on any failure', async () => {
  const h = harness();
  await h.admitActive();
  const messages = [{ role: 'user', content: h.state.prompts[0], timestamp: 1 }];
  await h.callModelGates({ messages });
  assert.equal(h.state.requests.filter(item => item.action === 'srs_stage_validate').length, 2, 'admission and the first provider call');
  h.state.requests.length = 0;
  h.state.guardAvailable = false;
  await assert.rejects(h.callModelGates({ messages }), /native_srs_guard_unavailable/);
  assert.equal(h.state.requests.some(item => item.action === 'srs_stage_validate'), false, 'no provider authorization without the native guard');
  assert.deepEqual(h.state.requests.at(-1), { action: 'srs_stage_yielded', payload: { ...ids, reason: 'guard_failure' } });
  assert.equal(h.srs.guard.toolCall({ toolName: 'bash', input: {} }, h.ctx).block, true);
});

test('a backend that stops authorizing a stage stops the provider call', async () => {
  let authorized = true;
  const h = harness({ handlers: { srs_stage_validate: () => ({ authorized }) } });
  await h.admitActive();
  const messages = [{ role: 'user', content: h.state.prompts[0], timestamp: 1 }];
  await h.callModelGates({ messages });
  authorized = false;
  await assert.rejects(h.callModelGates({ messages }), /srs_stage_not_authorized/);
  assert.equal(h.state.requests.at(-1).action, 'srs_stage_yielded');
});

test('compaction keeps SRS authorization: session.compacting adds stage context and a native synthetic continuation stays bound', async () => {
  const h = harness();
  await h.admitActive({ phase: 'specify' });
  const compacting = h.events.get('session.compacting')({ sessionId: 'native-session', messages: [] }, h.ctx);
  assert.match(compacting.context[0], new RegExp(`SRS stage ${STAGE}, job ${JOB}, attempt ${ATTEMPT}, phase specify`));
  assert.equal(compacting.preserveData.ompAnalysis[0].stageId, STAGE);
  h.events.get('agent_end')({ willContinue: true }, h.ctx);
  assert.equal(h.timers.length, 0, 'a continuing run schedules no release');
  // The automatic continuation has no stage nonce: native provenance (synthetic agent developer message) keeps authorization.
  await h.events.get('before_agent_start')({ prompt: 'Resume the user\'s latest intent.' }, h.ctx);
  assert.equal(h.srs.guard.toolCall({ toolName: 'bash', input: {} }, h.ctx).block, true, 'closed until the provider barrier establishes provenance');
  await h.callModelGates({ messages: [{ role: 'user', content: 'compaction summary', timestamp: 1 }, { role: 'developer', synthetic: true, attribution: 'agent', content: 'Resume', timestamp: 2 }] });
  assert.equal(h.srs.guard.activeSrs(h.ctx).stageId, STAGE);
  assert.equal(h.srs.guard.toolCall({ toolName: 'bash', input: {} }, h.ctx).block, true);
  assert.equal(h.srs.guard.toolCall({ toolName: 'srs_commit_document', input: ids }, h.ctx), undefined);
  assert.equal(h.srs.guard.toolCall({ toolName: 'srs_inspect', input: { ...ids, stageId: NONCE } }, h.ctx).block, true);
});

test('an unattributed post-compaction turn fails closed, pauses SRS and never grants unrestricted tools', async () => {
  const h = harness();
  await h.admitActive();
  h.events.get('session_compact')({}, h.ctx);
  h.events.get('agent_end')({ willContinue: true }, h.ctx);
  await h.events.get('before_agent_start')({ prompt: 'Something with no stage nonce' }, h.ctx);
  h.state.requests.length = 0;
  await assert.rejects(h.callModelGates({ messages: [{ role: 'assistant', content: 'unknown origin' }] }), /continuation_uncertain/);
  assert.equal(h.state.requests.some(item => item.action === 'srs_stage_validate'), false);
  assert.ok(h.state.requests.some(item => item.action === 'srs_stage_yielded' && item.payload.reason === 'continuation_uncertain'));
  assert.equal(h.srs.guard.toolCall({ toolName: 'bash', input: {} }, h.ctx).block, true);
});

test('an unrelated human prompt yields SRS: restrictions are released for that turn and the job is paused', async () => {
  const h = harness();
  await h.admitActive();
  await h.events.get('before_agent_start')({ prompt: 'Please look at the login bug instead' }, h.ctx);
  h.state.requests.length = 0;
  await h.callModelGates({ messages: [{ role: 'user', content: 'Please look at the login bug instead', timestamp: 99 }] });
  assert.equal(h.srs.guard.srs(h.ctx), null);
  assert.equal(h.srs.guard.toolCall({ toolName: 'bash', input: { command: 'ls' } }, h.ctx), undefined);
  assert.deepEqual(h.state.requests, [{ action: 'srs_stage_yielded', payload: { ...ids, reason: 'unrelated_prompt' } }]);
});

test('a committed stage is final: only a summary remains and the next stage is admitted after proven quiescence', async () => {
  const h = harness();
  await h.admitActive();
  const finish = { ...ids, expectedRevision: 2, review: { phase: 'survey', summary: 'surveyed', finalDeclaration: 'moreWorkNeeded' } };
  const done = await h.tools.get('srs_finish_stage').execute('1', finish, undefined, undefined, h.ctx);
  assert.equal(done.isError, undefined);
  h.events.get('tool_result')({ toolName: 'srs_finish_stage', input: finish, isError: false }, h.ctx);
  assert.equal(h.srs.guard.toolCall({ toolName: 'srs_inspect', input: ids }, h.ctx).block, true);
  assert.equal(h.srs.guard.toolCall({ toolName: 'ask', input: {} }, h.ctx).block, true);
  h.state.quiescent = false;
  const second = h.stagePayload({ stageId: '55555555-5555-4555-8555-555555555555', nonce: 'f'.repeat(32), phase: 'specify', stageNumber: 2 });
  await assert.rejects(h.srs.control('srs_stage_prompt', second, h.ctx), /not_quiescent/);
  h.state.quiescent = true;
  assert.equal((await h.srs.control('srs_stage_prompt', second, h.ctx)).admitted, true);
  assert.equal(h.state.prompts.length, 2);
  assert.equal(h.srs.guard.srs(h.ctx).stageId, second.stageId);
});

test('another visualization cannot be admitted while SRS owns the Architect, and a paused tombstone does not block forever', async () => {
  const h = harness();
  await h.admitActive();
  assert.throws(() => h.srs.guard.admit(h.ctx, { requestId: JOB, kind: 'systemOverview', message: 'x' }), /srs_owns_architect/);
  h.state.quiescent = false;
  await h.srs.control('srs_pause', { source: 'human', ...ids, reason: 'pause' }, h.ctx);
  assert.throws(() => h.srs.guard.admit(h.ctx, { requestId: JOB, kind: 'systemOverview', message: 'x' }), /srs_owns_architect/);
  h.state.quiescent = true;
  assert.equal(h.srs.guard.admit(h.ctx, { requestId: JOB, kind: 'systemOverview', message: 'x' }).duplicate, false);
});

test('restart recovery reads the native stage entry and fences an unfinished stage until Resume admits a new one', async () => {
  const h = harness();
  h.entries.push({ type: 'custom', customType: SRS_MARKER, data: { ...ids, nonce: NONCE } });
  h.srs.attachContext(h.ctx);
  assert.deepEqual(h.srs.guard.srs(h.ctx), { ...ids, nonce: NONCE, phase: 'survey', state: 'cancelled' });
  assert.equal((await h.srs.control('srs_stage_status', { source: 'human', ...ids }, h.ctx)).admission.state, 'ended', 'nothing from before a restart can still be running');
  const ended = harness();
  ended.entries.push({ type: 'custom', customType: SRS_MARKER, data: { ...ids, nonce: NONCE } }, { type: 'custom', customType: 'omp-srs-stage-ended', data: { ...ids, nonce: NONCE } });
  ended.srs.attachContext(ended.ctx);
  assert.equal(ended.srs.guard.srs(ended.ctx), null);
});

test('disconnection pauses once and reconnect re-enables provider authorization only through backend validation', async () => {
  const h = harness();
  await h.admitActive();
  h.state.requests.length = 0;
  h.srs.disconnected(); h.srs.disconnected();
  assert.deepEqual(h.state.requests, [{ action: 'srs_stage_yielded', payload: { ...ids, reason: 'disconnected' } }]);
  await assert.rejects(h.callModelGates({ messages: [{ role: 'user', content: h.state.prompts[0], timestamp: 1 }] }), /native_srs_guard_unavailable/);
  h.srs.reconnect();
  await h.callModelGates({ messages: [{ role: 'user', content: h.state.prompts[0], timestamp: 1 }] });
});

// ---- Ask mapping and trusted answer receipts --------------------------------------------------------------------

function askFixture({ active = true, map, receipt, retired } = {}) {
  const pending = new Map(), events = [], native = [], calls = [];
  const current = { instanceId: 'instance', generation: 7, sessionId: 'session', workspaceEpoch: 2, cwd: 'C:/repo' };
  const hooks = { active: () => active ? { ...ids } : null,
    map: map ?? (async (id, questions) => { calls.push(['map', id, questions.map(question => question.id)]); return { mapped: true }; }),
    receipt: receipt ?? (async (id, provenance, results) => { calls.push(['receipt', id, provenance, results.map(result => result.id)]); return { receiptId: 'receipt-1' }; }),
    retired: retired ?? (async (id, reason) => { calls.push(['retired', id, reason]); }) };
  const decisions = installNativeDecisions({ pending, identity: () => current, nativeAnswer, emit: (kind, payload) => events.push({ kind, payload }), opportunity() {}, srs: () => hooks });
  const ui = {};
  for (const method of ['approve', 'select', 'confirm', 'input', 'editor', 'askDialog', 'custom']) {
    ui[method] = (...args) => new Promise((resolve, reject) => {
      const options = args[['approve', 'askDialog', 'custom'].includes(method) ? 1 : 2];
      const item = { method, args, options, resolve, reject, aborted: false };
      native.push(item);
      options.signal.addEventListener('abort', () => { item.aborted = true; resolve(undefined); }, { once: true });
    });
  }
  assert.equal(decisions.installUi(ui), true);
  const questions = [{ id: 'q-1', question: 'Who approves refunds?', options: [{ label: 'Manager' }, { label: 'Clerk' }], recommended: 0 }];
  const answer = { kind: 'submit', results: [{ id: 'q-1', selectedOptions: ['Manager'], note: 'Stakeholder confirmed' }] };
  const target = () => [...pending.values()].at(-1).request;
  const respond = (response, source = 'human') => decisions.respond({ ...target(), source, response });
  return { pending, events, native, calls, decisions, ui, questions, answer, target, respond, current };
}

test('a mapped SRS Ask is human-only, has no recommendation timeout and is durably mapped before the dialog is shown', async () => {
  const f = askFixture();
  try {
    const result = f.ui.askDialog(f.questions, { timeout: 30000, signal: new AbortController().signal, acceptImages: true });
    assert.equal(f.native.length, 0, 'not displayed before the mapping call resolves');
    await tick();
    assert.deepEqual(f.calls, [['map', f.target().requestId, ['q-1']]]);
    assert.equal(f.native.length, 1);
    assert.equal('timeout' in f.native[0].options, false);
    assert.equal('ompManagerHumanOnly' in f.native[0].options, false);
    assert.equal(f.native[0].options.acceptImages, false);
    assert.equal(f.target().humanOnly, true);
    assert.equal(f.target().timeout, undefined);
    assert.throws(() => f.respond(f.answer, 'manager'), /human_only_decision/);
    assert.equal(f.pending.size, 1);
    assert.equal(f.respond(f.answer).accepted, true);
    const settled = await result;
    assert.equal(settled.kind, 'submit');
    assert.deepEqual(f.calls.slice(1), [['receipt', f.events[0].payload.requestId, 'human', ['q-1']]]);
  } finally { f.decisions.dispose(); }
});

test('the authenticated receipt is persisted before the native Ask promise is released and a single winner settles it', async () => {
  const gate = Promise.withResolvers();
  const f = askFixture({ receipt: async () => { await gate.promise; return { receiptId: 'late' }; } });
  try {
    let released = false;
    const result = f.ui.askDialog(f.questions, { signal: new AbortController().signal }).then(value => { released = true; return value; });
    await tick();
    const request = f.target();
    assert.equal(f.respond(f.answer).accepted, true);
    assert.throws(() => f.decisions.respond({ ...request, source: 'human', response: f.answer }), /decision_already_resolved/);
    assert.throws(() => f.decisions.respond({ ...request, source: 'manager', response: f.answer }), /decision_already_resolved/);
    f.native[0].resolve(f.answer);
    await tick();
    assert.equal(released, false, 'the model is not released before the receipt exists');
    assert.equal(f.events.some(event => event.kind === 'decision_resolved'), false);
    gate.resolve();
    assert.equal((await result).kind, 'submit');
    assert.equal(f.events.filter(event => event.kind === 'decision_resolved').length, 1);
  } finally { f.decisions.dispose(); }
});

test('a native human answer carries nativeHuman provenance and still needs the receipt', async () => {
  const f = askFixture();
  try {
    const result = f.ui.askDialog(f.questions, { signal: new AbortController().signal });
    await tick();
    f.native[0].resolve(f.answer);
    assert.equal((await result).kind, 'submit');
    assert.equal(f.calls.at(-1)[2], 'nativeHuman');
  } finally { f.decisions.dispose(); }
});

test('receipt failure retires the invocation with an error and leaves the question unanswered', async () => {
  const calls = [];
  const f = askFixture({ receipt: async () => { throw new Error('storage_error: disk full'); }, retired: async (id, reason) => { calls.push(reason); } });
  try {
    const result = f.ui.askDialog(f.questions, { signal: new AbortController().signal });
    await tick();
    f.respond(f.answer);
    await assert.rejects(result, /disk full/);
    assert.deepEqual(calls, ['failed']);
    assert.equal(f.native[0].aborted, true, 'the real overlay was removed');
  } finally { f.decisions.dispose(); }
});

test('cancel, chat, abort, timeout and retirement never satisfy a ledger question', async () => {
  for (const [label, resolve, expected] of [
    ['cancel', f => f.native[0].resolve(undefined), 'cancelled'],
    ['chat', f => f.native[0].resolve({ kind: 'chat' }), 'chat'],
    ['timeout', f => f.native[0].resolve({ kind: 'submit', results: [{ id: 'q-1', selectedOptions: ['Manager'], timedOut: true }] }), 'timeout'],
  ]) {
    const calls = [];
    const f = askFixture({ retired: async (id, reason) => { calls.push(reason); } });
    try {
      const result = f.ui.askDialog(f.questions, { signal: new AbortController().signal });
      await tick();
      resolve(f);
      await result;
      assert.deepEqual(calls, [expected], label);
      assert.equal(f.calls.some(call => call[0] === 'receipt'), false, label);
    } finally { f.decisions.dispose(); }
  }
  const aborted = new AbortController();
  const calls = [];
  const f = askFixture({ retired: async (id, reason) => { calls.push(reason); } });
  try {
    const result = f.ui.askDialog(f.questions, { signal: aborted.signal });
    await tick();
    aborted.abort();
    await result;
    assert.deepEqual(calls, ['aborted']);
    const retiring = f.ui.askDialog(f.questions, { signal: new AbortController().signal });
    await tick();
    f.decisions.dispose();
    await retiring;
    assert.equal(calls.at(-1), 'retired');
  } finally { f.decisions.dispose(); }
});

test('an unmapped SRS Ask never reaches the user, and non-SRS Asks keep their native timeout and Manager behavior', async () => {
  const rejecting = askFixture({ map: async () => { throw new Error('srs_question_not_mapped'); } });
  try {
    await assert.rejects(rejecting.ui.askDialog(rejecting.questions, { signal: new AbortController().signal }), /srs_question_not_mapped/);
    assert.equal(rejecting.native.length, 0);
    assert.equal(rejecting.pending.size, 0);
  } finally { rejecting.decisions.dispose(); }
  const plain = askFixture({ active: false });
  try {
    const result = plain.ui.askDialog(plain.questions, { timeout: 25, signal: new AbortController().signal });
    assert.equal(plain.native[0].options.timeout, 25);
    assert.equal(plain.target().humanOnly, false);
    assert.equal(plain.respond(plain.answer, 'manager').accepted, true);
    assert.equal((await result).kind, 'submit');
    assert.deepEqual(plain.calls, []);
    const select = plain.ui.select('Pick', ['A'], { signal: new AbortController().signal });
    plain.respond('A', 'manager');
    assert.equal(await select, 'A');
  } finally { plain.decisions.dispose(); }
});

// ---- captured native host ---------------------------------------------------------------------------------------

test('without the captured native session the workspace gate reports unavailable and refuses a Markdown hold', () => {
  const fake = { on() {}, pi: {} };
  const workspace = installWorkspaceChanges(fake, { request: async () => ({}), emit() {}, role: 'instance', hasPendingDecisions: () => false, isLeaseCurrent: () => true });
  try {
    assert.equal(workspace.nativeGuardAvailable(), false);
    const status = workspace.quiescence();
    assert.equal(status.quiescent, false);
    assert.ok(status.reasons.includes('native_session_unavailable'));
    assert.throws(() => workspace.holdMarkdown('tx'), /OMP workspace integration requires/);
    assert.throws(() => workspace.holdMarkdown(''), /invalid_transaction/);
    assert.throws(() => workspace.nativeSessionEntries(), /native_srs_guard_unavailable/);
    assert.deepEqual(workspace.releaseMarkdown('tx'), { released: true });
  } finally { void workspace.dispose(); }
});

const sdk = process.env.OMP_SDK_DIR;
test('installed SDK validator accepts the strict SRS schemas and rejects model-supplied authority', { skip: sdk ? false : 'Set OMP_SDK_DIR to exercise installed pi.zod' }, async () => {
  const zod = await import(pathToFileURL(path.join(sdk, '..', 'omptype', 'dist', 'js', 'zod.js')).href);
  const schemas = srsParameters(zod);
  assert.equal(schemas.srs_checkpoint.safeParse({ ...ids, expectedRevision: 0, updates: { summary: 's' } }).success, true);
  assert.equal(schemas.srs_checkpoint.safeParse({ ...ids, expectedRevision: 0, updates: {}, path: 'C:/x' }).success, false);
  assert.equal(schemas.srs_inspect.safeParse({ ...ids, section: 'work', limit: 500 }).success, false);
  assert.equal(schemas.srs_commit_document.safeParse({ ...ids, documentId: 'd', revisionId: 'r', expectedHash: HASH, metadata: { type: 'master', specificity: 'portable', approved: true } }).success, false);
  assert.equal(schemas.srs_write_document_chunk.safeParse({ ...ids, documentId: 'd', revisionId: 'r', offset: 0, text: '# x', expectedPrefixHash: HASH }).success, true);
});

test('installed SDK compaction, willContinue and automatic continuation keep SRS authorization (real AgentSession, offline mock provider)', {
  skip: sdk ? false : 'Set OMP_SDK_DIR for the isolated installed-SDK compaction proof', timeout: 120000,
}, async () => {
  const root = await mkdtemp(path.join(os.homedir(), '.omp-srs-compaction-'));
  try {
    await mkdir(path.join(root, 'agent'), { recursive: true });
    const { version } = JSON.parse(await readFile(path.join(sdk, 'package.json'), 'utf8'));
    const nativeRoot = path.join(root, 'natives');
    await cp(path.join(process.env.PI_NATIVES_DIR || path.join(os.homedir(), '.omp', 'natives'), version), path.join(nativeRoot, version), { recursive: true });
    // PI_CONFIG_DIR is relative to the real home; never override USERPROFILE.
    const config = path.relative(os.homedir(), path.join(root, 'config'));
    assert.equal(config.split(path.sep).includes('..'), false);
    const { stdout } = await promisify(execFile)(process.env.BUN_BIN || 'bun', [fileURLToPath(new URL('./fixtures/srs-compaction-sdk.mjs', import.meta.url))], {
      env: { ...process.env, SRS_COMPACTION_TEST_ROOT: root, PI_CODING_AGENT_DIR: path.join(root, 'agent'), PI_CONFIG_DIR: config, PI_NATIVES_DIR: nativeRoot, OMP_PROFILE: 'default', PI_COMPILED: 'false' },
      timeout: 100000, maxBuffer: 1024 * 1024,
    });
    assert.match(stdout, /Installed SDK compaction\/willContinue\/automatic-continuation SRS guard verified/);
  } finally { await rm(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 }); }
});

test('the shared guard is a standalone module reusable without the SRS bridge', () => {
  const guard = createVisualizationGuard();
  assert.equal(typeof guard.admitSrs, 'function');
  assert.deepEqual(guard.states(), []);
});

for (const state of ['active', 'submitted']) test(`native settlement retires the ${state} stage only after quiescence and the ended marker`, async () => {
  const h = harness();
  await h.admitActive();
  if (state === 'submitted') h.events.get('tool_result')({ toolName: 'srs_finish_stage', input: ids, isError: false }, h.ctx);
  h.state.quiescent = false;
  h.srs.settled();
  assert.equal(h.srs.guard.srs(h.ctx).state, state);
  assert.equal(h.entries.some(entry => entry.customType === 'omp-srs-stage-ended'), false);
  assert.equal(h.srs.guard.toolCall({ toolName: 'bash', input: {} }, h.ctx).block, true);
  const replacement = h.stagePayload({ attemptId: '66666666-6666-4666-8666-666666666666', stageId: '77777777-7777-4777-8777-777777777777', nonce: 'c'.repeat(32) });
  await assert.rejects(h.srs.control('srs_stage_prompt', replacement, h.ctx), /native_srs_not_quiescent/);
  // A failed marker append retains the stage and its provider/tool enforcement.
  h.state.quiescent = true;
  const append = h.workspace.appendNativeEntry;
  h.workspace.appendNativeEntry = () => { throw new Error('disk full'); };
  assert.throws(() => h.srs.settled(), /disk full/);
  assert.equal(h.srs.guard.srs(h.ctx).state, state);
  assert.equal(h.srs.guard.toolCall({ toolName: 'bash', input: {} }, h.ctx).block, true);
  h.workspace.appendNativeEntry = (type, data) => {
    assert.equal(h.srs.guard.srs(h.ctx).state, state, 'guard is still held while the marker is recorded');
    append(type, data);
  };
  h.srs.settled();
  h.workspace.appendNativeEntry = append;
  assert.equal(h.srs.guard.srs(h.ctx), null);
  assert.equal((await h.srs.control('srs_stage_status', { ...ids }, h.ctx)).admission.state, 'ended');
  h.srs.settled();
  assert.equal(h.entries.filter(entry => entry.customType === 'omp-srs-stage-ended').length, 1, 'settlement is idempotent');
  if (state === 'submitted') {
    assert.equal(h.srs.guard.admit(h.ctx, { requestId: '88888888-8888-4888-8888-888888888888', kind: 'systemOverview', message: 'Generate System.' }).duplicate, false);
  } else {
    assert.equal((await h.srs.control('srs_stage_prompt', replacement, h.ctx)).admitted, true, 'explicit Resume can admit a new attempt');
    const resumed = h.srs.guard.srs(h.ctx);
    assert.equal(resumed.attemptId, replacement.attemptId);
    // A late retirement for the prior exact stage cannot clear the new guard.
    h.srs.guard.retireSrs(h.ctx, { ...ids, nonce: NONCE });
    assert.deepEqual(h.srs.guard.srs(h.ctx), resumed);
  }
});

test('a partially mapped Ask retires its persisted invocation before rejecting and permits a corrected Ask', async () => {
  const live = new Map();
  const retire = Promise.withResolvers();
  const h = harness({ handlers: {
    srs_ask_map: payload => {
      const mappedIds = payload.questions.filter(question => question.id === 'q-1' && ![...live.values()].flat().includes(question.id)).map(question => question.id);
      live.set(payload.invocationId, mappedIds);
      const unmappedIds = payload.questions.filter(question => !mappedIds.includes(question.id)).map(question => question.id);
      return { ...ids, mapped: mappedIds.length > 0, mappedIds, unmappedIds };
    },
    srs_ask_retired: async payload => { await retire.promise; live.delete(payload.invocationId); return { ok: true }; },
    srs_ask_receipt: payload => ({ receiptId: 'receipt', satisfiedIds: live.get(payload.invocationId) }),
  } });
  await h.admitActive();
  const f = askFixture({ map: h.srs.ask.map, receipt: h.srs.ask.receipt, retired: h.srs.ask.retired });
  try {
    let rejected = false;
    const bad = f.ui.askDialog([...f.questions, { id: 'unknown', question: 'Unknown?', options: [{ label: 'Yes' }] }], {}).catch(error => { rejected = true; throw error; });
    const rejection = assert.rejects(bad, /srs_question_not_mapped/);
    await tick();
    assert.equal(live.size, 1, 'the backend mapped the valid subset');
    assert.equal(rejected, false, 'reject waits for durable retirement');
    assert.equal(f.native.length, 0);
    assert.equal(f.pending.size, 0, 'there is no Decisions card for the rejected Ask');
    retire.resolve();
    await rejection;
    assert.equal(live.size, 0, 'no ledger question points at a nonexistent dialog');
    const corrected = f.ui.askDialog(f.questions, {});
    await tick();
    assert.equal(f.native.length, 1);
    assert.deepEqual([...live.values()], [['q-1']]);
    f.respond(f.answer);
    assert.equal((await corrected).kind, 'submit');
  } finally { f.decisions.dispose(); }
});

test('failure to retire a partial Ask pauses the stage rather than pretending the question is available', async () => {
  const h = harness({ handlers: {
    srs_ask_map: () => ({ ...ids, mapped: true, mappedIds: ['q-1'], unmappedIds: ['unknown'] }),
    srs_ask_retired: () => { throw new Error('storage_error: disk full'); },
  } });
  await h.admitActive();
  await assert.rejects(h.srs.ask.map('invocation', [{ id: 'q-1', question: 'Q?', options: ['Yes'] }, { id: 'unknown', question: '?', options: ['Yes'] }]), /storage_error/);
  assert.ok(h.state.requests.some(item => item.action === 'srs_stage_yielded' && item.payload.reason === 'guard_failure'));
});
