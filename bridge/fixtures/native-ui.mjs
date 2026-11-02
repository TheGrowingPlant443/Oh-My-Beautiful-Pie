import assert from 'node:assert/strict';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { installNativeDecisions } from '../omp-native-ui.mjs';
import { installWorkspaceChanges } from '../omp-workspace.mjs';
import { nativeAnswer, confirmedPlanHandoff } from '../omp-manager.mjs';

const root = process.env.NATIVE_UI_TEST_ROOT;
const sdk = process.env.OMP_SDK_DIR;
assert.ok(root && sdk);
const fromSdk = relative => import(pathToFileURL(path.join(sdk, relative)).href);
const dirs = await import(pathToFileURL(Bun.resolveSync('@oh-my-pi/pi-utils/dirs', sdk)).href);
dirs.setProfile('default');
for (const actual of [dirs.getAgentDir(), dirs.getConfigRootDir(), dirs.getNativesDir(), dirs.getSessionsDir()]) {
  const relative = path.relative(root, actual);
  assert.ok(relative !== '..' && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative), `storage escaped scratch root: ${actual}`);
}
// Do not construct the CLI or AgentSession: onboarding and provider setup are
// unrelated. These are the installed runner, raw UI controller, approval gate
// and actual TUI components, with only the terminal/session host provided here.
const { ensureThemeSync } = await import(pathToFileURL(Bun.resolveSync('@oh-my-pi/pi-tui/theme', sdk)).href);
ensureThemeSync();
const { ExtensionRunner } = await fromSdk('src/extensibility/extensions/runner.ts');
const { ExtensionUiController } = await fromSdk('src/modes/controllers/extension-ui-controller.ts');
const { requestApproval } = await fromSdk('src/tools/approval.ts');
const { SessionManager } = await fromSdk('src/session/session-manager.ts');
const { ApprovalDialogComponent } = await import(pathToFileURL(Bun.resolveSync('@oh-my-pi/pi-tui/overlays/approval-dialog', sdk)).href);
const { AskDialogComponent } = await import(pathToFileURL(Bun.resolveSync('@oh-my-pi/pi-tui/overlays/ask-dialog', sdk)).href);
const { Container } = await import(pathToFileURL(Bun.resolveSync('@oh-my-pi/pi-tui', sdk)).href);
const manager = SessionManager.create(root);
const pending = new Map(), events = [], sessionListeners = new Set(), errors = [];
let workspace;
let providerCalls = 0;
const nativeUi = installNativeDecisions({ pending, nativeAnswer,
  identity: () => ({ instanceId: 'native-fixture', generation: 1, ...workspace.identity() }),
  emit: (kind, payload) => events.push({ kind, payload }), opportunity() {} });
let handlerAction, handlerValue, handlerContext;
const extension = {
  path: path.join(root, 'offline-extension.mjs'), resolvedPath: path.join(root, 'offline-extension.mjs'),
  handlers: new Map([['agent_start', [async (_event, ctx) => { handlerContext = ctx; handlerValue = await handlerAction(ctx); }]]]),
  tools: new Map(), messageRenderers: new Map(), composerShapes: new Map(), commands: new Map(), flags: new Map(), shortcuts: new Map(),
  assistantThinkingRenderers: [], fileWriteFallbackHandlers: [], fileDeleteFallbackHandlers: [],
};
const runner = new ExtensionRunner([extension], {}, root, manager, {});
const steering = [];
const agent = { peekSteeringQueue: () => steering, prioritizeSteering() {}, state: { messages: [], pendingToolCalls: new Set() } };
const session = {
  sessionManager: manager, extensionRunner: runner, agent,
  model: undefined, settings: undefined,
  getQueuedMessages: () => ({ steering, followUp: [] }),
  subscribe(fn) { sessionListeners.add(fn); return () => sessionListeners.delete(fn); },
  prompt() { providerCalls++; throw new Error('provider call prohibited'); },
};
// This transparent host exercises the same captured init/setToolUIContext seam
// without constructing a second core Agent identity or starting terminal IO.
class Host {
  constructor() {
    this.session = session; this.sessionManager = manager;
    this.editor = { pendingImages: [], pasteInFlight: false, focused: false, getText: () => '', handleDraftEdit() {} };
    this.editorContainer = new Container(); this.editorContainer.addChild(this.editor);
    this.ui = { terminal: { rows: 40, columns: 100 }, requestRender() {}, setFocus: component => { this.focus = component; } };
    this.controller = new ExtensionUiController(this);
  }
  async init() { this.isInitialized = true; await this.controller.initHooksAndCustomTools(); }
  setToolUIContext(ui, hasUI) { assert.equal(nativeUi.installed(ui), true, 'capture must precede native forwarding'); this.toolUi = ui; this.hasUI = hasUI; }
  getToolUIContext() { return this.controller.getToolUIContext(); }
  syncComposerShape() { /* Native composer layout is outside this offline decision proof. */ }
  present(component) { errors.push(component); }
}
workspace = installWorkspaceChanges({ pi: { InteractiveMode: Host } }, { nativeUi, hasPendingDecisions: () => pending.size > 0, emit() {}, isLeaseCurrent: () => true });
const host = new Host();
runner.onError(error => errors.push({ text: `${error.event}: ${error.error}` }));
workspace.attachContext({ agent: { kind: 'main' }, sessionManager: manager });
const tick = () => new Promise(resolve => setImmediate(resolve));
const target = () => [...pending.values()].at(-1)?.request;
const reply = (response, source = 'human') => nativeUi.respond({ ...target(), source, response });
async function waiting(componentType) {
  // Actual handler dispatch has async timeout/settings scopes. A bounded event
  // loop barrier waits for its native component rather than sleeping blindly.
  for (let i = 0; i < 50; i++) {
    if (pending.size && (!componentType || host.focus instanceof componentType)) return;
    await tick();
  }
  throw new Error(`native component not presented; errors=${errors.length} pending=${pending.size} ${errors.map(item => item.text ?? item.constructor?.name).join(' | ')}`);
}
async function handler(action, componentType) {
  handlerAction = action;
  const done = runner.emit({ type: 'agent_start' });
  await waiting(componentType);
  return { done };
}
function retiredOnce(id) { assert.equal(events.filter(event => event.kind === 'decision_resolved' && event.payload.requestId === id).length, 1); }
try {
  await host.init();
  assert.equal(workspace.nativeUiCapabilities().nativeDecisions, true);
  assert.equal(runner.getUIContext(), host.toolUi);
  const tool = { name: 'offline_exec', formatApprovalDetails: () => ['First exact line', 'Second exact line'] };
  // Actual native tool approval entrypoint uses runner.getUIContext directly,
  // bypassing handler facades. Its honest origin must remain unattributed.
  let done = requestApproval(runner.getUIContext(), tool, {}, { reason: 'offline fixture' });
  await waiting(ApprovalDialogComponent);
  let id = target().requestId;
  assert.equal(target().method, 'approve'); assert.equal(target().agentOrigin.kind, 'unattributed');
  assert.match(target().prompt.full, /First exact line\nSecond exact line/);
  assert.notEqual(target().prompt.full, target().prompt.compact);
  reply(true);
  assert.equal(await done, 'once');
  assert.equal(host.focus, host.editor); retiredOnce(id);

  for (const response of [false, { cancelled: true }]) {
    done = requestApproval(runner.getUIContext(), tool, {});
    await waiting(ApprovalDialogComponent); id = target().requestId;
    reply(response);
    assert.equal(await done, 'deny');
    assert.equal(host.focus, host.editor); retiredOnce(id);
  }

  // Native Escape wins, retires the card, and does not require synthesized
  // keystrokes for any remote answer. This key is native proof only.
  done = requestApproval(runner.getUIContext(), tool, {});
  await waiting(ApprovalDialogComponent); id = target().requestId;
  host.focus.handleInput('\x1b');
  assert.equal(await done, 'deny'); retiredOnce(id); assert.equal(pending.size, 0);

  // Native remembered choices remain SDK answers; remote booleans approve only this call.
  const approval = {
    full: 'Native approval choices\nExact target', compact: 'Native approval choices',
    session: 'Approve this tool for this session',
    grants: [{ scope: 'target', label: 'Approve File', alwaysLabel: 'Always Approve File' }],
  };
  for (const [inputs, expected] of [
    [[{ type: 'action', act: 'approve' }], 'once'],
    [[{ type: 'select', key: 'choices', item: 'session' }], 'session'],
    [[{ type: 'select', key: 'choices', item: 'grant0' }], { grant: 'target', always: false }],
    [[{ type: 'select', key: 'choices', item: 'remember' }, { type: 'activate', key: 'choices', item: 'grant0' }], { grant: 'target', always: true }],
  ]) {
    const running = await handler(ctx => ctx.ui.approve(approval), ApprovalDialogComponent);
    id = target().requestId;
    for (const input of inputs) host.focus.handleNativeEvent(input);
    await running.done;
    assert.deepEqual(handlerValue, expected);
    assert.equal(host.focus, host.editor); retiredOnce(id);
  }

  const questions = [{ id: 'design', header: 'Design', question: 'Which exact design?', options: [{ label: 'Simple', description: 'Minimal', preview: 'preview text' }, { label: 'Rich' }], recommended: 1 }];
  let running = await handler(async ctx => { await tick(); return ctx.ui.askDialog(questions, { acceptImages: false }); }, AskDialogComponent);
  id = target().requestId;
  assert.equal(target().method, 'askDialog'); assert.equal(target().agentOrigin.kind, 'main');
  assert.equal(target().questions[0].options[0].preview, 'preview text');
  assert.notEqual(handlerContext.ui, runner.getUIContext(), 'native runner must use its real handler proxy');
  reply({ kind: 'submit', results: [{ id: 'design', selectedOptions: [], customInput: 'Human custom answer', note: 'Human note' }] });
  await running.done;
  assert.equal(handlerValue.results[0].customInput, 'Human custom answer');
  assert.deepEqual(handlerValue.results[0].selectedOptions, []);
  assert.equal(host.focus, host.editor); retiredOnce(id);

  // Native cancellation must use the captured Ask callback, not leave the card.
  running = await handler(ctx => ctx.ui.askDialog(questions), AskDialogComponent);
  id = target().requestId; host.focus.handleInput('\x1b'); await running.done;
  assert.equal(handlerValue, undefined); assert.equal(pending.size, 0); retiredOnce(id);

  // The owning Manager must not bypass plan handoff's explicit human boundary.
  let handoffs = 0;
  const handoff = ctx => confirmedPlanHandoff(async action => {
    if (action === 'plan_handoff_info') return { name: 'Worker', revision: 'r', needsStart: false };
    handoffs++; return { accepted: true };
  }, ctx, { summary: 'Review native-only permission boundary' });
  running = await handler(handoff);
  id = target().requestId;
  assert.equal(target().humanOnly, true);
  assert.throws(() => reply(true, 'manager'), /human_only_decision/);
  assert.equal(handoffs, 0);
  reply(true, 'human'); await running.done;
  assert.equal(handoffs, 1); assert.equal(host.focus, host.editor); retiredOnce(id);
  for (const response of [false, { cancelled: true }]) {
    running = await handler(handoff);
    id = target().requestId;
    reply(response); await running.done;
    assert.deepEqual(handlerValue, { accepted: false, cancelled: true });
    assert.equal(handoffs, 1); assert.equal(host.focus, host.editor); retiredOnce(id);
  }

  const aborted = new AbortController();
  done = runner.getUIContext().approve(approval, { signal: aborted.signal });
  await waiting(ApprovalDialogComponent); id = target().requestId;
  aborted.abort();
  assert.equal(await done, 'deny'); retiredOnce(id); assert.equal(host.focus, host.editor);

  // Native timeout and native queued steering remain denials, not approvals.
  let timedOut = false;
  done = runner.getUIContext().approve({ full: 'Timeout approval', compact: 'Timeout approval' }, { timeout: 10, onTimeout: () => { timedOut = true; } });
  await waiting(ApprovalDialogComponent); id = target().requestId;
  assert.equal(await done, 'deny'); assert.equal(timedOut, true); retiredOnce(id);
  done = requestApproval(runner.getUIContext(), tool, {});
  await waiting(ApprovalDialogComponent); id = target().requestId;
  steering.push({ role: 'user', content: 'Stop; human steering wins' });
  for (const listener of sessionListeners) listener({ type: 'queue_update', steering });
  assert.equal(await done, 'deny'); retiredOnce(id);
  assert.equal(host.focus, host.editor);
  assert.equal(pending.size, 0);

  // An ordinary headless child uses native no-op UI; patching the main
  // runner's prototype must not invent a request in that independent runner.
  const child = new ExtensionRunner([], {}, root, manager, {}, undefined, undefined, undefined, undefined,
    { kind: 'sub', id: 'headless', name: 'task', depth: 1 });
  child.initialize({}, {}, undefined, undefined, 'print');
  assert.equal(child.hasUI(), false);
  assert.equal(await child.createContext().ui.confirm('Headless', 'No dialog'), false);
  child.clearManagedTimers(); child.disposeFileFallbacks();
  assert.equal(pending.size, 0); assert.equal(providerCalls, 0); assert.equal(errors.length, 0);
  console.log('Native raw UI and handler component settlement verified');
} finally {
  nativeUi.dispose(); await workspace.dispose();
  runner.clearManagedTimers(); runner.disposeFileFallbacks();
  await manager.close();
}
