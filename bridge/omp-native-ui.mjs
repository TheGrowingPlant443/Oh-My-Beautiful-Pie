import { AsyncLocalStorage } from 'node:async_hooks';

export const NATIVE_UI_CAPTURE_ERROR = 'native_ui_capture_unavailable: Reopen this session with a compatible OMP version.';
const METHODS = ['approve', 'select', 'confirm', 'input', 'editor', 'askDialog', 'custom'];
const PATCH = Symbol('omp-native-ui.raw');

// Only JSON prompt data crosses the bridge. Signals, callbacks and custom
// component factories stay on the original native invocation.
function immutable(value) {
  if (Array.isArray(value)) return Object.freeze(value.map(immutable));
  if (value && typeof value === 'object') {
    const result = {};
    for (const [key, item] of Object.entries(value)) {
      if (item !== undefined && typeof item !== 'function' && typeof item !== 'symbol') result[key] = immutable(item);
    }
    return Object.freeze(result);
  }
  return value;
}
function cancellation(method) { return method === 'approve' || method === 'confirm' ? false : undefined; }
function promptFields(method, args, options) {
  const fields = {};
  if (method === 'approve') fields.prompt = args[0];
  if (method === 'select') { fields.title = args[0]; fields.options = args[1]; }
  if (method === 'confirm') { fields.title = args[0]; fields.message = args[1]; }
  if (method === 'input') { fields.title = args[0]; fields.placeholder = args[1]; }
  if (method === 'editor') { fields.title = args[0]; fields.prefill = args[1]; }
  if (method === 'askDialog') { fields.questions = args[0]; fields.acceptImages = options.acceptImages ?? false; fields.supportsChat = true; }
  if (method === 'custom') fields.title = 'Native custom dialog';
  for (const key of ['timeout', 'initialIndex', 'disabledIndices', 'outline', 'helpText', 'selectionMarker', 'checkedIndices', 'markableCount']) {
    if (options[key] !== undefined) fields[key] = options[key];
  }
  return fields;
}

// The caller owns the one authoritative pending map; this module only installs
// capture and the shared synchronous native/human/Manager settlement boundary.
export function installNativeDecisions({ pending, identity, emit, opportunity, nativeAnswer, srs }) {
  const origins = new AsyncLocalStorage();
  const patches = new Set();
  let disposed = false;
  function sameSession(request) {
    const current = identity();
    return request.instanceId === current.instanceId && request.generation === current.generation &&
      request.sessionId === current.sessionId && request.workspaceEpoch === current.workspaceEpoch;
  }
  function captureIdentity() {
    const current = identity();
    const origin = origins.getStore();
    if (origin && (origin.sessionId !== current.sessionId || origin.workspaceEpoch !== current.workspaceEpoch)) throw new Error('stale_generation');
    return immutable({ ...current, createdAt: Date.now(), agentOrigin: origin?.agentOrigin ?? { kind: 'unattributed', id: null, name: null } });
  }
  const installed = ui => !!ui && patches.has(Object.getOwnPropertyDescriptor(ui, PATCH)?.value);
  function installUi(ui) {
    if (disposed || !ui) return false;
    if (installed(ui)) return METHODS.every(method => typeof ui[method] === 'function');
    const originals = new Map();
    for (const method of METHODS) {
      const descriptor = Object.getOwnPropertyDescriptor(ui, method);
      if (!descriptor || typeof descriptor.value !== 'function' || !descriptor.writable) return false;
      originals.set(method, descriptor);
    }
    const patch = { ui, originals, wrappers: new Map() };
    for (const [method, descriptor] of originals) {
      const wrapper = (...args) => {
        if (disposed) return descriptor.value.apply(ui, args);
        const optionsIndex = ['approve', 'askDialog', 'custom'].includes(method) ? 1 : 2;
        const options = args[optionsIndex] ?? {};
        if (options.signal?.aborted) return Promise.resolve(cancellation(method));
        let captured;
        try { captured = captureIdentity(); } catch (error) { return Promise.reject(error); }
        const requestId = crypto.randomUUID();
        const srsHooks = method === 'askDialog' && ['main', 'unattributed'].includes(captured.agentOrigin.kind) ? srs?.() : null;
        const mapped = !!srsHooks?.active();
        const begin = () => {
          if (disposed || !sameSession(captured)) throw new Error('stale_generation');
          const effective = mapped ? { ...options, ompManagerHumanOnly: true, acceptImages: false } : options;
          // Undefined disables the SDK's recommendation timeout for this invocation only.
          if (mapped) delete effective.timeout;
          const request = immutable({ ...captured, requestId, method, humanOnly: effective.ompManagerHumanOnly === true, ...promptFields(method, args, effective) });
          const localAbort = new AbortController();
          const nativeArgs = [...args];
          const { ompManagerHumanOnly: _humanOnly, ...nativeOptions } = effective;
          nativeArgs[optionsIndex] = { ...nativeOptions, signal: options.signal ? AbortSignal.any([options.signal, localAbort.signal]) : localAbort.signal };
          return new Promise((resolve, reject) => {
            let settled = false;
            const onAbort = () => finish('native', cancellation(method));
            const finish = (source, value, error) => {
              if (settled) return false;
              // Claim synchronously, before receipt I/O or any native/Manager race.
              settled = true;
              options.signal?.removeEventListener('abort', onAbort);
              pending.delete(requestId);
              if (source !== 'native') localAbort.abort();
              const release = (answer, problem) => {
                emit('decision_resolved', { requestId, source, sessionId: request.sessionId });
                opportunity();
                if (problem) reject(problem); else resolve(answer);
              };
              if (!mapped) { release(value, error); return true; }
              void (async () => {
                let answer = value, problem = error;
                try {
                  let reason = source === 'retired' ? 'retired' : options.signal?.aborted ? 'aborted' : error ? 'failed' :
                    value?.kind === 'chat' ? 'chat' : value?.results?.some(result => result.timedOut === true) ? 'timeout' :
                    value?.kind !== 'submit' ? 'cancelled' : null;
                  if (reason) {
                    await srsHooks.retired(requestId, reason);
                    if (reason === 'timeout') answer = undefined;
                  } else {
                    if (!['human', 'native'].includes(source)) throw new Error('human_only_decision');
                    if (!sameSession(request)) throw new Error('stale_generation');
                    answer = nativeAnswer(request, value);
                    const receipt = await srsHooks.receipt(requestId, source === 'human' ? 'human' : 'nativeHuman', answer.results);
                    if (typeof receipt?.receiptId !== 'string' || !receipt.receiptId) throw new Error('srs_answer_receipt_not_persisted');
                  }
                } catch (failure) {
                  problem = failure;
                  localAbort.abort();
                  try { await srsHooks.retired(requestId, 'failed'); } catch { /* Failed persistence remains unanswered; native tool rejects. */ }
                }
                release(answer, problem);
              })();
              return true;
            };
            pending.set(requestId, { request, finish, signal: options.signal, advisory: false });
            options.signal?.addEventListener('abort', onAbort, { once: true });
            if (options.signal?.aborted) { onAbort(); return; }
            emit('needs_decision', request);
            try {
              Promise.resolve(descriptor.value.apply(ui, nativeArgs)).then(value => finish('native', value), error => finish('native', undefined, error));
            } catch (error) { finish('native', undefined, error); }
          });
        };
        // Mapping is durable before the native dialog can be answered. No timeout runs while mapping.
        return mapped ? Promise.resolve(srsHooks.map(requestId, args[0])).then(begin) : begin();
      };
      patch.wrappers.set(method, wrapper);
      Object.defineProperty(ui, method, { ...descriptor, value: wrapper });
    }
    patches.add(patch);
    Object.defineProperty(ui, PATCH, { value: patch, configurable: true });
    return true;
  }
  function contextFacade(ctx) {
    if (!ctx.hasUI || !ctx.ui) return ctx;
    const current = identity();
    const agent = ctx.agent;
    const origin = immutable({ sessionId: ctx.sessionManager.getSessionId(), workspaceEpoch: current.workspaceEpoch,
      agentOrigin: agent && typeof agent.kind === 'string' ? { kind: agent.kind, id: agent.id ?? null, name: agent.name ?? null } : { kind: 'unattributed', id: null, name: null } });
    const methods = new Map();
    const ui = new Proxy(ctx.ui, { get(target, property) {
      const value = Reflect.get(target, property, target);
      if (!METHODS.includes(property) || typeof value !== 'function') return typeof value === 'function' ? value.bind(target) : value;
      if (!methods.has(property)) methods.set(property, (...args) => origins.run(origin, () => value.apply(target, args)));
      return methods.get(property);
    } });
    return Object.create(ctx, { ui: { value: ui, enumerable: true, configurable: true } });
  }
  function revalidate() {
    for (const item of [...pending.values()]) {
      if (!sameSession(item.request)) item.finish('retired', cancellation(item.request.method));
    }
  }
  function respond(payload) {
    const item = pending.get(payload.requestId);
    if (!item) throw new Error('decision_already_resolved');
    if (!sameSession(item.request)) { item.finish('retired', cancellation(item.request.method)); throw new Error('stale_generation'); }
    if (payload.generation !== item.request.generation || payload.sessionId !== item.request.sessionId ||
      payload.workspaceEpoch !== item.request.workspaceEpoch || (payload.instanceId !== undefined && payload.instanceId !== item.request.instanceId)) throw new Error('stale_generation');
    if (item.signal?.aborted) { item.finish('native', cancellation(item.request.method)); throw new Error('decision_already_resolved'); }
    if (item.request.humanOnly && payload.source !== 'human') throw new Error('human_only_decision');
    const answer = nativeAnswer(item.request, payload.response);
    if (!item.finish(payload.source === 'human' ? 'human' : 'manager', answer)) throw new Error('decision_already_resolved');
    return { accepted: true };
  }
  return { installUi, installed, contextFacade, captureIdentity, revalidate, respond,
    dispose() {
      disposed = true;
      for (const item of [...pending.values()]) item.finish('retired', cancellation(item.request.method));
      for (const patch of patches) {
        for (const [method, descriptor] of patch.originals) {
          if (Object.getOwnPropertyDescriptor(patch.ui, method)?.value === patch.wrappers.get(method)) Object.defineProperty(patch.ui, method, descriptor);
        }
        if (Object.getOwnPropertyDescriptor(patch.ui, PATCH)?.value === patch) delete patch.ui[PATCH];
      }
      patches.clear();
    },
  };
}
