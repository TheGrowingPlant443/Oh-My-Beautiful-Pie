import { randomUUID } from 'node:crypto';
import { registerVisualizationGuard, SRS_ALLOWLISTS } from './omp-visualization-guard.mjs';
import { installHostMonitorBridge } from './omp-host-monitor.mjs';
export { SRS_ALLOWLISTS } from './omp-visualization-guard.mjs';

export const MAX_SRS_FRAME = 1024 * 1024;
export const MAX_SRS_CHUNK = 64 * 1024;
export const SRS_MARKER = 'omp-srs-stage';
export const SRS_GUARD_ERROR = 'native_srs_guard_unavailable: Reopen the Architect with a compatible OMP version.';
export const SRS_PHASES = ['survey', 'specify', 'gapReview', 'verify'];
export const SRS_DOCUMENT_TYPES = ['master', 'requirement', 'domain', 'flow', 'interface', 'glossary', 'uiObservations', 'structureObservations', 'traceability', 'coverage', 'questions'];
export const SRS_TOOL_NAMES = ['srs_inspect', 'srs_read_evidence', 'srs_write_document_chunk', 'srs_commit_document', 'srs_checkpoint', 'srs_finish_stage', 'srs_delegate'];
const sameStage = (a, b) => !!a && !!b && a.jobId === b.jobId && a.attemptId === b.attemptId && a.stageId === b.stageId;
const stageIds = stage => ({ jobId: stage.jobId, attemptId: stage.attemptId, stageId: stage.stageId, nonce: stage.nonce });
const bounded = value => {
  if (Buffer.byteLength(JSON.stringify(value)) > MAX_SRS_FRAME) throw new Error('srs_transport_limit: Page/chunk exceeds 1 MiB; continue in bounded calls.');
  return value;
};
const matchesStage = (stage, ids) => !!stage && stage.jobId === ids.jobId && (!ids.attemptId || stage.attemptId === ids.attemptId) && (!ids.stageId || stage.stageId === ids.stageId);
function admissionState(found, ended, current) {
  if (!found) return 'absent';
  if (ended || current?.state === 'cancelled') return 'ended';
  return current?.state === 'active' || current?.state === 'submitted' ? 'active' : 'queued';
}
/** Exact UTF-8 (no lone surrogates) and at most 64 KiB; used for chunks because a JSON-schema refinement cannot reach the provider. */
export const exactUtf8Chunk = value => typeof value === 'string' && Buffer.byteLength(value) <= MAX_SRS_CHUNK && Buffer.from(value, 'utf8').toString('utf8') === value;

/** Strict bounded mutations, never an arbitrary JSON checkpoint or an output path supplied by a model. */
export function srsParameters(z) {
  const id = z.string().min(1).max(300);
  const uuid = z.string().regex(/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/);
  const hash = z.string().regex(/^[0-9a-f]{64}$/);
  const integer = z.number().int().min(0).max(Number.MAX_SAFE_INTEGER);
  const phase = z.enum(SRS_PHASES);
  const status = z.enum(['pending', 'active', 'blocked', 'reviewed', 'notApplicable']);
  const kind = z.enum(['sourceArea', 'capability', 'scenario', 'requirementReview', 'templateSection', 'contractReview']);
  const text = z.string().min(1).max(MAX_SRS_CHUNK);
  const ids = z.array(id).max(200);
  const relations = { evidenceIds: ids.optional(), requirementIds: ids.optional(), questionIds: ids.optional() };
  const base = { jobId: uuid, attemptId: uuid, stageId: uuid };
  const documentType = z.enum(SRS_DOCUMENT_TYPES);
  const option = z.object({ label: text, description: text.optional() }).strict();
  const updates = z.object({
    addWork: z.array(z.object({ ref: id.optional(), parentId: id.optional(), phase, kind, status: status.optional(), ...relations, rationale: text }).strict()).max(200).optional(),
    updateWork: z.array(z.object({ id, status: status.optional(), ...relations, rationale: text.optional() }).strict()).max(200).optional(),
    addDocuments: z.array(z.object({ ref: id.optional(), type: documentType, title: text, parentDocumentId: id.optional(), requirementId: id.optional() }).strict()).max(200).optional(),
    addRequirements: z.array(z.object({ ref: id.optional(), area: z.enum(['INT', 'PO', 'EI', 'FUN', 'QOS', 'COM', 'DES', 'AI', 'VER', 'APP']), title: text, workIds: ids.optional(), evidenceIds: ids.optional() }).strict()).max(200).optional(),
    addQuestions: z.array(z.object({ ref: id.optional(), workIds: ids.optional(), prompt: text, options: z.array(option).max(200), multi: z.boolean().optional(), required: z.boolean().optional() }).strict()).max(200).optional(),
    summary: text.optional(),
  }).strict();
  return {
    srs_inspect: z.object({ ...base, section: z.enum(['request', 'template', 'inventory', 'work', 'documents', 'questions', 'coverage', 'checkpoint', 'delegations']), id: id.optional(), offset: integer.optional(), limit: z.number().int().min(1).max(200).optional() }).strict(),
    srs_read_evidence: z.object({ ...base, evidenceId: id, offset: integer.optional() }).strict(),
    srs_write_document_chunk: z.object({ ...base, documentId: id, revisionId: id, offset: integer, text, expectedPrefixHash: hash }).strict(),
    srs_commit_document: z.object({ ...base, documentId: id, revisionId: id, expectedHash: hash, metadata: z.object({ type: documentType, title: text.optional(), requirementId: id.optional(), relatedDocumentIds: ids.optional(), specificity: z.enum(['portable', 'uiSpecific', 'implementationSpecific']), reviewedWorkIds: ids.optional(), citations: z.array(z.object({ evidenceId: id, startOffset: integer.optional(), endOffset: integer.optional() }).strict()).max(200).optional() }).strict() }).strict(),
    srs_checkpoint: z.object({ ...base, expectedRevision: integer, idempotencyKey: id.optional(), updates }).strict(),
    srs_finish_stage: z.object({ ...base, expectedRevision: integer, review: z.object({ phase, documentRevision: integer.optional(), workRevision: integer.optional(), gapIds: ids.optional(), unresolvedQuestionIds: ids.optional(), finalDeclaration: z.enum(['noKnownGap', 'moreWorkNeeded']).optional(), summary: text }).strict() }).strict(),
    srs_delegate: z.object({ ...base, assignment: z.string().min(1).max(16384), workIds: ids, evidenceIds: ids, documentIds: ids }).strict(),
    stage: z.object({ ...base, nonce: z.string().regex(/^[A-Za-z0-9_-]{16,128}$/), phase, stageNumber: z.number().int().min(1).max(Number.MAX_SAFE_INTEGER), message: text }).strict(),
    status: z.object({ jobId: uuid, attemptId: uuid.optional(), stageId: uuid.optional() }).strict(),
    pause: z.object({ jobId: uuid, attemptId: uuid, stageId: uuid.optional(), reason: z.enum(['pause', 'close', 'cancel']) }).strict(),
  };
}

const DESCRIPTIONS = {
  srs_inspect: 'Inspect the admitted SRS request/template/inventory/work/documents/questions/coverage/checkpoint. IDs only, no paths. Pages 1-200; follow nextOffset indefinitely. Template offsets are bytes. Native receipts, not assertions, establish evidence reads.',
  srs_read_evidence: 'Read the frozen admitted evidence by evidenceId from inventory; continue with nextOffset (64 KiB text pages). No arbitrary files, execution or network. Unsupported evidence remains a human blocker. Native image content is provided when supported.',
  srs_write_document_chunk: 'Stage exact UTF-8 Markdown for an allocated document/revision, at byte offset with expectedPrefixHash. Chunks at most 64 KiB; arbitrarily many chunks/documents. Replay identical chunks safely. No paths or overwrite authority.',
  srs_commit_document: 'Publish a staged Markdown revision after full hash, ID, link and obligation validation. Metadata cannot set paths or approve human questions. Requirements must be portable; legacy UI and implementation observations are separately labelled. Internal citations stay in private audit, never public notes.',
  srs_checkpoint: 'Commit one revision-fenced bounded ledger update. Backend allocates work/document/requirement/question IDs; ref associates returned IDs to this batch. Relations append; page additional updates without total job caps. Only humans exclude scope or confirm stakeholder questions. For native Ask, first allocate questions here, then use exactly each returned questionId as Ask question id.',
  srs_finish_stage: 'Commit a phase review against expectedRevision. Backend checks read receipts, ledger, current document hashes/links, template coverage, traceability and unresolved material questions. Gaps queue more work indefinitely. Prose, empty batch or a fixed number of passes cannot complete SRS.',
  srs_delegate: 'Run a real native SRS worker for preallocated work/documents and explicit evidence IDs in this stage. It can persist assigned findings and document revisions during inspection, but cannot allocate entities, use native Ask, answer humans, finish a stage, delegate, or use arbitrary tools. Calls may run in parallel for independent scopes. Inspect durable delegations and work after interruption; reassignment never grants an old worker renewed authority.',
};
const failure = error => ({ content: [{ type: 'text', text: String(error?.message ?? error) }], isError: true });
const uncertainAdmission = error => Object.assign(new Error(String(error?.message ?? error)), { srsAdmissionUncertain: true });
const controlBody = payload => Object.fromEntries(Object.entries(payload).filter(([key]) => !['source', 'generation', 'sessionId', 'ownershipEpoch', 'workspaceEpoch', 'instanceId'].includes(key)));

/** One Architect, one native conversation; no provider client, timer-driven generation, or detached observer. */
export function installSrsBridge(pi, { request, workspace, currentContext }) {
  const schemas = srsParameters(pi.zod);
  const guard = registerVisualizationGuard(pi);
  let admission = null, disconnected = false, disposed = false;
  let recoveredSessionId = null;
  const notifications = new Set();
  const children = new Map();
  const recording = new Map();
  const hasRecorded = state => !state?.pending || (state.checkpoint && state.document);
  async function recordedTool(name, params, delegation) {
    const key = delegation?.assignmentId ?? params.stageId;
    let state = recording.get(key);
    if (!state) { state = { pending: null, checkpoint: false, document: false, checkpoints: 0, commits: 0 }; recording.set(key, state); }
    const page = `${params.evidenceId}:${params.offset ?? 0}`;
    if (((name === 'srs_read_evidence' && state.pending?.evidenceId !== params.evidenceId) || name === 'srs_finish_stage') && !hasRecorded(state)) throw new Error('srs_record_findings_first: Persist concrete work rationale with srs_checkpoint, then commit its portable findings note with a private evidence citation before inspecting another evidence area or finishing. Same-file nextOffset continuation remains available.');
    if (name === 'srs_commit_document' && state.pending && (!state.checkpoint || !params.metadata.citations?.some(citation => citation.evidenceId === state.pending.evidenceId))) throw new Error('srs_record_findings_first: Checkpoint the discovered facts, then privately cite the inspected evidenceId in the document commit.');
    const result = bounded(await request('srs_tool', { action: name, ...params, ...(delegation ? { delegation } : {}) }));
    if (name === 'srs_read_evidence' && ['text', 'image'].includes(result.kind) && state.pending?.page !== page) {
      state.pending = { page, evidenceId: params.evidenceId }; state.checkpoint = state.document = false;
    }
    if (name === 'srs_checkpoint' && [...(params.updates.updateWork ?? []), ...(params.updates.addWork ?? [])].some(work => typeof work.rationale === 'string' && work.rationale.trim())) { state.checkpoint = true; state.checkpoints++; }
    if (name === 'srs_commit_document' && result.replayed !== true) { state.document = true; state.commits++; }
    return result;
  }
  function revokeChildren(stage) {
    for (const child of children.values()) if (!stage || sameStage(child.stage, stage)) child.controller.abort(new Error('srs_delegation_revoked'));
  }
  function yielded(stage, reason) {
    if (!stage) return;
    revokeChildren(stage);
    const key = stage.stageId;
    if (notifications.has(key)) return;
    notifications.add(key);
    void request('srs_stage_yielded', { jobId: stage.jobId, attemptId: stage.attemptId, stageId: stage.stageId, reason }).catch(() => {});
  }
  const available = () => !disposed && workspace.nativeGuardAvailable() && workspace.nativeUiCapabilities().nativeDecisions === true;
  // The monitor explanation shares this guard and the one native before-model gate below; it never installs its own host.
  const monitor = installHostMonitorBridge(pi, { request, workspace, currentContext, guard });
  guard.setNativeHost({
    quiescent: () => workspace.quiescence().quiescent,
    onYield: stage => yielded(stage, 'unrelated_prompt'),
    onUncertain: stage => yielded(stage, 'continuation_uncertain'),
    validateMonitor: stage => monitor.validate(stage),
    onMonitorRetired: (stage, reason) => monitor.retired(stage, reason),
    async validateSrs(stage) {
      if (disposed || disconnected || !available()) throw new Error(SRS_GUARD_ERROR);
      if (admission && sameStage(admission, stage)) await admission.persisted;
      const result = await request('srs_stage_validate', stageIds(stage));
      if (result?.authorized !== true) throw new Error('srs_stage_not_authorized');
    },
  });
  const detach = workspace.addBeforeModelCallGate(async (signal, nativeContext) => {
    if (signal?.aborted) throw signal.reason;
    try { await guard.beforeModelCall(currentContext(), nativeContext); }
    catch (error) { yielded(guard.srs(currentContext()), 'guard_failure'); throw error; }
  });
  for (const name of SRS_TOOL_NAMES) pi.registerTool({
    name, label: name.replaceAll('_', ' '), loadMode: 'essential', description: DESCRIPTIONS[name], parameters: schemas[name], approval: () => 'read',
    async execute(_id, params, signal, _update, ctx) {
      if (signal?.aborted) throw signal.reason;
      try {
        if (ctx?.agent?.kind !== 'main') throw new Error('SRS tools require the main Architect');
        const validated = bounded(schemas[name].parse(params));
        if (name === 'srs_write_document_chunk' && !exactUtf8Chunk(validated.text)) throw new Error('invalid_request: A chunk must be exact UTF-8 and at most 64 KiB');
        if (!sameStage(guard.activeSrs(ctx), validated) || guard.toolCall({ toolName: name, input: validated }, ctx)?.block) throw new Error('srs_stage_not_active');
        if (name === 'srs_delegate') return await delegate(validated, signal, _update);
        if (name === 'srs_finish_stage' && [...children.values()].some(child => sameStage(child.stage, validated))) throw new Error('srs_delegations_active: Wait for the admitted workers before declaring the stage review.');
        const result = await recordedTool(name, validated);
        const content = [{ type: 'text', text: JSON.stringify(result) }];
        attachImage(name, result, content);
        return { content, details: result };
      } catch (error) { return failure(error); }
    },
  });
  async function delegate(params, signal, onUpdate) {
    if (typeof workspace.runSrsDelegate !== 'function') throw new Error('native_srs_delegation_unavailable');
    if (Buffer.byteLength(params.assignment) > 16384) throw new Error('invalid_request: Assignment exceeds the 16 KiB ledger entry limit; split independent work across delegates.');
    if (!params.documentIds.length) throw new Error('invalid_request: Preallocate portable findings documents before delegating inspection.');
    const stage = guard.activeSrs(currentContext());
    const childId = `Srs-${randomUUID()}`;
    const controller = new AbortController();
    const childSignal = signal ? AbortSignal.any([signal, controller.signal]) : controller.signal;
    const child = { stage, controller };
    children.set(childId, child);
    let delegation, outcome = 'failed', summary;
    const validate = async () => {
      if (childSignal.aborted) throw childSignal.reason;
      if (disposed || disconnected || !sameStage(guard.activeSrs(currentContext()), stage) || guard.toolCall({ toolName: 'srs_inspect', input: stage }, currentContext())?.block) throw new Error('srs_delegation_revoked');
      const result = await request('srs_delegate_validate', { ...stageIds(stage), delegation });
      if (result?.authorized !== true) throw new Error('srs_delegation_revoked');
    };
    try {
      const opened = await request('srs_delegate_open', { ...params, childId });
      if (typeof opened?.assignmentId !== 'string') throw new Error('srs_delegation_not_persisted');
      delegation = { assignmentId: opened.assignmentId, childId };
      await validate();
      const customTools = SRS_TOOL_NAMES.filter(name => !['srs_delegate', 'srs_finish_stage'].includes(name) && SRS_ALLOWLISTS[stage.phase].includes(name)).map(name => ({
        name, label: name.replaceAll('_', ' '), description: DESCRIPTIONS[name], parameters: schemas[name], loadMode: 'essential', approval: () => 'read',
        async execute(_id, raw, _update, _ctx, toolSignal) {
          try {
            if (toolSignal?.aborted) throw toolSignal.reason;
            await validate();
            const value = bounded(schemas[name].parse(raw));
            if (!sameStage(stage, value)) throw new Error('stale_stage');
            if (name === 'srs_write_document_chunk' && !exactUtf8Chunk(value.text)) throw new Error('invalid_request: Chunk must be exact UTF-8 and at most 64 KiB');
            const result = await recordedTool(name, value, delegation);
            const content = [{ type: 'text', text: JSON.stringify(result) }];
            attachImage(name, result, content);
            return { content, details: result };
          } catch (error) { return failure(error); }
        },
      }));
      const result = await workspace.runSrsDelegate({ id: childId, signal: childSignal, validate, customTools,
        onProgress: progress => onUpdate?.({ content: [{ type: 'text', text: `Native SRS worker ${childId}: ${progress.status ?? 'running'}. Findings are recorded in the durable ledger.` }], details: { ...delegation, status: progress.status } }),
        task: `Trusted SRS assignment ${JSON.stringify({ ...params, delegation, purpose: opened.purpose, publicDocumentContracts: opened.publicDocumentContracts })}\nFirst inspect checkpoint and your assigned work/documents by exact ID; inventory is available only by assigned evidence ID. Only update your assigned work (status, rationale and assigned evidence relations); entity allocation, question/requirement relations and idempotency keys are root-only. Preserve the first topic line of contractReview rationale when appending findings. Persist each meaningful finding before inspecting the next area. Never infer approval or completeness from missing evidence. Return stakeholder questions to the root. Finish only your assignment, not the root stage. The ledger and committed notes, not this answer, are the durable handoff.` });
      outcome = delegateOutcome(childSignal, result);
      const saved = recording.get(delegation.assignmentId);
      if (outcome === 'completed' && (!hasRecorded(saved) || !saved?.checkpoints || !saved?.commits)) {
        outcome = 'failed';
        throw new Error('srs_record_findings_first: Native worker ended without its required durable findings checkpoint and document commit; inspect/reassign its unfinished work.');
      }
      summary = outcome === 'completed' ? 'Native worker completed; inspect assigned durable work and documents.' : Buffer.from(String(result.error ?? result.stderr ?? 'Native worker did not complete')).subarray(0, 16380).toString('utf8');
      return { content: [{ type: 'text', text: JSON.stringify({ ...delegation, outcome, summary }) }], details: { ...delegation, outcome }, ...(outcome === 'completed' ? {} : { isError: true }) };
    } finally {
      if (childSignal.aborted && outcome !== 'completed') outcome = 'cancelled';
      controller.abort(new Error('srs_delegation_closed'));
      children.delete(childId);
      if (delegation) recording.delete(delegation.assignmentId);
      if (delegation) await request('srs_delegate_close', { ...stageIds(stage), delegation, outcome, ...(summary ? { summary } : {}) });
    }
  }
  function marker(payload) {
    const entries = workspace.nativeSessionEntries();
    return [...entries].reverse().find(entry => entry.type === 'custom' && entry.customType === SRS_MARKER &&
      entry.data?.jobId === payload.jobId && (!payload.attemptId || entry.data.attemptId === payload.attemptId) && (!payload.stageId || entry.data.stageId === payload.stageId));
  }
  function retireStage(ctx, stage, sessionId) {
    if (workspace.identity().sessionId !== sessionId || ctx.sessionManager.getSessionId() !== sessionId) return;
    if (!workspace.nativeGuardAvailable()) throw new Error(SRS_GUARD_ERROR);
    const entries = workspace.nativeSessionEntries();
    if (!entries.some(entry => entry.type === 'custom' && entry.customType === SRS_MARKER && sameStage(entry.data, stage) && entry.data.nonce === stage.nonce)) throw new Error('native_srs_admission_marker_missing');
    if (!entries.some(entry => entry.type === 'custom' && entry.customType === 'omp-srs-stage-ended' && sameStage(entry.data, stage))) workspace.appendNativeEntry('omp-srs-stage-ended', stageIds(stage));
    guard.retireSrs(ctx, stage);
    recording.delete(stage.stageId);
    if (sameStage(admission, stage)) admission = null;
  }
  function failedStage(ctx, stage, sessionId) {
    guard.pauseSrs(ctx, stage);
    yielded(stage, 'guard_failure');
    const settled = () => {
      if (disposed || workspace.identity().sessionId !== sessionId) return;
      if (!workspace.quiescence().quiescent) { ctx.setTimeout(settled, 50); return; }
      try { retireStage(ctx, stage, sessionId); }
      catch { yielded(stage, 'guard_failure'); }
    };
    settled();
  }
  async function stagePrompt(ctx, raw) {
    const payload = bounded(schemas.stage.parse(controlBody(raw)));
    const sessionId = ctx.sessionManager.getSessionId();
    if (!available() || disconnected) throw new Error(SRS_GUARD_ERROR);
    const found = marker(payload);
    if (found) return duplicateStage(ctx, payload, found);
    const status = workspace.quiescence();
    if (!status.quiescent) throw new Error(`native_srs_not_quiescent: ${status.reasons.join(', ')}`);
    const authorized = await request('srs_stage_validate', { ...stageIds(payload), preAdmission: true });
    if (authorized?.authorized !== true) throw new Error('srs_stage_not_authorized');
    // Recheck after transport await and immediately before native admission.
    if (!workspace.quiescence().quiescent || ctx.sessionManager.getSessionId() !== workspace.identity().sessionId) throw new Error('native_srs_not_quiescent');
    // A prior committed stage is retired only at this proven boundary.
    const previous = guard.srs(ctx);
    if (previous?.state === 'submitted' || previous?.state === 'cancelled') guard.discard(previous.stageId);
    notifications.clear();
    const next = guard.admitSrs(ctx, payload);
    if (next.duplicate) return { admitted: true, duplicate: true, state: 'queued', sessionId: workspace.identity().sessionId };
    const admitted = Promise.withResolvers();
    let accepted = false;
    const persistence = Promise.withResolvers();
    // Register before calling native prompt: its synchronous admission callback may race the first model gate.
    admission = { ...stageIds(payload), persisted: persistence.promise };
    persistence.promise.catch(() => {});
    try {
      const running = workspace.submitPrompt(next.text, { onPromptAdmitted() {
        accepted = true;
        try {
          workspace.appendNativeEntry(SRS_MARKER, stageIds(payload));
          Promise.resolve(request('srs_stage_admitted', stageIds(payload))).then(result => {
            if (result?.persisted !== true) throw new Error('srs_stage_admission_not_persisted');
            persistence.resolve(); admitted.resolve();
          }).catch(error => { persistence.reject(error); admitted.reject(error); yielded(payload, 'guard_failure'); });
        } catch (error) { persistence.reject(error); admitted.reject(error); }
      } });
      Promise.resolve(running).then(() => {
        if (!accepted) { const error = new Error('native_srs_prompt_not_admitted'); persistence.reject(error); admitted.reject(error); }
      }, error => {
        if (!accepted) { persistence.reject(error); admitted.reject(error); }
        else failedStage(ctx, payload, sessionId);
      });
      await admitted.promise;
      return { admitted: true, duplicate: false, state: stageStatus(ctx, { jobId: payload.jobId, attemptId: payload.attemptId, stageId: payload.stageId }).admission.state, sessionId: workspace.identity().sessionId };
    } catch (error) {
      if (!accepted) guard.discard(payload.stageId);
      if (accepted) throw uncertainAdmission(error);
      throw error;
    }
  }
  async function duplicateStage(ctx, payload, found) {
    if (found.data.nonce !== payload.nonce) throw new Error('stale_stage');
    if (sameStage(admission, payload)) {
      try { await admission.persisted; }
      catch (error) { throw uncertainAdmission(error); }
    }
    return { admitted: true, duplicate: true, state: stageStatus(ctx, { jobId: payload.jobId, attemptId: payload.attemptId, stageId: payload.stageId }).admission.state, sessionId: workspace.identity().sessionId };
  }
  async function pauseStage(ctx, payload) {
    const ids = schemas.pause.parse(controlBody(payload));
    const paused = guard.pauseSrs(ctx, ids);
    if (paused.stage) revokeChildren(paused.stage);
    if (paused.active) await ctx.abort();
    if (workspace.quiescence().quiescent && paused.stage) guard.discard(paused.stage.stageId);
    return { aborted: paused.active, state: 'paused' };
  }
  function stageStatus(ctx, payload) {
    const ids = schemas.status.parse(controlBody(payload));
    const ready = available();
    const found = ready ? marker(ids) : null;
    const stage = guard.srs(ctx);
    const current = matchesStage(stage, ids) ? stage : null;
    const status = workspace.quiescence();
    const ended = found && workspace.nativeSessionEntries().some(entry => entry.type === 'custom' && entry.customType === 'omp-srs-stage-ended' && sameStage(entry.data, found.data));
    return { sessionId: workspace.identity().sessionId, ...status, guardAvailable: ready,
      active: current?.state === 'active' ? { jobId: current.jobId, attemptId: current.attemptId, stageId: current.stageId } : null,
      admission: { stageId: ids.stageId ?? found?.data.stageId ?? null, found: !!found, nonce: found?.data.nonce ?? null, state: admissionState(found, ended, current) } };
  }
  return {
    guard,
    monitor,
    attachContext(ctx) {
      const sessionId = ctx.sessionManager.getSessionId();
      if (sessionId !== recoveredSessionId) revokeChildren();
      if (sessionId === recoveredSessionId || !workspace.nativeGuardAvailable()) return;
      recoveredSessionId = sessionId;
      const entries = workspace.nativeSessionEntries();
      const last = [...entries].reverse().find(entry => entry.type === 'custom' && entry.customType === SRS_MARKER);
      if (last && !entries.some(entry => entry.type === 'custom' && entry.customType === 'omp-srs-stage-ended' && sameStage(entry.data, last.data))) guard.recoverSrs(ctx, last.data);
    },
    settled() {
      const stage = guard.srs(currentContext());
      if (!stage || !workspace.quiescence().quiescent || !workspace.nativeGuardAvailable()) return;
      retireStage(currentContext(), stage, workspace.identity().sessionId);
    },
    correlation: () => { const stage = guard.srs(currentContext()); return stage ? { jobId: stage.jobId, attemptId: stage.attemptId, stageId: stage.stageId } : null; },
    async control(action, payload, ctx = currentContext()) {
      if (ctx?.agent?.kind !== 'main') throw new Error('not_controlled');
      if (action === 'srs_stage_prompt') {
        try { return await stagePrompt(ctx, payload); }
        catch (error) {
          if (error?.srsAdmissionUncertain) throw error;
          const message = String(error?.message ?? error);
          return { admitted: false, error: { code: message.split(':')[0].replaceAll(' ', '_'), message } };
        }
      }
      if (action === 'srs_stage_status') return stageStatus(ctx, payload);
      if (action === 'srs_pause') return pauseStage(ctx, payload);
      throw new Error('invalid_srs_control');
    },
    ask: {
      active: () => guard.activeSrs(currentContext()),
      async map(invocationId, questions) {
        const active = guard.activeSrs(currentContext());
        if (!active) throw new Error('srs_stage_not_active');
        const result = await request('srs_ask_map', bounded({ invocationId, questions: questions.map(question => ({ id: question.id, question: question.question, options: question.options.map(option => typeof option === 'string' ? option : option.label), multi: question.multi ?? false })) }));
        if (result?.mapped !== true || !sameStage(result, active) || result.unmappedIds?.length || result.mappedIds?.length !== questions.length) {
          // The backend may have persisted a valid subset even though no native dialog can be opened.
          // Retire that exact invocation before rejecting so those ledger questions can be asked again.
          try {
            const retired = await request('srs_ask_retired', { invocationId, reason: 'failed' });
            if (retired?.ok !== true) throw new Error('srs_question_retirement_not_persisted');
          } catch (error) {
            yielded(active, 'guard_failure');
            throw error;
          }
          throw new Error('srs_question_not_mapped: Allocate the exact ledger questions using srs_checkpoint first.');
        }
        return result;
      },
      receipt: (invocationId, provenance, results) => request('srs_ask_receipt', bounded({ invocationId, provenance, results: results.map(({ id, selectedOptions, customInput, note }) => ({ id, selectedOptions, ...(customInput === undefined ? {} : { customInput }), ...(note === undefined ? {} : { note }) })) })),
      retired: (invocationId, reason) => request('srs_ask_retired', { invocationId, reason }),
    },
    disconnected() { disconnected = true; revokeChildren(); monitor.disconnected(); yielded(guard.srs(currentContext()), 'disconnected'); },
    reconnect() { disconnected = false; monitor.reconnect(); },
    dispose() { disposed = true; revokeChildren(); recording.clear(); monitor.dispose(); detach(); guard.setNativeHost(null); },
  };
}
function attachImage(name, result, content) {
  if (name !== 'srs_read_evidence' || result.kind !== 'image' || !result.image) return;
  if (!['image/png', 'image/jpeg', 'image/gif', 'image/webp'].includes(result.image.mimeType) || typeof result.image.data !== 'string') throw new Error('unsupported native image evidence');
  const { image, ...metadata } = result;
  content[0].text = JSON.stringify(metadata);
  content.push({ type: 'image', mimeType: image.mimeType, data: image.data });
}
function delegateOutcome(childSignal, result) {
  if (childSignal.aborted || result.aborted) return 'cancelled';
  return result.exitCode === 0 ? 'completed' : 'failed';
}
