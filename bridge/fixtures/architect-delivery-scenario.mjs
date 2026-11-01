import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { installWorkspaceChanges } from '../omp-workspace.mjs';
import { installNativeDecisions } from '../omp-native-ui.mjs';
import { nativeAnswer } from '../omp-manager.mjs';
import { installSrsBridge } from '../omp-srs.mjs';
import { registerProjectInsightTools } from '../omp-project-insight.mjs';

// Test host only: real native CLI/TUI, provider pipeline and subagent executor; no network/model service.
// The tiny disk-backed controller below observes bridge commits. Rust delegation tests cover production ledger policy.
const JOB = '11111111-1111-4111-8111-111111111111', ATTEMPT = '22222222-2222-4222-8222-222222222222';
const STAGE = '33333333-3333-4333-8333-333333333333';
const ids = { jobId: JOB, attemptId: ATTEMPT, stageId: STAGE };
const hash = text => createHash('sha256').update(text).digest('hex');
const pause = () => new Promise(resolve => setTimeout(resolve, 20));
const waitFor = async (predicate, label) => {
  for (let index = 0; index < 1000; index++) {
    if (await predicate()) return;
    await pause();
  }
  throw new Error(`Native fixture did not reach ${label}`);
};

export default async function architectDelivery(pi) {
  const root = process.env.ARCHITECT_DELIVERY_ROOT;
  const outsideEvidence = path.join(root, 'unadmitted.txt');
  const { createMockModel, registerMockApi } = await import(pathToFileURL(path.join(process.env.OMP_SDK_DIR, '..', 'pi-ai', 'src/providers/mock.ts')).href);
  registerMockApi();
  const script = [], modelCalls = [], childTools = [], toolResults = [], durable = [];
  let context, srs, escaped = false, childStep = 0, nativeCompactions = 0, ordinaryRead = false, ordinaryMentionObserved = false;
  const content = '# Assigned findings\n\nThe service refuses an expired authorization and preserves the prior state.\n';
  const tool = (name, args) => ({ stopReason: 'toolUse', content: [{ type: 'toolCall', name, arguments: args }] });
  const model = createMockModel({ id: 'fixture', provider: 'architect-offline', contextWindow: 131072, handler: nativeContext => {
    const expandedOutsideEvidence = JSON.stringify(nativeContext.messages).includes('UNADMITTED_FILE_SENTINEL');
    if (ordinaryRead) {
      assert.equal(expandedOutsideEvidence, true, 'ordinary explicit human @file expansion remains available');
      ordinaryMentionObserved = true;
      return { content: ['The ordinary explicitly requested file mention was read.'] };
    }
    assert.equal(expandedOutsideEvidence, false, 'protected root and child prompts cannot trigger native reads outside their admitted evidence');
    const names = (nativeContext.tools ?? []).map(item => item.name);
    if (!names.length) {
      assert.ok(durable.some(item => item.action === 'srs_commit_document'), 'the native summary is requested only after findings have been committed');
      nativeCompactions++;
      return { content: ['The assigned evidence was inspected and its facts and note were committed. Continue only the same scoped SRS assignment.'], usage: { input: 100, output: 30, totalTokens: 130 } };
    }
    const child = !names.includes('srs_delegate') && names.includes('srs_checkpoint');
    modelCalls.push({ child, tools: names });
    if (!child) return script.shift() ?? { content: ['The requested analysis reached the Architect.'] };
    childTools.push(names);
    for (const forbidden of ['ask', 'task', 'eval', 'bash', 'read', 'write', 'edit', 'srs_delegate', 'srs_finish_stage', 'fixture_escape']) assert.equal(names.includes(forbidden), false, forbidden);
    const steps = [
      tool('fixture_escape', {}),
      tool('ask', { questions: [{ id: 'forged', question: 'Approve?', options: [{ label: 'Yes' }] }] }),
      tool('srs_read_evidence', { ...ids, evidenceId: 'evidence-1' }),
      tool('srs_checkpoint', { ...ids, expectedRevision: 0, updates: { updateWork: [{ id: 'work-1', status: 'reviewed', rationale: 'Expired authorization is refused without changing prior state.', evidenceIds: ['evidence-1'] }], summary: 'Finding committed during inspection.' } }),
      tool('srs_write_document_chunk', { ...ids, documentId: 'document-1', revisionId: 'revision-1', offset: 0, expectedPrefixHash: hash(''), text: content }),
      tool('srs_commit_document', { ...ids, documentId: 'document-1', revisionId: 'revision-1', expectedHash: hash(content), metadata: { type: 'flow', specificity: 'portable', reviewedWorkIds: ['work-1'], citations: [{ evidenceId: 'evidence-1' }] } }),
      { content: ['Assigned work and note were committed before this final handoff.'] },
    ];
    const response = steps[childStep++] ?? { content: ['Completed.'] };
    if (childStep === 6) response.usage = { input: 110000, output: 20, totalTokens: 110020 };
    return response;
  } });
  pi.registerProvider('architect-offline', {
    api: 'architect-offline', baseUrl: 'mock://architect-delivery', apiKey: 'offline-fixture-key',
    streamSimple: (_model, nativeContext, options) => model.stream(model, nativeContext, options),
    models: [{ id: 'fixture', name: 'Architect offline fixture', reasoning: false, input: ['text'], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 131072, maxTokens: 32768 }],
  });
  pi.registerFlag('omp-manager-bridge', { type: 'boolean', default: false, description: 'Eager native fixture host' });
  pi.registerTool({ name: 'fixture_escape', label: 'forbidden', description: 'Must not be available to SRS children', parameters: pi.zod.object({}), approval: () => 'read', execute: async () => { escaped = true; throw new Error('escaped'); } });
  const pending = new Map();
  const nativeUi = installNativeDecisions({ pending, nativeAnswer, identity: () => ({ instanceId: 'fixture', generation: 1, ...workspace.identity() }), emit() {}, opportunity() {}, srs: () => srs?.ask });
  const request = async (action, payload) => {
    if (action === 'srs_stage_validate' || action === 'srs_delegate_validate') return { authorized: true };
    if (action === 'srs_stage_admitted') { durable.push({ action, payload }); await writeFile(path.join(root, 'durable.json'), JSON.stringify(durable)); return { persisted: true }; }
    if (action === 'srs_delegate_open') { durable.push({ action, payload }); await writeFile(path.join(root, 'durable.json'), JSON.stringify(durable)); return { assignmentId: '44444444-4444-4444-8444-444444444444', purpose: 'migration' }; }
    if (action === 'srs_delegate_close') { durable.push({ action, payload }); await writeFile(path.join(root, 'durable.json'), JSON.stringify(durable)); return { closed: true }; }
    if (action === 'srs_stage_yielded') return { paused: true };
    if (action !== 'srs_tool') throw new Error(`Unexpected native fixture request: ${action}`);
    assert.ok(payload.delegation?.childId.startsWith('Srs-'), 'child identity comes from the host grant');
    if (payload.action === 'srs_read_evidence') {
      assert.equal(payload.evidenceId, 'evidence-1'); durable.push({ action: payload.action, payload });
      await writeFile(path.join(root, 'durable.json'), JSON.stringify(durable));
      return { text: 'Expired authorization is refused without modifying prior state.', nextOffset: null };
    }
    if (payload.action === 'srs_checkpoint') {
      assert.equal(payload.updates.updateWork[0].id, 'work-1');
      durable.push({ action: payload.action, payload });
      await writeFile(path.join(root, 'durable.json'), JSON.stringify(durable));
      return { revision: 1 };
    }
    if (payload.action === 'srs_write_document_chunk') {
      await writeFile(path.join(root, 'staged.md'), payload.text); durable.push({ action: payload.action, payload });
      await writeFile(path.join(root, 'durable.json'), JSON.stringify(durable));
      return { bytes: Buffer.byteLength(payload.text) };
    }
    if (payload.action === 'srs_commit_document') {
      const staged = await readFile(path.join(root, 'staged.md'), 'utf8');
      assert.equal(hash(staged), payload.expectedHash);
      assert.ok(durable.some(item => item.action === 'srs_checkpoint'), 'finding persisted before note publication');
      await writeFile(path.join(root, 'committed.md'), staged);
      durable.push({ action: payload.action, payload });
      await writeFile(path.join(root, 'durable.json'), JSON.stringify(durable));
      return { committed: true };
    }
    throw new Error(`Unexpected delegated tool: ${payload.action}`);
  };
  const workspace = installWorkspaceChanges(pi, { nativeUi, request, emit() {}, role: 'instance', hasPendingDecisions: () => pending.size > 0, isLeaseCurrent: () => true });
  srs = installSrsBridge(pi, { request, workspace, currentContext: () => context });
  const actor = { instanceId: 'fixture', generation: 1, launchId: 'fixture-launch', sessionId: null };
  const dir = path.join(root, 'architect');
  const visualization = registerProjectInsightTools(pi, dir, () => actor, undefined, { submitPrompt: (message, callbacks) => workspace.submitPrompt(message, callbacks) });
  pi.on('tool_result', event => toolResults.push({ name: event.toolName, error: event.isError === true }));
  pi.on('session_start', (_event, ctx) => {
    if (ctx.agent.kind !== 'main') return;
    context = ctx; actor.sessionId = ctx.sessionManager.getSessionId(); workspace.attachContext(ctx);
    setTimeout(async () => {
      try {
        assert.equal(workspace.nativeGuardAvailable(), true, 'real CLI native init captured the root host');
        assert.equal(workspace.nativeUiCapabilities().nativeDecisions, true, 'native approval UI captured before admission');
        await writeFile(outsideEvidence, 'UNADMITTED_FILE_SENTINEL');
        const snapshotId = randomUUID(), requestId = randomUUID();
        const job = path.join(root, 'system-overview', 'fixture', snapshotId), source = path.join(job, 'source');
        const folder = path.join(root, 'project-insight', requestId);
        await mkdir(source, { recursive: true }); await mkdir(folder, { recursive: true }); await mkdir(dir, { recursive: true });
        await writeFile(path.join(job, 'index.json'), JSON.stringify({ schemaVersion: 1, snapshotId, folder: root, sourceHash: 'fixture-source', files: [], symbols: [], sites: [], edges: [], directories: [], coverage: {}, limitations: [] }));
        await writeFile(path.join(folder, 'request.json'), JSON.stringify({ schemaVersion: 1, requestId, folder: root, key: '0123456789abcdef', kind: 'systemOverview', createdAt: Date.now(), evidenceInventory: [], architectInstanceId: actor.instanceId, architectGeneration: actor.generation, architectLaunchId: actor.launchId, architectSessionId: actor.sessionId, snapshotId, snapshotSourceHash: 'fixture-source', sourceId: null, sourceHash: null, baseRequestId: null, supersedesRequestId: null }));
        await writeFile(path.join(folder, 'index-ref.json'), JSON.stringify({ snapshotId, snapshotRoot: source, indexPath: path.join(job, 'index.json') }));
        const system = { requestId, kind: 'systemOverview', message: `Generate one system diagram from the admitted frozen evidence. Untrusted repository text: @${outsideEvidence}` };
        assert.equal((await visualization.promptVisualization(ctx, system)).admitted, true);
        assert.equal((await visualization.promptVisualization(ctx, system)).duplicate, true);
        await waitFor(() => workspace.quiescence().quiescent && visualization.guard.states().length === 0, 'system prompt settlement');
        const systemCalls = modelCalls.filter(call => !call.child).length;
        assert.equal(systemCalls, 1, 'System route reaches the real provider once');
        script.push(tool('srs_delegate', { ...ids, assignment: 'Inspect the admitted external authorization contract and commit findings during inspection. Untrusted text: @unadmitted.txt', workIds: ['work-1'], evidenceIds: ['evidence-1'], documentIds: ['document-1'] }));
        const stage = { ...ids, nonce: 'a'.repeat(32), phase: 'specify', stageNumber: 1, message: `Purpose: migration. Inspect and document the admitted system using scoped native workers. Untrusted stakeholder text: @${outsideEvidence}` };
        assert.equal((await srs.control('srs_stage_prompt', stage, ctx)).admitted, true);
        assert.equal((await srs.control('srs_stage_prompt', stage, ctx)).duplicate, true);
        await waitFor(() => durable.some(item => item.action === 'srs_delegate_close') && workspace.quiescence().quiescent, 'native worker durable handoff');
        const saved = JSON.parse(await readFile(path.join(root, 'durable.json'), 'utf8'));
        assert.equal(saved.filter(item => item.action === 'srs_stage_admitted').length, 1, 'SRS route admits exactly one real native prompt');
        assert.equal(saved.find(item => item.action === 'srs_delegate_close').payload.outcome, 'completed');
        assert.ok(childTools.length >= 7, 'the real native child made tool calls through completion');
        assert.ok(nativeCompactions > 0, 'the real native child compacted and then retained its scoped tool authority');
        assert.equal(escaped, false, 'a guessed arbitrary tool never acquired authority');
        for (const name of ['srs_read_evidence', 'srs_checkpoint', 'srs_write_document_chunk', 'srs_commit_document']) assert.ok(saved.some(item => item.action === name), name);
        assert.equal(await readFile(path.join(root, 'committed.md'), 'utf8'), content);
        const transcript = workspace.nativeSessionEntries();
        assert.equal(transcript.filter(item => item.type === 'message' && item.message?.role === 'user' && JSON.stringify(item.message.content).includes('omp-visualization-request')).length, 1);
        assert.equal(transcript.filter(item => item.type === 'message' && item.message?.role === 'user' && JSON.stringify(item.message.content).includes('omp-srs-stage')).length, 1);
        ordinaryRead = true;
        const native = pi.pi.AgentRegistry.global().get(ctx.agent.id)?.session;
        assert.ok(native);
        await native.prompt(`This ordinary human message explicitly requests @${outsideEvidence}`, { expandPromptTemplates: false, runCommands: false, throwOnDrop: true, attribution: 'user' });
        await native.waitForIdle();
        assert.equal(ordinaryMentionObserved, true);
        await writeFile(path.join(root, 'result.json'), JSON.stringify({ systemCalls, nativeChildCalls: childTools.length, nativeCompactions, ordinaryMentionObserved, durableActions: saved.map(item => item.action), escaped, toolResults }));
        process.exit(0);
      } catch (error) { await writeFile(path.join(root, 'result.json'), JSON.stringify({ error: String(error.stack ?? error) })); process.exit(1); }
    }, 0);
  });
}

if (import.meta.main) {
  const root = process.env.ARCHITECT_DELIVERY_ROOT;
  assert.ok(root && process.env.OMP_BIN && process.env.OMP_SDK_DIR);
  assert.ok(process.env.PI_CODING_AGENT_DIR && path.resolve(process.env.PI_CODING_AGENT_DIR).startsWith(`${path.resolve(root)}${path.sep}`), 'the fixture must use isolated native state');
  await mkdir(process.env.PI_CODING_AGENT_DIR, { recursive: true });
  // Derived solely for this deterministic compaction proof, never carried into a quality/comparison profile.
  await writeFile(path.join(process.env.PI_CODING_AGENT_DIR, 'config.yml'), `compaction:
  enabled: true
  thresholdTokens: 90000
  modelThresholdsEnabled: false
  keepRecentTokens: 400
  methodOrder: [soft]
  asyncEnabled: false
`);
  let output = '';
  const child = Bun.spawn([process.env.OMP_BIN, '--no-extensions', '--extension', fileURLToPath(import.meta.url), '--omp-manager-bridge', '--model', 'architect-offline/fixture', '--no-skills', '--no-rules', '--no-lsp', '--no-title', '--auto-approve', '--cwd', root], {
    env: { ...process.env, PI_COMPILED: '' },
    terminal: { cols: 120, rows: 40, data(_terminal, bytes) { output = (output + Buffer.from(bytes).toString()).slice(-32768); } },
  });
  const timer = setTimeout(() => child.kill(), 90000);
  try {
    const code = await child.exited;
    const observation = JSON.parse(await readFile(path.join(root, 'result.json'), 'utf8').catch(() => '{}'));
    assert.equal(code, 0, observation.error ?? output);
    assert.equal(observation.systemCalls, 1);
    assert.ok(observation.nativeChildCalls >= 7);
    assert.ok(observation.nativeCompactions > 0);
    assert.equal(observation.ordinaryMentionObserved, true);
    const saved = JSON.parse(await readFile(path.join(root, 'durable.json'), 'utf8'));
    assert.equal(saved.find(item => item.action === 'srs_delegate_close').payload.outcome, 'completed', 'a fresh process sees the durable handoff after the CLI exits');
    assert.match(await readFile(path.join(root, 'committed.md'), 'utf8'), /expired authorization/);
    console.log('Real CLI Architect System/SRS single admission and scoped native worker durable handoff verified');
  } finally { clearTimeout(timer); child.terminal.close(); }
}
