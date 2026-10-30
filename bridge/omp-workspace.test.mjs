import test from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdtemp, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { outstandingToolCalls, installWorkspaceChanges, WORKSPACE_COMPATIBILITY_ERROR } from './omp-workspace.mjs';
import { controlAllowed, workspaceChangeRole } from './omp-manager.mjs';

test('workspace controls are human-only even for a Manager with owned workers', () => {
  for (const action of ['workspace_change', 'cancel_workspace_change', 'workspace_plan_prompt']) {
    assert.equal(controlAllowed({ role: 'instance', owner: 'manager', action, source: 'manager' }), false);
    assert.equal(controlAllowed({ role: 'manager', owner: null, action, source: 'human' }), true);
  }
});

test('every ordinary agent may be relocated by the human but helpers and plan sessions never', () => {
  assert.equal(workspaceChangeRole({ role: 'instance', architect: false, helper: false, plan: false }), true);
  assert.equal(workspaceChangeRole({ role: 'manager', architect: false, helper: false, plan: false }), true);
  assert.equal(workspaceChangeRole({ role: 'instance', architect: true, helper: false, plan: false }), true);
  assert.equal(workspaceChangeRole({ role: 'instance', architect: false, helper: true, plan: false }), false);
  assert.equal(workspaceChangeRole({ role: 'instance', architect: false, helper: false, plan: true }), false);
  for (const role of ['instance', 'manager']) assert.equal(controlAllowed({ role, owner: 'manager', action: 'workspace_change', source: 'manager' }), false);
});

test('native batch accounting includes siblings not yet dispatched and waits for actual results', () => {
  const assistant = { role: 'assistant', content: [
    { type: 'toolCall', id: 'wait-call', name: 'wait' },
    { type: 'toolCall', id: 'delayed-eval', name: 'eval' },
  ] };
  const state = { messages: [assistant], pendingToolCalls: new Set(['wait-call']) };
  assert.deepEqual([...outstandingToolCalls(state)], ['wait-call', 'delayed-eval']);
  state.messages.push({ role: 'toolResult', toolCallId: 'delayed-eval' });
  assert.deepEqual([...outstandingToolCalls(state)], ['wait-call']);
  state.messages.push({ role: 'toolResult', toolCallId: 'wait-call' });
  // An end event without the synchronous native pending-set update cannot
  // falsely identify quiescence; both sources must agree.
  assert.deepEqual([...outstandingToolCalls(state)], ['wait-call']);
  state.pendingToolCalls.clear();
  assert.equal(outstandingToolCalls(state).size, 0);
});

test('missing native move hooks fail explicitly rather than adopting registry-only cwd', async () => {
  const workspace = installWorkspaceChanges({ pi: {} }, { emit() {}, request() {}, hasPendingDecisions: () => false });
  await assert.rejects(workspace.control('workspace_change', { requestId: 'request', targetFolder: path.resolve('destination') }), { message: WORKSPACE_COMPATIBILITY_ERROR });
  await workspace.dispose();
});

async function workspaceFixture({ pause, beginError } = {}) {
  const paused = Promise.withResolvers();
  const resume = Promise.withResolvers();
  const records = [];
  const events = [];
  const prompts = [];
  const source = path.resolve('workspace-source');
  let cwd = source;
  let beforeModel;
  let moveHeld = false;
  const sessionManager = {
    getSessionId: () => 'workspace-session', getSessionFile: () => null, getCwd: () => cwd,
    captureState: () => ({ cwd }), rollbackMove: async snapshot => { cwd = snapshot.cwd; },
    getBranch: () => [], appendCustomEntry() {},
  };
  class Session {
    constructor() {
      this.isStreaming = false;
      this.hasPostPromptWork = false;
      this.agent = {
        addBeforeModelCallHook(callback) { beforeModel = callback; return () => {}; },
        subscribe: () => () => {}, hasQueuedMessages: () => false,
      };
    }
    async prompt(message, options = {}) {
      prompts.push(message);
      options.onPromptAdmitted?.();
    }
    async moveSession(targetFolder) { cwd = targetFolder; }
  }
  class Mode {
    constructor() {
      this.sessionManager = sessionManager;
      this.session = new Session();
      this.ui = { requestRender() {} };
      this.settings = { async flush() {
        if (pause !== 'flush') return;
        paused.resolve();
        await resume.promise;
      } };
    }
    async init() { this.isInitialized = true; }
    async applyCwdChange() { return true; }
    async withBtwSessionMove(operation) {
      moveHeld = true;
      try {
        const result = await operation();
        if (pause === 'finalize') { paused.resolve(); await resume.promise; }
        return result;
      } finally { moveHeld = false; }
    }
    updateEditorBorderColor() {}
    async reloadTodos() {}
    async shutdown() {}
  }
  const mode = new Mode();
  const workspace = installWorkspaceChanges({ pi: { InteractiveMode: Mode, AgentSession: Session } }, {
    role: 'instance', hasPendingDecisions: () => false, isLeaseCurrent: () => true,
    emit: (kind, payload) => events.push({ kind, payload }),
    async request(action, payload) {
      records.push({ action, payload, moveHeld });
      if (action === 'workspace_begin' && beginError) throw beginError;
      return action === 'workspace_ready' ? { committed: true } : {};
    },
  });
  workspace.attachContext({ agent: { kind: 'main' }, sessionManager,
    isIdle: () => !mode.session.isStreaming, hasPendingMessages: () => false });
  await mode.init();
  return { workspace, mode, records, events, prompts, source, paused: paused.promise,
    resume: () => resume.resolve(), beforeModel: () => beforeModel(),
    change: { requestId: 'workspace-request', sessionId: 'workspace-session', workspaceEpoch: 0,
      targetFolder: path.resolve('workspace-target') } };
}

for (const pause of ['flush', 'finalize']) {
  test(`idle workspace commits stay outside the native move barrier when a provider joins during ${pause}`, async () => {
    const f = await workspaceFixture({ pause });
    let provider;
    try {
      await f.workspace.control('workspace_change', f.change);
      await f.paused;
      f.mode.session.isStreaming = true;
      provider = f.beforeModel();
      f.resume();
      await provider;
      assert.deepEqual(f.records.map(record => record.action), ['workspace_begin', 'workspace_ready', 'workspace_complete']);
      const ready = f.records.find(record => record.action === 'workspace_ready');
      assert.equal(ready.payload.boundary, 'idle');
      assert.equal(ready.moveHeld, false);
      assert.equal(f.workspace.identity().cwd, f.change.targetFolder);
    } finally {
      f.resume();
      await provider?.catch(() => {});
      await f.workspace.dispose();
    }
  });
}

test('a streaming-entry workspace commits before releasing the native move barrier', async () => {
  const f = await workspaceFixture();
  try {
    await f.workspace.control('workspace_change', f.change);
    f.mode.session.isStreaming = true;
    await f.beforeModel();
    const ready = f.records.find(record => record.action === 'workspace_ready');
    assert.equal(ready.payload.boundary, 'streaming');
    assert.equal(ready.moveHeld, true);
    assert.deepEqual(f.records.map(record => record.action), ['workspace_begin', 'workspace_ready', 'workspace_complete']);
    assert.equal(f.workspace.identity().cwd, f.change.targetFolder);
  } finally { await f.workspace.dispose(); }
});

test('an idle-entry workspace failure is reported without aborting a provider that joins later', async () => {
  const f = await workspaceFixture({ pause: 'flush', beginError: new Error('Workspace admission rejected') });
  let provider;
  try {
    await f.workspace.control('workspace_change', f.change);
    await f.paused;
    f.mode.session.isStreaming = true;
    provider = f.beforeModel();
    f.resume();
    await provider;
    assert.deepEqual(f.records.map(record => record.action), ['workspace_begin', 'workspace_failed']);
    assert.equal(f.events.find(event => event.kind === 'workspace_error').payload.error, 'Workspace admission rejected');
    assert.equal(f.workspace.identity().cwd, f.source);
  } finally {
    f.resume();
    await provider?.catch(() => {});
    await f.workspace.dispose();
  }
});

test('a streaming-entry workspace failure still rejects provider admission', async () => {
  const f = await workspaceFixture({ beginError: new Error('Workspace admission rejected') });
  try {
    await f.workspace.control('workspace_change', f.change);
    f.mode.session.isStreaming = true;
    await assert.rejects(f.beforeModel(), { message: 'Workspace admission rejected' });
    assert.deepEqual(f.records.map(record => record.action), ['workspace_begin', 'workspace_failed']);
    assert.equal(f.workspace.identity().cwd, f.source);
  } finally { await f.workspace.dispose(); }
});

test('Markdown admission rechecks replacement holds without delaying an unheld native prompt', async () => {
  const f = await workspaceFixture();
  let blocked;
  try {
    let admitted = false;
    const unheld = f.workspace.submitPrompt('before deployment', { onPromptAdmitted() { admitted = true; } });
    assert.equal(admitted, true);
    await unheld;
    f.workspace.holdMarkdown('first-deployment');
    blocked = f.mode.session.prompt('after deployment');
    f.workspace.releaseMarkdown('first-deployment');
    f.workspace.holdMarkdown('replacement-deployment');
    await new Promise(resolve => setImmediate(resolve));
    assert.deepEqual(f.prompts, ['before deployment']);
    f.workspace.releaseMarkdown('replacement-deployment');
    await blocked;
    assert.deepEqual(f.prompts, ['before deployment', 'after deployment']);
  } finally {
    await f.workspace.dispose();
    await blocked;
  }
});

test('saved-session relocation uses the installed native writer lease, preserves transcript/artifacts, and rolls back rejected persistence', {
  skip: !process.env.OMP_SDK_DIR ? 'Set OMP_SDK_DIR for isolated native workspace integration' : false,
  timeout: 120000,
}, async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'omp-workspace-native-'));
  try {
    const { stdout } = await promisify(execFile)(process.env.BUN_BIN || 'bun', [fileURLToPath(new URL('./fixtures/workspace-saved-session.mjs', import.meta.url))], {
      env: { ...process.env, WORKSPACE_TEST_ROOT: root, HOME: root, USERPROFILE: root,
        PI_CODING_AGENT_DIR: path.join(root, 'agent'), PI_CONFIG_DIR: 'config', PI_NATIVES_DIR: process.env.PI_NATIVES_DIR || path.join(os.homedir(), '.omp', 'natives'), OMP_PROFILE: 'workspace-fixture', PI_COMPILED: '' },
      timeout: 110000, maxBuffer: 1024 * 1024,
    });
    assert.match(stdout, /Native workspace relocation verified/);
  } finally { await rm(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 }); }
});
