import { pathToFileURL } from 'node:url';
import path from 'node:path';

const excludedRoots = new Set(['extensions', 'hooks', 'customtools', 'mcp', 'telemetry', 'auth', 'shellpath']);
const secretKeys = new Set(['apikey', 'api_key', 'password', 'secret', 'token', 'accesstoken', 'refreshtoken', 'authorization', 'cookie', 'credentials', 'headers', 'env']);

function record(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}
function get(value, segments) {
  for (const segment of segments) {
    if (!record(value) || !Object.hasOwn(value, segment)) return undefined;
    value = value[segment];
  }
  return value;
}
function put(target, segments, value) {
  for (const segment of segments.slice(0, -1)) {
    if (!Object.hasOwn(target, segment)) target[segment] = Object.create(null);
    target = target[segment];
  }
  target[segments.at(-1)] = value;
}

// Pure filtering: registered native setting metadata determines what is portable.
export function sanitizeConfig(input, definitions) {
  if (!record(input) || !Object.keys(input).length) throw new Error('Config must be a non-empty YAML mapping');
  const omitted = new Set();
  const output = Object.create(null);
  const known = new Map(definitions.map(def => [def.id, def]));
  const prefixes = new Set();
  for (const def of definitions) {
    for (let index = 1; index < def.segments.length; index++) prefixes.add(def.segments.slice(0, index).join('.'));
  }
  function clean(value, location, ancestors = new Set()) {
    if (value === null || typeof value !== 'object') return value;
    if (ancestors.has(value)) throw new Error('Config must not contain cyclic YAML aliases');
    const next = new Set(ancestors);
    next.add(value);
    if (Array.isArray(value)) return value.map((item, index) => clean(item, `${location}[${index}]`, next));
    const result = Object.create(null);
    for (const [key, item] of Object.entries(value)) {
      const child = `${location}.${key}`;
      if (secretKeys.has(key.toLowerCase()) || ['__proto__', 'constructor', 'prototype'].includes(key)) omitted.add(child);
      else result[key] = clean(item, child, next);
    }
    return result;
  }
  function visit(value, segments) {
    const location = segments.join('.');
    const def = known.get(location);
    if (def) {
      if (def.isCredential || excludedRoots.has(segments[0].toLowerCase())) { omitted.add(location); return; }
      put(output, def.segments, clean(value, location));
    } else if (record(value) && prefixes.has(location)) {
      for (const [key, item] of Object.entries(value)) visit(item, [...segments, key]);
    } else omitted.add(location);
  }
  for (const [key, value] of Object.entries(input)) visit(value, [key]);
  if (!Object.keys(output).length) throw new Error('No portable settings remain after safety filtering');
  if (Object.hasOwn(output, 'modelRoles')) {
    if (!record(output.modelRoles)) throw new Error('modelRoles must be a mapping');
    for (const [role, value] of Object.entries(output.modelRoles)) {
      if (value !== null && (typeof value !== 'string' || !value.trim())) throw new Error(`modelRoles.${role} requires a model selection or null`);
    }
  }
  return { settings: output, omitted: [...omitted].sort() };
}

export async function runConfig(request) {
  const sdk = path.resolve(request.sdkDir);
  const dirsPath = Bun.resolveSync('@oh-my-pi/pi-utils/dirs', sdk);
  const dirs = await import(pathToFileURL(dirsPath).href);
  dirs.setProfile(request.profile || 'default');
  if (request.action === 'paths') return { globalOmpFolder: dirs.getBaseConfigRoot(), globalAgentFolder: dirs.getAgentDir() };
  const { Settings } = await import(pathToFileURL(path.join(sdk, 'src/config/settings.ts')).href);
  const registry = await import(pathToFileURL(path.join(sdk, 'src/config/registry.ts')).href);
  const definitions = registry.all();
  if (!definitions.length) throw new Error('OMP compatibility: native setting registry is empty');
  let input;
  if (request.action === 'export') {
    const settings = await Settings.loadReadOnly({ cwd: request.cwd, agentDir: dirs.getAgentDir(), configFiles: request.configFiles ?? [] });
    input = Object.create(null);
    for (const def of definitions) {
      if (settings.isConfigured(def)) put(input, def.segments, def.get(settings));
    }
  } else if (request.action === 'import') {
    const text = request.yaml ?? await Bun.file(request.path).text();
    input = Bun.YAML.parse(text);
  } else throw new Error('Invalid portable config action');
  const filtered = sanitizeConfig(input, definitions);
  Settings.isolated(filtered.settings);
  return { yaml: Bun.YAML.stringify(filtered.settings), modelRoles: filtered.settings.modelRoles ?? {}, omitted: filtered.omitted };
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
