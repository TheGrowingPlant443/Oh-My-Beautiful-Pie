import assert from 'node:assert/strict';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { createVisualizationGuard, registerVisualizationGuard } from '../omp-visualization-guard.mjs';

// Offline proof against the INSTALLED SDK: a real AgentSession with the SDK's mock provider, the real extension events
// (before_agent_start, tool_call, tool_result, session.compacting, session_compact, agent_end), the real before-model-call
// hook and a real native compaction. No network, no paid model, no production profile.
const root = process.env.SRS_COMPACTION_TEST_ROOT;
const sdk = process.env.OMP_SDK_DIR;
assert.ok(root && sdk);
const fromSdk = relative => import(pathToFileURL(path.join(sdk, relative)).href);
const dirs = await import(pathToFileURL(Bun.resolveSync('@oh-my-pi/pi-utils/dirs', sdk)).href);
dirs.setProfile('default');
for (const actual of [dirs.getAgentDir(), dirs.getConfigRootDir(), dirs.getNativesDir(), dirs.getSessionsDir()]) {
  const relative = path.relative(root, actual);
  assert.ok(relative !== '..' && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative), `storage escaped scratch root: ${actual}`);
}
const { createMockModel, registerMockApi } = await import(pathToFileURL(path.join(sdk, '..', 'pi-ai', 'src', 'providers', 'mock.ts')).href);
const { createAgentSession } = await fromSdk('src/sdk.ts');
const { Settings } = await fromSdk('src/config/settings.ts');
const { SessionManager } = await fromSdk('src/session/session-manager.ts');
registerMockApi();
const { resolveModelCompactionSettings } = await fromSdk('src/session/model-compaction-threshold.ts');
const { registerLogSink } = await import(pathToFileURL(path.join(sdk, '..', 'pi-utils', 'src', 'logger.ts')).href);
const diagnostics = [];
const detachDiagnostics = registerLogSink(({ message, context, level }) => { if (/compaction|maintenance/i.test(message) || level === 'error' || level === 'warn') diagnostics.push({ message, context }); });

const JOB = '11111111-1111-4111-8111-111111111111', ATTEMPT = '22222222-2222-4222-8222-222222222222', STAGE = '33333333-3333-4333-8333-333333333333';
const NONCE = 'a1b2c3d4e5f60718293a4b5c6d7e8f90';
const ids = { jobId: JOB, attemptId: ATTEMPT, stageId: STAGE };
let counter = 0;
const unique = () => Array.from({ length: 40 }, () => `Rule ${counter++} covers case ${crypto.randomUUID().replaceAll('-', '').slice(0, 11)}.`).join(' ');
const script = [];
const model = createMockModel({
  contextWindow: 12000,
  handler: context => {
    if ((context.tools?.length ?? 0) === 0) return { content: ['## Summary\nThe SRS stage was in progress.'], usage: { input: 100, output: 20, totalTokens: 120 } };
    const next = script.shift();
    return next ?? { content: [`Continuing ${unique().slice(0, 60)}`], usage: { input: 500, output: 100, totalTokens: 600 } };
  },
});

const events = [];
const toolOutcomes = [];
let guard, ctxRef, shellRan = false;
const extension = pi => {
  guard = registerVisualizationGuard(pi, createVisualizationGuard());
  const parameters = pi.zod.object({ jobId: pi.zod.string(), attemptId: pi.zod.string(), stageId: pi.zod.string(), section: pi.zod.string() });
  pi.registerTool({ name: 'srs_inspect', label: 'srs inspect', description: 'fixture', parameters, approval: () => 'read', async execute() { return { content: [{ type: 'text', text: 'inspected' }] }; } });
  pi.registerTool({ name: 'fixture_shell', label: 'shell', description: 'must never run during a stage', parameters: pi.zod.object({ command: pi.zod.string() }), approval: () => 'read', async execute() { shellRan = true; return { content: [{ type: 'text', text: 'ran' }] }; } });
  for (const name of ['session_start', 'before_agent_start', 'agent_end', 'session.compacting', 'session_compact', 'tool_call', 'tool_result']) {
    pi.on(name, (event, ctx) => {
      if (ctx?.agent?.kind === 'main') ctxRef = ctx;
      events.push({ name, willContinue: event?.willContinue, prompt: typeof event?.prompt === 'string' ? event.prompt : undefined });
      if (name === 'tool_result') toolOutcomes.push({ tool: event.toolName, error: event.isError === true, text: JSON.stringify(event.content ?? '') });
    });
  }
};
const { session, modelRegistry } = await createAgentSession({
  cwd: root, agentDir: dirs.getAgentDir(), model, thinkingLevel: 'off', sessionManager: SessionManager.inMemory(root),
  settings: Settings.isolated({
    'compaction.enabled': false, 'compaction.keepRecentTokens': 400, 'compaction.methodOrder': ['soft'],
    'compaction.thresholdTokens': 9000, 'compaction.asyncEnabled': false, 'compaction.autoContinue': true,
  }),
  customSystemPrompt: 'Inspect only admitted evidence. Follow the active stage tools.',
  tools: [], extensions: [extension], disableExtensionDiscovery: true, hasUI: false, enableMCP: false, skills: [], rules: [], contextFiles: [],
});
(modelRegistry ?? session.modelRegistry).authStorage.keys.setRuntime('mock', 'offline-fixture-key');
await session.extensionRunner?.emit?.({ type: 'session_start' });
// Native automatic maintenance asks the model registry for an available model before it summarizes. Expose the same
// scripted mock through a runtime provider so no provider traffic can leave the process.
(modelRegistry ?? session.modelRegistry).registerProvider('srs-offline', {
  baseUrl: 'mock://', apiKey: 'offline-fixture-key', api: 'srs-offline-mock',
  streamSimple: (_model, context, streamOptions) => model.stream(model, context, streamOptions),
  models: [{ id: 'mock-model', name: 'mock-model', reasoning: false, input: ['text'], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 12000, maxTokens: 32768 }],
}, 'srs-compaction-fixture');
assert.ok(ctxRef, 'the extension context was captured from a real event');
const sessionEvents = [];
session.subscribe(event => { if (/compaction|retry|error/i.test(event.type)) sessionEvents.push({ ...event, result: event.result ? Object.keys(event.result) : undefined }); });

// The very hook a real Architect installs through the captured workspace host.
const barrier = [];
session.agent.addBeforeModelCallHook(async (_signal, context) => {
  barrier.push(context.messages.map(message => ({ role: message.role, synthetic: message.synthetic === true, attribution: message.attribution ?? null })));
  await guard.beforeModelCall(ctxRef, context);
});
const settle = async () => {
  for (let index = 0; index < 400; index++) {
    if (!session.isStreaming && !session.isCompacting && session.hasPostPromptWork === false && !session.agent.hasQueuedMessages()) return;
    await new Promise(resolve => setTimeout(resolve, 25));
  }
  throw new Error('session never settled');
};
const options = { expandPromptTemplates: false, runCommands: false };
const names = () => events.map(event => event.name);

{
  for (let index = 0; index < 3; index++) { await session.prompt(`warm-up ${index}: ${unique()}`, options); await settle(); }

  // ---- 1. A real SRS stage turn: bound tools run, every other tool is refused by the real tool_call event ----------
  const admitted = guard.admitSrs(ctxRef, { ...ids, nonce: NONCE, phase: 'specify', stageNumber: 1, message: 'Specify the admitted obligations.' });
  script.push(
    { content: [{ type: 'toolCall', name: 'srs_inspect', arguments: { ...ids, section: 'work' } }], usage: { input: 600, output: 20, totalTokens: 620 } },
    { content: [{ type: 'toolCall', name: 'fixture_shell', arguments: { command: 'rm -rf /' } }], usage: { input: 700, output: 20, totalTokens: 720 } },
    { content: ['Stage turn finished.'], usage: { input: 800, output: 20, totalTokens: 820 } },
  );
  events.length = 0; barrier.length = 0;
  await session.prompt(admitted.text, { ...options, streamingBehavior: 'followUp', throwOnDrop: true, attribution: 'agent' });
  await settle();
  assert.equal(events.find(event => event.name === 'before_agent_start').prompt.includes(NONCE), true, 'native before_agent_start carried the stage nonce');
  assert.equal(toolOutcomes.find(item => item.tool === 'srs_inspect').error, false, 'the bound stage tool ran');
  assert.equal(shellRan, false, 'the unrelated tool never ran');
  const refused = session.messages.find(message => message.role === 'toolResult' && message.toolName === 'fixture_shell');
  assert.ok(refused?.isError === true && JSON.stringify(refused.content).includes('visualization analysis or a bound SRS stage'), 'the real tool_call event blocked it with the guard reason');
  assert.equal(guard.activeSrs(ctxRef)?.stageId, STAGE);
  assert.ok(barrier.length >= 3, 'the provider barrier ran before every model call');
  console.log('SRS stage tools restricted by the installed tool_call event');

  // ---- 2. Force the real native post-turn threshold path and its automatic continuation ----------------------
  guard.release();
  const autoStageId = '55555555-5555-4555-8555-555555555555';
  const autoIds = { ...ids, stageId: autoStageId };
  const auto = guard.admitSrs(ctxRef, { ...autoIds, nonce: 'd'.repeat(32), phase: 'specify', stageNumber: 2, message: 'Continue the recorded specification stage.' });
  const before = model.calls.length;
  session.setAutoCompactionEnabled(true);
  // A toolUse stop without a terminal answer preserves unfinished work. The SDK threshold scheduler, not this
  // fixture, performs the compaction and admits its own nonce-less developer continuation.
  script.push(
    { stopReason: 'toolUse', content: ['Checkpoint recorded; specification remains in progress.'], usage: { input: 11000, output: 20, totalTokens: 11020 } },
    { content: [{ type: 'toolCall', name: 'fixture_shell', arguments: { command: 'must not execute after compaction' } }], usage: { input: 600, output: 20, totalTokens: 620 } },
    { content: [{ type: 'toolCall', name: 'srs_inspect', arguments: { ...autoIds, section: 'checkpoint' } }], usage: { input: 700, output: 20, totalTokens: 720 } },
    { content: ['Continuation checkpoint reached.'], usage: { input: 800, output: 20, totalTokens: 820 } },
  );
  events.length = 0; barrier.length = 0;
  await session.prompt(auto.text, { ...options, streamingBehavior: 'followUp', throwOnDrop: true, attribution: 'agent' });
  await settle();
  const trace = JSON.stringify({ modelWindow: session.model.contextWindow, policy: resolveModelCompactionSettings(session.settings, session.model), last: session.messages.at(-1), diagnostics, sessionEvents, events, calls: model.calls.slice(before).length, barriers: barrier });
  assert.ok(names().includes('session.compacting'), `native automatic compaction did not start: ${trace}`);
  assert.ok(names().includes('session_compact'), `native automatic compaction did not finish: ${trace}`);
  assert.ok(events.some(event => event.name === 'agent_end' && event.willContinue === true), `no native willContinue: ${trace}`);
  const turns = events.filter(event => event.name === 'before_agent_start');
  assert.equal(turns.length, 2, `expected the admitted stage and one native automatic continuation: ${trace}`);
  assert.equal(turns[1].prompt.includes('d'.repeat(32)), false, 'the actual native continuation carries no stage nonce');
  assert.ok(barrier.some(messages => messages.some(message => message.role === 'developer' && message.synthetic && message.attribution === 'agent')), `native continuation did not cross the provider gate: ${trace}`);
  const summaryCall = model.calls.slice(before).find(call => (call.context.tools?.length ?? 0) === 0);
  assert.ok(summaryCall, 'a real summarization request was made');
  assert.ok(JSON.stringify(summaryCall.context.messages).includes(`SRS stage ${autoStageId}, job ${JOB}, attempt ${ATTEMPT}, phase specify`), 'stage context reached the native summary request');
  assert.equal(guard.activeSrs(ctxRef)?.stageId, autoStageId, 'the nonce-less continuation retained the exact stage authorization');
  assert.equal(shellRan, false, 'the unrelated tool never ran, including after compaction');
  const continuedRead = session.messages.find(message => message.role === 'toolResult' && message.toolName === 'srs_inspect' && JSON.stringify(message.content).includes('inspected'));
  assert.ok(continuedRead && !continuedRead.isError, 'the exact stage read still runs after automatic continuation');
  session.setAutoCompactionEnabled(false);
  console.log('SRS real native compaction, willContinue and nonce-less automatic continuation retained tool restrictions');
}

// ---- 4. A fresh stage, then a real unrelated human prompt yields it and is unrestricted ---------------------------
let yielded = null;
guard.release();
const stage = guard.admitSrs(ctxRef, { ...ids, stageId: '44444444-4444-4444-8444-444444444444', nonce: 'b'.repeat(32), phase: 'survey', stageNumber: 2, message: 'Survey again.' });
guard.setNativeHost({ quiescent: () => true, onYield: value => { yielded = value; }, onUncertain() {}, async validateSrs() {} });
script.push({ content: ['Surveyed.'], usage: { input: 900, output: 20, totalTokens: 920 } });
await session.prompt(stage.text, { ...options, streamingBehavior: 'followUp', throwOnDrop: true, attribution: 'agent' });
await settle();
assert.equal(guard.activeSrs(ctxRef)?.phase, 'survey');
script.push({ content: ['Here is the login explanation.'], usage: { input: 1000, output: 20, totalTokens: 1020 } });
await session.prompt('Please explain the login bug instead.', options);
await settle();
assert.equal(yielded?.stageId, '44444444-4444-4444-8444-444444444444', 'an unrelated human turn yielded the SRS stage');
assert.equal(guard.srs(ctxRef), null);
assert.equal(guard.toolCall({ toolName: 'fixture_shell', input: { command: 'ls' } }, ctxRef), undefined, 'unrelated turns are unrestricted again');
detachDiagnostics();
await session.dispose?.();
console.log('Installed SDK compaction/willContinue/automatic-continuation SRS guard verified');
