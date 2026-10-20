import assert from 'node:assert/strict';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { MONITOR_BLOCK, createVisualizationGuard, registerVisualizationGuard } from '../omp-visualization-guard.mjs';
import { NATIVE_PROMPT_OPTIONS } from '../omp-workspace.mjs';

// Offline proof against the INSTALLED SDK for the host-monitor explanation mode of the one shared guard: a real AgentSession
// with the SDK's mock provider, the real extension events (before_agent_start, tool_call, tool_result, session.compacting,
// session_compact, agent_end), the real before-model-call hook and a real native automatic compaction + continuation.
// No network, no paid model, no production profile, no real process snapshot.
const root = process.env.MONITOR_COMPACTION_TEST_ROOT;
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
const { registerLogSink } = await import(pathToFileURL(path.join(sdk, '..', 'pi-utils', 'src', 'logger.ts')).href);
const diagnostics = [];
const detachDiagnostics = registerLogSink(({ message, context, level }) => { if (/compaction|maintenance/i.test(message) || level === 'error' || level === 'warn') diagnostics.push({ message, context }); });

const FIRST = '11111111-1111-4111-8111-111111111111', SECOND = '22222222-2222-4222-8222-222222222222', THIRD = '33333333-3333-4333-8333-333333333333';
const SNAPSHOT = 'snap-A', OTHER = 'snap-B';
let counter = 0;
const unique = () => Array.from({ length: 40 }, () => `Observation ${counter++} lists row ${Math.random().toString(36).slice(2)}.`).join(' ');
const script = [];
const model = createMockModel({
  contextWindow: 12000,
  handler: context => {
    if ((context.tools?.length ?? 0) === 0) return { content: ['## Summary\nThe frozen snapshot explanation was in progress.'], usage: { input: 100, output: 20, totalTokens: 120 } };
    const next = script.shift();
    return next ?? { content: [`Continuing ${unique().slice(0, 60)}`], usage: { input: 500, output: 100, totalTokens: 600 } };
  },
});

const events = [];
const reads = [];
let guard, ctxRef, shellRan = 0;
const extension = pi => {
  guard = registerVisualizationGuard(pi, createVisualizationGuard());
  const parameters = pi.zod.object({ requestId: pi.zod.string(), snapshotId: pi.zod.string(), section: pi.zod.string(), offset: pi.zod.number(), limit: pi.zod.number() });
  pi.registerTool({ name: 'monitor_snapshot_read', label: 'monitor snapshot read', description: 'fixture', parameters, approval: () => 'read',
    async execute(_id, params) { reads.push({ ...params, active: guard.activeMonitor(ctxRef)?.requestId ?? null }); return { content: [{ type: 'text', text: 'paged' }] }; } });
  pi.registerTool({ name: 'fixture_shell', label: 'shell', description: 'must never run during a monitor explanation', parameters: pi.zod.object({ command: pi.zod.string() }), approval: () => 'read',
    async execute() { shellRan++; return { content: [{ type: 'text', text: 'ran' }] }; } });
  for (const name of ['session_start', 'before_agent_start', 'agent_end', 'session.compacting', 'session_compact', 'tool_call', 'tool_result']) {
    pi.on(name, (event, ctx) => {
      if (ctx?.agent?.kind === 'main') ctxRef = ctx;
      events.push({ name, willContinue: event?.willContinue, prompt: typeof event?.prompt === 'string' ? event.prompt : undefined });
    });
  }
};
const { session, modelRegistry } = await createAgentSession({
  cwd: root, agentDir: dirs.getAgentDir(), model, thinkingLevel: 'off', sessionManager: SessionManager.inMemory(root),
  settings: Settings.isolated({
    'compaction.enabled': false, 'compaction.keepRecentTokens': 400, 'compaction.methodOrder': ['soft'],
    'compaction.thresholdTokens': 9000, 'compaction.asyncEnabled': false, 'compaction.autoContinue': true,
  }),
  customSystemPrompt: 'Explain only the admitted frozen snapshot with its exact reader.',
  tools: [], extensions: [extension], disableExtensionDiscovery: true, hasUI: false, enableMCP: false, skills: [], rules: [], contextFiles: [],
});
(modelRegistry ?? session.modelRegistry).authStorage.keys.setRuntime('mock', 'offline-fixture-key');
await session.extensionRunner?.emit?.({ type: 'session_start' });
// Native automatic maintenance asks the model registry for an available model before it summarizes. Expose the same
// scripted mock through a runtime provider so no provider traffic can leave the process.
(modelRegistry ?? session.modelRegistry).registerProvider('monitor-offline', {
  baseUrl: 'mock://', apiKey: 'offline-fixture-key', api: 'monitor-offline-mock',
  streamSimple: (_model, context, streamOptions) => model.stream(model, context, streamOptions),
  models: [{ id: 'mock-model', name: 'mock-model', reasoning: false, input: ['text'], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 12000, maxTokens: 32768 }],
}, 'monitor-compaction-fixture');
assert.ok(ctxRef, 'the extension context was captured from a real event');
const sessionEvents = [];
session.subscribe(event => { if (/compaction|retry|error/i.test(event.type)) sessionEvents.push({ ...event, result: event.result ? Object.keys(event.result) : undefined }); });

// The very hook a real Architect installs through the captured workspace host, plus the host callbacks omp-srs.mjs installs.
const barrier = [], validations = [], retirements = [];
guard.setNativeHost({
  quiescent: () => true,
  validateMonitor: async stage => { validations.push(`${stage.requestId}:${stage.snapshotId}`); },
  onMonitorRetired: (stage, reason) => retirements.push({ requestId: stage.requestId, snapshotId: stage.snapshotId, reason }),
  onYield() {}, onUncertain() {},
});
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
const until = async (predicate, label) => {
  for (let index = 0; index < 200; index++) { if (predicate()) return; await new Promise(resolve => setTimeout(resolve, 25)); }
  assert.fail(label);
};
// The exact options omp-workspace.mjs::submitPrompt gives every app-created prompt (no template/command expansion).
const options = { ...NATIVE_PROMPT_OPTIONS, streamingBehavior: 'followUp' };
const human = { expandPromptTemplates: false, runCommands: false };
const names = () => events.map(event => event.name);
const toolResults = name => session.messages.filter(message => message.role === 'toolResult' && message.toolName === name);
const ask = (requestId, snapshotId = SNAPSHOT) => guard.admitMonitor(ctxRef, { requestId, snapshotId, message: 'Explain the frozen snapshot.' });
const read = (requestId, snapshotId = SNAPSHOT) => ({ content: [{ type: 'toolCall', name: 'monitor_snapshot_read', arguments: { requestId, snapshotId, section: 'processes', offset: 0, limit: 50 } }], usage: { input: 600, output: 20, totalTokens: 620 } });
const shell = command => ({ content: [{ type: 'toolCall', name: 'fixture_shell', arguments: { command } }], usage: { input: 700, output: 20, totalTokens: 720 } });

{
  for (let index = 0; index < 3; index++) { await session.prompt(`warm-up ${index}: ${unique()}`, human); await settle(); }

  // ---- 1. A real monitor turn: only the exact reader runs; every other tool, and another snapshot, is refused by the real tool_call event
  const first = ask(FIRST);
  script.push(read(FIRST), shell('rm -rf /'), read(FIRST, OTHER), { content: ['The frozen snapshot shows ordinary activity.'], usage: { input: 800, output: 20, totalTokens: 820 } });
  events.length = 0; barrier.length = 0; validations.length = 0;
  await session.prompt(first.text, options);
  await settle();
  const nonce = /nonce=([0-9a-f]{32})/.exec(first.text)[1];
  assert.equal(events.find(event => event.name === 'before_agent_start').prompt.includes(nonce), true, 'native before_agent_start carried the monitor nonce');
  assert.deepEqual(reads.map(item => [item.requestId, item.snapshotId, item.active]), [[FIRST, SNAPSHOT, FIRST]], 'only the exact request+snapshot read ran, inside the active guard');
  assert.equal(shellRan, 0, 'the unrelated tool never ran');
  const refused = [...toolResults('fixture_shell'), ...toolResults('monitor_snapshot_read').filter(message => message.isError)];
  assert.equal(refused.length, 2, 'the real tool_call event refused the shell and the other snapshot');
  assert.ok(refused.every(message => message.isError === true && JSON.stringify(message.content).includes(MONITOR_BLOCK.slice(0, 40))), 'with the monitor guard reason');
  assert.ok(barrier.length >= 4 && validations.length >= 4 && validations.every(item => item === `${FIRST}:${SNAPSHOT}`), 'the provider barrier validated the exact binding before every model call');
  await until(() => retirements.length === 1, 'native quiescence retires the monitor guard');
  assert.deepEqual(retirements, [{ requestId: FIRST, snapshotId: SNAPSHOT, reason: 'settled' }], 'exact request+snapshot, once, at genuine quiescence');
  assert.equal(guard.toolCall({ toolName: 'fixture_shell', input: { command: 'ls' } }, ctxRef), undefined, 'nothing restricts later turns');
  console.log('Monitor turn restricted by the installed tool_call event and retired at quiescence');

  // ---- 2. The real native post-turn threshold path and its nonce-less automatic continuation --------------------
  retirements.length = 0; reads.length = 0;
  const auto = ask(SECOND);
  const before = model.calls.length;
  session.setAutoCompactionEnabled(true);
  // A toolUse stop without a terminal answer preserves unfinished work. The SDK threshold scheduler, not this fixture,
  // performs the compaction and admits its own nonce-less developer continuation.
  script.push(
    { stopReason: 'toolUse', content: ['The first pages are read; more remain.'], usage: { input: 11000, output: 20, totalTokens: 11020 } },
    shell('must not execute after compaction'),
    read(SECOND),
    { content: ['Continuation finished.'], usage: { input: 800, output: 20, totalTokens: 820 } },
  );
  events.length = 0; barrier.length = 0; validations.length = 0;
  await session.prompt(auto.text, options);
  await settle();
  const trace = JSON.stringify({ last: session.messages.at(-1), diagnostics, sessionEvents, events, calls: model.calls.slice(before).length, barriers: barrier, retirements });
  assert.ok(names().includes('session.compacting'), `native automatic compaction did not start: ${trace}`);
  assert.ok(names().includes('session_compact'), `native automatic compaction did not finish: ${trace}`);
  assert.ok(events.some(event => event.name === 'agent_end' && event.willContinue === true), `no native willContinue: ${trace}`);
  const turns = events.filter(event => event.name === 'before_agent_start');
  assert.equal(turns.length, 2, `expected the admitted monitor turn and one native automatic continuation: ${trace}`);
  assert.equal(turns[1].prompt.includes(/nonce=([0-9a-f]{32})/.exec(auto.text)[1]), false, 'the actual native continuation carries no nonce');
  assert.ok(barrier.some(messages => messages.some(message => message.role === 'developer' && message.synthetic && message.attribution === 'agent')), `native continuation did not cross the provider gate: ${trace}`);
  const summaryCall = model.calls.slice(before).find(call => (call.context.tools?.length ?? 0) === 0);
  assert.ok(summaryCall, 'a real summarization request was made');
  assert.ok(JSON.stringify(summaryCall.context.messages).includes(`frozen host-monitor snapshot ${SNAPSHOT}, request ${SECOND}`), 'the monitor context reached the native summary request');
  assert.equal(shellRan, 0, 'the unrelated tool never ran, including after compaction');
  assert.deepEqual(reads.map(item => [item.requestId, item.active]), [[SECOND, SECOND]], 'the exact reader still runs inside the same guard after the automatic continuation');
  assert.ok(validations.length >= 3 && validations.every(item => item === `${SECOND}:${SNAPSHOT}`), 'every provider call after compaction was still validated against the exact binding');
  await until(() => retirements.length === 1, 'the continued turn retires once');
  assert.deepEqual(retirements, [{ requestId: SECOND, snapshotId: SNAPSHOT, reason: 'settled' }], 'compaction and willContinue never retired it early');
  session.setAutoCompactionEnabled(false);
  console.log('Monitor real native compaction, willContinue and nonce-less automatic continuation retained tool restrictions');
}

// ---- 3. A queued monitor prompt never restricts an unrelated real human turn and is retired at quiescence --------
retirements.length = 0; shellRan = 0;
ask(THIRD);
assert.equal(guard.activeMonitor(ctxRef), null, 'pending, not yet running');
script.push(shell('ls'), { content: ['Here is the login explanation.'], usage: { input: 1000, output: 20, totalTokens: 1020 } });
await session.prompt('Please run ls and explain the login bug instead.', human);
await settle();
assert.equal(shellRan, 1, 'the unrelated turn is unrestricted');
await until(() => retirements.length === 1, 'the dropped pending guard is retired at quiescence');
assert.deepEqual(retirements, [{ requestId: THIRD, snapshotId: SNAPSHOT, reason: 'settled' }]);
detachDiagnostics();
await session.dispose?.();
console.log('Installed SDK compaction/willContinue/automatic-continuation host-monitor guard verified');
