// Display-only publication of saved dependency results in The Architect's own console.
// Native sendMessage(..., { triggerTurn: false }) is display-only ONLY while the session is idle; while streaming it can
// become steering input. So results are queued and printed after the existing `agent_end` quiescence convention
// (`isIdle()` and no pending messages), never from inside a submission tool.
export const RESULT_TYPE = 'omp-architect.dependency-result';
export const REPLAY_HEADER = 'Saved analysis — not rerun';
const POLL_MS = 50;

// Everything except ordinary newline/tab is shown as visible text, so saved or model-written text cannot drive the terminal.
export function escapeTerminal(value) {
  return String(value).replace(/[\u0000-\u0008\u000b-\u001f\u007f-\u009f\u2028\u2029]/g, ch => `\\u${ch.charCodeAt(0).toString(16).padStart(4, '0')}`);
}

const SEPARATOR = '\n';
const when = ms => Number.isFinite(ms) ? new Date(ms).toISOString() : 'unknown time';
const lines = (items, indent = '  ') => items.map(item => `${indent}- ${item}`);
const cite = item => `${item.path}:${item.lineStart}${item.lineEnd !== item.lineStart ? `-${item.lineEnd}` : ''} — ${item.explanation}`;

function reviewed(request) {
  const parts = [`resolved ${request.resolvedVersions.join(', ') || 'none'}`, `declared ${request.declaredVersions.join(', ') || 'none'}`,
    `newest registry release ${request.newestVersion ?? 'unverified'} (${request.newestStatus})`];
  return `Reviewed versions: ${parts.join('; ')}`;
}

function header(title, request, completedAt) {
  return [`${title} — ${request.packageId}`, `Request ${request.requestId} · completed ${when(completedAt)}${request.contextId ? ` · context ${request.contextId}` : ''}`,
    reviewed(request), 'Saved analysis — source code and dependencies may have changed since this review.'];
}

/** Complete, unshortened text of a saved security review (findings, applicability, evidence, limitations). */
export function formatSecurityResult(request, result) {
  const out = header('Dependency security review', request, result.completedAt);
  out.push('', `Summary: ${result.summary || '(none)'}`, '', `Findings (${result.findings.length}):`);
  result.findings.forEach((finding, index) => {
    out.push(`${index + 1}. [${String(finding.severity).toUpperCase()}] ${finding.title}`,
      `   Affected: ${finding.affectedVersions || 'unspecified'} · Fixed in: ${finding.fixedVersion ?? 'unknown'} · Advisory: ${finding.advisoryId ?? 'none'} · Major: ${finding.majorVersion ?? 'unknown'}`);
    for (const evidence of finding.evidence) out.push(`   Source: ${evidence.title} — ${evidence.url}`);
    const applicability = finding.applicability;
    out.push(`   Project applicability: ${applicability.status} — ${applicability.reason}`);
    if (applicability.conditions.length) out.push('   Conditions:', ...lines(applicability.conditions, '     '));
    if (applicability.evidence.length) out.push('   Local evidence:', ...lines(applicability.evidence.map(cite), '     '));
  });
  if (!result.findings.length) out.push('  No verified findings. This is not a clean bill of health.');
  out.push('', 'Limitations:', ...(result.limitations.length ? lines(result.limitations) : ['  - none stated']));
  return out.join(SEPARATOR);
}

/** Complete text of a saved package-usage explanation. */
export function formatUsageResult(request, result) {
  const out = header('Dependency usage explanation', request, result.completedAt);
  out.push('', `Status: ${result.status} · Project role: ${result.projectRole}`, `Summary: ${result.summary}`, `Purpose: ${result.purpose}`);
  if (result.benefits.length) out.push('', 'Benefits:', ...lines(result.benefits));
  if (result.sourceEvidence.length) out.push('', 'Local evidence:', ...lines(result.sourceEvidence.map(cite)));
  if (result.paths.length) {
    out.push('', 'Dependency paths:');
    for (const item of result.paths) {
      out.push(`  ${item.packageIds.join(' -> ')}`, `    ${item.explanation}`, ...lines(item.evidence.map(cite), '    '));
    }
  }
  out.push('', 'Limitations:', ...(result.limitations.length ? lines(result.limitations) : ['  - none stated']));
  return out.join(SEPARATOR);
}

const sameIdentity = (a, b) => a.instanceId === b.instanceId && a.generation === b.generation && a.launchId === b.launchId && a.sessionId === b.sessionId;

/**
 * `identity(ctx)` returns the Architect producer tuple of the current main session (and throws when unavailable).
 * Queue items are bound to the identity at enqueue time; switch, shutdown and a replaced launch discard them.
 */
export function createResultConsole(pi, { identity }) {
  const loaders = new Map();
  const queue = new Map();
  let version = 0;

  const keyOf = (kind, requestId) => `${kind}:${requestId}`;
  const matches = (entry, item) => entry?.type === 'custom_message' && entry.customType === RESULT_TYPE
    && entry.details?.requestId === item.requestId && entry.details?.kind === item.kind;
  function branchState(ctx, item) {
    const branch = ctx.sessionManager.getBranch();
    const visible = [...branch].reverse().find(entry => entry?.type === 'message' || entry?.type === 'custom_message');
    return { any: branch.some(entry => matches(entry, item)), last: matches(visible, item) };
  }

  // True when the queue is drained (delivered or discarded); false while the session is busy.
  function attempt(ctx) {
    let current;
    try { current = identity(ctx); } catch { queue.clear(); return true; }
    for (const [key, item] of queue) if (!sameIdentity(item.identity, current)) queue.delete(key);
    if (!queue.size) return true;
    if (!ctx.isIdle() || ctx.hasPendingMessages()) return false;
    for (const [key, item] of [...queue]) {
      queue.delete(key);
      try {
        const state = branchState(ctx, item);
        // Automatic publication happens once per native branch; a replay only repeats text that is no longer the last message.
        if (item.replay ? state.last : state.any) continue;
        const text = item.replay ? `${REPLAY_HEADER}\n${item.text}` : item.text;
        pi.sendMessage({ customType: RESULT_TYPE, content: escapeTerminal(text), display: true, attribution: 'agent', details: { requestId: item.requestId, kind: item.kind } }, { triggerTurn: false });
      } catch { /* a console that cannot print keeps the saved popup readable */ }
    }
    return true;
  }

  function schedule(ctx) {
    const mine = ++version;
    const run = () => {
      if (mine !== version || !queue.size) return;
      if (!attempt(ctx)) ctx.setTimeout(run, POLL_MS);
    };
    ctx.setTimeout(run, 0);
  }

  const api = {
    registerKind(kind, load) { loaders.set(kind, load); },
    /** Queue the complete formatted text; an immediate guarded check prints it at once when already idle. */
    enqueue(ctx, { kind, requestId, text, replay = false }) {
      const item = { kind, requestId, text, replay, identity: identity(ctx) };
      const key = keyOf(kind, requestId);
      const existing = queue.get(key);
      // Duplicate queued replays/submissions for one result merge; a first-time print wins over a replay.
      queue.set(key, existing ? { ...item, replay: existing.replay && replay } : item);
      attempt(ctx);
    },
    /** Replay a saved result: the loader reads it from the app-owned directory by UUID/kind only. */
    showSaved(ctx, dir, kind, requestId, { replay = true } = {}) {
      const load = loaders.get(kind);
      if (!load) throw new Error(`No saved ${kind} result display is available`);
      api.enqueue(ctx, { kind, requestId, text: load(dir, requestId), replay });
    },
    has: (kind, requestId) => queue.has(keyOf(kind, requestId)),
    discard() { queue.clear(); version++; },
    onAgentEnd(event, ctx) {
      if (ctx.agent?.kind !== 'main' || event?.willContinue === true) return;
      if (queue.size) schedule(ctx);
    },
    onSessionChange(ctx) { if (ctx.agent?.kind === 'main') api.discard(); },
  };
  pi.on?.('agent_end', (event, ctx) => api.onAgentEnd(event, ctx));
  pi.on?.('session_switch', (_event, ctx) => api.onSessionChange(ctx));
  pi.on?.('session_shutdown', (_event, ctx) => api.onSessionChange(ctx));
  return api;
}
