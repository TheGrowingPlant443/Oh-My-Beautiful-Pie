import { AsyncLocalStorage } from 'node:async_hooks';
import { readFileSync } from 'node:fs';
import { copyFile, mkdir, mkdtemp, readFile, rename, rm, stat, writeFile, chmod } from 'node:fs/promises';
import { randomUUID } from 'node:crypto';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { isDeepStrictEqual } from 'node:util';
import { assetMetadata, bundleDigest, configPolicyFiles, isAppSetupPath, mergeSetupAsset, noSymlinks, parseBundleJson, requiresAssetExecutionReview, sanitizeConfig, safeSetupName, verifySavedBundle, resolveMcpServer, safeAssetPath } from './config-bundle.mjs';
export { configPolicyFiles } from './config-bundle.mjs';
// OMBP_AGENT=omsp drives the omsp fork, whose packages use their own npm scope.
const sdkScope = process.env.OMBP_AGENT === 'omsp' ? '@oh-my-striped-pi' : '@oh-my-pi';

const own = (value, segments) => segments.reduce((node, key) => node && Object.hasOwn(node, key) ? node[key] : undefined, value);
const modelId = ctx => ctx?.model ? `${ctx.model.provider}/${ctx.model.id}` : null;
const fail = message => new Error(`config_policy: ${message}`);
const safeError = cause => String(cause?.message ?? '').startsWith('config_policy:') ? cause.message : 'config_policy: setup could not be loaded or validated';
const emptyItems = () => ({ skills: [], rules: [], 'context-files': [], mcps: [] });
// Upstream explicitly subscribes these child consumers; other changed startup fields need a managed root.
const childLiveSettings = new Set(['temperature', 'topP', 'topK', 'minP', 'presencePenalty', 'repetitionPenalty',
  'steeringMode', 'followUpMode', 'interruptMode', 'skillful', 'externalThinking', 'modelRoles',
  'tier.openai', 'tier.anthropic', 'tier.google', 'advisor.enabled', 'advisor.maxNotesPerUpdate']);
const resourceScope = new AsyncLocalStorage();
const resources = new WeakMap(), sessions = new WeakMap(), modes = new WeakMap(), roots = new WeakMap();
const registered = new WeakSet();

const restores = [];
let owners = 0;

async function ownerPrivate(directory) {
  await noSymlinks(directory);
  const info = await stat(directory);
  if (!info.isDirectory() || (process.getuid && info.uid !== process.getuid())) throw fail('runtime directory is not owned by this user');
  await chmod(directory, 0o700);
  if (process.platform !== 'win32' && ((await stat(directory)).mode & 0o077)) throw fail('runtime directory is not private');
}

async function readSetupConfig(file, root, definitions, resourcesEnabled = true) {
  if (typeof file !== 'string' || !path.isAbsolute(file) || path.basename(file) !== 'config.yml' || !isAppSetupPath(file, root)) throw fail('setup is not an app-owned snapshot');
  await noSymlinks(file);
  let metadata, saved = false;
  try { metadata = parseBundleJson(await readFile(path.join(path.dirname(file), 'setup.json'), 'utf8')); saved = true; }
  catch (cause) { if (cause.code !== 'ENOENT') throw cause; }
  metadata ??= { version: 1, name: path.basename(path.dirname(file)), assets: [], requiresExecutionReview: false, executableReview: false };
  if (metadata.version !== 1 || typeof metadata.name !== 'string' || !metadata.name.trim() || !Array.isArray(metadata.assets) || typeof metadata.requiresExecutionReview !== 'boolean' || typeof metadata.executableReview !== 'boolean') throw fail('invalid setup metadata');
  const name = safeSetupName(metadata.name);
  for (const asset of metadata.assets) safeAssetPath(asset);
  const needsReview = metadata.requiresExecutionReview || requiresAssetExecutionReview(metadata.assets.map((path, index) => ({ path, mode: metadata.assetMetadata?.[index]?.mode })));
  if (resourcesEnabled && needsReview && !metadata.executableReview) throw fail('execution review required before activating setup assets');
  let verified;
  try {
    verified = !saved
      ? { yaml: new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(await readFile(file)), assets: [] }
      : await verifySavedBundle(file, metadata);
  } catch { throw fail('setup content integrity or safety check failed; import and save it again'); }
  const filtered = sanitizeConfig(Bun.YAML.parse(verified.yaml), definitions, metadata.assets.length > 0);
  if (filtered.omitted.length) throw fail('setup contains non-portable settings; import and save it again');
  if (filtered.settings.modelRoles) {
    delete filtered.settings.modelRoles.default;
    if (!Object.keys(filtered.settings.modelRoles).length) delete filtered.settings.modelRoles;
  }
  return { configPath: file, configName: name, settings: filtered.settings, assetFiles: verified.assets };
}

async function atomicSlot(file, content) {
  try { if (await readFile(file, 'utf8') === content) { await chmod(file, 0o600); return; } }
  catch (cause) { if (cause.code !== 'ENOENT') throw cause; }
  const temporary = path.join(path.dirname(file), `.write-${randomUUID()}.tmp`);
  try { await writeFile(temporary, content, { mode: 0o600, flag: 'wx' }); await rename(temporary, file); }
  finally { await rm(temporary, { force: true }); }
}

function slotSource(manifest, entry, selection) {
  if (path.dirname(entry.path) !== manifest.directory || !/^slot-\d+\.yml$/.test(path.basename(entry.path))) throw fail('invalid runtime slot path');
  if (entry.kind === 'session') return selection.source === 'session' ? selection.path : null;
  if (entry.kind === 'automatic') return selection.source === 'model' ? selection.path : null;
  if (!['fixed', 'session', 'automatic', 'selected', 'guard', 'native'].includes(entry.kind)) throw fail('invalid runtime slot kind');
  if (entry.kind === 'selected') return ['session', 'model'].includes(selection.source) ? selection.path : null;
  return entry.source;
}

function guardSettings(utility, hasSources) {
  if (utility) return { skills: { enabled: false, customDirectories: [] }, extensions: [],
    advisor: { enabled: false }, autolearn: { enabled: false }, prewalk: { enabled: false }, ttsr: { enabled: false } };
  return hasSources ? { skills: { customDirectories: [] } } : {};
}

async function renderSlots(manifest, selection, root, definitions, snapshots = new Map()) {
  await ownerPrivate(manifest.directory);
  const sources = [];
  for (const entry of manifest.entries) {
    if (entry.kind === 'native') continue;
    const source = slotSource(manifest, entry, selection);
    if (entry.kind === 'guard') continue;
    let settings = {};
    if (source) {
      if (!snapshots.has(source)) snapshots.set(source, await readSetupConfig(source, root, definitions, !manifest.utility));
      sources.push(source);
      settings = snapshots.get(source).settings;
    }
    await atomicSlot(entry.path, Bun.YAML.stringify(settings));
  }
  const guard = manifest.entries.find(entry => entry.kind === 'guard');
  if (!guard) throw fail('runtime safety slot is missing');
  await atomicSlot(guard.path, Bun.YAML.stringify(guardSettings(manifest.utility, sources.length > 0)));
}

function gapKinds(index, explicitGap, automaticGap) {
  if (explicitGap === automaticGap && index === explicitGap) return ['selected'];
  const kinds = [];
  if (index === automaticGap) kinds.push('automatic');
  if (index === explicitGap) kinds.push('session');
  return kinds;
}

function slotEntries(directory, root, fixed, explicitGap, automaticGap) {
  const entries = [];
  const slot = (kind, source = null) => entries.push({ kind, source, path: path.join(directory, `slot-${entries.length}.yml`) });
  for (let index = 0; index <= fixed.length; index++) {
    for (const kind of gapKinds(index, explicitGap, automaticGap)) slot(kind);
    if (index === fixed.length) break;
    if (isAppSetupPath(fixed[index], root)) slot('fixed', fixed[index]);
    else entries.push({ kind: 'native', path: fixed[index] });
  }
  slot('guard');
  return entries;
}

/** Prepare native CLI layers before its Settings, tools and UI are constructed. */
export async function prepareConfigPolicy({ root, directory, files, defaultPath = null, selection, cwd, sdkDir, utility = false, authoritativeConfigFiles = false }) {
  root = path.resolve(root); directory = path.resolve(directory);
  if (path.dirname(directory) !== root || !path.basename(directory).startsWith('.config-session-')) throw fail('invalid session settings capsule');
  await ownerPrivate(directory);
  const fromSdk = file => import(pathToFileURL(path.join(sdkDir, file)).href);
  const { Settings } = await fromSdk('src/config/settings.ts');
  const { all } = await fromSdk('src/config/registry.ts');
  const marker = path.join(root, 'global-configs', '__runtime-slot__', 'config.yml');
  const options = { root, defaultPath };
  const explicit = authoritativeConfigFiles ? files.map(file => path.resolve(file)) : configPolicyFiles(files, { ...options, selection: { source: 'session', path: marker } });
  const automatic = authoritativeConfigFiles ? explicit : configPolicyFiles(files, { ...options, selection: { source: 'model', path: marker } });
  const fixed = explicit.filter(file => file !== marker);
  const explicitGap = explicit.indexOf(marker), automaticGap = automatic.indexOf(marker);
  const entries = slotEntries(directory, root, fixed, explicitGap, automaticGap);
  const manifest = { version: 1, directory, utility, sourceCwd: cwd, entries, configFiles: entries.map(entry => entry.path) };
  try {
    await renderSlots(manifest, selection, root, all());
    const settings = await Settings.loadReadOnly({ cwd, configFiles: manifest.configFiles });
    if (utility) {
      const overrides = Object.fromEntries(all().filter(setting => setting.isConfigured(settings)).map(setting => [setting.id, setting.get(settings)]));
      manifest.settingsSnapshot = path.join(directory, 'effective-settings.json');
      await atomicSlot(manifest.settingsSnapshot, JSON.stringify({ version: 1, cwd, overrides }));
    }
    return { configFiles: manifest.configFiles, slotManifest: manifest, directory };
  } catch (cause) {
    await Promise.all([...entries.filter(entry => entry.kind !== 'native').map(entry => entry.path), ...(manifest.settingsSnapshot ? [manifest.settingsSnapshot] : [])].map(file => rm(file, { force: true })));
    throw cause;
  }
}

/** Only reviewed setup resources, never ambient project context or MCP/tool authority. */
export function configSetupResources(session) {
  const items = resources.get(session.settings);
  return {
    contextFiles: [...(items?.['context-files'] ?? [])],
    rules: [...(items?.rules ?? [])],
    skills: session.skills.filter(skill => skill._source?.provider === 'omp-manager-setup'),
  };
}

export async function configSetupSettings(session) {
  let definitions = roots.get(session.modelRegistry)?.definitions;
  if (!definitions) {
    const sdk = process.env.OMP_MANAGER_SDK_DIR || process.env.OMP_SDK_DIR;
    if (!sdk) throw fail('native settings registry is required to snapshot this worker');
    definitions = (await import(pathToFileURL(path.join(sdk, 'src/config/registry.ts')).href)).all();
  }
  const overrides = Object.fromEntries(definitions.filter(setting => setting.isConfigured(session.settings))
    .map(setting => [setting.id, structuredClone(setting.get(session.settings))]));
  const detached = session.settings.constructor.isolated(overrides, { storage: session.settings.getStorage() });
  await detached.reloadForCwd(session.sessionManager.getCwd());
  return detached;
}

export function registerConfigSetupChild(parentSession, childSession, id = childSession.sessionManager.getSessionId()) {
  const parent = roots.get(parentSession.modelRegistry);
  if (!parent) return () => {};
  const expected = parentSession.model && `${parentSession.model.provider}/${parentSession.model.id}`;
  const current = () => childSession.model && `${childSession.model.provider}/${childSession.model.id}`;
  if (current() !== expected) throw fail('setup snapshot child must use the exact parent model');
  const state = parent.snapshot();
  const update = () => {
    parent.children.set(id, { id, model: current(), configPath: state.configPath, configName: state.configName, source: state.source,
      ...(current() !== expected ? { error: 'config_policy: child model no longer matches its frozen setup' } : {}) });
    parent.publish();
  };
  update();
  const unsubscribe = childSession.subscribe(event => { if (event.type === 'model_changed') update(); });
  const detach = childSession.agent.addBeforeModelCallHook(() => { if (current() !== expected) throw fail('child model no longer matches its frozen setup'); });
  const remove = () => { unsubscribe(); detach(); if (parent.children.delete(id)) parent.publish(); };
  childSession.addDisposer(remove);
  return remove;
}

// Native discovery providers are process-wide; their data must be session-scoped even for concurrent task children.
function installNativeCapture(api) {
  for (const [prototype, name, invoke] of [
    [api.AgentSession?.prototype, 'subscribe', (original, self, args) => { sessions.set(self.sessionManager, self); return original.apply(self, args); }],
    [api.InteractiveMode?.prototype, 'init', (original, self, args) => { sessions.set(self.session.sessionManager, self.session); modes.set(self.session.sessionManager, self); return original.apply(self, args); }],
    ...['prompt', 'newSession', 'switchSession', 'moveSession', 'reload', 'setModel', 'setModelTemporary', 'applyRoleModel', 'cycleModel',
      'refreshSkills', 'refreshBaseSystemPrompt', 'refreshMCPTools'].map(name => [api.AgentSession?.prototype, name,
      (original, self, args) => resourceScope.run(self.settings, () => original.apply(self, args))]),
  ]) {
    const descriptor = prototype && Object.getOwnPropertyDescriptor(prototype, name);
    if (!descriptor?.configurable || typeof descriptor.value !== 'function') continue;
    const wrapper = function (...args) { return invoke(descriptor.value, this, args); };
    Object.defineProperty(prototype, name, { ...descriptor, value: wrapper });
    restores.push(() => { if (prototype[name] === wrapper) Object.defineProperty(prototype, name, descriptor); });
  }
}

function installLiveSampling(agent) {
  const original = agent.streamFn;
  if (typeof original !== 'function') throw fail('native provider stream is unavailable');
  let capturedTemperature, firstRequest = true;
  const unsubscribe = agent.subscribe(event => { if (event.type === 'agent_start') firstRequest = true; });
  const stream = function (model, context, options) {
    // Native loop options are captured once per turn; the public agent properties are live.
    if (firstRequest) { capturedTemperature = options?.temperature; firstRequest = false; }
    const temperature = liveTemperature(agent.temperature, capturedTemperature, options);
    return original.call(this, model, context, { ...options, temperature,
      topP: agent.topP, topK: agent.topK, minP: agent.minP,
      presencePenalty: agent.presencePenalty, repetitionPenalty: agent.repetitionPenalty,
      hideThinkingSummary: agent.hideThinkingSummary });
  };
  agent.streamFn = stream;
  return () => { unsubscribe(); if (agent.streamFn === stream) agent.streamFn = original; };
}

function liveTemperature(live, captured, options) {
  if (Object.is(live, captured)) return options?.temperature;
  if (live === undefined) return undefined;
  // Preserve the native Harmony retry adjustment.
  if (typeof captured === 'number' && typeof options?.temperature === 'number') return live + (options.temperature - captured);
  return live;
}

function readJson(file, fallback) {
  try {
    return parseBundleJson(readFileSync(file, 'utf8'));
  } catch (cause) {
    if (cause.code === 'ENOENT') return fallback;
    throw cause;
  }
}

function text(file) {
  return new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(readFileSync(file));
}

const isContextAsset = asset => asset === 'AGENTS.md' || /^rules\/\.omp-context\/\d+-AGENTS\.md$/.test(asset);
const isRuleAsset = asset => asset === 'RULES.md' || (asset.startsWith('rules/') && /\.(md|mdc)$/.test(asset));

function mcpAssetItems(file, assets, source) {
  const data = readJson(file);
  if (!data?.mcpServers || typeof data.mcpServers !== 'object' || Array.isArray(data.mcpServers)) throw fail('invalid MCP asset');
  return Object.entries(data.mcpServers).map(([name, server]) => {
    if (!server || typeof server !== 'object' || Array.isArray(server) || (!server.command && !server.url)) throw fail('invalid MCP server');
    return { ...resolveMcpServer(server, assets.files, assets.root), name, _source: source() };
  });
}

function assertSubWritable(setting, current, value, live) {
  if (!childLiveSettings.has(setting.id) && !setting.id.startsWith('retry.') && !setting.id.startsWith('compaction.')) throw fail('native task startup configuration is inherited; use an independent managed session for this setup');
  const resets = value === undefined ? current !== undefined : !isDeepStrictEqual(setting.get(live.overlay({ [setting.id]: value })), value);
  if (resets) throw fail('native task settings cannot reset inherited record or unset values; use an independent managed session');
}

/** App-owned, non-persistent native Settings and resource policy; never selects a model or credential. */
export function installConfigPolicy(pi, { emit = () => {}, isQuiescent } = {}) {
  const root = process.env.OMP_MANAGER_CONFIG_ROOT && path.resolve(process.env.OMP_MANAGER_CONFIG_ROOT);
  const Settings = pi.pi?.Settings;
  const sdk = process.env.OMP_MANAGER_SDK_DIR || process.env.OMP_SDK_DIR;
  let frozenDefault = process.env.OMP_MANAGER_CONFIG_DEFAULT || null;
  const originals = new Map(), bases = new Map(), runtimeRoots = new Set();
  let context, session, mode, baseline, definitions, helpers, detach, detachGate, detachSampling, slotManifest, formatSelector, planPin, planOriginal;
  let inputFiles = [], baseFiles = [], activeAssets = null, assetItems = emptyItems();
  let explicit = process.env.OMP_MANAGER_CONFIG_SELECTION || null;
  let effective = { configPath: null, configName: null, source: 'native', configFiles: [], setupPaths: [] };
  let error, initialized, appliedModel, switching = false, disposed = false, started = false, modelPending = false;
  let transition = Promise.resolve();
  if (root && owners++ === 0) installNativeCapture(pi.pi ?? {});
  const snapshot = () => {
    const children = context?.agent?.kind === 'main' ? [...(roots.get(context.modelRegistry)?.children.values() ?? [])] : [];
    return { model: modelId(context), ...effective, ...(error ? { error } : {}), ...(children.length ? { children } : {}) };
  };
  const publish = () => {
    const state = snapshot();
    if (context?.agent?.kind === 'main') emit('config_state', state);
    else if (context) {
      const parent = roots.get(context.modelRegistry);
      if (parent) {
        parent.children.set(context.agent.id, { id: context.agent.id, ...state, ...(parent.mcpAvailable ? { mcpInheritedFrom: parent.id } : {}) });
        parent.publish();
      }
    }
    return state;
  };
  const cwd = () => context.sessionManager.getCwd();
  async function load(file) {
    const saved = await readSetupConfig(file, root, definitions);
    const flat = {};
    for (const setting of definitions) { const value = own(saved.settings, setting.segments); if (value !== undefined) flat[setting.id] = value; }
    const validated = Settings.isolated(flat);
    return { ...saved, values: new Map(definitions.filter(setting => Object.hasOwn(flat, setting.id)).map(setting => [setting, setting.get(validated)])) };
  }
  const isSetup = file => isAppSetupPath(file, root);
  const orderedFiles = selection => configPolicyFiles(inputFiles, { root, defaultPath: frozenDefault, selection });
  async function compose(selected) {
    const configFiles = orderedFiles(selected), setupPaths = configFiles.filter(isSetup);
    const snapshots = new Map(), mergedAssets = new Map(), keys = new Set();
    for (const file of setupPaths) {
      if (!snapshots.has(file)) snapshots.set(file, await load(file));
      const saved = snapshots.get(file);
      for (const setting of saved.values.keys()) keys.add(setting);
      for (const asset of saved.assetFiles) mergeSetupAsset(mergedAssets, { ...asset, setupPath: file });
    }
    const settings = setupPaths.length ? await Settings.loadReadOnly({ cwd: cwd(), configFiles }) : baseline;
    const values = new Map([...keys].map(setting => {
      let value = setting.get(settings);
      if (setting.id === 'modelRoles') { value = { ...value }; delete value.default; }
      return [setting, value];
    }));
    const assets = [...mergedAssets.values()], metadata = assetMetadata(assets);
    const signature = bundleDigest('{}\n', metadata);
    const mcp = mergedAssets.get('mcp.json');
    return { configPath: selected.path, configName: selected.path ? snapshots.get(selected.path)?.configName ?? null : null,
      configFiles, setupPaths, snapshots, values, assetFiles: assets, signature,
      mcpOrigin: mcp ? `${mcp.setupPath}:${mcp.file.sha256}` : null,
      manifest: { assets: assets.map(asset => asset.path), assetMetadata: metadata, contentSha256: signature } };
  }
  async function discard(assets) {
    if (assets?.directory && runtimeRoots.has(assets.directory)) {
      await rm(assets.directory, { recursive: true, force: true });
      runtimeRoots.delete(assets.directory);
    }
  }
  async function materialize(next) {
    if (!next.assetFiles.length) return null;
    if (activeAssets?.signature === next.signature) {
      await verifySavedBundle(path.join(activeAssets.directory, 'config.yml'), next.manifest);
      return { ...activeAssets, mcpOrigin: next.mcpOrigin };
    }
    if (!slotManifest) throw fail('private session capsule is unavailable');
    const parent = slotManifest.directory;
    await ownerPrivate(parent);
    const directory = await mkdtemp(path.join(parent, 'generation-'));
    await ownerPrivate(directory);
    const assets = { directory, root: path.join(directory, 'assets'), files: next.manifest.assets, signature: next.signature, mcpOrigin: next.mcpOrigin };
    runtimeRoots.add(directory);
    try {
      await writeFile(path.join(directory, 'config.yml'), '{}\n', { mode: 0o600 });
      for (const asset of next.assetFiles) {
        const destination = path.join(assets.root, asset.path);
        await mkdir(path.dirname(destination), { recursive: true, mode: 0o700 });
        await copyFile(asset.file.sourcePath, destination);
        await chmod(destination, asset.mode & 0o777);
      }
      await verifySavedBundle(path.join(directory, 'config.yml'), next.manifest);
      return assets;
    } catch (cause) { await discard(assets); throw cause; }
  }
  function selection(intent) {
    if (intent) return { path: intent, source: 'session' };
    const assignments = readJson(path.join(root, 'model-configs.json'), []);
    if (!Array.isArray(assignments) || assignments.some(row => !row || typeof row.model !== 'string' || typeof row.configPath !== 'string')) throw fail('invalid model assignments');
    const matches = assignments.filter(row => row.model === modelId(context));
    if (matches.length > 1) throw fail('duplicate exact model assignment');
    if (matches.length) return { path: matches[0].configPath, source: 'model' };
    return frozenDefault ? { path: frozenDefault, source: 'default' } : { path: null, source: 'native' };
  }
  async function prepareAssets(assets) {
    const items = emptyItems();
    if (!assets) return items;
    const source = asset => helpers.createSourceMeta('omp-manager-setup', path.join(assets.root, asset), 'user');
    const scanned = await helpers.scanSkillsFromDir({}, { dir: path.join(assets.root, 'skills'), providerId: 'omp-manager-setup', level: 'user' });
    if (scanned.warnings?.length) throw fail('setup skills could not be parsed');
    items.skills = scanned.items.filter(skill => assets.files.includes(path.relative(assets.root, skill.path).split(path.sep).join('/')))
      .map(skill => ({ ...skill, containRoot: assets.root }));
    for (const asset of assets.files) {
      const file = path.join(assets.root, asset);
      if (isContextAsset(asset)) items['context-files'].push({ path: file, content: text(file), level: 'user', additive: true, _source: source(asset) });
      else if (isRuleAsset(asset)) {
        const rule = helpers.discoverRuleFromMarkdown(asset.replace(/\.(md|mdc)$/, ''), text(file), file, source(asset));
        if (rule) items.rules.push(asset === 'RULES.md' ? { ...rule, alwaysApply: true } : rule);
      } else if (asset === 'mcp.json') items.mcps = mcpAssetItems(file, assets, () => source(asset));
    }
    return items;
  }
  async function refreshMcp() {
    await mode.mcpManager.disconnectAll();
    session.setMCPPromptCommands([]);
    const setting = definitions.find(item => item.id === 'mcp.enableProjectConfig');
    const result = await resourceScope.run(session.settings, () => mode.mcpManager.discoverAndConnect({
      enableProjectConfig: setting ? setting.get(session.settings) : true, filterExa: true,
      filterBrowser: session.getEvalPreludes().some(item => item.name === 'browser'), extensionRoots: session.effectiveExtensionRoots,
    }));
    await session.refreshMCPTools(mode.mcpManager.getTools());
    if (result.errors?.size) throw fail('MCP assets could not be connected; inspect native MCP status');
  }
  function restorePlanRole(roles) {
    if (planOriginal && Object.hasOwn(planOriginal, 'plan')) roles.plan = planOriginal.plan;
    else delete roles.plan;
  }
  function preservePlanIntent() {
    const setting = definitions.find(item => item.id === 'modelRoles');
    if (!setting) return;
    const current = own(session.settings.overlayLayers().overrides, setting.segments);
    if (mode?.planModeEnabled && context.model) {
      const original = current ? { ...current } : undefined;
      if (planPin && original?.plan === planPin) restorePlanRole(original);
      planOriginal = original;
      planPin = formatSelector(modelId(context), session.configuredThinkingLevel());
      setting.override(session.settings, { ...original, plan: planPin });
    } else if (planPin) {
      if (current?.plan === planPin) setting.override(session.settings, planOriginal);
      planPin = planOriginal = undefined;
    }
  }
  const modelMoved = (selected, selectingModel) => selected.source !== 'session' && modelId(context) !== selectingModel;
  const activeChildren = owner => (owner?.children.size ?? 0) || (appliedModel !== undefined ? context.getAsyncJobSnapshot?.()?.running.length ?? 0 : 0);
  async function resolveTarget(intent) {
    baseline = await Settings.loadReadOnly({ cwd: cwd(), configFiles: baseFiles });
    const owner = roots.get(context.modelRegistry);
    const parent = context.agent.kind === 'sub' ? (owner?.members.get(context.agent.parentId) ?? owner) : null;
    if (context.agent.kind === 'sub' && !appliedModel && parent) {
      effective = { ...parent.effective, source: 'inherited' };
      activeAssets = parent.assets;
      assetItems = resources.get(parent.settings) ?? emptyItems();
      resources.set(session.settings, assetItems);
    }
    const selectingModel = modelId(context);
    const selected = selection(parent ? parent.explicit : intent);
    return { owner, parent, selectingModel, selected, next: await compose(selected) };
  }
  function assessChanges(next, owner) {
    const changed = next.assetFiles.length ? activeAssets?.signature !== next.signature : activeAssets !== null;
    const customSkills = definitions.find(setting => setting.id === 'skills.customDirectories');
    if (next.setupPaths.length && customSkills) next.values.set(customSkills, []);
    const inheritedMcp = context.agent.kind === 'sub' && next.mcpOrigin === owner?.assets?.mcpOrigin;
    const mcpChanged = changed && !inheritedMcp && !!(activeAssets?.mcpOrigin || next.mcpOrigin);
    if (context.agent.kind === 'sub' && (mcpChanged || changed)) throw fail('native task resources and MCP are inherited at construction; change the parent setup or run this model in its own managed session');
    return { changed, mcpChanged };
  }
  async function holdForChildren(selected, next) {
    if (!isDeepStrictEqual(next.configFiles, effective.configFiles) || modelId(context) !== appliedModel) throw fail('busy: setup transition must wait for active native work or children to finish');
    if (activeAssets) await verifySavedBundle(path.join(activeAssets.directory, 'config.yml'), next.manifest);
    effective.source = selected.source;
    error = undefined;
    return publish();
  }
  function assertMcpBoundary(atBoundary) {
    if (process.env.OMP_MANAGER_HELPER_REPO) throw fail('read-only helpers cannot activate MCP; use an unrestricted managed session');
    if (!mode?.mcpManager) throw fail('restart-required: native MCP reload is unavailable; start a managed session with MCP enabled');
    const boundarySafe = atBoundary && !session.isCompacting && !session.hasPostPromptWork && !session.agent.state.pendingToolCalls?.size && !context.getPendingPlanReview?.();
    if ((!context.isIdle() && !boundarySafe) || ((started || appliedModel !== undefined) && isQuiescent?.({ modelBoundary: atBoundary }) !== true)) throw fail('busy: MCP setup changes must wait for a safe native model boundary');
  }
  function rememberSubOriginal(setting, parent, live, previousOverrides) {
    if (originals.has(setting)) return;
    const original = own(previousOverrides, setting.segments);
    const inherited = parent.originals.has(setting) ? parent.baseValue(setting) : setting.get(live);
    bases.set(setting, inherited);
    originals.set(setting, original === undefined && parent.originals.has(setting) ? inherited : original);
  }
  function subWrites(parent, changes, next, live, previousOverrides) {
    const writes = new Map();
    if (context.agent.kind !== 'sub') return writes;
    if (!parent) throw fail('native parent setup is unavailable; use an independent managed session');
    for (const setting of changes) {
      rememberSubOriginal(setting, parent, live, previousOverrides);
      const value = next.values.has(setting) ? next.values.get(setting) : originals.get(setting);
      const current = setting.get(live);
      if (isDeepStrictEqual(current, value)) continue;
      assertSubWritable(setting, current, value, live);
      writes.set(setting, value);
    }
    Settings.isolated(Object.fromEntries([...writes].filter(([, value]) => value !== undefined).map(([setting, value]) => [setting.id, value])));
    return writes;
  }
  async function writeMainSettings(txn, selected, next, live) {
    if (!slotManifest || typeof live.reloadFromDisk !== 'function') throw fail('native settings capsule is unavailable; restart through the app-managed launcher');
    const previous = await Promise.all(slotManifest.entries.filter(entry => entry.kind !== 'native').map(async entry => [entry.path, await readFile(entry.path, 'utf8')]));
    const watch = context.mode === 'tui';
    preservePlanIntent();
    live.stopWatching();
    txn.restoreSettings = async () => {
      live.stopWatching();
      try {
        for (const [file, content] of previous) {
          await atomicSlot(file, content);
        }
        await resourceScope.run(live, () => live.reloadFromDisk());
      } finally { if (watch) live.startWatching(); }
    };
    try {
      await renderSlots(slotManifest, selected, root, definitions, next.snapshots);
      await resourceScope.run(live, () => live.reloadFromDisk());
    } finally { if (watch) live.startWatching(); }
  }
  async function writeSettings(txn, selected, next, live, writes, previousOverrides) {
    if (context.agent.kind === 'main') {
      await writeMainSettings(txn, selected, next, live);
      return;
    }
    txn.restoreSettings = async () => {
      for (const setting of writes.keys()) setting.override(live, own(previousOverrides, setting.segments));
    };
    for (const [setting, value] of writes) setting.override(live, value);
  }
  async function activateAssets(txn, live, nextItems, mcpChanged) {
    const previousAssets = activeAssets, previousItems = assetItems;
    activeAssets = txn.preparedAssets; assetItems = nextItems; resources.set(live, assetItems);
    try {
      await session.runToolRegistryMutation(() => Promise.resolve());
      await session.refreshSkills();
      if (mcpChanged) { txn.touchedMcp = true; await refreshMcp(); }
    } catch (cause) {
      activeAssets = previousAssets; assetItems = previousItems; resources.set(live, assetItems);
      throw cause;
    }
    return previousAssets;
  }
  function mainOriginal(setting, live) {
    const original = own(live.overlayLayers().overrides, setting.segments);
    if (setting.id !== 'modelRoles' || !planPin || original?.plan !== planPin) return original;
    const roles = { ...original };
    restorePlanRole(roles);
    return !Object.keys(roles).length && planOriginal === undefined ? undefined : roles;
  }
  function recordOriginals(changes, next, live) {
    for (const setting of changes) {
      if (context.agent.kind === 'main') originals.set(setting, mainOriginal(setting, live));
      if (!next.values.has(setting)) { originals.delete(setting); bases.delete(setting); }
    }
  }
  async function deferForModel(preparedAssets) {
    modelPending = true;
    if (preparedAssets?.directory !== activeAssets?.directory) await discard(preparedAssets);
    return snapshot();
  }
  // Returns a state for early exits; undefined once the setup is fully applied.
  async function transitionSetup(txn, intent, manual, atBoundary) {
    const { owner, parent, selectingModel, selected, next } = await resolveTarget(intent);
    if (modelMoved(selected, selectingModel)) { modelPending = true; return snapshot(); }
    const { changed, mcpChanged } = assessChanges(next, owner);
    const children = activeChildren(owner);
    if (context.agent.kind === 'main' && children) return holdForChildren(selected, next);
    if (mcpChanged) assertMcpBoundary(atBoundary);
    const live = session.settings, previousOverrides = live.overlayLayers().overrides;
    const changes = new Set([...originals.keys(), ...(parent?.originals.keys() ?? []), ...next.values.keys()]);
    const writes = subWrites(parent, changes, next, live, previousOverrides);
    txn.preparedAssets = context.agent.kind === 'sub' ? activeAssets : await materialize(next);
    const nextItems = changed ? await prepareAssets(txn.preparedAssets) : assetItems;
    if (manual && busy()) throw fail('busy: switch setups only while the session is idle');
    if (modelMoved(selected, selectingModel)) return deferForModel(txn.preparedAssets);
    await writeSettings(txn, selected, next, live, writes, previousOverrides);
    const retiredAssets = await activateAssets(txn, live, nextItems, mcpChanged);
    recordOriginals(changes, next, live);
    effective = { configPath: next.configPath, configName: next.configName, source: selected.source, configFiles: next.configFiles, setupPaths: next.setupPaths };
    appliedModel = modelId(context);
    if (retiredAssets?.directory !== activeAssets?.directory) await discard(retiredAssets);
    error = undefined;
  }
  async function rollback(txn) {
    if (txn.restoreSettings) {
      await txn.restoreSettings();
      await session.refreshSkills();
      if (txn.touchedMcp) await refreshMcp();
    }
    if (txn.preparedAssets?.directory !== activeAssets?.directory) await discard(txn.preparedAssets);
  }
  async function apply(intent = explicit, manual = false, atBoundary = false) {
    if (!root || disposed) return snapshot();
    const txn = { preparedAssets: undefined, restoreSettings: undefined, touchedMcp: false };
    try {
      const early = await transitionSetup(txn, intent, manual, atBoundary);
      if (early) return early;
    } catch (cause) {
      try {
        await rollback(txn);
      } catch {
        error = 'config_policy: native setup rollback failed; restart this managed session';
        return publish();
      }
      error = safeError(cause);
    }
    return publish();
  }
  function baseValue(setting) {
    if (context?.agent.kind !== 'main') return bases.get(setting);
    const original = originals.get(setting);
    return setting.get(baseline.overlay(original === undefined ? {} : { [setting.id]: original }));
  }
  function schedule(intent, remember = false, atBoundary = false) {
    transition = transition.then(async () => {
      const state = await apply(intent, remember, atBoundary);
      if (remember && !state.error) explicit = intent;
      const owner = context && roots.get(context.modelRegistry);
      const current = { id: context?.agent.id, explicit, originals: new Map(originals), bases: new Map(bases), baseValue, assets: activeAssets, definitions, settings: session?.settings, effective, inputFiles, defaultPath: frozenDefault };
      if (context?.agent?.kind === 'main') {
        const state = owner ?? { children: new Map(), members: new Map(), publish, snapshot };
        Object.assign(state, current, { id: context.agent.id, mcpAvailable: !!mode?.mcpManager });
        roots.set(context.modelRegistry, state);
      } else if (owner) owner.members.set(context.agent.id, current);
      return state;
    });
    return transition;
  }
  async function initialize() {
    if (!Settings || !sdk) throw fail('native settings integration unavailable');
    const fromSdk = relative => import(pathToFileURL(path.join(sdk, relative)).href);
    const sourceSettings = await fromSdk('src/config/settings.ts');
    if (sourceSettings.Settings !== Settings) throw fail('native module identity mismatch: restart this app session with the pinned SDK source CLI');
    const { all } = await fromSdk('src/config/registry.ts');
    const capabilities = await fromSdk('src/capability/index.ts');
    helpers = await fromSdk('src/discovery/helpers.ts');
    ({ formatModelSelectorValue: formatSelector } = await import(pathToFileURL(Bun.resolveSync(`${sdkScope}/pi-tui/overlays/model-selector`, sdk)).href));
    if (!registered.has(capabilities)) {
      for (const id of ['skills', 'rules', 'context-files', 'mcps']) capabilities.registerProvider(id, {
        id: 'omp-manager-setup', displayName: 'OMP Manager setup', description: 'Reviewed app-owned setup assets', priority: 1000,
        load: async () => ({ items: resources.get(resourceScope.getStore())?.[id] ?? [] }),
      });
      registered.add(capabilities);
    }
    definitions = all();
    if (!definitions.length) throw fail('native setting registry unavailable');
    const parent = context.agent.kind === 'sub' ? roots.get(context.modelRegistry) : null;
    const files = parent?.inputFiles ?? JSON.parse(process.env.OMP_MANAGER_CONFIG_FILES || '[]');
    if (!Array.isArray(files) || files.some(file => typeof file !== 'string')) throw fail('invalid base overlays');
    if (parent) frozenDefault = parent.defaultPath;
    inputFiles = files.map(file => path.resolve(file));
    baseFiles = inputFiles.filter(file => !isSetup(file));
    if (context.agent.kind === 'main') {
      slotManifest = JSON.parse(process.env.OMP_MANAGER_CONFIG_SLOT_MANIFEST || 'null');
      if (slotManifest?.version !== 1 || !Array.isArray(slotManifest.entries) || path.dirname(slotManifest.directory) !== root || !path.basename(slotManifest.directory).startsWith('.config-session-')) throw fail('native settings capsule is unavailable; restart through the app-managed launcher');
      await ownerPrivate(slotManifest.directory);
    }
  }
  async function attachContext(ctx, atBoundary = false) {
    if (!ctx?.agent || disposed) return snapshot();
    context = ctx;
    if (!root) return snapshot();
    const next = sessions.get(ctx.sessionManager);
    if (!next) { error = 'config_policy: native session unavailable'; return publish(); }
    if (modelId(ctx) !== modelId({ model: next.model })) {
      error = 'config_policy: native extension model binding is unavailable; initialize the native runtime before attaching';
      return publish();
    }
    mode = modes.get(ctx.sessionManager);
    if (session !== next) {
      detach?.(); detachGate?.(); detachSampling?.(); session = next;
      detachSampling = installLiveSampling(session.agent);
      detach = session.subscribe(event => {
        if (event.type !== 'model_changed' || !definitions) return;
        modelPending = true;
        if (idle()) { modelPending = false; void schedule(); }
      });
      detachGate = session.agent.addBeforeModelCallHook(async () => {
        do {
          if (modelPending) { modelPending = false; await schedule(undefined, false, true); }
          else await transition;
        } while (modelPending);
        if (error) throw new Error(error);
      });
    }
    try {
      initialized ??= initialize();
      await initialized;
    } catch (cause) { error = safeError(cause); return publish(); }
    modelPending = false;
    return schedule(undefined, false, atBoundary);
  }
  const idle = () => context && session && context.isIdle() && !context.hasPendingMessages() && !session.isStreaming &&
    !session.isCompacting && !session.hasPostPromptWork && !session.agent.hasQueuedMessages() && !session.agent.state.pendingToolCalls?.size &&
    !(context.agent.kind === 'main' && (roots.get(context.modelRegistry)?.children.size || context.getAsyncJobSnapshot?.()?.running.length));
  const busy = () => !idle() || (isQuiescent && !isQuiescent());
  async function control(payload) {
    if (!payload || !Object.hasOwn(payload, 'path') || (payload.path !== null && (typeof payload.path !== 'string' || !payload.path))) throw fail('path must be a setup path or null');
    if (disposed) throw fail('native session is closed');
    if (switching || busy()) throw fail('busy: switch setups only while the session is idle');
    switching = true;
    try {
      initialized ??= initialize();
      await initialized;
      return await schedule(payload.path, true);
    } finally { switching = false; }
  }
  pi.on('session_start', async (_event, ctx) => { if (ctx.agent?.kind === 'sub') await attachContext(ctx); });
  pi.on('session_switch', async (_event, ctx) => { if (ctx.agent?.kind === 'sub') await attachContext(ctx); });
  pi.on('before_agent_start', async (event, ctx) => {
    const previousPrompt = ctx.getSystemPrompt();
    const state = await attachContext(ctx, true);
    if (state.error) throw new Error(state.error);
    if (!session || JSON.stringify(previousPrompt) === JSON.stringify(session.agent.state.systemPrompt)) return;
    if (JSON.stringify(event.systemPrompt) !== JSON.stringify(previousPrompt)) {
      error = 'config_policy: restart-required: setup resources changed after another extension replaced the prompt';
      publish();
      throw new Error(error);
    }
    return { systemPrompt: session.agent.state.systemPrompt };
  });
  pi.on('agent_start', () => { started = true; });
  let cleanup;
  function dispose() {
    if (disposed) return cleanup;
    disposed = true; detach?.(); detachGate?.(); detachSampling?.();
    if (session) resources.delete(session.settings);
    if (context?.agent?.kind === 'sub') {
      const parent = roots.get(context.modelRegistry);
      parent?.members.delete(context.agent.id);
      if (parent?.children.delete(context.agent.id)) parent.publish();
    }
    if (root && --owners === 0) { for (const restore of restores.splice(0).reverse()) restore(); }
    cleanup = transition.finally(() => Promise.all([...runtimeRoots].map(directory => discard({ directory })))).then(() => undefined);
    return cleanup;
  }
  pi.on('session_shutdown', dispose);
  return { attachContext, control, snapshot, dispose };
}
