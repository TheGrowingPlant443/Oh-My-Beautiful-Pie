import test from 'node:test';
import assert from 'node:assert/strict';
import { installWorkspaceChanges } from './omp-workspace.mjs';
import { controlAllowed } from './omp-manager.mjs';

function fixture({ roster = [], startup = false } = {}) {
  let pending = false;
  let finishInit;
  const startupWait = new Promise(resolve => { finishInit = resolve; });
  class InteractiveMode {
    constructor() {
      this.sessionManager = { getSessionId: () => 'session' };
      this.isInitialized = false;
      this.roster = roster;
      this.calls = [];
      this.ui = { overlayStack: [], hasOverlay() { return this.overlayStack.some(item => !item.hidden); } };
    }
    async init() { this.isInitialized = true; if (startup) await startupWait; }
    showAgentHub(options) {
      this.calls.push(options);
      // Model the native singleton: opening again neither mounts another component
      // nor submits a message, and native dismissal remains outside the bridge.
      if (!this.ui.overlayStack.some(item => item.component === this.hub && !item.hidden)) {
        this.hub = { roster: this.roster };
        this.ui.overlayStack.push({ component: this.hub, hidden: false });
      }
    }
  }
  const workspace = installWorkspaceChanges({ pi: { InteractiveMode } }, {
    hasPendingDecisions: () => pending, emit() {}, request() {}, isLeaseCurrent: () => true,
  });
  const mode = new InteractiveMode();
  const context = { agent: { kind: 'main' }, sessionManager: mode.sessionManager,
    isIdle() { throw new Error('Hub must not wait for idle or inspect streaming state'); },
    hasPendingMessages() { throw new Error('Hub must not inspect queued composer messages'); } };
  workspace.attachContext(context);
  return { workspace, mode, context, finishInit, setPending(value) { pending = value; } };
}

for (const roster of [[], [{ id: 'child' }]]) {
  test(`native hub opens existing parent with ${roster.length} agents while streaming and reuses its singleton`, async () => {
    const f = fixture({ roster });
    try {
      assert.deepEqual(f.workspace.nativeUiCapabilities(), { agentHub: false });
      await f.mode.init();
      assert.deepEqual(f.workspace.nativeUiCapabilities(), { agentHub: true });
      assert.deepEqual(f.workspace.openAgentHub(), { accepted: true });
      f.workspace.openAgentHub();
      assert.deepEqual(f.mode.calls, [{ initialSection: 'agents' }, { initialSection: 'agents' }]);
      assert.equal(f.mode.ui.overlayStack.length, 1);
      assert.equal(f.mode.hub.roster, roster);
      f.mode.ui.overlayStack.length = 0; // Native dismissal.
      f.workspace.openAgentHub();
      assert.equal(f.mode.ui.overlayStack.length, 1);
    } finally { await f.workspace.dispose(); }
  });
}

test('native initialized startup can open hub before init resolves', async () => {
  const f = fixture({ startup: true });
  const initializing = f.mode.init();
  try {
    await Promise.resolve();
    assert.deepEqual(f.workspace.nativeUiCapabilities(), { agentHub: true });
    f.workspace.openAgentHub();
  } finally { f.finishInit(); await initializing; await f.workspace.dispose(); }
});

test('pending decisions and unrelated native overlays are never displaced or queued', async () => {
  const f = fixture();
  try {
    await f.mode.init();
    f.setPending(true);
    assert.throws(() => f.workspace.openAgentHub(), /native_ui_busy/);
    f.setPending(false);
    for (const field of ['hookSelector', 'hookInput', 'hookEditor']) {
      const dialog = {};
      f.mode[field] = dialog;
      assert.throws(() => f.workspace.openAgentHub(), /native_ui_busy/);
      assert.equal(f.mode[field], dialog);
      f.mode[field] = undefined;
    }
    const unrelated = { component: {}, hidden: false };
    f.mode.ui.overlayStack.push(unrelated);
    assert.throws(() => f.workspace.openAgentHub(), /native_ui_busy/);
    assert.equal(f.mode.calls.length, 0);
    assert.equal(f.mode.ui.overlayStack[0], unrelated);
    unrelated.hidden = true;
    f.workspace.openAgentHub();
    f.mode.ui.overlayStack.push({ component: {}, hidden: false });
    assert.throws(() => f.workspace.openAgentHub(), /native_ui_busy/);
    assert.equal(f.mode.calls.length, 1);
  } finally { await f.workspace.dispose(); }
});

test('exact manager binding, startup readiness, missing host and disposal fail explicitly', async () => {
  const f = fixture();
  const unavailable = /native_agent_hub_unavailable: Reopen this session with a compatible OMP version\./;
  try {
    assert.throws(() => f.workspace.openAgentHub(), unavailable);
    await f.mode.init();
    // Even an identical session ID is not the same adopted native manager.
    f.workspace.attachContext({ ...f.context, sessionManager: { getSessionId: () => 'session' } });
    assert.equal(f.workspace.nativeUiCapabilities().agentHub, false);
    assert.throws(() => f.workspace.openAgentHub(), unavailable);
    f.workspace.attachContext(f.context);
    f.mode.showAgentHub = undefined;
    assert.throws(() => f.workspace.openAgentHub(), unavailable);
  } finally { await f.workspace.dispose(); }
  assert.equal(f.workspace.nativeUiCapabilities().agentHub, false);
  assert.throws(() => f.workspace.openAgentHub(), unavailable);
  const missing = installWorkspaceChanges({ pi: {} }, { hasPendingDecisions: () => false });
  assert.throws(() => missing.openAgentHub(), unavailable);
  await missing.dispose();
});

test('agent hub transport is human-only for every role and ownership state', () => {
  for (const role of ['instance', 'manager']) for (const owner of [null, 'manager']) {
    assert.equal(controlAllowed({ role, owner, action: 'agent_hub', source: 'human' }), true);
    for (const source of ['manager', undefined, 'model']) {
      assert.equal(controlAllowed({ role, owner, action: 'agent_hub', source }), false);
    }
  }
});
