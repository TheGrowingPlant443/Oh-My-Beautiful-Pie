import { closeSync, fstatSync, lstatSync, openSync, readFileSync, realpathSync, renameSync, unlinkSync, writeFileSync } from 'node:fs';
import { createHash, randomUUID } from 'node:crypto';
import path from 'node:path';

// Architect-only dependency-update proposal tool. A proposal is a saved draft that the human reviews; it never
// reaches The Manager, edits the project or touches a Manager plan. `dir` comes from OMP_MANAGER_ARCHITECT_DIR only.
const REQUEST_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const MAX_REQUEST_BYTES = 1024 * 1024;
const MAX_RESULT_BYTES = 8 * 1024 * 1024;
const MAX_TITLE = 200;
const MAX_SUMMARY = 4000;
const MAX_MARKDOWN = 262144;
const MAX_SOURCE_REVIEWS = 20001;
const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
const text = (value, max, required = false) => typeof value === 'string' && value.length <= max && (!required || value.trim().length > 0);
const LONE_SURROGATE = /[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/;
// Control characters other than newline, carriage return and tab never belong in a plan shown to a human or a model.
const CONTROL = /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/;
const clean = value => !LONE_SURROGATE.test(value) && !CONTROL.test(value);

/** Identity of the displayed proposal text. Rust recomputes it, so the two encodings must stay identical. */
export function proposalHash(title, summary, markdown) {
  const hash = createHash('sha256');
  for (const part of [title, summary, markdown]) hash.update(`${Buffer.byteLength(part, 'utf8')}:${part}`, 'utf8');
  return hash.digest('hex');
}

function exists(file) {
  try { lstatSync(file); return true; }
  catch (error) { if (error.code === 'ENOENT') return false; throw error; }
}

function readBoundedJson(file, maximum) {
  const meta = lstatSync(file);
  if (!meta.isFile() || meta.isSymbolicLink()) throw new Error('invalid request file');
  const fd = openSync(file, 'r');
  try {
    const stat = fstatSync(fd);
    if (!stat.isFile() || stat.size > maximum) throw new Error('request file too large');
    const bytes = readFileSync(fd);
    if (bytes.length > maximum) throw new Error('request file too large');
    return JSON.parse(bytes.toString('utf8'));
  } finally { closeSync(fd); }
}

function validateProducer(producer) {
  if (!object(producer) || !text(producer.instanceId, 300, true) || !Number.isSafeInteger(producer.generation) || producer.generation < 0 ||
      !text(producer.launchId, 300, true) || !text(producer.sessionId, 300, true)) throw new Error('invalid Architect producer');
}

function validateRequest(request, requestId, producer) {
  validateProducer(producer);
  if (!object(request) || request.requestId !== requestId || request.kind !== 'updatePlan') throw new Error('unknown dependency update request');
  if (request.architectInstanceId !== producer.instanceId || request.architectGeneration !== producer.generation ||
      request.architectLaunchId !== producer.launchId || request.architectSessionId !== producer.sessionId) {
    throw new Error('request targets a different Architect launch or session');
  }
  if (!text(request.folder, 32768, true) || !path.isAbsolute(request.folder) || !text(request.contextId, 200, true) ||
      !(request.sourceContextId === null || text(request.sourceContextId, 200, true)) ||
      !Array.isArray(request.sourceReviewIds) || request.sourceReviewIds.length < 1 || request.sourceReviewIds.length > MAX_SOURCE_REVIEWS ||
      !request.sourceReviewIds.every(id => typeof id === 'string' && REQUEST_ID.test(id))) throw new Error('invalid admitted request');
}

function validatePayload(payload) {
  if (!object(payload) || !text(payload.requestId, 36, true) || !REQUEST_ID.test(payload.requestId)) throw new Error('invalid request id');
  if (!text(payload.title, MAX_TITLE, true) || /[\r\n]/.test(payload.title) || !clean(payload.title)) throw new Error(`title must be 1..${MAX_TITLE} characters on one line`);
  if (!text(payload.summary, MAX_SUMMARY, true) || !clean(payload.summary)) throw new Error(`summary must be 1..${MAX_SUMMARY} characters`);
  if (!text(payload.markdown, MAX_MARKDOWN, true) || !clean(payload.markdown)) throw new Error(`markdown must be 1..${MAX_MARKDOWN} characters`);
}

export function saveDependencyUpdatePlan(dir, payload, producer) {
  validatePayload(payload);
  validateProducer(producer);
  const root = realpathSync(dir);
  const plans = path.join(root, 'dependency-update-plans');
  const folder = path.join(plans, payload.requestId);
  for (const candidate of [plans, folder]) {
    const stat = lstatSync(candidate);
    if (!stat.isDirectory() || stat.isSymbolicLink() || realpathSync(candidate) !== candidate) throw new Error('invalid request directory');
  }
  const requestFile = path.join(folder, 'request.json');
  const request = readBoundedJson(requestFile, MAX_REQUEST_BYTES);
  validateRequest(request, payload.requestId, producer);
  const file = path.join(folder, 'result.json');
  const lock = path.join(folder, 'result.lock');
  const temporary = path.join(folder, `result.${process.pid}.${randomUUID()}.tmp`);
  // Persist only validated fields; identity, source reviews and timestamps never come from the model.
  const result = {
    requestId: payload.requestId, architectInstanceId: producer.instanceId, architectGeneration: producer.generation,
    architectLaunchId: producer.launchId, architectSessionId: producer.sessionId,
    revisionId: randomUUID(), hash: proposalHash(payload.title, payload.summary, payload.markdown),
    title: payload.title, summary: payload.summary, markdown: payload.markdown,
    sourceReviewIds: request.sourceReviewIds, contextId: request.contextId, sourceContextId: request.sourceContextId,
    completedAt: Date.now(),
  };
  const bytes = JSON.stringify(result);
  if (Buffer.byteLength(bytes) > MAX_RESULT_BYTES) throw new Error('proposal file too large');
  // Exclusive publication lock covers the exists checks and the rename. A proposal revision is never overwritten.
  const fd = openSync(lock, 'wx');
  try {
    if (exists(file)) throw new Error('plan already submitted; ask the user to regenerate it for a new revision');
    // A request already ended by the app (for example producerEnded) must not accept a late proposal.
    if (exists(path.join(folder, 'terminal.json')) && readBoundedJson(path.join(folder, 'terminal.json'), MAX_REQUEST_BYTES)?.status === 'error') {
      throw new Error('this request already ended; ask the user to regenerate the plan');
    }
    validateRequest(readBoundedJson(requestFile, MAX_REQUEST_BYTES), payload.requestId, producer);
    writeFileSync(temporary, bytes, { flag: 'wx' });
    renameSync(temporary, file);
  } finally {
    try { unlinkSync(temporary); }
    catch (error) { if (error.code !== 'ENOENT') throw error; }
    finally {
      try { closeSync(fd); } finally { unlinkSync(lock); }
    }
  }
  return { requestId: payload.requestId, revisionId: result.revisionId, hash: result.hash, saved: true };
}

function validRevision(result, requestId) {
  return object(result) && result.requestId === requestId && typeof result.revisionId === 'string' && REQUEST_ID.test(result.revisionId) &&
    text(result.title, MAX_TITLE, true) && text(result.summary, MAX_SUMMARY, true) && text(result.markdown, MAX_MARKDOWN, true) &&
    result.hash === proposalHash(result.title, result.summary, result.markdown) && Number.isSafeInteger(result.completedAt);
}

export function formatDependencyUpdatePlan(result) {
  return `Dependency update plan (draft for your review; nothing has been changed or handed to The Manager)\nTitle: ${result.title}\nRevision: ${result.revisionId}\n\n${result.summary}\n\n${result.markdown}`;
}

/** Read a saved proposal for console replay. `dir` is the fixed app-owned Architect directory; only a request id is accepted. */
export function loadDependencyUpdatePlanText(dir, requestId) {
  if (!text(requestId, 36, true) || !REQUEST_ID.test(requestId)) throw new Error('invalid request id');
  const root = realpathSync(dir);
  const plans = path.join(root, 'dependency-update-plans');
  const folder = path.join(plans, requestId);
  for (const candidate of [plans, folder]) {
    const stat = lstatSync(candidate);
    if (!stat.isDirectory() || stat.isSymbolicLink() || realpathSync(candidate) !== candidate) throw new Error('invalid request directory');
  }
  const result = readBoundedJson(path.join(folder, 'result.json'), MAX_RESULT_BYTES);
  if (!validRevision(result, requestId)) throw new Error('invalid saved update plan');
  return formatDependencyUpdatePlan(result);
}

/**
 * `producerFor(ctx)` must return `{ instanceId, generation, launchId, sessionId }` for the current main session, or throw.
 * `resultConsole` queues the display-only console copy of a saved plan and registers its replay loader.
 */
export function registerDependencyUpdateTool(pi, dir, producerFor, resultConsole) {
  const z = pi.zod;
  resultConsole?.registerKind('updatePlan', (consoleDir, requestId) => loadDependencyUpdatePlanText(consoleDir, requestId));
  pi.registerTool({
    name: 'submit_dependency_update_plan',
    label: 'Submit Dependency Update Plan',
    loadMode: 'essential',
    description: 'Save the dependency update PLAN for the exact app requestId: a title, a short summary and the complete markdown plan. This only writes an app-owned proposal for the user to review; it does not edit the project, install anything or contact The Manager. A saved proposal is final; terminal prose alone does not submit one.',
    parameters: z.object({ requestId: z.string(), title: z.string(), summary: z.string(), markdown: z.string() }),
    approval: () => 'read',
    async execute(_id, params, signal, _update, ctx) {
      if (signal?.aborted) throw signal.reason;
      try {
        if (ctx?.agent?.kind !== 'main') throw new Error('Update plans require the main Architect session');
        const saved = saveDependencyUpdatePlan(dir, params, producerFor(ctx));
        // Display-only delivery is best effort: the proposal is already saved and stays readable in the dialog.
        try { resultConsole?.enqueue(ctx, { kind: 'updatePlan', requestId: saved.requestId, text: loadDependencyUpdatePlanText(dir, saved.requestId) }); } catch { /* console unavailable */ }
        return { content: [{ type: 'text', text: `Saved dependency update plan ${saved.requestId} for the user's review.` }] };
      } catch (error) { return { content: [{ type: 'text', text: String(error.message ?? error) }], isError: true }; }
    },
  });
}
