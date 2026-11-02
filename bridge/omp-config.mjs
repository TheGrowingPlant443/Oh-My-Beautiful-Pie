import { pathToFileURL } from 'node:url';
import path from 'node:path';
import { isLocalModel } from './local-ai-models.mjs';
import fs from 'node:fs/promises';
import { homedir } from 'node:os';
import { assetFromFile, collectAssets, configPolicyFiles, directoryEntries, exportedSettings, isAppSetupPath, mergeSetupAsset, noSymlinks, parseBundleJson, readBundle, readText, requiresAssetExecutionReview, safeSetupName, sanitizeAssets, sanitizeConfig, validateAssetFiles, verifySavedBundle } from './config-bundle.mjs';
// OMBP_AGENT=omsp drives the omsp fork, whose packages use their own npm scope.
const sdkScope = process.env.OMBP_AGENT === 'omsp' ? '@oh-my-striped-pi' : '@oh-my-pi';

const loadSdk = (sdk, file) => import(pathToFileURL(path.join(sdk, file)).href);

// Session selectors are data: rendering does not require a current model registration or credential.
async function savedSessionSelectors(sdk, sessionPath) {
  const { SessionManager } = await loadSdk(sdk, 'src/session/session-manager.ts');
  const { getRestorableSessionModels } = await loadSdk(sdk, 'src/session/session-context.ts');
  const session = await SessionManager.open(sessionPath, undefined, undefined, { suppressBreadcrumb: true, throwIfMissing: true });
  try {
    const context = session.buildSessionContext();
    return getRestorableSessionModels(context.models, session.getLastModelChangeRole());
  } finally { await session.close(); }
}

async function savedSessionModel(sdk, resolveSaved, sessionPath) {
  for (const selector of await savedSessionSelectors(sdk, sessionPath)) {
    const saved = resolveSaved(selector);
    if (saved?.model) return saved;
  }
  return undefined;
}

async function validateSetupPath(file, root) {
  if (typeof file !== 'string' || !path.isAbsolute(file) || path.basename(file) !== 'config.yml' || root && !isAppSetupPath(file, root)) throw new Error('Invalid app-owned setup path');
  await noSymlinks(file);
  if (!(await fs.lstat(file)).isFile()) throw new Error('Setup source must be a regular file');
}

async function readSetupMetadata(file) {
  try {
    return parseBundleJson(await readText(path.join(path.dirname(file), 'setup.json')));
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
    return undefined;
  }
}

async function verifySetupMetadata(file, metadata, utility) {
  if (metadata.version !== 1 || !Array.isArray(metadata.assets) || typeof metadata.executableReview !== 'boolean' || typeof metadata.requiresExecutionReview !== 'boolean') throw new Error('Invalid setup review metadata');
  if (!utility && !metadata.executableReview && (metadata.requiresExecutionReview || requiresAssetExecutionReview(metadata.assetMetadata ?? metadata.assets.map(path => ({ path }))))) throw new Error('Setup execution review is required before use');
  await verifySavedBundle(file, metadata);
}

async function requireNoUnreviewedAssets(file) {
  try { await fs.lstat(path.join(path.dirname(file), 'assets')); throw new Error('Setup review metadata is missing'); }
  catch (error) { if (error.code !== 'ENOENT') throw error; }
}

async function reviewedSetup(file, root, utility = false) {
  await validateSetupPath(file, root);
  const metadata = await readSetupMetadata(file);
  if (metadata) await verifySetupMetadata(file, metadata, utility);
  else await requireNoUnreviewedAssets(file);
  return utility ? undefined : readBundle(path.dirname(file));
}

function initialSelection(request) {
  if (request.configSelection) return { path: request.configSelection, source: 'session' };
  if (request.configDefault) return { path: request.configDefault, source: 'default' };
  return { path: null, source: 'native' };
}

async function selectedFiles(request, selection) {
  const configFiles = configPolicyFiles(request.configFiles ?? [], { root: request.configRoot, defaultPath: request.configDefault ?? null, selection });
  const setupPaths = configFiles.filter(file => isAppSetupPath(file, request.configRoot));
  await Promise.all([...new Set(setupPaths)].map(file => reviewedSetup(file, request.configRoot, request.utility === true)));
  return { configFiles, setupPaths };
}

async function modelAssignments(root) {
  let rows;
  try {
    rows = parseBundleJson(await readText(path.join(root, 'model-configs.json')));
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
    rows = [];
  }
  if (!Array.isArray(rows)) throw new Error('Invalid persisted model setup assignments');
  const seen = new Set();
  for (const row of rows) {
    if (!row || typeof row.model !== 'string' || !/^[^/\s@][^/\s]*\/\S+$/.test(row.model) || /[\x00-\x1f\x7f]/.test(row.model) || seen.has(row.model) || typeof row.configPath !== 'string' || !path.isAbsolute(row.configPath) || !isAppSetupPath(row.configPath, root) || path.basename(row.configPath) !== 'config.yml') throw new Error('Invalid persisted model setup assignments');
    seen.add(row.model);
  }
  return rows;
}

async function resolvedSelection(request, selector) {
  const selection = initialSelection(request);
  if (selection.source === 'session' || !selector) return selection;
  const assigned = (await modelAssignments(request.configRoot)).find(row => row.model === selector);
  return assigned ? { path: assigned.configPath, source: 'model' } : selection;
}

async function stoppedExport(request, sdk, dirs, Settings) {
  const resolved = await resolveSourceModel(request, sdk, dirs, Settings);
  const selection = await resolvedSelection(request, resolved.selector);
  return { ...request, ...await selectedFiles(request, selection) };
}

async function reviewedConfigRequest(request) {
  if (!request.configRoot) return request;
  if (!request.authoritativeConfigFiles) return { ...request, ...await selectedFiles(request, initialSelection(request)) };
  const setups = (request.configFiles ?? []).filter(file => isAppSetupPath(file, request.configRoot));
  await Promise.all([...new Set(setups)].map(file => reviewedSetup(file, request.configRoot, request.utility === true)));
  return request;
}

async function resolveSourceModel(request, sdk, dirs, Settings) {
  request = await reviewedConfigRequest(request);
  if (request.renderOnly) {
    if (!request.utility) throw new Error('Render-only preparation is restricted to non-generating utilities');
    const selector = request.model || (request.sessionPath ? (await savedSessionSelectors(sdk, request.sessionPath))[0] : null);
    return { local: false, selector: selector || null, thinkingLevel: null };
  }
  return resolveNativeModel(request, sdk, dirs, Settings);
}

async function resolveNativeModel(request, sdk, dirs, Settings) {
  const settings = await Settings.loadReadOnly({ cwd: request.cwd, agentDir: dirs.getAgentDir(), configFiles: request.configFiles ?? [] });
  const { discoverAuthStorage } = await loadSdk(sdk, 'src/sdk.ts');
  const { ModelRegistry } = await loadSdk(sdk, 'src/config/model-registry.ts');
  const { extractExplicitThinkingSelector, resolveCliModel, resolveSessionModelSelector } = await loadSdk(sdk, 'src/config/model-resolver.ts');
  const auth = await discoverAuthStorage(dirs.getAgentDir(), { settings, cwd: request.cwd });
  try {
    // Static/cached metadata only: admission never probes a model or performs inference.
    const models = new ModelRegistry(auth, path.join(dirs.getAgentDir(), 'models.yml'), { settings, fetch: () => Promise.reject(new Error('Local setup admission does not discover models online')) });
    await models.hydrateCredentialScopedModelCaches();
    const { resolved, exactUtilitySelector } = await requestedModel(request, sdk, models, settings, resolveCliModel, resolveSessionModelSelector);
    if (!resolved.model) {
      if (request.model || exactUtilitySelector) throw new Error('Configured model could not be resolved from cached native model metadata');
      return { local: false, selector: null, thinkingLevel: null };
    }
    if (exactUtilitySelector) assertExactUtilitySelector(resolved.model, exactUtilitySelector, extractExplicitThinkingSelector);
    const explicitThinking = request.model?.includes(':') && resolved.thinkingLevel !== undefined
      ? extractExplicitThinkingSelector(request.model) : undefined;
    return { local: isLocalModel(resolved.model), selector: `${resolved.model.provider}/${resolved.model.id}`, thinkingLevel: explicitThinking ?? null };
  } finally { auth.close(); }
}

async function requestedModel(request, sdk, models, settings, resolveCliModel, resolveSessionModelSelector) {
  const cli = cliModel => resolveCliModel({ cliModel, modelRegistry: models, settings });
  const exact = request.utility && request.model?.includes('/') && !request.model.startsWith('@') ? request.model : null;
  if (request.model) return { resolved: cli(request.model), exactUtilitySelector: exact };
  if (!request.sessionPath) return { resolved: cli('@default'), exactUtilitySelector: exact };
  if (!request.utility) {
    const saved = await savedSessionModel(sdk, selector => resolveSessionModelSelector(models, selector), request.sessionPath);
    return { resolved: saved ?? cli('@default'), exactUtilitySelector: exact };
  }
  const selector = (await savedSessionSelectors(sdk, request.sessionPath))[0];
  if (!selector) return { resolved: cli('@default'), exactUtilitySelector: exact };
  return { resolved: cli(selector) ?? cli('@default'), exactUtilitySelector: selector };
}

function assertExactUtilitySelector(model, exactUtilitySelector, extractExplicitThinkingSelector) {
  const canonical = `${model.provider}/${model.id}`, explicit = extractExplicitThinkingSelector(exactUtilitySelector);
  if (exactUtilitySelector !== canonical && (explicit === undefined || exactUtilitySelector !== `${canonical}:${explicit}`)) throw new Error('Utility generation requires its exact registered native model selector');
}

async function prepareSession(request, sdk, dirs, Settings) {
  if (!request.configRoot || !request.directory) throw new Error('Session configuration preparation requires an app-owned capsule');
  const resolved = await resolveSourceModel(request, sdk, dirs, Settings);
  const selection = request.authoritativeConfigFiles ? initialSelection(request) : await resolvedSelection(request, resolved.selector);
  const { prepareConfigPolicy } = await import('./omp-config-policy.mjs');
  const prepared = await prepareConfigPolicy({
    root: request.configRoot, directory: request.directory, files: request.configFiles ?? [],
    defaultPath: request.configDefault ?? null, selection, model: resolved.selector,
    cwd: request.cwd, profile: request.profile || 'default', sdkDir: sdk, helper: request.helper === true,
    utility: request.utility === true, authoritativeConfigFiles: request.authoritativeConfigFiles === true,
    renderOnly: request.renderOnly === true,
  });
  return {
    configFiles: prepared.configFiles, model: resolved.selector, local: resolved.local, slotManifest: prepared.slotManifest,
    ...(resolved.thinkingLevel === null ? {} : { thinkingLevel: resolved.thinkingLevel }),
  };
}

async function configModels(request, sdk, dirs, Settings) {
  const settings = await Settings.loadReadOnly({ cwd: request.cwd, agentDir: dirs.getAgentDir(), configFiles: request.configFiles ?? [] });
  const { discoverAuthStorage } = await loadSdk(sdk, 'src/sdk.ts');
  const { ModelRegistry } = await loadSdk(sdk, 'src/config/model-registry.ts');
  const auth = await discoverAuthStorage(dirs.getAgentDir(), { settings, cwd: request.cwd });
  try {
    const registry = new ModelRegistry(auth, path.join(dirs.getAgentDir(), 'models.yml'), { settings, fetch: () => Promise.reject(new Error('Config model listing does not discover models online')) });
    await registry.hydrateCredentialScopedModelCaches();
    const available = new Set(registry.getAvailable().map(model => `${model.provider}/${model.id}`));
    return { profile: request.profile || 'default', models: registry.getAll().map(model => ({ selector: `${model.provider}/${model.id}`, name: model.name, available: available.has(`${model.provider}/${model.id}`) })), error: null };
  } finally { auth.close(); }
}

function nativePath(nativeRoots, file, kind, fallback) {
  for (const root of nativeRoots) {
    const relative = path.relative(path.join(root, kind), file);
    if (relative && relative !== '..' && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative)) return `${kind}/${relative.split(path.sep).join('/')}`;
  }
  return fallback;
}

async function addNativeRootAssets(add, nativeRoots) {
  for (const directory of nativeRoots) {
    try { add(await collectAssets(directory, ['rules', 'RULES.md', 'mcp.json'])); } catch (error) { if (error.code !== 'ENOENT') throw error; }
  }
}

function skillFiltered(self, skills, disabledExtensions) {
  if (!self || disabledExtensions.includes(`skill:${self.name}`)) return true;
  if (skills.ignoredSkills.some(pattern => new Bun.Glob(pattern).match(self.name))) return true;
  return skills.includeSkills.length && !skills.includeSkills.some(pattern => new Bun.Glob(pattern).match(self.name));
}

async function missingSkillEntry(entry) {
  try {
    await noSymlinks(entry);
    return false;
  } catch (error) {
    if (error.code === 'ENOENT') return true;
    throw error;
  }
}

async function addStandaloneSkills(request, sdk, skills, loaded, disabledExtensions) {
  const { scanSkillsFromDir } = await loadSdk(sdk, 'src/discovery/helpers.ts');
  const { expandTilde } = await loadSdk(sdk, 'src/tools/path-utils.ts');
  const paths = new Set(loaded.skills.map(skill => path.resolve(skill.filePath)));
  for (const directory of skills.customDirectories) {
    const root = path.resolve(request.cwd, expandTilde(directory)), entry = path.join(root, 'SKILL.md');
    if (paths.has(entry) || await missingSkillEntry(entry)) continue;
    // Native custom roots scan children; portable bundles also preserve a configured standalone skill root.
    const scanned = await scanSkillsFromDir({ cwd: request.cwd, home: homedir(), repoRoot: null }, { dir: root, providerId: 'custom', level: 'user', requireDescription: true, includeSelf: true });
    const self = scanned.items.find(skill => path.resolve(skill.path) === entry);
    if (skillFiltered(self, skills, disabledExtensions)) continue;
    loaded.skills.push({ name: self.name, filePath: self.path });
    paths.add(entry);
  }
}

async function addSkillAssets(add, nativeRoots, skills) {
  for (const skill of skills) {
    if (!/^[\p{L}\p{N}_.~/-]+$/u.test(skill.name) || skill.name.split('/').some(part => part === '..' || part === '.')) throw new Error('Native skill name cannot be represented safely');
    await noSymlinks(skill.filePath);
    const target = nativePath(nativeRoots, skill.filePath, 'skills', `skills/${skill.name}/SKILL.md`);
    // Directory skills retain every supporting resource under their original native relative path.
    if (path.basename(skill.filePath) === 'SKILL.md') {
      const root = path.dirname(skill.filePath);
      add(await collectAssets(root, await directoryEntries(root), `${path.posix.dirname(target)}/`));
    } else add([await assetFromFile(skill.filePath, target)]);
  }
}

async function activeAssets(request, sdk, dirs, settings, nativeSettings, setups) {
  const discovery = await loadSdk(sdk, 'src/discovery/index.ts');
  const release = discovery.initializeWithSettings(nativeSettings);
  try {
    const assets = new Map();
    const add = items => { for (const asset of items) mergeSetupAsset(assets, asset); };
    const nativeRoots = [dirs.getAgentDir(), request.cwd, path.join(request.cwd, process.env.OMBP_AGENT === 'omsp' ? '.omsp' : '.omp')];
    await addNativeRootAssets(add, nativeRoots);
    const contexts = (await discovery.loadCapability('context-files', { cwd: request.cwd, agentDir: dirs.getAgentDir() })).items.filter(file => path.basename(file.path) === 'AGENTS.md');
    for (const [index, context] of contexts.entries()) {
      add([await assetFromFile(context.path, index === 0 ? 'AGENTS.md' : `rules/.omp-context/${index}-AGENTS.md`)]);
    }
    const { loadSkills } = await loadSdk(sdk, 'src/extensibility/skills.ts');
    const { cfgSkills, cfgDisabledExtensions } = await loadSdk(sdk, 'src/extensibility/settings.ts');
    const configuredSkills = cfgSkills.get(nativeSettings);
    const skills = setups.length ? { ...configuredSkills, customDirectories: [] } : configuredSkills;
    const disabledExtensions = cfgDisabledExtensions.get(nativeSettings);
    const loaded = await loadSkills({ ...skills, cwd: request.cwd, disabledExtensions });
    if (skills.enabled) await addStandaloneSkills(request, sdk, skills, loaded, disabledExtensions);
    await addSkillAssets(add, nativeRoots, loaded.skills);
    const rules = (await discovery.loadCapability('rules', { cwd: request.cwd, agentDir: dirs.getAgentDir() })).items;
    for (const [index, rule] of rules.entries()) {
      if (rule._source?.provider === 'builtin-defaults' || settings.ttsr?.disabledRules?.includes(rule.name)) continue;
      add([await assetFromFile(rule.path, nativePath(nativeRoots, rule.path, 'rules', `rules/discovered-${index}-${path.basename(rule.path)}`))]);
    }
    for (const setup of setups) add(setup.assets);
    return [...assets.values()];
  } finally { release(); }
}

async function exportSource(request, sdk, dirs, Settings, definitions) {
  if (request.reconstructSelection) {
    if (!request.configRoot) throw new Error('Stopped source setup reconstruction requires the app config root');
    request = await stoppedExport(request, sdk, dirs, Settings);
  }
  const setupPaths = request.setupPaths ?? (request.setupPath ? [request.setupPath] : []);
  if (!Array.isArray(setupPaths) || setupPaths.some(file => typeof file !== 'string')) throw new Error('Invalid setup source paths');
  const setups = await Promise.all(setupPaths.map(file => reviewedSetup(file, request.configRoot)));
  const configFiles = [...(request.configFiles ?? [])];
  if (request.setupPaths === undefined && request.setupPath && !configFiles.includes(request.setupPath)) configFiles.push(request.setupPath);
  const settings = await Settings.loadReadOnly({ cwd: request.cwd, agentDir: dirs.getAgentDir(), configFiles });
  const input = exportedSettings(settings, definitions);
  const assets = await activeAssets(request, sdk, dirs, input, settings, setups);
  const name = request.name ?? setups.at(-1)?.name ?? `${path.basename(request.cwd) || 'Global'} setup`;
  return { input, assets, name };
}

async function importSource(request) {
  const bundle = request.yaml === undefined ? await readBundle(request.path) : { yaml: request.yaml, assets: request.assets ?? [], name: request.name ?? 'Imported setup' };
  if (typeof bundle.yaml !== 'string') throw new Error('Config must be YAML text');
  let input;
  try { input = Bun.YAML.parse(bundle.yaml); } catch { throw new Error('Invalid setup YAML'); }
  return { input, assets: bundle.assets, name: request.name ?? bundle.name };
}

export async function runConfig(request) {
  const sdk = path.resolve(request.sdkDir);
  const dirsPath = Bun.resolveSync(`${sdkScope}/pi-utils/dirs`, sdk);
  const dirs = await import(pathToFileURL(dirsPath).href);
  dirs.setProfile(request.profile || 'default');
  if (request.action === 'paths') return { globalOmpFolder: dirs.getBaseConfigRoot(), globalAgentFolder: dirs.getAgentDir() };
  const { Settings } = await loadSdk(sdk, 'src/config/settings.ts');
  if (request.action === 'prepare_session') return prepareSession(request, sdk, dirs, Settings);
  if (request.action === 'models') return configModels(request, sdk, dirs, Settings);
  const registry = await loadSdk(sdk, 'src/config/registry.ts');
  const definitions = registry.all();
  if (!definitions.length) throw new Error('OMP compatibility: native setting registry is empty');
  let source;
  if (request.action === 'export') source = await exportSource(request, sdk, dirs, Settings, definitions);
  else if (request.action === 'import') source = await importSource(request);
  else throw new Error('Invalid portable config action');
  const { input, assets, name } = source;
  const bundle = sanitizeAssets(assets);
  await validateAssetFiles(bundle.assets);
  bundle.requiresExecutionReview = requiresAssetExecutionReview(bundle.assets);
  const filtered = sanitizeConfig(input, definitions, bundle.assets.length > 0);
  try { Settings.isolated(filtered.settings); } catch { throw new Error('Setup contains invalid native settings'); }
  return { yaml: Bun.YAML.stringify(filtered.settings), modelRoles: filtered.settings.modelRoles ?? {}, omitted: [...filtered.omitted, ...bundle.omitted], name: safeSetupName(name), assets: bundle.assets, requiresExecutionReview: bundle.requiresExecutionReview };
}

if (import.meta.main) {
  try {
    const request = JSON.parse(await Bun.stdin.text());
    const data = await runConfig(request);
    process.stdout.write(JSON.stringify({ ok: true, data }) + '\n');
  } catch (error) {
    // Native diagnostics may contain configured values. Do not echo them to previews.
    const safe = /^(Config must|No portable|modelRoles\.|Invalid portable|OMP compatibility)/.test(String(error.message));
    process.stdout.write(JSON.stringify({ ok: false, error: safe ? error.message : 'OMP config could not be loaded or validated. Check the selected source and native setting types.' }) + '\n');
    process.exitCode = 1;
  }
}
