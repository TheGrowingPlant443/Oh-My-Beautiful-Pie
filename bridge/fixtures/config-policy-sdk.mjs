import assert from 'node:assert/strict';
import { chmod, mkdir, mkdtemp, writeFile, readFile, rm, stat } from 'node:fs/promises';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { configSetupResources, configSetupSettings, installConfigPolicy, prepareConfigPolicy, registerConfigSetupChild } from '../omp-config-policy.mjs';
import { assetMetadata, bundleDigest, requiresAssetExecutionReview } from '../config-bundle.mjs';
import { installLocalModelPolicy } from '../local-ai-models.mjs';
import { runConfig } from '../omp-config.mjs';

// Real native Settings, AgentSession model_changed and capability reloads. The provider is an in-process mock.
const root = process.env.CONFIG_POLICY_TEST_ROOT;
const sdk = process.env.OMP_SDK_DIR;
assert.ok(root && sdk);
const fromSdk = file => import(pathToFileURL(path.join(sdk, file)).href);
const dirs = await import(pathToFileURL(Bun.resolveSync('@oh-my-pi/pi-utils/dirs', sdk)).href);
dirs.setProfile('default');
for (const actual of [dirs.getAgentDir(), dirs.getConfigRootDir(), dirs.getNativesDir()]) {
  const relative = path.relative(root, actual);
  assert.ok(relative !== '..' && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative));
}
const { Settings } = await fromSdk('src/config/settings.ts');
const { createAgentSession } = await fromSdk('src/sdk.ts');
const { createSubagentSettings } = await fromSdk('src/task/executor.ts');
const { initializeExtensions } = await fromSdk('src/modes/runtime-init.ts');
const startExtensions = session => initializeExtensions(session, {
  reportSendError: (_action, error) => { throw error; },
  reportRuntimeError: error => { throw new Error(`Native extension ${error.event}: ${error.error}`); },
});
const { SessionManager } = await fromSdk('src/session/session-manager.ts');
const { lookup } = await fromSdk('src/config/registry.ts');
const { getActiveSkills } = await fromSdk('src/extensibility/skills.ts');
const { getActiveRules } = await fromSdk('src/capability/rule.ts');
const { createMockModel, registerMockApi } = await import(pathToFileURL(path.join(sdk, '..', 'pi-ai/src/providers/mock.ts')).href);
registerMockApi();
const app = path.join(root, 'app');
await mkdir(app, { recursive: true });
async function save(id, yaml, assets = {}, approved = true) {
  const directory = path.join(app, 'global-configs', id);
  await mkdir(directory, { recursive: true });
  await writeFile(path.join(directory, 'config.yml'), yaml);
  const records = Object.entries(assets).map(([path, value]) => {
    const wrapped = value && typeof value === 'object' && !Buffer.isBuffer(value);
    return { path, content: wrapped ? value.content : value, mode: wrapped ? value.mode ?? 0o644 : 0o644 };
  });
  const metadata = assetMetadata(records);
  await writeFile(path.join(directory, 'setup.json'), JSON.stringify({ version: 1, name: id, assets: records.map(asset => asset.path), assetMetadata: metadata, requiresExecutionReview: requiresAssetExecutionReview(records), executableReview: approved, contentSha256: bundleDigest(yaml, metadata) }));
  for (const asset of records) {
    const destination = path.join(directory, 'assets', asset.path);
    await mkdir(path.dirname(destination), { recursive: true });
    await writeFile(destination, asset.content);
    await chmod(destination, asset.mode);
  }
  return path.join(directory, 'config.yml');
}
async function managedSettings(files, defaultPath, selection, overrides = {}) {
  const directory = await mkdtemp(path.join(app, '.config-session-'));
  await chmod(directory, 0o700);
  const prepared = await prepareConfigPolicy({ root: app, directory, files, defaultPath, selection, cwd: root, sdkDir: sdk });
  process.env.OMP_MANAGER_CONFIG_SLOT_MANIFEST = JSON.stringify(prepared.slotManifest);
  return { directory, settings: await Settings.loadIsolated({ cwd: root, agentDir: dirs.getAgentDir(), configFiles: prepared.configFiles, overrides }) };
}
const assigned = await save('assigned', 'temperature: 0.2\nmodelRoles:\n  default: paid-provider/must-never-select\n  task: mock/second\n', {
  'AGENTS.md': 'SETUP_AGENT_A: Keep the session offline.',
  'rules/.omp-context/1-AGENTS.md': 'SETUP_ANCESTOR_A: Preserved additional active AGENTS instructions.',
  'RULES.md': 'SETUP_RULE_A: Never replace the selected mock model.',
  'rules/scoped.md': '---\nalwaysApply: true\n---\nSETUP_SCOPED_A',
  'skills/setup-fixture/SKILL.md': '---\nname: setup-fixture\ndescription: Offline configuration fixture\n---\nFixture skill body',
  'skills/setup-fixture/manual.bin': Buffer.alloc(1024 * 1024 + 3, 0xa5),
});
const secondConfig = await save('second', 'temperature: 0.7\n', { 'AGENTS.md': 'SETUP_AGENT_B: Isolated second-model guidance.' });
const fallback = await save('fallback', 'temperature: 0.4\ntopP: 0.6\n');
const assetsOnly = await save('assets-only', '{}\n', { 'RULES.md': 'SETUP_ASSET_ONLY: Instructions without YAML settings.' });
const chosen = await save('chosen', 'topK: 12\n');
const unreviewed = await save('unreviewed', 'temperature: 0.9\n', { 'skills/unreviewed/SKILL.md': '---\nname: unreviewed\ndescription: Must not load\n---\nNot approved' }, false);
const mcp = await save('mcp', 'temperature: 0.9\n', { 'mcp.json': '\uFEFF{"mcpServers":{"must-not-spawn":{"command":"must-not-exist"}}}' });
const invalid = await save('invalid', 'temperature: not-a-number\n');
const traversal = await save('traversal', 'temperature: 0.9\n');
await writeFile(path.join(path.dirname(traversal), 'setup.json'), JSON.stringify({ version: 1, name: 'traversal', assets: ['rules/../../escape.md'], requiresExecutionReview: false, executableReview: false }));
const assignmentFile = path.join(app, 'model-configs.json');
const firstModel = createMockModel({ id: 'first', handler: () => ({ content: ['Offline fixture response'] }) });
const secondModel = createMockModel({ id: 'second' });
await writeFile(assignmentFile, '\uFEFF' + JSON.stringify([{ model: 'mock/first', configPath: assigned }, { model: 'mock/second', configPath: secondConfig }]));
await writeFile(path.join(app, 'global-config.json'), JSON.stringify({ version: 1, path: fallback }));
process.env.OMP_MANAGER_CONFIG_ROOT = app;
process.env.OMP_MANAGER_CONFIG_SELECTION = '';
process.env.OMP_MANAGER_CONFIG_FILES = JSON.stringify([fallback]);
process.env.OMP_MANAGER_CONFIG_USE_DEFAULT = 'true';
process.env.OMP_MANAGER_CONFIG_DEFAULT = fallback;
let policy, ctx;
const events = [];
let receiveState;
const extension = pi => {
  policy = installConfigPolicy(pi, { emit: (kind, state) => { events.push({ kind, ...state }); receiveState?.(state); } });
  pi.on('session_start', async (_event, context) => { ctx = context; await policy.attachContext(context); });
  pi.registerTool({
    name: 'fixture_boundary_step', label: 'fixture boundary', description: 'Offline model-change boundary proof',
    parameters: pi.zod.object({}), approval: () => 'read',
    async execute() {
      await session.setModel(secondModel, 'default', { persist: false });
      assert.equal(value('temperature'), 0.2, 'active native tool execution does not mutate its existing setup');
      return { content: [{ type: 'text', text: 'The native model changed; the next provider call is a safe boundary.' }] };
    },
  });
};
const externalSkills = path.join(root, 'native-custom-skills');
await mkdir(path.join(externalSkills, 'outside'), { recursive: true });
await writeFile(path.join(externalSkills, 'outside', 'SKILL.md'), '---\nname: native-outside\ndescription: Existing native custom directory\n---\nOutside the selected app setup');
await writeFile(path.join(dirs.getAgentDir(), 'config.yml'), Bun.YAML.stringify({
  temperature: 0.1, topK: 3, compaction: { enabled: false }, retry: { enabled: false },
  skills: { customDirectories: [externalSkills] }, modelRoles: { nativeFixtureAlias: 'mock/first' },
}));
const preparedRoot = await managedSettings([fallback], fallback, { source: 'model', path: assigned });
const settings = preparedRoot.settings;
const { session, modelRegistry } = await createAgentSession({
  cwd: root, agentDir: dirs.getAgentDir(), model: firstModel, thinkingLevel: 'off', settings,
  sessionManager: SessionManager.inMemory(root), tools: [], extensions: [extension],
  disableExtensionDiscovery: true, hasUI: false, enableMCP: false,
});
(modelRegistry ?? session.modelRegistry).authStorage.keys.setRuntime('mock', 'offline-fixture-key');
const nativeEvents = [];
const unsubscribe = session.subscribe(event => nativeEvents.push(event.type));
await startExtensions(session);
const value = id => lookup(id).get(session.settings);
const settlePolicy = () => policy.attachContext(ctx);
const prompt = () => session.agent.state.systemPrompt.join('\n');
async function switchNative(model, configPath) {
  const changed = new Promise(resolve => { receiveState = state => { if (state.model === `${model.provider}/${model.id}` && state.configPath === configPath && !state.error) resolve(); }; });
  await session.setModel(model, 'default', { persist: false });
  await changed;
  receiveState = undefined;
}
async function exerciseOrderedOverlays() {
  const globalPath = path.join(dirs.getAgentDir(), 'config.yml');
  const globalBefore = Bun.YAML.parse(await readFile(globalPath, 'utf8'));
  globalBefore.retry.fallbackChains = { 'mock/*': ['mock/second'] };
  await writeFile(globalPath, Bun.YAML.stringify(globalBefore));
  const a = await save('ordered-A', 'temperature: 0.2\ntopP: 0.6\n', {
    'AGENTS.md': 'ORDERED_A_CONTEXT',
    'rules/a-only.md': '---\nalwaysApply: true\n---\nORDERED_A_ONLY_RULE',
    'skills/ordered/SKILL.md': '---\nname: ordered-overlay-skill\ndescription: Ordered overlay fixture\n---\nRead shared.txt beside this skill.',
    'skills/ordered/shared.txt': 'A supporting bytes',
    'skills/ordered/a-only.bin': Buffer.from([0, 255, 8, 9]),
    'skills/ordered/helper.sh': { content: '#!/bin/sh\nprintf ordered\n', mode: 0o755 },
  });
  const b = await save('ordered-B', 'temperature: 0.7\nretry:\n  fallbackChains: null\n', {
    'AGENTS.md': 'ORDERED_B_CONTEXT',
    'skills/ordered/shared.txt': 'B supporting bytes',
  });
  const m = await save('ordered-M', 'temperature: 0.8\ntopK: 17\nretry:\n  maxRetries: 11\n', { 'AGENTS.md': 'ORDERED_M_CONTEXT' });
  const x = path.join(root, 'ordered-X.yml'), y = path.join(root, 'ordered-Y.yml');
  await writeFile(x, 'temperature: 0.3\ntopK: 5\n');
  await writeFile(y, 'topK: 9\n');
  const files = [a, x, b, y];
  const model = createMockModel({ id: 'ordered-overlay', handler: () => ({ content: ['Offline ordered response'] }) });
  await writeFile(assignmentFile, JSON.stringify([{ model: 'mock/ordered-overlay', configPath: m }]));
  process.env.OMP_MANAGER_CONFIG_FILES = JSON.stringify(files);
  process.env.OMP_MANAGER_CONFIG_DEFAULT = a;
  process.env.OMP_MANAGER_CONFIG_SELECTION = b;
  let randomCounter = 0;
  async function open() {
    const selected = process.env.OMP_MANAGER_CONFIG_SELECTION
      ? { source: 'session', path: process.env.OMP_MANAGER_CONFIG_SELECTION }
      : { source: 'model', path: m };
    const slot = {};
    const prepared = await managedSettings(files, a, selected);
    slot.directory = prepared.directory;
    const base = prepared.settings;
    const created = await createAgentSession({
      cwd: root, agentDir: dirs.getAgentDir(), model, thinkingLevel: 'off', settings: base,
      sessionManager: SessionManager.inMemory(root), tools: [], disableExtensionDiscovery: true,
      hasUI: false, enableMCP: false, bindProcessState: false, agentId: `ordered-root-${randomCounter++}`,
      extensions: [pi => {
        slot.policy = installConfigPolicy(pi);
        pi.on('session_start', async (_event, nativeCtx) => { slot.ctx = nativeCtx; await slot.policy.attachContext(nativeCtx); });
      }],
    });
    slot.session = created.session;
    assert.equal(slot.session.agent.temperature, selected.source === 'session' ? 0.7 : 0.3, 'private settings are loaded by the native constructor before session_start');
    if (selected.source === 'session') assert.deepEqual(lookup('retry.fallbackChains').get(slot.session.settings), {}, 'native-valid record null is applied before the constructor');
    await slot.session.sessionManager.setSessionName('Offline ordered fixture');
    slot.session.modelRegistry.authStorage.keys.setRuntime('mock', 'offline-fixture-key');
    slot.stop = slot.session.subscribe(() => {});
    await startExtensions(slot.session);
    return slot;
  }
  const first = await open();
  const current = id => lookup(id).get(first.session.settings);
  const activeSkill = () => first.session.skills.find(skill => skill.name === 'ordered-overlay-skill');
  let finalGeneration;
  try {
    assert.ok(!first.policy.snapshot().error);
    assert.deepEqual(first.policy.snapshot().configFiles, files);
    assert.deepEqual(first.policy.snapshot().setupPaths, [a, b]);
    assert.equal(current('temperature'), 0.7, 'B overrides X before its explicit slot');
    assert.deepEqual(current('retry.fallbackChains'), {}, 'record tombstone clears nonempty native global keys');
    assert.equal(current('topP'), 0.6, 'A-only fields survive selection of B');
    assert.equal(current('topK'), 9, 'Y after the setup slot retains native overlay precedence');
    assert.equal(current('retry.maxRetries'), lookup('retry.maxRetries').default, 'explicit B does not inherit automatic model M');
    assert.match(first.session.agent.state.systemPrompt.join('\n'), /ORDERED_B_CONTEXT/);
    assert.match(first.session.agent.state.systemPrompt.join('\n'), /ORDERED_A_ONLY_RULE/);
    assert.doesNotMatch(first.session.agent.state.systemPrompt.join('\n'), /ORDERED_A_CONTEXT/);
    const skill = activeSkill();
    assert.ok(skill);
    assert.equal(await readFile(path.join(skill.baseDir, 'shared.txt'), 'utf8'), 'B supporting bytes', 'native skill entry from A sees B supporting file in one coherent root');
    assert.deepEqual(await readFile(path.join(skill.baseDir, 'a-only.bin')), Buffer.from([0, 255, 8, 9]));
    if (process.platform !== 'win32') assert.equal((await stat(path.join(skill.baseDir, 'helper.sh'))).mode & 0o777, 0o755);
    const firstGeneration = path.resolve(skill.baseDir, '../../..');
    const copiedSupportingFile = path.join(skill.baseDir, 'shared.txt');
    await writeFile(copiedSupportingFile, 'Changed copied file after review');
    assert.ok((await first.policy.attachContext(first.ctx)).error, 'mutation of a copied runtime asset is rejected even though source snapshots are intact');
    await writeFile(copiedSupportingFile, 'B supporting bytes');
    assert.ok(!(await first.policy.attachContext(first.ctx)).error);
    if (process.platform !== 'win32') {
      const executable = path.join(activeSkill().baseDir, 'helper.sh');
      await chmod(executable, 0o644);
      assert.ok((await first.policy.attachContext(first.ctx)).error, 'mode-only changes invalidate reviewed runtime assets');
      await chmod(executable, 0o755);
      assert.ok(!(await first.policy.attachContext(first.ctx)).error);
    }
    const exported = await runConfig({ sdkDir: sdk, cwd: root, profile: 'default', action: 'export',
      configFiles: first.policy.snapshot().configFiles, setupPaths: first.policy.snapshot().setupPaths });
    const yaml = Bun.YAML.parse(exported.yaml);
    assert.equal(yaml.temperature, 0.7);
    assert.equal(yaml.topP, 0.6);
    const shared = exported.assets.find(asset => asset.path === 'skills/ordered/shared.txt');
    assert.ok(shared);
    assert.equal(shared.file ? await readFile(shared.file.sourcePath, 'utf8') : shared.content, 'B supporting bytes', 'Save source receives the observed full union rather than just B');
    await first.session.prompt('Exercise the actual offline provider options.', { expandPromptTemplates: false, runCommands: false });
    while (first.session.hasPostPromptWork) await new Promise(resolve => setImmediate(resolve));
    assert.equal(model.calls.at(-1).options.temperature, 0.7);
    assert.equal(model.calls.at(-1).options.topP, 0.6);
    assert.equal(model.calls.at(-1).options.topK, 9);
    assert.match(model.calls.at(-1).context.systemPrompt.join('\n'), /ORDERED_B_CONTEXT/);
    assert.ok(!(await first.policy.control({ path: m })).error);
    assert.deepEqual(first.policy.snapshot().configFiles, [a, x, m, y]);
    assert.equal(current('temperature'), 0.8, 'live session replacement keeps the original B slot after X');
    await assert.rejects(stat(firstGeneration), cause => cause.code === 'ENOENT', 'successful native reload removes the old private generation');
    assert.ok(!(await first.policy.control({ path: b })).error);
    assert.deepEqual(current('retry.fallbackChains'), {}, 'live return to B clears inherited record keys through native reload');
    assert.equal(current('temperature'), 0.7);
    assert.ok(!(await first.policy.control({ path: null })).error);
    assert.deepEqual(first.policy.snapshot().configFiles, [a, m, x, y]);
    assert.deepEqual(first.policy.snapshot().setupPaths, [a, m]);
    assert.equal(current('temperature'), 0.3, 'automatic M is inserted before X, not into the former explicit B slot');
    assert.equal(current('retry.maxRetries'), 11);
    await first.session.prompt('Observe options after the live automatic setup change.', { expandPromptTemplates: false, runCommands: false });
    assert.equal(model.calls.at(-1).options.temperature, 0.3);
    while (first.session.hasPostPromptWork) await new Promise(resolve => setImmediate(resolve));
    assert.match(model.calls.at(-1).context.systemPrompt.join('\n'), /ORDERED_M_CONTEXT/);
    process.env.OMP_MANAGER_CONFIG_SELECTION = '';
    const restarted = await open();
    try {
      assert.ok(!restarted.policy.snapshot().error);
      assert.deepEqual(restarted.policy.snapshot().configFiles, [a, m, x, y], 'null intent survives restart without re-inferring original B');
      assert.equal(lookup('temperature').get(restarted.session.settings), 0.3);
      await writeFile(assignmentFile, '[]');
      assert.ok(!(await restarted.policy.attachContext(restarted.ctx)).error);
      assert.deepEqual(restarted.policy.snapshot().configFiles, [a, x, y]);
      assert.equal(lookup('temperature').get(restarted.session.settings), 0.3, 'default A is not reapplied after X when Automatic has no model assignment');
    } finally { restarted.stop(); await restarted.session.dispose(); restarted.session.settings.cancelPendingSaves(); await rm(restarted.directory, { recursive: true, force: true }); }
    finalGeneration = path.resolve(activeSkill().baseDir, '../../..');
    assert.equal(model.calls.length, 2, 'only the two explicit in-process mock turns ran');
  } finally { first.stop(); await first.session.dispose(); first.session.settings.cancelPendingSaves(); await rm(first.directory, { recursive: true, force: true }); }
  await assert.rejects(stat(finalGeneration), cause => cause.code === 'ENOENT', 'native shutdown removes its own remaining private generation');
  assert.equal(await readFile(globalPath, 'utf8'), Bun.YAML.stringify(globalBefore), 'managed setup switches never rewrite native global configuration');
}
async function exerciseUnownedNativePreferences() {
  const globalPath = path.join(dirs.getAgentDir(), 'config.yml');
  const initial = Bun.YAML.parse(await readFile(globalPath, 'utf8'));
  initial.temperature = 0.2;
  await writeFile(globalPath, Bun.YAML.stringify(initial));
  const partial = await save('native-liveness', 'topP: 0.5\nmodelRoles:\n  task: mock/native-liveness\n');
  process.env.OMP_MANAGER_CONFIG_DEFAULT = '';
  process.env.OMP_MANAGER_CONFIG_SELECTION = partial;
  process.env.OMP_MANAGER_CONFIG_FILES = JSON.stringify([partial]);
  const prepared = await managedSettings([partial], null, { source: 'session', path: partial });
  const model = createMockModel({ id: 'native-liveness', handler: () => ({ content: ['Native preference response'] }) });
  let nativePolicy, nativeCtx;
  const created = await createAgentSession({
    cwd: root, agentDir: dirs.getAgentDir(), model, thinkingLevel: 'off', settings: prepared.settings,
    sessionManager: SessionManager.inMemory(root), agentId: 'native-liveness-root', tools: [],
    hasUI: false, enableMCP: false, disableExtensionDiscovery: true, bindProcessState: false,
    extensions: [pi => { nativePolicy = installConfigPolicy(pi); pi.on('session_start', async (_event, ctx) => { nativeCtx = ctx; await nativePolicy.attachContext(ctx); }); }],
  });
  const nativeSession = created.session;
  nativeSession.modelRegistry.authStorage.keys.setRuntime('mock', 'offline-fixture-key');
  const stop = nativeSession.subscribe(() => {});
  try {
    await startExtensions(nativeSession);
    assert.equal(nativeSession.agent.temperature, 0.2);
    lookup('temperature').set(nativeSession.settings, 0.7);
    lookup('modelRoles').setEntry(nativeSession.settings, 'nativePreference', 'mock/native-liveness');
    await nativeSession.prompt('Observe the user-set native preference.', { expandPromptTemplates: false, runCommands: false });
    while (nativeSession.hasPostPromptWork) await new Promise(resolve => setImmediate(resolve));
    assert.equal(model.calls.at(-1).options.temperature, 0.7, 'a partial setup never masks an unowned native setter');
    assert.equal(lookup('modelRoles').get(nativeSession.settings).nativePreference, 'mock/native-liveness', 'unowned native record entries remain live');
    assert.ok(!(await nativePolicy.control({ path: null })).error);
    assert.equal(nativePolicy.snapshot().source, 'native');
    assert.equal(nativeSession.agent.temperature, 0.7);
    lookup('temperature').set(nativeSession.settings, 0.8);
    assert.ok(!(await nativePolicy.control({ path: partial })).error);
    await nativeSession.prompt('Observe another native preference edit after switching back.', { expandPromptTemplates: false, runCommands: false });
    while (nativeSession.hasPostPromptWork) await new Promise(resolve => setImmediate(resolve));
    assert.equal(model.calls.at(-1).options.temperature, 0.8, 'a later setup application does not restore a captured stale scalar');
    assert.equal(lookup('modelRoles').get(nativeSession.settings).nativePreference, 'mock/native-liveness', 'app-owned role entries do not freeze unrelated native roles');
    assert.equal(nativePolicy.snapshot().configPath, partial);
    assert.equal(nativeCtx.model.id, 'native-liveness');
  } finally {
    stop();
    await nativeSession.dispose();
    prepared.settings.cancelPendingSaves();
    await rm(prepared.directory, { recursive: true, force: true });
  }
}
try {
  assert.deepEqual(policy.snapshot(), { model: 'mock/first', configPath: assigned, configName: 'assigned', source: 'model', configFiles: [fallback, assigned], setupPaths: [fallback, assigned] });
  assert.equal(value('temperature'), 0.2);
  assert.equal(session.model.id, 'first', 'an assigned setup never follows its modelRoles selector');
  assert.equal(value('modelRoles').task, 'mock/second', 'runtime setup preserves native task/helper role assignments');
  assert.match(prompt(), /SETUP_AGENT_A/);
  assert.match(prompt(), /SETUP_ANCESTOR_A/);
  assert.match(prompt(), /SETUP_RULE_A/);
  assert.match(prompt(), /SETUP_SCOPED_A/);
  assert.ok(getActiveSkills().some(skill => skill.name === 'setup-fixture'));
  assert.deepEqual(value('skills.customDirectories'), [], 'selected setups cannot inherit unreviewed native custom directory roots');
  assert.ok(!session.skills.some(skill => skill.name === 'native-outside'));
  assert.ok(getActiveRules().some(rule => rule.content.includes('SETUP_RULE_A')));
  assert.equal(value('retry.enabled'), false, 'unrelated native overrides are preserved');
  let childPolicy, childCtx;
  const child = await createAgentSession({
    cwd: root, agentDir: dirs.getAgentDir(), model: secondModel, thinkingLevel: 'off',
    modelRegistry: session.modelRegistry, settings: createSubagentSettings(session.settings), sessionManager: SessionManager.inMemory(root),
    parentTaskPrefix: 'setup-child', parentAgentId: 'Main', agentId: 'setup-child', agentName: 'setup-child', taskDepth: 1,
    tools: [], disableExtensionDiscovery: true, hasUI: false, enableMCP: false, bindProcessState: false,
    skills: [...session.skills], rules: configSetupResources(session).rules,
    contextFiles: configSetupResources(session).contextFiles.filter(file => path.basename(file.path).toLowerCase() !== 'agents.md'),
    extensions: [pi => { childPolicy = installConfigPolicy(pi); pi.on('session_start', (_event, nativeCtx) => { childCtx = nativeCtx; }); }],
  });
  const stopChild = child.session.subscribe(() => {});
  try {
    await startExtensions(child.session);
    await childPolicy.attachContext(childCtx);
    assert.equal(childCtx.agent.kind, 'sub', 'a real native child owns its own resolved model');
    assert.match(childPolicy.snapshot().error, /inherited at construction/);
    assert.equal(childPolicy.snapshot().source, 'inherited');
    assert.equal(childPolicy.snapshot().configPath, assigned, 'actual inherited setup is reported instead of unapplied requested B');
    assert.equal(lookup('temperature').get(child.session.settings), 0.2, 'incompatible child changes are refused before any Settings writes');
    assert.match(child.session.agent.state.systemPrompt.join('\n'), /SETUP_RULE_A/);
    assert.doesNotMatch(child.session.agent.state.systemPrompt.join('\n'), /SETUP_AGENT_B/);
    assert.ok(child.session.skills.some(skill => skill.name === 'setup-fixture'));
    assert.equal(value('temperature'), 0.2, 'child setup overrides never write back to parent Settings');
    assert.equal(policy.snapshot().children[0].model, 'mock/second');
    assert.equal(policy.snapshot().model, 'mock/first', 'child state never replaces root identity');
    await assert.rejects(policy.control({ path: chosen }), /busy/);
  } finally { stopChild(); await child.session.dispose(); }
  assert.equal(policy.snapshot().children, undefined, 'native child shutdown removes its active relationship');
  let inheritedPolicy, inheritedCtx;
  const inherited = await createAgentSession({
    cwd: root, agentDir: dirs.getAgentDir(), model: firstModel, thinkingLevel: 'off',
    modelRegistry: session.modelRegistry, settings: createSubagentSettings(session.settings), sessionManager: SessionManager.inMemory(root),
    parentTaskPrefix: 'inherited-child', parentAgentId: 'Main', agentId: 'inherited-child', taskDepth: 1,
    tools: [], disableExtensionDiscovery: true, hasUI: false, enableMCP: false, bindProcessState: false,
    skills: [...session.skills], rules: configSetupResources(session).rules,
    contextFiles: configSetupResources(session).contextFiles.filter(file => path.basename(file.path).toLowerCase() !== 'agents.md'),
    extensions: [pi => { inheritedPolicy = installConfigPolicy(pi); pi.on('session_start', (_event, nativeCtx) => { inheritedCtx = nativeCtx; }); }],
  });
  const stopInherited = inherited.session.subscribe(() => {});
  try {
    await startExtensions(inherited.session);
    assert.ok(!(await inheritedPolicy.attachContext(inheritedCtx)).error);
    await inherited.session.prompt('Observe inherited child settings before the parent model change.', { expandPromptTemplates: false, runCommands: false });
    assert.equal(firstModel.calls.at(-1).options.temperature, 0.2);
    await session.setModel(secondModel, 'default', { persist: false });
    assert.match((await policy.attachContext(ctx)).error, /active native work|active native children/);
    assert.equal(value('temperature'), 0.2, 'refused parent transition leaves the native live parent layer unchanged');
    await inherited.session.prompt('Observe child A after the refused parent B setup.', { expandPromptTemplates: false, runCommands: false });
    assert.equal(firstModel.calls.at(-1).options.temperature, 0.2, 'actual child provider never inherits B-specific values');
    assert.equal(inherited.session.model.id, 'first');
    await session.setModel(firstModel, 'default', { persist: false });
  } finally { stopInherited(); await inherited.session.dispose(); }
  assert.ok(!(await policy.attachContext(ctx)).error);
  const setupResources = configSetupResources(session);
  const strict = await createAgentSession({
    cwd: root, agentDir: dirs.getAgentDir(), model: firstModel, thinkingLevel: 'off',
    modelRegistry: session.modelRegistry, settings: createSubagentSettings(await configSetupSettings(session)), sessionManager: SessionManager.inMemory(root),
    parentTaskPrefix: 'strict-child', parentAgentId: 'Main', agentId: 'strict-child', taskDepth: 1,
    tools: [], restrictToolNames: true, extensions: [], disableExtensionDiscovery: true, hasUI: false,
    enableMCP: false, enableLsp: false, enableIrc: false, bindProcessState: false, ...setupResources,
  });
  const removeStrict = registerConfigSetupChild(session, strict.session, 'strict-child');
  try {
    assert.match(strict.session.agent.state.systemPrompt.join('\n'), /SETUP_AGENT_A/);
    assert.match(strict.session.agent.state.systemPrompt.join('\n'), /SETUP_RULE_A/);
    assert.deepEqual(strict.session.getActiveToolNames(), [], 'setup instructions do not widen a restricted child tool surface');
    const previousTopP = value('topP');
    lookup('topP').override(session.settings, 0.9);
    assert.equal(lookup('topP').get(strict.session.settings), previousTopP, 'frozen native child settings ignore newly introduced parent setup keys');
    lookup('topP').clearOverride(session.settings);
    const previousChains = session.settings.overlayLayers().overrides.retry?.fallbackChains;
    const detachedChains = lookup('retry.fallbackChains').get(strict.session.settings);
    lookup('retry.fallbackChains').override(session.settings, { 'mock/*': ['mock/second'] });
    assert.deepEqual(lookup('retry.fallbackChains').get(strict.session.settings), detachedChains, 'detached owned worker does not inherit future parent record keys');
    lookup('retry.fallbackChains').override(session.settings, previousChains);
    await assert.rejects(policy.control({ path: chosen }), /busy/);
    assert.equal(policy.snapshot().children[0].id, 'strict-child');
    assert.equal(policy.snapshot().children[0].mcpInheritedFrom, undefined, 'strict SRS-style children never claim MCP authority');
  } finally { removeStrict(); await strict.session.dispose(); }

  await switchNative(secondModel, secondConfig);
  assert.ok(nativeEvents.includes('model_changed'), 'the real native model switch lifetime event was emitted');
  assert.equal(policy.snapshot().model, 'mock/second');
  assert.equal(policy.snapshot().configPath, secondConfig);
  assert.equal(value('temperature'), 0.7);
  assert.equal(value('modelRoles').task, undefined, 'switching removes the previous setup role keys');
  assert.doesNotMatch(prompt(), /SETUP_AGENT_A|SETUP_RULE_A|SETUP_SCOPED_A|SETUP_ANCESTOR_A/);
  assert.ok(!getActiveSkills().some(skill => skill.name === 'setup-fixture'));
  assert.ok(!getActiveRules().some(rule => rule.content.includes('SETUP_RULE_A')));

  await policy.control({ path: chosen });
  assert.equal(value('topK'), 12);
  assert.equal(value('temperature'), 0.4, 'explicit setup inherits the frozen default without inheriting automatic model setup keys');
  await switchNative(firstModel, chosen);
  assert.equal(policy.snapshot().source, 'session');
  assert.equal(policy.snapshot().configPath, chosen, 'explicit intent survives native model changes');
  await policy.control({ path: null });
  assert.equal(policy.snapshot().source, 'model');
  assert.equal(policy.snapshot().configPath, assigned);
  assert.equal(value('topK'), 3, 'clearing a setup removes its old keys');
  await switchNative(secondModel, secondConfig);
  await switchNative(firstModel, assigned);
  firstModel.push({ stopReason: 'toolUse', content: [{ type: 'toolCall', name: 'fixture_boundary_step', arguments: {} }] });
  secondModel.push({ content: ['The continued native turn used the model-specific setup.'] });
  await session.prompt('Exercise automatic switching before the next native provider call.', { expandPromptTemplates: false, runCommands: false });
  while (session.hasPostPromptWork) await new Promise(resolve => setImmediate(resolve));
  assert.equal(secondModel.calls.at(-1).options.temperature, 0.7, 'automatic profile application precedes the next real provider request');
  assert.match(secondModel.calls.at(-1).context.systemPrompt.join('\n'), /SETUP_AGENT_B/);
  assert.equal(policy.snapshot().configPath, secondConfig);
  const providerContext = secondModel.calls.at(-1).context;
  secondModel.push({ content: ['Harmony retry sampling fixture.'] });
  const retryStream = await session.agent.streamFn(secondModel, providerContext, { apiKey: 'offline-fixture-key', temperature: 0.25 });
  const retryResult = await retryStream.result();
  assert.match(retryResult.content.filter(block => block.type === 'text').map(block => block.text).join('\n'), /Harmony retry sampling fixture/);
  assert.ok(Math.abs(secondModel.calls.at(-1).options.temperature - 0.75) < 1e-12, 'native loop-captured A .2 plus retry .05 reaches the real provider as live B .7 plus .05');
  const previousTemperature = session.agent.temperature;
  try {
    session.agent.temperature = undefined;
    secondModel.push({ content: ['Unset sampling fixture.'] });
    const unsetStream = await session.agent.streamFn(secondModel, providerContext, { apiKey: 'offline-fixture-key', temperature: 0.2 });
    const unsetResult = await unsetStream.result();
    assert.match(unsetResult.content.filter(block => block.type === 'text').map(block => block.text).join('\n'), /Unset sampling fixture/);
    assert.equal(secondModel.calls.at(-1).options.temperature, undefined, 'unset live sampling never leaks the old captured temperature into the provider');
  } finally { session.agent.temperature = previousTemperature; }
  await switchNative(firstModel, assigned);
  assert.match(prompt(), /SETUP_AGENT_A/);
  assert.doesNotMatch(prompt(), /SETUP_AGENT_B/);

  session.agent.state.isStreaming = true;
  await assert.rejects(policy.control({ path: chosen }), /busy/);
  session.agent.state.isStreaming = false;
  assert.equal(policy.snapshot().configPath, assigned, 'busy requests never change intent');
  for (const [file, reason] of [[unreviewed, /execution review/], [mcp, /restart-required/], [invalid, /validated/], [traversal, /validated|asset path/]]) {
    const state = await policy.control({ path: file });
    assert.match(state.error, reason);
    assert.equal(state.configPath, assigned);
    assert.equal(value('temperature'), 0.2, 'failed switches leave native settings untouched');
  }
  assert.ok(!getActiveSkills().some(skill => skill.name === 'unreviewed'));
  assert.ok(!(await policy.control({ path: assetsOnly })).error);
  assert.match(prompt(), /SETUP_ASSET_ONLY/);
  assert.equal(value('temperature'), 0.4);
  await policy.control({ path: null });
  const agentFile = path.join(path.dirname(assigned), 'assets', 'AGENTS.md');
  const originalAgent = await readFile(agentFile, 'utf8');
  await writeFile(agentFile, 'Edited after execution review');
  assert.match((await settlePolicy()).error, /integrity/);
  assert.equal(policy.snapshot().configPath, assigned);
  await writeFile(agentFile, originalAgent);
  assert.ok(!(await settlePolicy()).error);

  await writeFile(assignmentFile, '[]');
  await session.extensionRunner.emit({ type: 'before_agent_start', prompt: 'offline', systemPrompt: session.agent.state.systemPrompt });
  assert.equal(policy.snapshot().source, 'default', 'safe native boundary reloads changed persisted assignments');
  assert.equal(value('temperature'), 0.4);
  assert.equal(value('topP'), 0.6);
  await writeFile(path.join(app, 'global-config.json'), 'null');
  await settlePolicy();
  assert.equal(policy.snapshot().source, 'default', 'changing the global default never changes existing sessions');
  assert.equal(value('temperature'), 0.4);
  assert.equal(value('topP'), 0.6);
  process.env.OMP_MANAGER_CONFIG_DEFAULT = '';
  process.env.OMP_MANAGER_CONFIG_FILES = '[]';
  let freshPolicy, freshCtx;
  const preparedFresh = await managedSettings([], null, { source: 'native', path: null }, { temperature: 0.15, 'skills.customDirectories': [externalSkills] });
  const fresh = await createAgentSession({
    cwd: root, agentDir: dirs.getAgentDir(), model: secondModel, thinkingLevel: 'off',
    settings: preparedFresh.settings, sessionManager: SessionManager.inMemory(root),
    tools: [], disableExtensionDiscovery: true, hasUI: false, enableMCP: false, bindProcessState: false, agentId: 'fresh-native-root',
    extensions: [pi => { freshPolicy = installConfigPolicy(pi); pi.on('session_start', (_event, nativeCtx) => { freshCtx = nativeCtx; }); }],
  });
  const stopFresh = fresh.session.subscribe(() => {});
  try {
    await startExtensions(fresh.session);
    await freshPolicy.attachContext(freshCtx);
    assert.equal(freshPolicy.snapshot().source, 'native', 'a new session with no frozen default uses native settings');
    assert.equal(lookup('temperature').get(fresh.session.settings), 0.15);
    assert.deepEqual(lookup('skills.customDirectories').get(fresh.session.settings), [externalSkills], 'native-only sessions keep their original directory settings');
  } finally { stopFresh(); await fresh.session.dispose(); preparedFresh.settings.cancelPendingSaves(); await rm(preparedFresh.directory, { recursive: true, force: true }); }
  assert.equal(firstModel.calls.length + secondModel.calls.length, 6, 'only explicit in-process child, boundary and sampling mock requests ran; no paid provider calls');
  assert.ok(events.some(event => event.kind === 'config_state' && event.model === 'mock/second'));
  assert.match(await readFile(assigned, 'utf8'), /paid-provider\/must-never-select/, 'immutable setup and native global config are never rewritten');
  await exerciseOrderedOverlays();
  await exerciseUnownedNativePreferences();
  const { ModelRegistry } = await fromSdk('src/config/model-registry.ts');
  installLocalModelPolicy(ModelRegistry, Settings, session.settings);
  const remoteCompaction = await save('remote-compaction', 'compaction:\n  remoteEndpoint: https://hosted.invalid/compact\n');
  const guarded = await policy.control({ path: remoteCompaction });
  assert.match(guarded.error, /validated/);
  assert.equal(guarded.configPath, fallback, 'the installed local-only guard rejects setup routes before live writes');
  assert.equal(value('temperature'), 0.4);
  console.log('Installed SDK setup policy Settings/model_changed/assets/idle/intent verified');
} finally {
  unsubscribe();
  await session.dispose();
  settings.cancelPendingSaves();
  await rm(preparedRoot.directory, { recursive: true, force: true });
}
