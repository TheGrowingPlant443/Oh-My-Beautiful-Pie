import { SRS_ALLOWLISTS, registerVisualizationGuard } from './omp-visualization-guard.mjs';
export { SRS_ALLOWLISTS };

export const MAX_SRS_FRAME = 1024 * 1024;
export const MAX_SRS_CHUNK = 64 * 1024;
export const SRS_MARKER = 'omp-srs-stage';
export const SRS_GUARD_ERROR = 'native_srs_guard_unavailable: Reopen the Architect with a compatible OMP version.';
export const SRS_PHASES = ['survey', 'specify', 'gapReview', 'verify'];
export const SRS_DOCUMENT_TYPES = ['master', 'requirement', 'domain', 'flow', 'interface', 'glossary', 'uiObservations', 'structureObservations', 'traceability', 'coverage', 'questions'];
export const SRS_TOOL_NAMES = ['srs_inspect', 'srs_read_evidence', 'srs_write_document_chunk', 'srs_commit_document', 'srs_checkpoint', 'srs_finish_stage'];
const sameStage = (a, b) => !!a && !!b && a.jobId === b.jobId && a.attemptId === b.attemptId && a.stageId === b.stageId;
const stageIds = stage => ({ jobId: stage.jobId, attemptId: stage.attemptId, stageId: stage.stageId, nonce: stage.nonce });
const bounded = value => { if (Buffer.byteLength(JSON.stringify(value)) > MAX_SRS_FRAME) throw new Error('srs_transport_limit: Page/chunk exceeds 1 MiB; continue in bounded calls.'); return value; };
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
  const kind = z.enum(['sourceArea', 'capability', 'scenario', 'requirementReview', 'templateSection']);
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
    srs_inspect: z.object({ ...base, section: z.enum(['request', 'template', 'inventory', 'work', 'documents', 'questions', 'coverage', 'checkpoint']), id: id.optional(), offset: integer.optional(), limit: z.number().int().min(1).max(200).optional() }).strict(),
    srs_read_evidence: z.object({ ...base, evidenceId: id, offset: integer.optional() }).strict(),
    srs_write_document_chunk: z.object({ ...base, documentId: id, revisionId: id, offset: integer, text, expectedPrefixHash: hash }).strict(),
    srs_commit_document: z.object({ ...base, documentId: id, revisionId: id, expectedHash: hash, metadata: z.object({ type: documentType, title: text.optional(), requirementId: id.optional(), relatedDocumentIds: ids.optional(), specificity: z.enum(['portable', 'uiSpecific', 'implementationSpecific']), reviewedWorkIds: ids.optional(), citations: z.array(z.object({ evidenceId: id, startOffset: integer.optional(), endOffset: integer.optional() }).strict()).max(200).optional() }).strict() }).strict(),
    srs_checkpoint: z.object({ ...base, expectedRevision: integer, idempotencyKey: id.optional(), updates }).strict(),
    srs_finish_stage: z.object({ ...base, expectedRevision: integer, review: z.object({ phase, documentRevision: integer.optional(), workRevision: integer.optional(), gapIds: ids.optional(), unresolvedQuestionIds: ids.optional(), finalDeclaration: z.enum(['noKnownGap', 'moreWorkNeeded']).optional(), summary: text }).strict() }).strict(),
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
};
const failure = error => ({ content: [{ type: 'text', text: String(error?.message ?? error) }], isError: true });
const controlBody = payload => Object.fromEntries(Object.entries(payload).filter(([key]) => !['source', 'generation', 'sessionId', 'ownershipEpoch', 'workspaceEpoch', 'instanceId'].includes(key)));

/** One Architect, one native conversation; no provider client, timer-driven generation, or detached observer. */
export function installSrsBridge(pi, { request, workspace, currentContext }) {
  const schemas = srsParameters(pi.zod);
  const guard = registerVisualizationGuard(pi);
  let admission = null, disconnected = false, disposed = false;
  let recoveredSessionId = null;
  const notifications = new Set();
  function yielded(stage, reason) {
    if (!stage) return;
    const key = stage.stageId;
    if (notifications.has(key)) return;
    notifications.add(key);
    void request('srs_stage_yielded', { jobId: stage.jobId, attemptId: stage.attemptId, stageId: stage.stageId, reason }).catch(() => {});
  }
  const available = () => !disposed && workspace.nativeGuardAvailable() && workspace.nativeUiCapabilities().nativeDecisions === true;
  guard.setNativeHost({
    quiescent: () => workspace.quiescence().quiescent,
    onYield: stage => yielded(stage, 'unrelated_prompt'),
    onUncertain: stage => yielded(stage, 'continuation_uncertain'),
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
        const result = bounded(await request('srs_tool', { action: name, ...validated }));
        const content = [{ type: 'text', text: JSON.stringify(result) }];
        if (name === 'srs_read_evidence' && result.kind === 'image' && result.image) {
          if (!['image/png', 'image/jpeg', 'image/gif', 'image/webp'].includes(result.image.mimeType) || typeof result.image.data !== 'string') throw new Error('unsupported native image evidence');
          const { image: _image, ...metadata } = result;
          content[0].text = JSON.stringify(metadata);
          content.push({ type: 'image', mimeType: result.image.mimeType, data: result.image.data });
        }
        return { content, details: result };
      } catch (error) { return failure(error); }
    },
  });
  function marker(payload) {
    const entries = workspace.nativeSessionEntries();
    return [...entries].reverse().find(entry => entry.type === 'custom' && entry.customType === SRS_MARKER &&
      entry.data?.jobId === payload.jobId && (!payload.attemptId || entry.data.attemptId === payload.attemptId) && (!payload.stageId || entry.data.stageId === payload.stageId));
  }
  async function stagePrompt(ctx, raw) {
    const payload = bounded(schemas.stage.parse(controlBody(raw)));
    if (!available() || disconnected) throw new Error(SRS_GUARD_ERROR);
    const found = marker(payload);
    if (found) {
      if (found.data.nonce !== payload.nonce) throw new Error('stale_stage');
      return { admitted: true, duplicate: true, state: guard.activeSrs(ctx)?.stageId === payload.stageId ? 'active' : 'queued', sessionId: workspace.identity().sessionId };
    }
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
      }, error => { if (!accepted) { persistence.reject(error); admitted.reject(error); } });
      await admitted.promise;
      return { admitted: true, duplicate: false, state: guard.activeSrs(ctx)?.stageId === payload.stageId ? 'active' : 'queued', sessionId: workspace.identity().sessionId };
    } catch (error) {
      if (!accepted) guard.discard(payload.stageId);
      throw error;
    }
  }
  return {
    guard,
    attachContext(ctx) {
      const sessionId = ctx.sessionManager.getSessionId();
      if (sessionId === recoveredSessionId || !workspace.nativeGuardAvailable()) return;
      recoveredSessionId = sessionId;
      const entries = workspace.nativeSessionEntries();
      const last = [...entries].reverse().find(entry => entry.type === 'custom' && entry.customType === SRS_MARKER);
      if (last && !entries.some(entry => entry.type === 'custom' && entry.customType === 'omp-srs-stage-ended' && sameStage(entry.data, last.data))) guard.recoverSrs(ctx, last.data);
    },
    settled() {
      const stage = guard.srs(currentContext());
      if (!stage || !workspace.quiescence().quiescent || !workspace.nativeGuardAvailable()) return;
      const entries = workspace.nativeSessionEntries();
      if (!entries.some(entry => entry.type === 'custom' && entry.customType === 'omp-srs-stage-ended' && sameStage(entry.data, stage))) workspace.appendNativeEntry('omp-srs-stage-ended', stageIds(stage));
      // The native ended marker remains authoritative for reconciliation after releasing turn ownership.
      // Never release before the append succeeds: a storage failure must keep the stage fail-closed.
      guard.retireSrs(currentContext(), stage);
      if (sameStage(admission, stage)) admission = null;
    },
    correlation: () => { const stage = guard.srs(currentContext()); return stage ? { jobId: stage.jobId, attemptId: stage.attemptId, stageId: stage.stageId } : null; },
    async control(action, payload, ctx = currentContext()) {
      if (ctx?.agent?.kind !== 'main') throw new Error('not_controlled');
      if (action === 'srs_stage_prompt') return stagePrompt(ctx, payload);
      if (action === 'srs_stage_status') {
        const ids = schemas.status.parse(controlBody(payload));
        const ready = available();
        const found = ready ? marker(ids) : null;
        const stage = guard.srs(ctx);
        const current = stage && stage.jobId === ids.jobId && (!ids.attemptId || stage.attemptId === ids.attemptId) && (!ids.stageId || stage.stageId === ids.stageId) ? stage : null;
        const status = workspace.quiescence();
        const ended = found && workspace.nativeSessionEntries().some(entry => entry.type === 'custom' && entry.customType === 'omp-srs-stage-ended' && sameStage(entry.data, found.data));
        return { sessionId: workspace.identity().sessionId, ...status, guardAvailable: ready,
          active: current && current.state === 'active' ? { jobId: current.jobId, attemptId: current.attemptId, stageId: current.stageId } : null,
          admission: { stageId: ids.stageId ?? found?.data.stageId ?? null, found: !!found, nonce: found?.data.nonce ?? null,
            state: !found ? 'absent' : ended || current?.state === 'cancelled' ? 'ended' : current?.state === 'active' || current?.state === 'submitted' ? 'active' : 'queued' } };
      }
      if (action === 'srs_pause') {
        const ids = schemas.pause.parse(controlBody(payload));
        const paused = guard.pauseSrs(ctx, ids);
        if (paused.active) await ctx.abort();
        if (workspace.quiescence().quiescent && paused.stage) guard.discard(paused.stage.stageId);
        return { aborted: paused.active, state: 'paused' };
      }
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
    disconnected() { disconnected = true; yielded(guard.srs(currentContext()), 'disconnected'); },
    reconnect() { disconnected = false; },
    dispose() { disposed = true; detach(); guard.setNativeHost(null); },
  };
}
