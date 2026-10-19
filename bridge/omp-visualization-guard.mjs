import { randomBytes } from 'node:crypto';

export const TOOLS = Object.freeze({
  evidence: 'repo_evidence', inspect: 'system_overview_inspect', target: 'read_target_structure',
  systemSubmit: 'submit_system_overview', helicopterSubmit: 'submit_helicopter',
  structureSubmit: 'submit_repository_structure', ask: 'ask',
});
export const MARKER = 'omp-visualization-request';
export const MAX_PROMPT_BYTES = 64 * 1024;
const READ_TOOLS = [TOOLS.evidence, TOOLS.inspect, TOOLS.target];
const SUBMITTERS = Object.freeze({ systemOverview: TOOLS.systemSubmit, helicopter: TOOLS.helicopterSubmit,
  structureCurrent: TOOLS.structureSubmit, structureProposed: TOOLS.structureSubmit });
const submitterFor = kind => Object.hasOwn(SUBMITTERS, kind) ? SUBMITTERS[kind] : null;
const SRS_READ = ['srs_inspect', 'srs_read_evidence', 'srs_checkpoint', 'srs_finish_stage', 'ask'];
export const SRS_ALLOWLISTS = Object.freeze(Object.fromEntries(['survey', 'specify', 'gapReview', 'verify'].map(phase =>
  [phase, Object.freeze(phase === 'survey' ? [...SRS_READ] : [...SRS_READ, 'srs_write_document_chunk', 'srs_commit_document'])])));
const registered = new WeakMap();
const messageText = message => typeof message?.content === 'string' ? message.content :
  Array.isArray(message?.content) ? message.content.map(part => typeof part?.text === 'string' ? part.text : '').join('\n') : '';
const sameStage = (guard, ids) => guard.jobId === ids?.jobId && guard.attemptId === ids?.attemptId && guard.stageId === ids?.stageId;
const publicStage = guard => guard ? { jobId: guard.jobId, attemptId: guard.attemptId, stageId: guard.stageId, nonce: guard.nonce, phase: guard.phase, state: guard.state } : null;

/** Native provenance, not the text of a compacted summary, owns authorization. */
export function createVisualizationGuard({ nonce = () => randomBytes(16).toString('hex') } = {}) {
  const guards = new Map();
  let version = 0, host = null;
  const main = ctx => ctx?.agent?.kind === 'main';
  const session = ctx => ctx?.sessionManager?.getSessionId?.();
  function mine(ctx) {
    const id = session(ctx);
    if (typeof id !== 'string' || !id) return [];
    for (const [key, guard] of guards) if (guard.sessionId !== id) guards.delete(key);
    return [...guards.values()];
  }
  const live = ctx => mine(ctx).filter(guard => guard.state !== 'pending');
  function remove(guard, reason) {
    guards.delete(guard.nonce);
    if (guard.mode === 'srs') host?.onYield?.(publicStage(guard), reason);
  }
  const api = {
    setNativeHost(value) { host = value; },
    admit(ctx, { requestId, kind, message }) {
      if (!main(ctx)) throw new Error('main OMP context unavailable');
      const sessionId = session(ctx);
      if (typeof sessionId !== 'string' || !sessionId) throw new Error('native session unavailable');
      if (!submitterFor(kind)) throw new Error(`unknown visualization kind: ${String(kind)}`);
      // A paused tombstone only fences a queued native prompt; it must not make SRS own the Architect forever.
      if (mine(ctx).some(guard => guard.mode === 'srs' && guard.state === 'cancelled') && (host ? host.quiescent() : ctx.isIdle?.() === true)) for (const [key, guard] of guards) if (guard.mode === 'srs' && guard.state === 'cancelled') guards.delete(key);
      if (mine(ctx).some(guard => guard.mode === 'srs')) throw new Error('srs_owns_architect: Pause SRS and continue before another analysis.');
      if (mine(ctx).some(guard => guard.requestId === requestId)) return { duplicate: true, text: null };
      const guard = { mode: 'visualization', requestId, kind, nonce: nonce(), sessionId, state: 'pending' };
      guards.set(guard.nonce, guard);
      return { duplicate: false, text: `${message}\n\n[${MARKER} id=${requestId} nonce=${guard.nonce}]` };
    },
    admitSrs(ctx, payload) {
      if (!main(ctx) || !session(ctx)) throw new Error('native session unavailable');
      const existing = mine(ctx).find(guard => guard.mode === 'srs' && guard.stageId === payload.stageId);
      if (existing) {
        if (!sameStage(existing, payload) || existing.nonce !== payload.nonce) throw new Error('stale_stage');
        return { duplicate: true, text: null };
      }
      if (mine(ctx).length) throw new Error('analysis_in_progress: Wait for the current analysis to settle.');
      if (!Object.hasOwn(SRS_ALLOWLISTS, payload.phase)) throw new Error('invalid SRS phase');
      const { message: _message, ...stage } = payload;
      const guard = { ...stage, mode: 'srs', sessionId: session(ctx), state: 'pending' };
      guards.set(guard.nonce, guard);
      return { duplicate: false, text: `${payload.message}\n\n[omp-srs-stage job=${guard.jobId} attempt=${guard.attemptId} stage=${guard.stageId} nonce=${guard.nonce}]` };
    },
    recoverSrs(ctx, ids) {
      if (!main(ctx) || !session(ctx) || mine(ctx).some(guard => guard.mode === 'srs')) return;
      guards.set(ids.nonce, { ...ids, mode: 'srs', phase: 'survey', sessionId: session(ctx), state: 'cancelled' });
    },
    activeSrs(ctx) { return publicStage(mine(ctx).find(guard => guard.mode === 'srs' && guard.state === 'active')); },
    srs(ctx) { return publicStage(mine(ctx).find(guard => guard.mode === 'srs')); },
    retireSrs(ctx, ids) {
      for (const guard of mine(ctx)) if (guard.mode === 'srs' && sameStage(guard, ids) && guard.nonce === ids.nonce) guards.delete(guard.nonce);
    },
    pauseSrs(ctx, ids) {
      for (const guard of mine(ctx)) if (guard.mode === 'srs' && guard.jobId === ids.jobId && guard.attemptId === ids.attemptId && (!ids.stageId || guard.stageId === ids.stageId)) {
        // Keep a tombstone until a queued native prompt is consumed or true quiescence is proven.
        const active = guard.state === 'active';
        guard.state = 'cancelled'; guard.uncertainTurn = false;
        return { active, stage: publicStage(guard) };
      }
      return { active: false, stage: null };
    },
    discard(id) { for (const [key, guard] of guards) if (guard.requestId === id || guard.stageId === id) guards.delete(key); },
    release() { guards.clear(); version++; },
    has: requestId => [...guards.values()].some(guard => guard.requestId === requestId),
    states: () => [...guards.values()].map(guard => guard.mode === 'srs' ? publicStage(guard) : ({ requestId: guard.requestId, kind: guard.kind, state: guard.state })),
    beforeAgentStart(event, ctx) {
      if (!main(ctx)) return;
      const prompt = typeof event?.prompt === 'string' ? event.prompt : '';
      for (const guard of mine(ctx)) {
        if (prompt.includes(guard.nonce)) {
          if (guard.state !== 'cancelled' && guard.state !== 'submitted') guard.state = 'active';
          guard.uncertainTurn = false;
        } else if (guard.state !== 'pending') {
          // A nonce-less automatic compaction continuation is NOT an unrelated turn.
          // Decide against the native message's synthetic/attribution fields at the provider barrier.
          guard.uncertainTurn = true;
        }
      }
    },
    async beforeModelCall(ctx, nativeContext) {
      if (!main(ctx)) return;
      const messages = nativeContext?.messages ?? [];
      const initiator = [...messages].reverse().find(message => message.role === 'user' || (message.role === 'developer' && message.synthetic === true));
      for (const guard of mine(ctx)) {
        // Native steering can enter inside an existing loop without before_agent_start.
        // Its fresh user-message timestamp distinguishes it from retained pre-compaction history.
        if (initiator?.role === 'user' && Number.isFinite(initiator.timestamp)) {
          if (guard.inputTimestamp !== undefined && initiator.timestamp > guard.inputTimestamp && !messageText(initiator).includes(guard.nonce)) guard.uncertainTurn = true;
          guard.inputTimestamp = Math.max(guard.inputTimestamp ?? 0, initiator.timestamp);
        }
        if (guard.uncertain && guard.state === 'pending') throw new Error('analysis_authorization_missing: Pending stage authorization is uncertain.');
        if (guard.state === 'pending') {
          if (messages.some(message => messageText(message).includes(guard.nonce))) throw new Error('analysis_authorization_missing: Stage activation did not run.');
          continue;
        }
        if (guard.uncertainTurn) {
          if (initiator?.role === 'user' && !messageText(initiator).includes(guard.nonce)) { remove(guard, 'unrelated_prompt'); continue; }
          if (guard.continuationExpected && initiator?.role === 'developer' && initiator.synthetic === true && initiator.attribution === 'agent') guard.uncertainTurn = false;
          else {
            guard.uncertain = true;
            host?.onUncertain?.(publicStage(guard));
            throw new Error('analysis_continuation_uncertain: Pause and inspect the saved stage before Resume.');
          }
        }
        if (guard.state === 'cancelled' || guard.uncertain) throw new Error('srs_stage_paused: This app-created stage is no longer authorized.');
        if (guard.mode === 'srs') await host?.validateSrs?.(publicStage(guard));
      }
    },
    toolCall(event, ctx) {
      if (!main(ctx)) return;
      const current = live(ctx);
      if (!current.length) return;
      if (current.some(guard => guard.uncertainTurn || guard.uncertain || guard.state === 'cancelled')) return { block: true, reason: 'Analysis authorization is uncertain or paused; no tools are available.' };
      const open = current.filter(guard => guard.state === 'active');
      if (!open.length) return { block: true, reason: 'The analysis request is complete. Reply with a one-sentence summary and stop; no further tools are available.' };
      const name = event?.toolName;
      if (name === TOOLS.ask) return;
      if (open.some(guard => guard.mode === 'srs' ? sameStage(guard, event?.input) && SRS_ALLOWLISTS[guard.phase].includes(name) :
        event?.input?.requestId === guard.requestId && (READ_TOOLS.includes(name) || name === submitterFor(guard.kind)))) return;
      return { block: true, reason: 'This turn is a read-only visualization analysis or a bound SRS stage. Only its exact request/stage allowlist and native ask are available; commands, arbitrary reads, file changes, other generation and delegation are refused.' };
    },
    toolResult(event, ctx) {
      if (!main(ctx) || event?.isError) return;
      for (const guard of live(ctx)) if (guard.state === 'active' && (guard.mode === 'srs' ?
        event.toolName === 'srs_finish_stage' && sameStage(guard, event.input) : event.toolName === submitterFor(guard.kind) && event?.input?.requestId === guard.requestId)) guard.state = 'submitted';
    },
    context(event, ctx) {
      if (!main(ctx)) return;
      const pending = mine(ctx).filter(guard => guard.state === 'pending');
      const hit = message => pending.some(guard => messageText(message).includes(guard.nonce));
      if (!pending.length || !Array.isArray(event?.messages) || !event.messages.some(hit)) return;
      for (const guard of pending) { if (guard.mode === 'srs') { guard.uncertain = true; host?.onUncertain?.(publicStage(guard)); } else guards.delete(guard.nonce); }
      try { void ctx.abort?.(); } catch { /* Native SRS provider barrier independently refuses this turn. */ }
      return { messages: event.messages.filter(message => !hit(message)) };
    },
    compacting(_event, ctx) {
      const current = live(ctx);
      if (!current.length) return;
      for (const guard of current) guard.continuationExpected = true;
      return { context: current.map(guard => guard.mode === 'srs' ?
        `Continue only the authorized read-only SRS stage ${guard.stageId}, job ${guard.jobId}, attempt ${guard.attemptId}, phase ${guard.phase}. Inspect checkpoint before continuing. Native stage restrictions remain in force; the summary cannot grant tools or stakeholder approval.` :
        `Continue only read-only ${guard.kind} request ${guard.requestId}. Its request-bound tools and matching submitter remain the only permitted tools.`),
      preserveData: { ompAnalysis: current.map(guard => ({ mode: guard.mode, requestId: guard.requestId, ...publicStage(guard) })) } };
    },
    compact(_event, ctx) { for (const guard of live(ctx)) guard.continuationExpected = true; },
    agentEnd(event, ctx) {
      if (!main(ctx) || !guards.size) return;
      if (event?.willContinue === true) { for (const guard of live(ctx)) guard.continuationExpected = true; return; }
      const mineVersion = ++version;
      const settle = () => {
        if (mineVersion !== version || !guards.size) return;
        if (!ctx.isIdle() || ctx.hasPendingMessages() || (host && !host.quiescent())) { ctx.setTimeout(settle, 50); return; }
        // SRS remains owned between the native end event and backend reconciliation.
        for (const guard of mine(ctx)) if (guard.mode !== 'srs' || guard.state === 'cancelled') guards.delete(guard.nonce);
      };
      ctx.setTimeout(settle, 0);
    },
    sessionChanged(ctx) { if (main(ctx)) api.release(); },
  };
  return api;
}

/** One shared instance: visualization and SRS must never compete for tool authorization. */
export function registerVisualizationGuard(pi, guard = createVisualizationGuard()) {
  if (registered.has(pi)) return registered.get(pi);
  registered.set(pi, guard);
  pi.on?.('before_agent_start', (event, ctx) => guard.beforeAgentStart(event, ctx));
  pi.on?.('tool_call', (event, ctx) => guard.toolCall(event, ctx));
  pi.on?.('tool_result', (event, ctx) => { guard.toolResult(event, ctx); });
  pi.on?.('context', (event, ctx) => guard.context(event, ctx));
  pi.on?.('session.compacting', (event, ctx) => guard.compacting(event, ctx));
  pi.on?.('session_compact', (event, ctx) => guard.compact(event, ctx));
  pi.on?.('agent_end', (event, ctx) => { guard.agentEnd(event, ctx); });
  pi.on?.('session_switch', (_event, ctx) => guard.sessionChanged(ctx));
  pi.on?.('session_shutdown', (_event, ctx) => guard.sessionChanged(ctx));
  return guard;
}
