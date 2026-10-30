import { randomBytes } from 'node:crypto';

export const TOOLS = Object.freeze({
  evidence: 'repo_evidence', inspect: 'system_overview_inspect', target: 'read_target_structure',
  systemSubmit: 'submit_system_overview', helicopterSubmit: 'submit_helicopter',
  structureSubmit: 'submit_repository_structure', ask: 'ask', monitorRead: 'monitor_snapshot_read',
});
export const MARKER = 'omp-visualization-request';
export const MAX_PROMPT_BYTES = 64 * 1024;
export const MONITOR_MARKER = 'omp-host-monitor';
export const MONITOR_GUARD_ERROR = 'native_monitor_guard_unavailable: Reopen the Architect with a compatible OMP version.';
export const MONITOR_BLOCK = 'This turn explains one frozen host-monitor snapshot. Only the exact monitor_snapshot_read request/snapshot and native ask are available; commands, file reads, file changes, refreshes, other generation and delegation are refused.';
const MONITOR_REASONS = Object.freeze({ unrelated_prompt: 'unrelatedPrompt', settled: 'settled', sessionChanged: 'sessionChanged', uncertainContinuation: 'uncertainContinuation' });
const READ_TOOLS = new Set([TOOLS.evidence, TOOLS.inspect, TOOLS.target]);
const SUBMITTERS = Object.freeze({ systemOverview: TOOLS.systemSubmit, helicopter: TOOLS.helicopterSubmit,
  structureCurrent: TOOLS.structureSubmit, structureProposed: TOOLS.structureSubmit });
const submitterFor = kind => Object.hasOwn(SUBMITTERS, kind) ? SUBMITTERS[kind] : null;
const SRS_READ = ['srs_inspect', 'srs_read_evidence', 'srs_checkpoint', 'srs_finish_stage', 'ask'];
export const SRS_ALLOWLISTS = Object.freeze(Object.fromEntries(['survey', 'specify', 'gapReview', 'verify'].map(phase =>
  [phase, Object.freeze(phase === 'survey' ? [...SRS_READ] : [...SRS_READ, 'srs_write_document_chunk', 'srs_commit_document'])])));
const registered = new WeakMap();
function messageText(message) {
  if (typeof message?.content === 'string') return message.content;
  if (Array.isArray(message?.content)) return message.content.map(part => typeof part?.text === 'string' ? part.text : '').join('\n');
  return '';
}
const sameStage = (guard, ids) => guard.jobId === ids?.jobId && guard.attemptId === ids?.attemptId && guard.stageId === ids?.stageId;
const publicStage = guard => guard ? { jobId: guard.jobId, attemptId: guard.attemptId, stageId: guard.stageId, nonce: guard.nonce, phase: guard.phase, state: guard.state } : null;
const publicMonitor = guard => guard ? { requestId: guard.requestId, snapshotId: guard.snapshotId, nonce: guard.nonce, state: guard.state } : null;
function publicOf(guard) {
  if (guard.mode === 'srs') return publicStage(guard);
  if (guard.mode === 'monitor') return publicMonitor(guard);
  return { requestId: guard.requestId, kind: guard.kind, state: guard.state };
}

function guardState(guard) {
  if (guard.mode === 'srs') return publicStage(guard);
  if (guard.mode === 'monitor') return { mode: 'monitor', requestId: guard.requestId, snapshotId: guard.snapshotId, state: guard.state };
  return { requestId: guard.requestId, kind: guard.kind, state: guard.state };
}

function permitsTool(guard, name, input) {
  if (guard.mode === 'srs') return sameStage(guard, input) && SRS_ALLOWLISTS[guard.phase].includes(name);
  if (guard.mode === 'monitor') return name === TOOLS.monitorRead && input?.requestId === guard.requestId && input?.snapshotId === guard.snapshotId;
  return input?.requestId === guard.requestId && (READ_TOOLS.has(name) || name === submitterFor(guard.kind));
}

function continuationContext(guard) {
  if (guard.mode === 'srs') return `Continue only the authorized read-only SRS stage ${guard.stageId}, job ${guard.jobId}, attempt ${guard.attemptId}, phase ${guard.phase}. Inspect checkpoint before continuing. Native stage restrictions remain in force; the summary cannot grant tools or stakeholder approval.`;
  if (guard.mode === 'monitor') return `Continue only the read-only explanation of frozen host-monitor snapshot ${guard.snapshotId}, request ${guard.requestId}. Only monitor_snapshot_read for exactly that request and snapshot, and native ask, remain permitted; the summary cannot grant tools or live reads.`;
  return `Continue only read-only ${guard.kind} request ${guard.requestId}. Its request-bound tools and matching submitter remain the only permitted tools.`;
}

function submittedBy(guard, event) {
  if (guard.state !== 'active' || guard.mode === 'monitor') return false;
  if (guard.mode === 'srs') return event.toolName === 'srs_finish_stage' && sameStage(guard, event.input);
  return event.toolName === submitterFor(guard.kind) && event?.input?.requestId === guard.requestId;
}

function noteInitiator(guard, initiator) {
  // Native steering can enter inside an existing loop without before_agent_start.
  // Its fresh user-message timestamp distinguishes it from retained pre-compaction history.
  if (initiator?.role !== 'user' || !Number.isFinite(initiator.timestamp)) return;
  if (guard.inputTimestamp !== undefined && initiator.timestamp > guard.inputTimestamp && !messageText(initiator).includes(guard.nonce)) guard.uncertainTurn = true;
  guard.inputTimestamp = Math.max(guard.inputTimestamp ?? 0, initiator.timestamp);
}

function assertPendingInactive(guard, messages) {
  if (guard.uncertain) throw new Error('analysis_authorization_missing: Pending stage authorization is uncertain.');
  if (messages.some(message => messageText(message).includes(guard.nonce))) throw new Error('analysis_authorization_missing: Stage activation did not run.');
}

/** Native provenance, not the text of a compacted summary, owns authorization. */
export function createVisualizationGuard({ nonce = () => randomBytes(16).toString('hex') } = {}) {
  const guards = new Map();
  let version = 0, host = null;
  const main = ctx => ctx?.agent?.kind === 'main';
  const session = ctx => ctx?.sessionManager?.getSessionId?.();
  function mine(ctx) {
    const id = session(ctx);
    if (typeof id !== 'string' || !id) return [];
    for (const [key, guard] of guards) if (guard.sessionId !== id) { guards.delete(key); retireMonitor(guard, 'sessionChanged'); }
    return [...guards.values()];
  }
  const live = ctx => mine(ctx).filter(guard => guard.state !== 'pending');
  // The host hears about a monitor guard's end exactly once: exact request+snapshot, never a UI idle guess.
  function retireMonitor(guard, reason) {
    if (guard.mode !== 'monitor' || guard.notified) return;
    guard.notified = true;
    host?.onMonitorRetired?.(publicMonitor(guard), MONITOR_REASONS[reason] ?? reason);
  }
  function uncertain(guard) {
    guard.uncertain = true;
    if (guard.mode === 'srs') host?.onUncertain?.(publicStage(guard));
    else if (guard.mode === 'monitor') { retireMonitor(guard, 'uncertainContinuation'); }
  }
  function remove(guard, reason) {
    guards.delete(guard.nonce);
    if (guard.mode === 'srs') host?.onYield?.(publicStage(guard), reason);
    else retireMonitor(guard, reason);
  }
  // False when an unrelated human turn ended this guard; throws when the continuation cannot be attributed.
  function settleUncertainTurn(guard, initiator) {
    if (initiator?.role === 'user' && !messageText(initiator).includes(guard.nonce)) { remove(guard, 'unrelated_prompt'); return false; }
    if (guard.continuationExpected && initiator?.role === 'developer' && initiator.synthetic === true && initiator.attribution === 'agent') { guard.uncertainTurn = false; return true; }
    uncertain(guard);
    throw new Error('analysis_continuation_uncertain: Pause and inspect the saved stage before Resume.');
  }
  async function authorize(guard) {
    if (guard.state === 'cancelled' || guard.uncertain) throw new Error('srs_stage_paused: This app-created stage is no longer authorized.');
    if (guard.mode === 'srs') await host?.validateSrs?.(publicStage(guard));
    // Fail closed: a monitor turn never reaches the provider without the host's exact current validation.
    else if (guard.mode === 'monitor') {
      if (typeof host?.validateMonitor !== 'function') throw new Error(MONITOR_GUARD_ERROR);
      await host.validateMonitor(publicMonitor(guard));
    }
  }
  // A paused tombstone only fences a queued native prompt; it must not make SRS own the Architect forever.
  function clearPausedSrs(ctx) {
    if (mine(ctx).some(guard => guard.mode === 'srs' && guard.state === 'cancelled') && (host ? host.quiescent() : ctx.isIdle?.() === true)) for (const [key, guard] of guards) if (guard.mode === 'srs' && guard.state === 'cancelled') guards.delete(key);
  }
  const api = {
    setNativeHost(value) { host = value; },
    admit(ctx, { requestId, kind, message }) {
      if (!main(ctx)) throw new Error('main OMP context unavailable');
      const sessionId = session(ctx);
      if (typeof sessionId !== 'string' || !sessionId) throw new Error('native session unavailable');
      if (!submitterFor(kind)) throw new Error(`unknown visualization kind: ${String(kind)}`);
      clearPausedSrs(ctx);
      if (mine(ctx).some(guard => guard.mode === 'srs')) throw new Error('srs_owns_architect: Pause SRS and continue before another analysis.');
      if (mine(ctx).some(guard => guard.mode === 'visualization' && guard.requestId === requestId)) return { duplicate: true, text: null };
      if (mine(ctx).some(guard => guard.mode === 'monitor')) throw new Error('analysis_in_progress: Wait for the host-monitor explanation to settle.');
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
      const { message, ...stage } = payload;
      const guard = { ...stage, mode: 'srs', sessionId: session(ctx), state: 'pending' };
      guards.set(guard.nonce, guard);
      return { duplicate: false, text: `${message}\n\n[omp-srs-stage job=${guard.jobId} attempt=${guard.attemptId} stage=${guard.stageId} nonce=${guard.nonce}]` };
    },
    /** A frozen host-monitor explanation: only its exact reader and native Ask run; there is no submitter. */
    admitMonitor(ctx, { requestId, snapshotId, message }) {
      if (!main(ctx)) throw new Error('main OMP context unavailable');
      const sessionId = session(ctx);
      if (typeof sessionId !== 'string' || !sessionId) throw new Error('native session unavailable');
      if (typeof requestId !== 'string' || !requestId || typeof snapshotId !== 'string' || !snapshotId || typeof message !== 'string') throw new Error('invalid monitor admission');
      clearPausedSrs(ctx);
      if (mine(ctx).some(guard => guard.mode === 'srs')) throw new Error('srs_owns_architect: Pause SRS and continue before another analysis.');
      const existing = mine(ctx).find(guard => guard.mode === 'monitor' && guard.requestId === requestId);
      if (existing) {
        if (existing.snapshotId !== snapshotId) throw new Error('stale_monitor_request');
        return { duplicate: true, text: null };
      }
      if (mine(ctx).length) throw new Error('analysis_in_progress: Wait for the current analysis to settle.');
      const guard = { mode: 'monitor', requestId, snapshotId, nonce: nonce(), sessionId, state: 'pending' };
      guards.set(guard.nonce, guard);
      return { duplicate: false, text: `${message}\n\n[${MONITOR_MARKER} request=${requestId} snapshot=${snapshotId} nonce=${guard.nonce}]` };
    },
    monitor(ctx) { return publicMonitor(mine(ctx).find(guard => guard.mode === 'monitor')); },
    activeMonitor(ctx) { return publicMonitor(mine(ctx).find(guard => guard.mode === 'monitor' && guard.state === 'active')); },
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
    release() {
      for (const guard of guards.values()) retireMonitor(guard, 'sessionChanged');
      guards.clear();
      version++;
    },
    has: requestId => [...guards.values()].some(guard => guard.requestId === requestId),
    states: () => [...guards.values()].map(guardState),
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
        noteInitiator(guard, initiator);
        if (guard.state === 'pending') { assertPendingInactive(guard, messages); continue; }
        if (guard.uncertainTurn && !settleUncertainTurn(guard, initiator)) continue;
        await authorize(guard);
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
      if (open.some(guard => permitsTool(guard, name, event?.input))) return;
      if (open.some(guard => guard.mode === 'monitor')) return { block: true, reason: MONITOR_BLOCK };
      return { block: true, reason: 'This turn is a read-only visualization analysis or a bound SRS stage. Only its exact request/stage allowlist and native ask are available; commands, arbitrary reads, file changes, other generation and delegation are refused.' };
    },
    toolResult(event, ctx) {
      if (!main(ctx) || event?.isError) return;
      for (const guard of live(ctx)) {
        if (submittedBy(guard, event)) guard.state = 'submitted';
      }
    },
    context(event, ctx) {
      if (!main(ctx)) return;
      const pending = mine(ctx).filter(guard => guard.state === 'pending');
      const hit = message => pending.some(guard => messageText(message).includes(guard.nonce));
      if (!pending.length || !Array.isArray(event?.messages) || !event.messages.some(hit)) return;
      for (const guard of pending) { if (guard.mode === 'srs' || guard.mode === 'monitor') uncertain(guard); else guards.delete(guard.nonce); }
      try { void ctx.abort?.(); } catch { /* Native SRS provider barrier independently refuses this turn. */ }
      return { messages: event.messages.filter(message => !hit(message)) };
    },
    compacting(_event, ctx) {
      const current = live(ctx);
      if (!current.length) return;
      for (const guard of current) guard.continuationExpected = true;
      return { context: current.map(continuationContext),
        preserveData: { ompAnalysis: current.map(guard => ({ mode: guard.mode, requestId: guard.requestId, ...publicOf(guard) })) } };
    },
    compact(_event, ctx) { for (const guard of live(ctx)) guard.continuationExpected = true; },
    agentEnd(event, ctx) {
      if (!main(ctx) || !guards.size) return;
      if (event?.willContinue === true) {
        for (const guard of live(ctx)) {
          guard.continuationExpected = true;
        }
        return;
      }
      const mineVersion = ++version;
      const settle = () => {
        if (mineVersion !== version || !guards.size) return;
        if (!ctx.isIdle() || ctx.hasPendingMessages() || (host && !host.quiescent())) { ctx.setTimeout(settle, 50); return; }
        // SRS remains owned between the native end event and backend reconciliation.
        for (const guard of mine(ctx)) { if (guard.mode === 'monitor') remove(guard, 'settled'); else if (guard.mode !== 'srs' || guard.state === 'cancelled') guards.delete(guard.nonce); }
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
