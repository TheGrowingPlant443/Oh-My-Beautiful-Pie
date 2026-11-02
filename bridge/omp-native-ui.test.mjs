import test from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdtemp, rm, mkdir, readFile, cp } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { installNativeDecisions, NATIVE_UI_CAPTURE_ERROR } from './omp-native-ui.mjs';
import { installWorkspaceChanges } from './omp-workspace.mjs';
import { nativeAnswer, confirmedPlanHandoff, controlAllowed } from './omp-manager.mjs';

const tick = () => new Promise(resolve => setImmediate(resolve));
function fixture({ install = true } = {}) {
  const pending = new Map(), events = [], invocations = [];
  const current = { instanceId: 'instance', generation: 7, sessionId: 'session', workspaceEpoch: 2, cwd: 'C:/repo' };
  const nativeUi = installNativeDecisions({ pending, identity: () => current, nativeAnswer,
    emit: (kind, payload) => events.push({ kind, payload }), opportunity() {} });
  const ui = {};
  const cancellations = new Map([['approve', 'deny'], ['confirm', false]]);
  for (const method of ['approve', 'select', 'confirm', 'input', 'editor', 'askDialog', 'custom']) {
    ui[method] = (...args) => new Promise((resolve, reject) => {
      const options = args[['approve', 'askDialog', 'custom'].includes(method) ? 1 : 2];
      const item = { method, args, options, resolve, reject, aborted: false };
      invocations.push(item);
      options.signal.addEventListener('abort', () => { item.aborted = true; resolve(cancellations.get(method)); }, { once: true });
    });
  }
  const originals = Object.getOwnPropertyDescriptors(ui);
  if (install) assert.equal(nativeUi.installUi(ui), true);
  const target = () => [...pending.values()].at(-1).request;
  const respond = (response, source = 'human', targetOverride = {}) => nativeUi.respond({ ...target(), source, response, ...targetOverride });
  return { pending, events, invocations, current, nativeUi, ui, originals, target, respond };
}

test('all raw dialog methods preserve exact prompt data and native-only executable options', async () => {
  const f = fixture();
  try {
    const signal = new AbortController();
    const onTimeout = () => {};
    const prompts = [
      ['approve', [{ full: 'Run\nfull command', compact: 'Run command' }], false, 'deny'],
      ['select', ['Choose', [{ label: 'Safe', description: 'Details', preview: 'diff' }, 'Disabled']], 'Safe'],
      ['confirm', ['Confirm', 'Exact message'], true],
      ['input', ['Name', 'placeholder'], 'typed'],
      ['editor', ['Edit', 'prefill\nverbatim'], 'edited\ntext'],
      ['askDialog', [[{ id: 'q', header: 'Header', question: 'Question?', multi: true, recommended: 1, options: [{ label: 'A', description: 'description', preview: 'preview' }, { label: 'B' }] }]], { kind: 'submit', results: [{ id: 'q', selectedOptions: [], customInput: 'custom', note: 'human note' }] }],
    ];
    for (const [method, args, answer, expected = answer] of prompts) {
      const options = { signal: signal.signal, timeout: 17, onTimeout, disabledIndices: [1], initialIndex: 0, acceptImages: false };
      const value = f.ui[method](...args, options);
      const request = f.target();
      assert.equal(request.method, method);
      assert.equal(request.timeout, 17);
      assert.equal(request.instanceId, 'instance');
      assert.equal(request.generation, 7);
      assert.equal(request.cwd, 'C:/repo');
      assert.equal(request.agentOrigin.kind, 'unattributed');
      assert.equal(Object.isFrozen(request), true);
      assert.ok(request.createdAt > 0);
      if (method === 'approve') assert.deepEqual(request.prompt, args[0]);
      if (method === 'askDialog') { assert.deepEqual(request.questions, args[0]); assert.equal(request.supportsChat, true); }
      const invocation = f.invocations.at(-1);
      assert.equal(invocation.options.onTimeout, onTimeout);
      assert.equal(invocation.options.timeout, 17);
      assert.notEqual(invocation.options.signal, signal.signal);
      assert.equal(JSON.stringify(request).includes('onTimeout'), false);
      f.respond(answer);
      const result = await value;
      if (method === 'askDialog') { assert.deepEqual(result.results[0].selectedOptions, []); assert.equal(result.results[0].note, 'human note'); }
      else assert.deepEqual(result, expected);
      assert.equal(invocation.aborted, true);
      assert.equal(f.pending.size, 0);
    }
    const factory = () => { throw new Error('must not serialize or invoke here'); };
    const custom = f.ui.custom(factory, { overlay: true });
    assert.equal(f.target().title, 'Native custom dialog');
    assert.equal(JSON.stringify(f.target()).includes('factory'), false);
    assert.throws(() => f.respond({ cancelled: true }), /native_terminal_decision/);
    f.invocations.at(-1).resolve('native custom value');
    assert.equal(await custom, 'native custom value');
  } finally { f.nativeUi.dispose(); }
});

test('snapshot prompt arrays cannot be mutated after capture or substituted in native validation', async () => {
  const f = fixture();
  try {
    const options = [{ label: 'Allowed', description: 'first' }, { label: 'Disabled' }];
    const disabledIndices = [1];
    const promise = f.ui.select('Select', options, { disabledIndices });
    const request = f.target();
    options[0].label = 'Forged'; disabledIndices.length = 0;
    assert.equal(request.options[0].label, 'Allowed');
    assert.deepEqual(request.disabledIndices, [1]);
    assert.throws(() => f.respond('Forged'), /native option/);
    assert.throws(() => f.respond('Disabled'), /disabled/);
    assert.equal(f.pending.size, 1);
    f.respond('Allowed');
    assert.equal(await promise, 'Allowed');
  } finally { f.nativeUi.dispose(); }
});

test('native, human and Manager races settle and retire once, aborting the native winner loser', async () => {
  for (const source of ['human', 'manager', 'native']) {
    const f = fixture();
    try {
      const promise = f.ui.confirm('Title', 'Message');
      const target = f.target();
      const native = f.invocations.at(-1);
      if (source === 'native') { native.resolve(false); await tick(); }
      else f.respond(true, source);
      native.resolve(false);
      assert.equal(await promise, source !== 'native');
      assert.throws(() => f.nativeUi.respond({ ...target, response: false, source: 'human' }), /already_resolved/);
      await tick();
      assert.equal(f.events.filter(event => event.kind === 'decision_resolved').length, 1);
      assert.equal(f.pending.size, 0);
      assert.equal(native.aborted, source !== 'native');
    } finally { f.nativeUi.dispose(); }
  }
});

test('caller cancellation wins synchronously and already-aborted callers never register or open native UI', async () => {
  const f = fixture();
  try {
    for (const [method, args, cancelled] of [
      ['approve', [{ full: 'F', compact: 'C' }], 'deny'],
      ['confirm', ['T', 'M'], false],
      ['input', ['T', 'M']],
      ['editor', ['T', 'M']],
      ['select', ['T', ['A']]],
      ['askDialog', [[]]],
    ]) {
      const controller = new AbortController();
      controller.abort();
      assert.equal(await f.ui[method](...args, { signal: controller.signal }), cancelled);
    }
    assert.equal(f.invocations.length, 0);
    assert.equal(f.events.length, 0);
    const controller = new AbortController();
    const promise = f.ui.approve({ full: 'F', compact: 'C' }, { signal: controller.signal });
    const target = f.target();
    controller.abort();
    assert.throws(() => f.nativeUi.respond({ ...target, source: 'human', response: true }), /already_resolved/);
    assert.equal(await promise, 'deny');
    await tick();
    assert.equal(f.events.filter(event => event.kind === 'decision_resolved').length, 1);
  } finally { f.nativeUi.dispose(); }
});

test('human-only plan handoff is stripped natively and cannot be satisfied by its owning Manager', async () => {
  const f = fixture();
  let handoffs = 0;
  try {
    const result = confirmedPlanHandoff(async action => {
      if (action === 'plan_handoff_info') return { needsStart: false, name: 'Worker', revision: 'revision' };
      handoffs++; return { accepted: true };
    }, { hasUI: true, ui: f.ui }, { summary: 'Human reviewed changes' });
    await tick();
    assert.equal(f.target().humanOnly, true);
    assert.equal(Object.hasOwn(f.invocations.at(-1).options, 'ompManagerHumanOnly'), false);
    assert.throws(() => f.respond(true, 'manager'), /human_only_decision/);
    assert.equal(f.pending.size, 1);
    assert.equal(handoffs, 0);
    f.respond(true, 'human');
    assert.deepEqual(await result, { accepted: true });
    assert.equal(handoffs, 1);
  } finally { f.nativeUi.dispose(); }
});

test('reply targets reject instance/session/generation/workspace mismatches without settling a current request', async () => {
  const f = fixture();
  try {
    const promise = f.ui.input('Name', 'initial');
    for (const override of [{ instanceId: 'other' }, { sessionId: 'other' }, { generation: 8 }, { workspaceEpoch: 3 }]) {
      assert.throws(() => f.respond('answer', 'human', override), /stale_generation/);
      assert.equal(f.pending.size, 1);
    }
    f.current.workspaceEpoch++;
    assert.throws(() => f.respond('answer'), /stale_generation/);
    assert.equal(await promise, undefined);
    assert.equal(f.invocations.at(-1).aborted, true);
    const next = f.ui.confirm('Confirm', 'message');
    const identity = f.target();
    f.current.sessionId = 'replaced';
    f.nativeUi.revalidate();
    assert.equal(await next, false);
    assert.equal(identity.sessionId, 'session');
    assert.equal(identity.cwd, 'C:/repo');
    assert.equal(f.pending.size, 0);
  } finally { f.nativeUi.dispose(); }
});

test('context UI facades retain trusted advisor origins across awaits without inventing headless requests', async () => {
  const f = fixture();
  try {
    const main = f.nativeUi.contextFacade({ hasUI: true, ui: f.ui, agent: { kind: 'main' }, sessionManager: { getSessionId: () => 'session' } });
    const advisor = f.nativeUi.contextFacade({ hasUI: true, ui: f.ui, agent: { kind: 'advisor', id: 'a', name: 'Architect advisor' }, sessionManager: { getSessionId: () => 'session' } });
    const invoke = async ctx => { await tick(); return ctx.ui.confirm('Same prompt', 'Same details'); };
    const first = invoke(main), second = invoke(advisor);
    await tick();
    const requests = [...f.pending.values()].map(item => item.request);
    assert.deepEqual(requests.map(item => item.agentOrigin.kind), ['main', 'advisor']);
    assert.equal(requests[1].agentOrigin.id, 'a');
    for (const request of requests) f.nativeUi.respond({ ...request, source: 'human', response: true });
    assert.equal(await first, true); assert.equal(await second, true);
    const headless = { hasUI: false, ui: {}, agent: { kind: 'subagent', id: 'headless' } };
    assert.equal(f.nativeUi.contextFacade(headless), headless);
    assert.equal(f.pending.size, 0);
    f.current.workspaceEpoch++;
    await assert.rejects(advisor.ui.confirm('Old context', 'must not reopen'), /stale_generation/);
    assert.equal(f.pending.size, 0);
  } finally { f.nativeUi.dispose(); }
});

test('Ask supports empty selections/custom notes/chat/cancel and rejects images without native opt-in', async () => {
  const f = fixture();
  try {
    const questions = [{ id: 'one', question: 'One?', options: [{ label: 'A' }] }, { id: 'many', question: 'Many?', multi: true, options: [{ label: 'B' }, { label: 'C' }] }];
    const image = { type: 'image', data: 'selected-human-image', mimeType: 'image/png' };
    let promise = f.ui.askDialog(questions, { acceptImages: false });
    assert.throws(() => f.respond({ kind: 'submit', results: [{ id: 'one', selectedOptions: [], noteImages: [image] }, { id: 'many', selectedOptions: [] }] }), /image/);
    f.respond({ kind: 'submit', results: [{ id: 'one', selectedOptions: [], customInput: 'custom', note: 'note' }, { id: 'many', selectedOptions: ['B', 'C'] }] });
    assert.deepEqual((await promise).results[1].selectedOptions, ['B', 'C']);
    promise = f.ui.askDialog(questions, { acceptImages: true });
    f.respond({ kind: 'submit', results: [{ id: 'one', selectedOptions: [], noteImages: [image] }, { id: 'many', selectedOptions: [] }] });
    assert.deepEqual((await promise).results[0].noteImages, [image]);
    promise = f.ui.askDialog(questions); f.respond({ kind: 'chat' }); assert.deepEqual(await promise, { kind: 'chat' });
    promise = f.ui.askDialog(questions); f.respond({ cancelled: true }); assert.equal(await promise, undefined);
  } finally { f.nativeUi.dispose(); }
});

function hostFixture({ late = false, missing = false } = {}) {
  const f = fixture({ install: false });
  let initializedUi;
  class Runner {
    initialize(_a, _b, _c, ui, mode) { this.ui = ui; this.mode = mode; initializedUi = ui; }
    getUIContext() { return this.ui; }
    createContext(_model, _delegation, agent = { kind: 'main' }) {
      return { agent, hasUI: this.mode === 'tui', mode: this.mode, ui: this.ui, sessionManager: mode.sessionManager };
    }
  }
  class Mode {
    constructor() {
      this.sessionManager = { getSessionId: () => f.current.sessionId, getCwd: () => f.current.cwd, getSessionFile: () => null };
      this.session = { extensionRunner: new Runner() };
      this.isInitialized = false;
    }
    async init() {
      this.setToolUIContext(f.ui, true);
      this.session.extensionRunner.initialize({}, {}, undefined, f.ui, 'tui');
      this.isInitialized = true;
    }
    setToolUIContext(ui, hasUI) { assert.equal(f.nativeUi.installed(ui), true); this.toolUi = ui; this.hasUI = hasUI; }
    getToolUIContext() { return this.toolUi; }
    async applyCwdChange() { return true; }
  }
  if (missing) delete Mode.prototype.setToolUIContext;
  const mode = new Mode();
  if (late) {
    mode.toolUi = f.ui;
    mode.session.extensionRunner.initialize({}, {}, undefined, f.ui, 'tui');
  }
  const workspace = installWorkspaceChanges({ pi: { InteractiveMode: Mode } }, {
    nativeUi: f.nativeUi, hasPendingDecisions: () => f.pending.size > 0, emit() {}, isLeaseCurrent: () => true,
  });
  const ctx = { agent: { kind: 'main' }, sessionManager: mode.sessionManager };
  workspace.attachContext(ctx);
  return { ...f, mode, workspace, ctx, Runner, Mode, initializedUi: () => initializedUi };
}

test('captured host installs raw UI before forwarding and initializes only its own live runner', async () => {
  const f = hostFixture();
  try {
    await f.mode.init();
    assert.equal(f.initializedUi(), f.ui);
    assert.equal(f.workspace.nativeUiCapabilities().nativeDecisions, true);
    const other = new f.Runner();
    other.initialize({}, {}, undefined, {}, 'print');
    assert.equal(other.createContext().hasUI, false);
    const context = f.mode.session.extensionRunner.createContext(undefined, undefined, { kind: 'advisor', id: 'advice' });
    const promise = context.ui.approve({ full: 'Actual raw approve', compact: 'Approve' });
    assert.equal(f.target().agentOrigin.id, 'advice');
    f.respond(false); assert.equal(await promise, 'deny');
  } finally { await f.workspace.dispose(); f.nativeUi.dispose(); }
});

test('late and missing capture never advertise retroactive handler repair; restoration keeps later descriptors', async () => {
  const f = hostFixture({ late: true });
  try {
    // No init was captured. An existing mode is not guessed from a handler facade.
    assert.equal(f.workspace.nativeUiCapabilities().nativeDecisions, false);
    assert.equal(f.workspace.nativeUiCapabilities().nativeDecisionsError, NATIVE_UI_CAPTURE_ERROR);
    const replacement = function () {};
    Object.defineProperty(f.Mode.prototype, 'setToolUIContext', { value: replacement, writable: true, configurable: true });
    Object.defineProperty(f.Runner.prototype, 'initialize', { value: replacement, writable: true, configurable: true });
    Object.defineProperty(f.ui, 'confirm', { value: replacement, writable: true, configurable: true });
    await f.workspace.dispose(); f.nativeUi.dispose();
    assert.equal(f.Mode.prototype.setToolUIContext, replacement);
    assert.equal(f.Runner.prototype.initialize, replacement);
    assert.equal(f.ui.confirm, replacement);
    assert.equal(f.ui.approve, f.originals.approve.value);
  } finally { await f.workspace.dispose(); f.nativeUi.dispose(); }
  const pending = new Map();
  const capture = installNativeDecisions({ pending, identity: () => ({}), nativeAnswer, emit() {}, opportunity() {} });
  assert.equal(capture.installUi({ confirm() {} }), false);
  capture.dispose();
});

test('human reply route is allowed for every app role without granting Manager hub control', () => {
  for (const role of ['instance', 'manager']) for (const owner of [null, 'manager']) {
    assert.equal(controlAllowed({ role, owner, action: 'reply_ui', source: 'human' }), true);
    assert.equal(controlAllowed({ role, owner, action: 'agent_hub', source: 'manager' }), false);
  }
});

test('installed SDK raw controller → handler proxies → real native components settles without a provider', {
  skip: process.env.OMP_SDK_DIR ? false : 'Set OMP_SDK_DIR for the isolated installed-SDK native UI proof', timeout: 90000,
}, async () => {
  const root = await mkdtemp(path.join(os.homedir(), '.omp-native-ui-'));
  try {
    await mkdir(path.join(root, 'agent'), { recursive: true });
    const { version } = JSON.parse(await readFile(path.join(process.env.OMP_SDK_DIR, 'package.json'), 'utf8'));
    const nativeRoot = path.join(root, 'natives');
    await cp(path.join(process.env.PI_NATIVES_DIR || path.join(os.homedir(), '.omp', 'natives'), version), path.join(nativeRoot, version), { recursive: true });
    // PI_CONFIG_DIR is relative to the real Windows home. Never override
    // USERPROFILE: native addon lookup and session confinement depend on it.
    const config = path.relative(os.homedir(), path.join(root, 'config'));
    assert.equal(config.split(path.sep).includes('..'), false, 'scratch config must remain under the real home');
    const { stdout } = await promisify(execFile)(process.env.BUN_BIN || 'bun', [fileURLToPath(new URL('./fixtures/native-ui.mjs', import.meta.url))], {
      env: { ...process.env, NATIVE_UI_TEST_ROOT: root, PI_CODING_AGENT_DIR: path.join(root, 'agent'), PI_CONFIG_DIR: config, PI_NATIVES_DIR: nativeRoot, OMP_PROFILE: 'default', PI_COMPILED: '' },
      timeout: 80000, maxBuffer: 1024 * 1024,
    });
    assert.match(stdout, /Native raw UI and handler component settlement verified/);
  } finally { await rm(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 }); }
});
