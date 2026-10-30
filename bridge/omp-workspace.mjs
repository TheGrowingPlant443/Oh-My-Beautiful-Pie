import { existsSync } from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { createInterface } from 'node:readline';
import { NATIVE_UI_CAPTURE_ERROR } from './omp-native-ui.mjs';

const PATCH = Symbol.for('omp-manager.workspace-host');
export const WORKSPACE_COMPATIBILITY_ERROR = 'OMP workspace integration requires native move, rollback, BTW and removable pre-model hooks';
/** Every app-created native prompt: no template or slash-command expansion, a dropped prompt throws, agent attribution. */
export const NATIVE_PROMPT_OPTIONS = Object.freeze({ expandPromptTemplates: false, runCommands: false, throwOnDrop: true, attribution: 'agent' });
const descriptor = (prototype, name) => {
  const value = prototype && Object.getOwnPropertyDescriptor(prototype, name);
  return value?.writable && typeof value.value === 'function' ? value : null;
};

// Include not-yet-dispatched siblings: pendingToolCalls alone misses a parallel call
// whose approval/dispatch has not started yet. Tool results, not extension events,
// establish completion of each call in the native assistant batch.
export function outstandingToolCalls(state) {
  const outstanding = new Set();
  for (const message of state.messages ?? []) {
    if (message.role === 'assistant') for (const block of message.content ?? []) {
      if (block.type === 'toolCall') outstanding.add(block.id);
    }
    if (message.role === 'toolResult') outstanding.delete(message.toolCallId);
  }
  for (const id of state.pendingToolCalls ?? []) outstanding.add(id);
  return outstanding;
}

export function installWorkspaceChanges(pi, options) {
  const prototype = pi.pi?.InteractiveMode?.prototype;
  const init = descriptor(prototype, 'init');
  const apply = descriptor(prototype, 'applyCwdChange');
  const setToolUi = descriptor(prototype, 'setToolUIContext');
  const prompt = descriptor(pi.pi?.AgentSession?.prototype, 'prompt');
  const waitPrototype = pi.pi?.WaitTool?.prototype;
  const wait = descriptor(waitPrototype, 'execute');
  if (prototype?.[PATCH]) return { ...prototype[PATCH], dispose() {} };
  let context, mode, initialized = false, disposed = false, head = null;
  let epoch = 0, active = null, idleScheduled = false, detachHook, detachEvents;
  let admissionGuard = null;
  let transportLost = false;
  const modelGates = new Set();
  const promptFlows = new Set();
  let markdownHold = null;
  function releaseMarkdownHold() {
    const hold = markdownHold;
    markdownHold = null;
    hold?.resolve();
  }
  const waits = new Map();
  const completed = new Map();
  const reconcile = new Map();
  const pendingInit = new WeakMap();
  const hubComponents = new WeakSet();
  const runnerPatches = new Map();
  const capturedRunners = new WeakSet();
  const rawUi = new WeakSet();
  const liveRunner = () => mode?.session?.extensionRunner;
  const nativeDecisionsReady = () => !disposed && !!options.nativeUi && !!setToolUi &&
    mode?.sessionManager === context?.sessionManager && capturedRunners.has(liveRunner());
  function attachRunner() {
    const runner = liveRunner();
    if (!runner || !options.nativeUi) return;
    const target = Object.getPrototypeOf(runner);
    if (runnerPatches.has(target)) return;
    const initialize = descriptor(target, 'initialize');
    const createContext = descriptor(target, 'createContext');
    if (!initialize || !createContext) return;
    const initializeWrapper = function (...args) {
      if (this === liveRunner() && !disposed && args[4] === 'tui') {
        if (options.nativeUi.installUi(args[3])) {
          rawUi.add(args[3]);
          capturedRunners.add(this);
        } else capturedRunners.delete(this);
      }
      return initialize.value.apply(this, args);
    };
    const contextWrapper = function (...args) {
      const ctx = createContext.value.apply(this, args);
      return this === liveRunner() && !disposed ? options.nativeUi.contextFacade(ctx) : ctx;
    };
    const entries = [[target, 'initialize', initialize, initializeWrapper], [target, 'createContext', createContext, contextWrapper]];
    runnerPatches.set(target, entries);
    for (const [owner, name, original, wrapper] of entries) Object.defineProperty(owner, name, { ...original, value: wrapper });
  }
  function catchUpUi() {
    if (!options.nativeUi || !mode || mode.sessionManager !== context?.sessionManager) return;
    attachRunner();
    const toolUi = mode.getToolUIContext?.();
    const runnerUi = liveRunner()?.getUIContext?.();
    // These are native base-UI getters, never a handler's ctx.ui facade.
    // Late wrapping helps direct tools, but cannot repair captured approve/Ask
    // references in existing proxies: only intercepted initialize proves that.
    if (toolUi && !rawUi.has(toolUi) && options.nativeUi.installUi(toolUi)) rawUi.add(toolUi);
    if (!toolUi && runnerUi && !options.nativeUi.installed(runnerUi) && options.nativeUi.installUi(runnerUi)) rawUi.add(runnerUi);
  }
  const nativeHubReady = () => !disposed && !!init && context?.agent?.kind === 'main' &&
    mode?.sessionManager === context.sessionManager && mode?.isInitialized === true &&
    typeof mode.showAgentHub === 'function';
  let hookedAgent;
  const root = () => initialized && context?.agent?.kind === 'main' && mode?.sessionManager === context.sessionManager;
  const identity = () => {
    const manager = mode?.sessionManager ?? context?.sessionManager;
    const file = manager?.getSessionFile();
    return { cwd: manager?.getCwd(), sessionId: manager?.getSessionId(), sessionFile: file ?? null,
      materialized: !!file && existsSync(file), workspaceEpoch: epoch };
  };
  const compatible = () => !!init && !!apply && !!prompt && root() &&
    typeof mode.session?.moveSession === 'function' && typeof mode.session?.prompt === 'function' &&
    typeof mode.sessionManager?.captureState === 'function' && typeof mode.sessionManager?.rollbackMove === 'function' &&
    typeof mode.withBtwSessionMove === 'function' && typeof mode.settings?.flush === 'function' &&
    typeof mode.reloadTodos === 'function' && typeof mode.updateEditorBorderColor === 'function' &&
    typeof mode.ui?.requestRender === 'function' && typeof mode.shutdown === 'function' &&
    typeof mode.session.agent?.addBeforeModelCallHook === 'function';
  const idle = () => root() && context.isIdle() && !context.hasPendingMessages() && !options.hasPendingDecisions();
  function nativeGuardAvailable() {
    return !disposed && root() && hookedAgent === mode?.session?.agent && typeof detachHook === 'function' &&
      typeof mode?.session?.hasPostPromptWork === 'boolean' && typeof mode.sessionManager?.getBranch === 'function' &&
      typeof mode.sessionManager?.appendCustomEntry === 'function';
  }
  function quiescence() {
    const session = mode?.session;
    const reasons = [];
    if (!root() || disposed) reasons.push('native_session_unavailable');
    if (session?.isStreaming || !context?.isIdle()) reasons.push('streaming');
    if (session?.isCompacting) reasons.push('compacting');
    if (context?.hasPendingMessages() || session?.agent?.hasQueuedMessages?.()) reasons.push('pending_messages');
    if (options.hasPendingDecisions()) reasons.push('pending_decisions');
    if (session?.hasPostPromptWork !== false) reasons.push('post_prompt_work');
    if (promptFlows.size) reasons.push('prompt_flow_unsettled');
    if (active || head) reasons.push('workspace_transition');
    if (markdownHold) reasons.push('markdown_deployment');
    return { quiescent: reasons.length === 0, reasons };
  }
  const binding = request => request.sessionId === identity().sessionId && options.isLeaseCurrent(request) &&
    ((active?.request === request && active.ready) || request.workspaceEpoch === epoch);
  const report = error => options.emit('workspace_error', { error: String(error.message ?? error), ...identity() });

  function scheduleIdle() {
    if (active && idle()) active.boundary.resolve();
    if (disposed || idleScheduled || !head || active) return;
    idleScheduled = true;
    // Native agent_end precedes prompt unwinding. This macrotask is not a tool
    // barrier; only a genuinely idle native context can enter from here.
    setImmediate(() => {
      idleScheduled = false;
      if (!disposed && head && !active && idle()) void enter(false).catch(report);
    });
  }
  function attachHooks() {
    detachHook?.(); detachEvents?.();
    detachHook = detachEvents = undefined;
    const agent = mode?.session?.agent;
    hookedAgent = agent;
    if (!agent?.addBeforeModelCallHook) return;
    detachHook = agent.addBeforeModelCallHook(async (signal, nativeContext) => {
      if (markdownHold) await markdownHold.promise;
      if (admissionGuard) await admissionGuard.promise;
      if (disposed || !root()) return;
      for (const gate of modelGates) await gate(signal, nativeContext);
      if (active) {
        active.streaming = true;
        active.boundary.resolve();
        await active.done;
      }
      else if (head) await enter(true, signal);
    });
    detachEvents = agent.subscribe(() => { scheduleIdle(); wakePureWait(); });
  }
  function wakePureWait() {
    if (options.role !== 'manager' || !head || active || !root() || head.notified) return;
    const state = mode.session.agent.state;
    const outstanding = outstandingToolCalls(state);
    if (outstanding.size !== 1) return;
    const id = [...outstanding][0];
    const running = waits.get(id);
    if (!running || running.manager !== context.sessionManager || !state.pendingToolCalls.has(id)) return;
    head.notified = true;
    // No triggerTurn: if wait finishes during native normalization/admission,
    // this remains a passive notice and cannot start an unintended new run.
    void mode.session.sendCustomMessage({ customType: 'omp-manager.workspace-pending',
      content: `A human-requested workspace transition to ${head.targetFolder} is pending. Background workers continue in their own directories; this session will relocate at the next safe tool-batch boundary.`,
      display: true, attribution: 'agent' }, { deliverAs: 'aside', triggerTurn: false }).catch(report);
  }
  async function refresh() {
    mode.updateEditorBorderColor();
    await mode.reloadTodos();
    mode.ui.requestRender();
  }
  async function restore(snapshot) {
    let problem;
    try { await mode.sessionManager.rollbackMove(snapshot); }
    catch (error) { problem = error; }
    let aligned = false;
    try { aligned = await mode.applyCwdChange(problem ? mode.sessionManager.getCwd() : snapshot.cwd); }
    catch (error) { problem ??= error; }
    if (!aligned) {
      try { aligned = await mode.applyCwdChange(mode.sessionManager.getCwd()); } catch (error) { problem ??= error; }
      problem ??= new Error('Source workspace could not be restored');
    }
    if (!aligned) await mode.shutdown();
    if (problem) throw new Error(`Workspace rollback failed: ${problem.message ?? problem}; actual cwd: ${mode.sessionManager.getCwd()}`);
    await refresh();
  }
  async function enter(streaming, signal) {
    if (!head || active || !root()) return;
    if (!compatible()) {
      const requestId = head.requestId;
      head = null;
      await options.request('workspace_failed', { requestId, error: WORKSPACE_COMPATIBILITY_ERROR, identity: identity() });
      throw new Error(WORKSPACE_COMPATIBILITY_ERROR);
    }
    const request = head;
    if (!binding(request)) { head = null; await options.request('workspace_failed', { requestId: request.requestId, error: 'Workspace session or ownership was replaced', identity: identity() }); return; }
    if (signal?.aborted) throw signal.reason;
    const task = { request, streaming, committed: false, entered: false, ready: false, readySent: false, restored: false, snapshot: null, prompt: null, done: Promise.resolve(), boundary: Promise.withResolvers() };
    active = task;
    // Commit and error behavior follow entry, even if a joining provider changes task.streaming.
    task.done = runWorkspaceTask(task, streaming);
    try { await task.done; }
    finally {
      if (active === task) {
        active = null;
      }
      if (head !== request) {
        scheduleIdle();
      }
    }
  }
  // A lost acknowledgement may have committed ownership or admitted input; never roll back underneath that work.
  const ackUncertain = (task, error) => task.readySent && !task.committed && !error.workspaceRejected;
  async function requestCommit(task, boundary) {
    task.readySent = true;
    const response = await options.request('workspace_ready', { requestId: task.request.requestId, ...identity(), boundary });
    if (response?.committed !== true) { const error = new Error('Workspace commit was rejected'); error.workspaceRejected = true; throw error; }
    task.committed = true;
  }
  async function relocate(task, streaming) {
    const { request } = task;
    if (disposed || request.cancelled || !binding(request)) throw new Error('Workspace request cancelled or replaced');
    await options.request('workspace_begin', { requestId: request.requestId });
    task.entered = true;
    task.snapshot = mode.sessionManager.captureState();
    if (path.resolve(task.snapshot.cwd) !== path.resolve(request.targetFolder)) {
      await mode.session.moveSession(request.targetFolder);
      if (!await mode.applyCwdChange(request.targetFolder)) throw new Error('Native workspace rescope failed');
      await refresh();
    }
    task.ready = true;
    if (streaming) await requestCommit(task, 'streaming');
    return true;
  }
  async function restoreAfterFailure(task, error) {
    if (!task.entered || !task.snapshot || task.committed || task.restored || ackUncertain(task, error)) return error;
    task.restored = true;
    try { await restore(task.snapshot); return error; }
    catch (error_) { return new Error(`${error.message}; ${error_.message}`); }
  }
  async function completeWorkspaceTask(task) {
    const { request } = task;
    await options.request('workspace_complete', { requestId: request.requestId, error: null, identity: identity() });
    completed.set(request.requestId, { accepted: true, ...identity() });
    while (completed.size > 32) completed.delete(completed.keys().next().value);
    if (head === request) head = null;
  }
  async function failWorkspaceTask(task, error, streaming) {
    const { request } = task;
    const uncertain = ackUncertain(task, error);
    const failure = await restoreAfterFailure(task, error);
    if (head === request) head = null;
    const diagnostic = uncertain || task.committed ? new Error(`Workspace acknowledgement uncertain; native cwd retained at ${identity().cwd}: ${failure.message}`) : failure;
    const recovery = { requestId: request.requestId, error: diagnostic.message, uncertain, committed: task.committed, identity: identity() };
    try { await options.request('workspace_failed', recovery); }
    catch { reconcile.set(request.requestId, recovery); }
    if (streaming) throw diagnostic;
    report(diagnostic);
  }
  async function runWorkspaceTask(task, streaming) {
    let operationError;
    try {
      await mode.settings.flush();
      // The native BTW controller deliberately catches operation errors and returns false. Capture them here so
      // refusal and failed relocation cannot be confused, and restore while its exclusion is still held.
      const moved = await mode.withBtwSessionMove(async () => {
        try { return await relocate(task, streaming); }
        catch (error) { operationError = await restoreAfterFailure(task, error); return false; }
      });
      if (operationError) throw operationError;
      if (!moved && task.entered) throw new Error('Native BTW workspace finalization failed');
      if (!moved) return; // BTW refusal is Pending; retry only on a native opportunity.
      // The native move/BTW barrier is released before idle prompt admission.
      if (!streaming) await requestCommit(task, 'idle');
      await completeWorkspaceTask(task);
    } catch (error) { await failWorkspaceTask(task, error, streaming); }
  }
  function cancelChange(payload) {
    if (active?.request.requestId === payload.requestId) {
      if (payload.shutdown === true) { active.request.cancelled = true; active.boundary.resolve(); }
      if (active.entered) return { cancelled: false, applying: true };
      active.request.cancelled = true;
    }
    if (head?.requestId === payload.requestId) head = null;
    return { cancelled: true };
  }
  async function planPrompt(payload) {
    const task = active;
    if (!task?.ready || task.request.cancelled || task.request.requestId !== payload.requestId || task.request.kind !== 'dependencyPlan' || !binding(task.request)) throw new Error('workspace_plan_not_admitted: No matching dependency-plan workspace boundary');
    if (task.prompt) return task.prompt;
    if (typeof payload.message !== 'string' || !payload.message.trim()) throw new Error('workspace_plan_not_admitted: Approved plan message is required');
    task.prompt = (async () => {
      if (!task.streaming && !idle()) await task.boundary.promise;
      if (disposed || transportLost || task.request.cancelled || !binding(task.request)) throw new Error('workspace_plan_not_admitted: Workspace plan admission was cancelled or replaced');
      const before = identity();
      const guard = Promise.withResolvers();
      if (!task.streaming) admissionGuard = guard;
      let accepted = false;
      try {
        const admitted = Promise.withResolvers();
        const running = mode.session.prompt(payload.message, {
          ...NATIVE_PROMPT_OPTIONS, streamingBehavior: 'steer',
          onPromptAdmitted() {
            accepted = true;
            if (before.sessionId !== identity().sessionId || before.workspaceEpoch !== epoch) {
              admitted.reject(new Error('Workspace identity changed during plan admission'));
            } else { admitted.resolve({ accepted: true }); }
            guard.resolve();
          },
        });
        Promise.resolve(running).then(() => { if (!accepted) admitted.reject(new Error('Native plan prompt was not admitted')); }, error => {
          if (!accepted) admitted.reject(error); else report(error);
        });
        return await admitted.promise;
      } catch (error) {
        if (!accepted) throw new Error(`workspace_plan_not_admitted: ${error.message ?? error}`);
        throw error;
      } finally { guard.resolve(); if (admissionGuard === guard) admissionGuard = null; }
    })();
    return task.prompt;
  }
  const captureMode = host => { mode = host; };
  const initWrapper = init && async function (...args) {
    const existing = pendingInit.get(this);
    if (existing) return existing;
    captureMode(this); // Capture before the first native init await or session_start.
    initialized = false;
    attachRunner();
    attachHooks();
    const pending = Promise.resolve().then(async () => {
      try {
        const result = await init.value.apply(this, args);
        if (mode === this) { initialized = true; attachHooks(); scheduleIdle(); }
        return result;
      } finally { pendingInit.delete(this); }
    });
    pendingInit.set(this, pending);
    return pending;
  };
  const applyWrapper = apply && async function (...args) {
    const result = await apply.value.apply(this, args);
    if (this === mode && result === true) {
      epoch++;
      options.nativeUi?.revalidate();
      options.emit('session_changed', identity());
    }
    return result;
  };
  const setToolUiWrapper = setToolUi && function (ui, hasUI, ...args) {
    if (this === mode && !disposed && hasUI && options.nativeUi) {
      attachRunner();
      if (options.nativeUi.installUi(ui)) rawUi.add(ui);
    }
    return setToolUi.value.call(this, ui, hasUI, ...args);
  };
  // Re-read the shared hold after each release: a replacement hold must keep blocking admission.
  async function awaitMarkdownHolds() {
    for (let hold = markdownHold; hold; hold = markdownHold) await hold.promise;
  }
  const promptWrapper = prompt && async function (...args) {
    const owned = this === mode?.session && !disposed;
    if (owned && markdownHold) await awaitMarkdownHolds();
    const flow = {};
    if (owned) promptFlows.add(flow);
    try { return await prompt.value.apply(this, args); }
    finally { promptFlows.delete(flow); scheduleIdle(); }
  };
  const waitWrapper = wait && async function (id, ...args) {
    const manager = this.session?.sessionManager;
    const call = { manager };
    waits.set(id, call);
    queueMicrotask(wakePureWait);
    try { return await wait.value.call(this, id, ...args); }
    finally { if (waits.get(id) === call) waits.delete(id); }
  };
  const submissions = ['promptCustomMessage', 'sendCustomMessage'].map(name => {
    const target = pi.pi?.AgentSession?.prototype;
    const original = descriptor(target, name);
    const wrapper = original && async function (...args) {
      try { return await original.value.apply(this, args); }
      finally { scheduleIdle(); }
    };
    return [target, name, original, wrapper];
  });
  const api = {
    nativeGuardAvailable,
    quiescence,
    addBeforeModelCallGate(gate) {
      modelGates.add(gate);
      return () => modelGates.delete(gate);
    },
    nativeSessionEntries() {
      if (!nativeGuardAvailable()) throw new Error('native_srs_guard_unavailable');
      return mode.sessionManager.getBranch();
    },
    appendNativeEntry(customType, data) {
      if (!nativeGuardAvailable()) throw new Error('native_srs_guard_unavailable');
      return mode.sessionManager.appendCustomEntry(customType, data);
    },
    holdMarkdown(transactionId) {
      if (typeof transactionId !== 'string' || !transactionId.trim()) throw new Error('invalid_transaction');
      if (markdownHold?.transactionId === transactionId) return { held: true, ...identity() };
      if (!nativeGuardAvailable()) throw new Error(WORKSPACE_COMPATIBILITY_ERROR);
      const status = quiescence();
      if (!status.quiescent) throw new Error(`native_session_busy: ${status.reasons.join(', ')}`);
      markdownHold = { transactionId, ...Promise.withResolvers() };
      return { held: true, ...identity() };
    },
    releaseMarkdown(transactionId) {
      if (markdownHold && markdownHold.transactionId !== transactionId) throw new Error('stale_transaction');
      releaseMarkdownHold();
      return { released: true };
    },
    identity,
    nativeUiCapabilities() {
      const capabilities = { agentHub: nativeHubReady() };
      if (options.nativeUi) {
        capabilities.nativeDecisions = nativeDecisionsReady();
        capabilities.nativeDecisionsError = capabilities.nativeDecisions ? null : NATIVE_UI_CAPTURE_ERROR;
      }
      return capabilities;
    },
    openAgentHub() {
      if (!nativeHubReady()) throw new Error('native_agent_hub_unavailable: Reopen this session with a compatible OMP version.');
      // Public TUI overlays and InteractiveMode hook-dialog references expose
      // surface ownership. Remember only components mounted by this action;
      // never inspect the selector controller's private registry.
      const ui = mode.ui;
      const visible = Array.isArray(ui?.overlayStack) ? ui.overlayStack.filter(item => !item.hidden) : [];
      if (options.hasPendingDecisions() || mode.hookSelector || mode.hookInput || mode.hookEditor ||
        (ui?.hasOverlay?.() && (!visible.length || visible.some(item => !hubComponents.has(item.component))))) {
        throw new Error('native_ui_busy');
      }
      const before = new Set(ui?.overlayStack?.map(item => item.component) ?? []);
      mode.showAgentHub({ initialSection: 'agents' });
      for (const item of ui?.overlayStack ?? []) {
        if (!before.has(item.component)) hubComponents.add(item.component);
      }
      return { accepted: true };
    },
    /**
     * Submits one prompt to the captured native session with template and command expansion disabled. While the session
     * streams it is queued as a follow-up (never a steer); while idle it starts a turn. Resolves when the whole prompt flow
     * ends, so callers wait for `onPromptAdmitted` (synchronously called once it is queued or dispatched).
     */
    submitPrompt(message, { onPromptAdmitted } = {}) {
      if (disposed || context?.agent?.kind !== 'main' || mode?.sessionManager !== context.sessionManager || mode.isInitialized !== true || typeof mode.session?.prompt !== 'function') {
        throw new Error('native_prompt_unavailable: Reopen this session with a compatible OMP version.');
      }
      return mode.session.prompt(message, { ...NATIVE_PROMPT_OPTIONS, streamingBehavior: 'followUp', onPromptAdmitted });
    },
    attachContext(ctx) {
      if (ctx.agent?.kind !== 'main') return;
      if (context && context.sessionManager.getSessionId() !== ctx.sessionManager.getSessionId()) { head = null; releaseMarkdownHold(); }
      context = ctx;
      catchUpUi();
      options.nativeUi?.revalidate();
      if (initialized && mode?.session?.agent !== hookedAgent) attachHooks();
      scheduleIdle();
    },
    async control(action, payload) {
      if (disposed) throw new Error('Workspace integration is shutting down');
      if (action === 'workspace_plan_prompt') return planPrompt(payload);
      if (action === 'cancel_workspace_change') return cancelChange(payload);
      if (!init || !apply || !prompt) throw new Error(WORKSPACE_COMPATIBILITY_ERROR);
      if (completed.has(payload.requestId)) return completed.get(payload.requestId);
      if (head?.requestId === payload.requestId) return { accepted: true, pending: true };
      if (head || active) throw new Error('Only the authorized FIFO head may be submitted');
      if (typeof payload.requestId !== 'string' || typeof payload.targetFolder !== 'string' || !path.isAbsolute(payload.targetFolder)) throw new Error('Invalid workspace request');
      head = { ...payload };
      scheduleIdle(); wakePureWait();
      return { accepted: true, pending: true };
    },
    opportunity() { scheduleIdle(); wakePureWait(); },
    disconnected() {
      transportLost = true;
      if (head && !active) head = null;
      if (active && !active.entered) active.request.cancelled = true;
      active?.boundary.resolve();
    },
    async reconnect() {
      transportLost = false;
      for (const [requestId, recovery] of reconcile) {
        try { await options.request('workspace_reconcile', recovery); reconcile.delete(requestId); }
        catch (error) { report(error); }
      }
      scheduleIdle();
    },
    async dispose() {
      disposed = true; head = null;
      releaseMarkdownHold();
      modelGates.clear();
      active?.boundary.resolve();
      if (active) await active.done.catch(report);
      detachHook?.(); detachEvents?.();
      for (const entries of runnerPatches.values()) for (const [target, name, original, wrapper] of entries) {
        if (Object.getOwnPropertyDescriptor(target, name)?.value === wrapper) Object.defineProperty(target, name, original);
      }
      for (const [target, name, original, wrapper] of [[prototype, 'init', init, initWrapper], [prototype, 'applyCwdChange', apply, applyWrapper],
        [prototype, 'setToolUIContext', setToolUi, setToolUiWrapper], [pi.pi?.AgentSession?.prototype, 'prompt', prompt, promptWrapper], [waitPrototype, 'execute', wait, waitWrapper], ...submissions]) {
        if (original && Object.getOwnPropertyDescriptor(target, name)?.value === wrapper) Object.defineProperty(target, name, original);
      }
      if (prototype?.[PATCH] === api) delete prototype[PATCH];
    },
  };
  for (const [target, name, original, wrapper] of [[prototype, 'init', init, initWrapper], [prototype, 'applyCwdChange', apply, applyWrapper],
    [prototype, 'setToolUIContext', setToolUi, setToolUiWrapper], [pi.pi?.AgentSession?.prototype, 'prompt', prompt, promptWrapper], [waitPrototype, 'execute', wait, waitWrapper], ...submissions]) {
    if (original) Object.defineProperty(target, name, { ...original, value: wrapper });
  }
  if (prototype) Object.defineProperty(prototype, PATCH, { value: api, configurable: true });
  return api;
}

export async function relocateSavedSession(request, decide) {
  const sdk = path.resolve(request.sdkDir);
  const dirs = await import(pathToFileURL(Bun.resolveSync('@oh-my-pi/pi-utils/dirs', sdk)).href);
  dirs.setProfile(request.profile || 'default');
  const { SessionManager } = await import(pathToFileURL(path.join(sdk, 'src/session/session-manager.ts')).href);
  const manager = await SessionManager.open(request.sessionFile, undefined, undefined, { throwIfMissing: true, initialCwd: request.cwd });
  const state = manager.captureState();
  const identity = () => ({ cwd: manager.getCwd(), sessionId: manager.getSessionId(), sessionFile: manager.getSessionFile() ?? null,
    materialized: !!manager.getSessionFile() && existsSync(manager.getSessionFile()), workspaceEpoch: 0 });
  try {
    await manager.moveTo(request.targetFolder);
    const moved = identity();
    if (!await decide(moved)) { await manager.rollbackMove(state); return { committed: false, identity: identity() }; }
    return { committed: true, identity: moved };
  } catch (error) {
    try { await manager.rollbackMove(state); }
    catch (error_) { throw new Error(`${error.message}; native rollback failed: ${error_.message}; actual identity: ${JSON.stringify(identity())}`); }
    throw error;
  } finally { await manager.close(); }
}

if (import.meta.main) {
  const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
  const iterator = lines[Symbol.asyncIterator]();
  try {
    const first = await iterator.next();
    if (first.done) throw new Error('Missing workspace request');
    const request = JSON.parse(first.value);
    const result = await relocateSavedSession(request, async identity => {
      process.stdout.write(JSON.stringify({ phase: 'ready', identity }) + '\n');
      const decision = await iterator.next();
      return !decision.done && JSON.parse(decision.value).commit === true;
    });
    process.stdout.write(JSON.stringify({ phase: 'complete', ...result }) + '\n');
  } catch (error) { process.stdout.write(JSON.stringify({ phase: 'failed', error: String(error.message ?? error) }) + '\n'); process.exitCode = 1; }
  finally { lines.close(); }
}
