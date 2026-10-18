import { randomBytes } from 'node:crypto';

// Analysis-only guard for app-created visualization turns (System overview / Repository structure). The app submits a
// backend-built prompt through the human-only native control `project_visualization_prompt`; this module keeps that
// prompt's random nonce as a session-bound guard. While a turn that carries the nonce runs, only request-bound read
// tools, the matching submitter and native Ask are available. Unrelated Architect turns are never touched.
export const TOOLS = Object.freeze({
  evidence: 'repo_evidence',
  inspect: 'system_overview_inspect',
  target: 'read_target_structure',
  systemSubmit: 'submit_system_overview',
  structureSubmit: 'submit_repository_structure',
  ask: 'ask',
});
export const MARKER = 'omp-visualization-request';
export const MAX_PROMPT_BYTES = 64 * 1024;
const POLL_MS = 50;

const READ_TOOLS = [TOOLS.evidence, TOOLS.inspect, TOOLS.target];
const submitterFor = kind => (kind === 'systemOverview' ? TOOLS.systemSubmit : TOOLS.structureSubmit);
const marker = (requestId, nonce) => `[${MARKER} id=${requestId} nonce=${nonce}]`;

function messageText(message) {
  const content = message?.content;
  if (typeof content === 'string') return content;
  return Array.isArray(content) ? content.map(part => (typeof part?.text === 'string' ? part.text : '')).join('\n') : '';
}

/**
 * Guard states: `pending` (admitted, its prompt not yet in a running turn), `active` (the turn carrying the nonce runs),
 * `submitted` (the matching result was saved: the generation is finished and every further tool is refused).
 */
export function createVisualizationGuard({ nonce = () => randomBytes(16).toString('hex') } = {}) {
  const guards = new Map();
  let version = 0;
  const main = ctx => ctx?.agent?.kind === 'main';
  const session = ctx => ctx?.sessionManager?.getSessionId?.();
  // A guard belongs to the native session it was admitted in; any other session never inherits it.
  function mine(ctx) {
    const id = session(ctx);
    for (const [key, guard] of guards) if (guard.sessionId !== id) guards.delete(key);
    return [...guards.values()];
  }
  const live = ctx => mine(ctx).filter(guard => guard.state === 'active' || guard.state === 'submitted');

  const api = {
    /** Registers the guard and returns the exact text to submit. A repeated admission of one request id is a duplicate (never resent). */
    admit(ctx, { requestId, kind, message }) {
      if (!main(ctx)) throw new Error('main OMP context unavailable');
      const sessionId = session(ctx);
      if (typeof sessionId !== 'string' || !sessionId) throw new Error('native session unavailable');
      if (mine(ctx).some(guard => guard.requestId === requestId)) return { duplicate: true, text: null };
      const guard = { requestId, kind, nonce: nonce(), sessionId, state: 'pending' };
      guards.set(guard.nonce, guard);
      return { duplicate: false, text: `${message}\n\n${marker(requestId, guard.nonce)}` };
    },
    /** Removes a request's guard (for a prompt the native session refused before admission). */
    discard(requestId) { for (const [key, guard] of guards) if (guard.requestId === requestId) guards.delete(key); },
    release() { guards.clear(); version++; },
    has: requestId => [...guards.values()].some(guard => guard.requestId === requestId),
    states: () => [...guards.values()].map(({ requestId, kind, state }) => ({ requestId, kind, state })),

    /** `before_agent_start`: the turn that carries a pending nonce activates its guard; a finished one is released. */
    beforeAgentStart(event, ctx) {
      if (!main(ctx)) return undefined;
      const prompt = typeof event?.prompt === 'string' ? event.prompt : '';
      for (const guard of mine(ctx)) {
        if (prompt.includes(guard.nonce)) guard.state = 'active';
        else if (guard.state !== 'pending') guards.delete(guard.nonce);
      }
      return undefined;
    },

    /** `tool_call`: while a visualization turn runs, only its read tools, matching submitter and native Ask are allowed. */
    toolCall(event, ctx) {
      if (!main(ctx)) return undefined;
      const current = live(ctx);
      if (!current.length) return undefined;
      const open = current.filter(guard => guard.state === 'active');
      if (!open.length) return { block: true, reason: 'The visualization request is complete. Reply with a one-sentence summary and stop; no further tools are available.' };
      const name = event?.toolName;
      if (name === TOOLS.ask) return undefined;
      const requestId = event?.input?.requestId;
      if (open.some(guard => requestId === guard.requestId && (READ_TOOLS.includes(name) || name === submitterFor(guard.kind)))) return undefined;
      const submitters = [...new Set(open.map(guard => submitterFor(guard.kind)))].join(' or ');
      return {
        block: true,
        reason: `This turn is a read-only visualization analysis. Only ${READ_TOOLS.join(', ')} (with requestId ${open.map(guard => guard.requestId).join(' or ')}), ${submitters} and ask are available; commands, file changes and delegation are refused.`,
      };
    },

    /** `tool_result`: a saved matching result ends the generation. */
    toolResult(event, ctx) {
      if (!main(ctx) || event?.isError) return;
      for (const guard of live(ctx)) {
        if (guard.state === 'active' && event?.toolName === submitterFor(guard.kind) && event?.input?.requestId === guard.requestId) guard.state = 'submitted';
      }
    },

    /**
     * `context` runs before every provider request. A pending nonce already inside the model context means the
     * activation hook did not run for this generation: fail closed by aborting the turn and withholding that message.
     */
    context(event, ctx) {
      if (!main(ctx)) return undefined;
      const pending = mine(ctx).filter(guard => guard.state === 'pending');
      if (!pending.length || !Array.isArray(event?.messages)) return undefined;
      const hit = message => pending.some(guard => messageText(message).includes(guard.nonce));
      if (!event.messages.some(hit)) return undefined;
      for (const guard of pending) guards.delete(guard.nonce);
      try { void ctx.abort?.(); } catch { /* the stripped context below is the enforcement */ }
      return { messages: event.messages.filter(message => !hit(message)) };
    },

    /** `agent_end`: release the guards once the native session is truly quiescent (idle with nothing queued). */
    agentEnd(event, ctx) {
      if (!main(ctx) || event?.willContinue === true || !guards.size) return;
      const mineVersion = ++version;
      const settle = () => {
        if (mineVersion !== version || !guards.size) return;
        if (!ctx.isIdle() || ctx.hasPendingMessages()) { ctx.setTimeout(settle, POLL_MS); return; }
        guards.clear();
      };
      ctx.setTimeout(settle, 0);
    },

    /** Session switch/shutdown/replacement: nothing carries over. */
    sessionChanged(ctx) { if (main(ctx)) api.release(); },
  };
  return api;
}

/** Wires one guard to the native extension events. */
export function registerVisualizationGuard(pi, guard = createVisualizationGuard()) {
  pi.on?.('before_agent_start', (event, ctx) => guard.beforeAgentStart(event, ctx));
  pi.on?.('tool_call', (event, ctx) => guard.toolCall(event, ctx));
  pi.on?.('tool_result', (event, ctx) => { guard.toolResult(event, ctx); });
  pi.on?.('context', (event, ctx) => guard.context(event, ctx));
  pi.on?.('agent_end', (event, ctx) => { guard.agentEnd(event, ctx); });
  pi.on?.('session_switch', (_event, ctx) => guard.sessionChanged(ctx));
  pi.on?.('session_shutdown', (_event, ctx) => guard.sessionChanged(ctx));
  return guard;
}
