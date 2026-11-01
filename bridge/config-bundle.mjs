import fs from 'node:fs/promises';
import { createReadStream } from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';

// This is a preview threshold, not an asset limit: larger files travel as checked local references.
const inlineBytes = 64 * 1024;
const roots = new Set(['AGENTS.md', 'RULES.md', 'mcp.json']);
// Each list is one alternation split into simpler regexes; a key or value matches when any regex does.
const credentialKeys = [
  /^(?:auths?|_auth|password|passwd|secret|token|authorization|cookie|credentials?|headers?|env)$/i,
  /^(?:api|private)[_-]?key$/i,
  /^(?:access|refresh)[_-]?token$/i,
  /^client[_-]?secret$/i,
  /(?:^|[_-])(?:api|private)[_-]?key$/i,
  /(?:^|[_-])(?:access|refresh)[_-]?token$/i,
  /(?:^|[_-])(?:client[_-]?secret|password|secret|token)$/i,
  /(?:^|[_-])(?:secret[_-]?)?access[_-]?key(?:[_-]?id)?$/i,
  /(?:Token|PrivateKey|SecretKey|ClientSecret)$/i,
];
export const isCredentialKey = key => credentialKeys.some(pattern => pattern.test(key));
const credentialValue = String.raw`["']?\s*[:=]\s*[^\s,;}]+`;
const credentialContents = [
  /-----BEGIN [A-Z ]*PRIVATE KEY-----/i,
  /\b(?:Bearer|Basic)\s+[\w+/.=-]+/i,
  /\b(?:sk-[\w-]{12,}|gh[pousr]_\w{12,}|AKIA[A-Z0-9]{16})\b/i,
  /\beyJ[\w-]{8,}\.[\w-]{8,}\.[\w-]{8,}\b/i,
  /https?:\/\/[^\s/@]+@/i,
  /[?&](?:key|token|secret|password|auth|authorization|signature|sig)=[^\s&#]+/i,
  /[?&](?:api[_-]?key|access[_-]?token|client[_-]?secret)=[^\s&#]+/i,
  new RegExp(String.raw`\b["']?(?:\w*[_-])?(?:password|passwd|secret|token|authorization|cookie)${credentialValue}`, 'i'),
  new RegExp(String.raw`\b["']?(?:\w*[_-])?(?:api|private)[_-]?key${credentialValue}`, 'i'),
  new RegExp(String.raw`\b["']?(?:\w*[_-])?(?:secret[_-]?)?access[_-]?key(?:[_-]?id)?${credentialValue}`, 'i'),
  /--(?:api[_-]?key|password|secret|token)(?:=|\s+)\S+/i,
];
const credentialStores = [
  /^(?:\.env(?:\..*)?|\.ssh|\.gnupg|\.docker|\.npmrc|\.pypirc|\.netrc|\.git-credentials)$/i,
  /^(?:\.yarnrc(?:\.yml)?|auth\.(?:json|ya?ml)|token|hosts\.yml)$/i,
  /^\.?credentials?(?:\.[^/]*)?$/i,
  /^id_(?:rsa|dsa|ecdsa|ed25519)$/i,
  /^.*\.(?:pem|key|p12|pfx|kdbx)$/i,
];
const credentialStore = part => credentialStores.some(pattern => pattern.test(part));
const ordinal = (a, b) => (a < b ? -1 : Number(a > b));
const excludedRoots = new Set(['extensions', 'hooks', 'customtools', 'mcp', 'telemetry', 'auth', 'shellpath']);
const excludedSettings = new Set(['skills.customDirectories']);

function record(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}
export function containsCredentials(value) {
  if (credentialContents.some(pattern => pattern.test(value))) return true;
  const sensitive = key => isCredentialKey(key) && !/^(?:env|headers?)$/i.test(key);
  for (const match of value.matchAll(/(?<![\w-])["']?([A-Za-z_][A-Za-z0-9_-]*)["']?\s*[:=]\s*(?=\S)/g)) {
    if (sensitive(match[1])) return true;
  }
  for (const match of value.matchAll(/(?<![\w-])--([A-Za-z][A-Za-z0-9_-]*)(?:=|\s+|["']\s*,\s*["'])(?=\S)/g)) {
    if (sensitive(match[1])) return true;
  }
  return false;
}
export const parseBundleJson = text => JSON.parse(text.replace(/^\uFEFF/, ''));
function put(target, segments, value) {
  for (const segment of segments.slice(0, -1)) {
    if (!Object.hasOwn(target, segment)) target[segment] = Object.create(null);
    target = target[segment];
  }
  target[segments.at(-1)] = value;
}

function settingPrefixes(definitions) {
  const prefixes = new Set();
  for (const def of definitions) {
    for (let index = 1; index < def.segments.length; index++) prefixes.add(def.segments.slice(0, index).join('.'));
  }
  return prefixes;
}

function validateModelRoles(output) {
  if (!Object.hasOwn(output, 'modelRoles')) return;
  if (!record(output.modelRoles)) throw new Error('modelRoles must be a mapping');
  for (const [role, value] of Object.entries(output.modelRoles)) {
    if (value !== null && (typeof value !== 'string' || !value.trim())) throw new Error(`modelRoles.${role} requires a model selection or null`);
  }
}

// Pure filtering: registered native setting metadata determines what is portable.
export function sanitizeConfig(input, definitions, allowEmpty = false) {
  if (!record(input) || (!Object.keys(input).length && !allowEmpty)) throw new Error('Config must be a non-empty YAML mapping');
  const omitted = new Set();
  const output = Object.create(null);
  const known = new Map(definitions.map(def => [def.id, def]));
  const prefixes = settingPrefixes(definitions);
  function clean(value, location, ancestors = new Set()) {
    if (value === null || typeof value !== 'object') {
      if (typeof value === 'string' && containsCredentials(value)) throw new Error('Credential-bearing setting content cannot be exported');
      return value;
    }
    if (ancestors.has(value)) throw new Error('Config must not contain cyclic YAML aliases');
    const next = new Set(ancestors);
    next.add(value);
    if (Array.isArray(value)) return value.map((item, index) => clean(item, `${location}[${index}]`, next));
    const result = Object.create(null);
    for (const [key, item] of Object.entries(value)) {
      if (containsCredentials(key)) { omitted.add('Credential-bearing setting key omitted'); continue; }
      const child = `${location}.${key}`;
      if (excludedSettings.has(child) || isCredentialKey(key) || ['__proto__', 'constructor', 'prototype'].includes(key)) omitted.add(child);
      else result[key] = clean(item, child, next);
    }
    return result;
  }
  function visit(value, segments) {
    if (segments.some(containsCredentials)) { omitted.add('Credential-bearing setting key omitted'); return; }
    const location = segments.join('.');
    const def = known.get(location);
    if (def) {
      if (def.isCredential || excludedRoots.has(segments[0].toLowerCase()) || excludedSettings.has(location)) { omitted.add(location); return; }
      put(output, def.segments, clean(value, location));
    } else if (record(value) && prefixes.has(location)) {
      for (const [key, item] of Object.entries(value)) visit(item, [...segments, key]);
    } else omitted.add(location);
  }
  for (const [key, value] of Object.entries(input)) visit(value, [key]);
  if (!Object.keys(output).length && !allowEmpty) throw new Error('No portable settings remain after safety filtering');
  validateModelRoles(output);
  return { settings: output, omitted: [...omitted].sort(ordinal) };
}

const inside = (root, target) => { const relative = path.relative(root, target); return relative !== '..' && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative); };
export const isAppSetupPath = (file, root) => inside(path.join(root, 'global-configs'), path.resolve(file));

export function configPolicyFiles(files, { root, defaultPath = null, selection }) {
  const input = files.map(file => path.resolve(file));
  const isSetup = file => isAppSetupPath(file, root);
  const slot = input.findLastIndex((file, index) => isSetup(file) && !(index === 0 && file === defaultPath));
  const result = input.filter((_file, index) => index !== slot);
  const prependDefault = defaultPath && input.includes(defaultPath) && input[0] !== defaultPath;
  if (prependDefault) result.unshift(defaultPath);
  if (selection.source === 'session') {
    if (slot >= 0) result.splice(slot + (prependDefault ? 1 : 0), 0, selection.path);
    else if (selection.path !== defaultPath) result.splice(Math.max(0, result.indexOf(defaultPath) + 1), 0, selection.path);
  } else if (selection.source === 'model') result.splice(Math.max(0, result.indexOf(defaultPath) + 1), 0, selection.path);
  else if (selection.source === 'default' && !result.includes(selection.path)) throw new Error('config_policy: frozen default is missing from session overlays');
  return result;
}

export function mergeSetupAsset(target, asset) {
  const relative = safeAssetPath(asset.path), key = relative.toLowerCase();
  const previous = target.get(key);
  if (previous && previous.path !== relative) throw new Error('Setup asset paths differ only by casing; rename conflicting files before combining setups');
  target.set(key, asset);
}

export function requiresAssetExecutionReview(assets) {
  return assets.some(asset => ((asset.mode ?? 0) & 0o111) !== 0 || asset.path === 'mcp.json' || asset.path.startsWith('skills/') || !/\.(?:md|mdc|txt|json|ya?ml)$/i.test(asset.path));
}

export function safeAssetPath(value) {
  if (typeof value !== 'string' || !value || /[\\:\x00-\x1f\x7f]/.test(value)) throw new Error('Invalid setup asset path');
  const parts = value.split('/');
  if (parts.some(part => !part || part === '.' || part === '..') || !(roots.has(value) || (['skills', 'rules'].includes(parts[0]) && parts.length > 1))) throw new Error('Setup asset path is outside approved roots');
  return value;
}

export function safeSetupName(value, fallback = 'Imported setup') {
  const name = typeof value === 'string' ? value.trim() : fallback;
  if (!name || name.length > 120 || /[\x00-\x1f\x7f]/.test(name) || containsCredentials(name)) throw new Error('Enter a setup name without credentials (1–120 characters)');
  return name;
}

function cleanJsonArray(value, changed) {
  const result = [];
  for (let index = 0; index < value.length; index++) {
    if (typeof value[index] === 'string' && /^--?/.test(value[index]) && isCredentialKey(value[index].replace(/^--?/, '').split('=')[0])) {
      if (!value[index].includes('=')) index++;
      changed.value = true;
    } else result.push(cleanJson(value[index], changed));
  }
  return result;
}

function cleanJson(value, changed) {
  if (Array.isArray(value)) return cleanJsonArray(value, changed);
  if (!value || typeof value !== 'object') return value;
  const result = Object.create(null);
  for (const [key, item] of Object.entries(value)) {
    if (isCredentialKey(key) || containsCredentials(key) || ['__proto__', 'constructor', 'prototype'].includes(key)) { changed.value = true; continue; }
    result[key] = cleanJson(item, changed);
  }
  return result;
}

const mcpDependencyError = () => new Error('MCP relative dependencies must exist inside preserved setup assets; move support files under skills/ or rules/ before saving');
const slashPath = value => value.replaceAll('\\', '/');
const anyAbsolute = value => path.isAbsolute(value) || path.win32.isAbsolute(value);

function mcpCwd(server, assetPaths) {
  const cwd = server.cwd === undefined ? '.' : server.cwd;
  if (typeof cwd !== 'string' || anyAbsolute(cwd) || /[\x00-\x1f]/.test(cwd)) throw mcpDependencyError();
  const normalized = path.posix.normalize(slashPath(cwd));
  if (normalized === '..' || normalized.startsWith('../') || (normalized !== '.' && !assetPaths.some(file => file.startsWith(`${normalized}/`)))) throw mcpDependencyError();
  return normalized;
}

export function resolveMcpServer(server, assetPaths, root) {
  if (!server || typeof server !== 'object' || Array.isArray(server)) throw new Error('Invalid MCP server');
  const files = new Set(assetPaths);
  const cwd = mcpCwd(server, assetPaths);
  function localFile(value) {
    const target = path.posix.normalize(path.posix.join(cwd, slashPath(value)));
    if (target === '..' || target.startsWith('../') || !files.has(target)) throw mcpDependencyError();
    return root ? path.join(root, target) : value;
  }
  let command = server.command;
  if (command !== undefined && (typeof command !== 'string' || !command.trim())) throw new Error('Invalid MCP command');
  if (!command && (typeof server.url !== 'string' || !server.url.trim())) throw new Error('MCP server requires a command or URL');
  if (command && !anyAbsolute(command) && /[\\/]/.test(command)) command = localFile(command);
  if (server.args !== undefined && (!Array.isArray(server.args) || server.args.some(arg => typeof arg !== 'string'))) throw new Error('Invalid MCP arguments');
  const args = server.args?.map(arg => /^(?:\.\.?[\\/]|skills[\\/]|rules[\\/])/.test(arg) ? localFile(arg) : arg);
  return root ? { ...server, command, ...(args ? { args } : {}), cwd: path.join(root, cwd) } : server;
}

function fileAsset(relative, asset, mode) {
  if (asset.content !== '' || typeof asset.file.sourcePath !== 'string' || !path.isAbsolute(asset.file.sourcePath) || !Number.isSafeInteger(asset.file.size) || asset.file.size < 0 || !/^[a-f0-9]{64}$/.test(asset.file.sha256)) throw new Error('Invalid setup file reference');
  if (relative === 'mcp.json') throw new Error('MCP commands must remain visible as reviewed text');
  return { path: relative, content: '', file: { ...asset.file }, ...mode };
}

function cleanJsonContent(relative, content, omitted) {
  let parsed;
  try { parsed = parseBundleJson(content); } catch { throw new Error('Setup JSON asset is invalid'); }
  const changed = { value: false };
  const cleaned = cleanJson(parsed, changed);
  if (!changed.value) return content;
  const contentJson = `${JSON.stringify(cleaned, null, 2)}\n`;
  omitted.push(`${relative}: credential fields`);
  return contentJson;
}

function inlineAsset(relative, asset, mode, omitted) {
  if (typeof asset.content !== 'string' || asset.content.includes('\0') || /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/u.test(asset.content)) throw new Error('Inline setup assets must be UTF-8 text');
  const content = relative.toLowerCase().endsWith('.json') ? cleanJsonContent(relative, asset.content, omitted) : asset.content;
  if (containsCredentials(content)) {
    omitted.push(`${relative}: credential-bearing content omitted`);
    return null;
  }
  return { path: relative, content, ...mode };
}

function sanitizeAsset(relative, asset, omitted) {
  if (containsCredentials(relative)) { omitted.push('Asset omitted: credential-bearing path'); return null; }
  if (asset.omitted || relative.split('/').some(credentialStore)) { omitted.push(`${relative}: credential-bearing source omitted`); return null; }
  if (asset.mode !== undefined && (!Number.isInteger(asset.mode) || asset.mode < 0 || asset.mode > 0xffffffff)) throw new Error('Invalid setup asset permissions');
  const mode = asset.mode === undefined ? {} : { mode: asset.mode & 0o777 };
  return asset.file ? fileAsset(relative, asset, mode) : inlineAsset(relative, asset, mode, omitted);
}

function validateMcpAsset(assets) {
  const mcp = assets.find(asset => asset.path === 'mcp.json');
  if (!mcp) return;
  const parsed = parseBundleJson(mcp.content);
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed) || !parsed.mcpServers || typeof parsed.mcpServers !== 'object' || Array.isArray(parsed.mcpServers)) throw new Error('MCP asset requires a mcpServers mapping');
  for (const server of Object.values(parsed.mcpServers)) resolveMcpServer(server, assets.map(asset => asset.path));
}

export function sanitizeAssets(input = []) {
  if (!Array.isArray(input)) throw new Error('Setup assets must be a list');
  const assets = [], omitted = [], seen = new Set();
  for (const asset of input) {
    const relative = safeAssetPath(asset?.path);
    if (seen.has(relative.toLowerCase())) throw new Error('Duplicate setup asset path');
    seen.add(relative.toLowerCase());
    const clean = sanitizeAsset(relative, asset, omitted);
    if (clean) assets.push(clean);
  }
  validateMcpAsset(assets);
  const requiresExecutionReview = requiresAssetExecutionReview(assets);
  return { assets, omitted, requiresExecutionReview };
}

export async function noSymlinks(file) {
  if (typeof file !== 'string') throw new Error('Invalid setup source');
  const absolute = path.resolve(file);
  let current = path.parse(absolute).root;
  for (const part of absolute.slice(current.length).split(path.sep).filter(Boolean)) {
    current = path.join(current, part);
    if ((await fs.lstat(current)).isSymbolicLink()) throw new Error('Setup sources must not contain symlinks');
  }
  return absolute;
}

export async function readText(file) {
  await noSymlinks(file);
  if (!(await fs.lstat(file)).isFile()) throw new Error('Setup source must be a regular file');
  try { return new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(await fs.readFile(file)); }
  catch { throw new Error('Setup source must be UTF-8 text'); }
}

const decodes = decode => {
  try {
    decode();
    return true;
  } catch {
    return false;
  }
};

async function scanFile(file) {
  const hash = createHash('sha256');
  const decoder = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });
  let text = true, secret = false, size = 0, carry = '', parts = [];
  for await (const chunk of createReadStream(file, { highWaterMark: inlineBytes })) {
    hash.update(chunk);
    size += chunk.length;
    if (parts && size <= inlineBytes) parts.push(chunk); else parts = null;
    text = text && decodes(() => decoder.decode(chunk, { stream: true }));
    if (chunk.includes(0)) text = false;
    const scan = carry + chunk.toString('latin1');
    if (containsCredentials(scan)) secret = true;
    carry = scan.slice(-8192);
  }
  text = text && decodes(() => decoder.decode());
  return { size, sha256: hash.digest('hex'), secret, text, parts };
}

async function inspectFile(file) {
  await noSymlinks(file);
  const stat = await fs.lstat(file);
  if (!stat.isFile() || !Number.isSafeInteger(stat.size)) throw new Error('Setup source must be a regular file with a representable size');
  const { size, sha256, secret, text, parts } = await scanFile(file);
  if (size !== stat.size) throw new Error('Setup source changed while reading');
  return { size, sha256, secret, text, mode: stat.mode & 0o777, content: text && parts ? new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(Buffer.concat(parts)) : undefined };
}

export async function assetFromFile(file, relative) {
  safeAssetPath(relative);
  if (file.split(/[\\/]/).some(credentialStore) || containsCredentials(file)) return { path: relative, content: '', omitted: true };
  if (relative === 'mcp.json') return { path: relative, content: await readText(file), mode: (await fs.stat(file)).mode & 0o777 };
  const inspected = await inspectFile(file);
  if ((roots.has(relative) || path.basename(relative) === 'SKILL.md' || relative.startsWith('rules/') && /\.(?:md|mdc)$/i.test(relative)) && !inspected.text) throw new Error('Setup instruction entry must be UTF-8 text');
  if (inspected.content !== undefined) return { path: relative, content: inspected.content, mode: inspected.mode };
  if (inspected.secret) return { path: relative, content: '', omitted: true };
  return { path: relative, content: '', mode: inspected.mode, file: { sourcePath: path.resolve(file), size: inspected.size, sha256: inspected.sha256 } };
}

async function assertCleanJsonFile(asset) {
  let parsed;
  try { parsed = parseBundleJson(await readText(asset.file.sourcePath)); } catch { throw new Error('Invalid setup JSON file'); }
  const changed = { value: false }; cleanJson(parsed, changed);
  if (changed.value) throw new Error('Credential-bearing JSON file must be sanitized before inclusion');
}

export async function validateAssetFiles(assets) {
  for (const asset of assets) {
    if (!asset.file) continue;
    if (asset.file.sourcePath.split(/[\\/]/).some(credentialStore) || containsCredentials(asset.file.sourcePath)) throw new Error('Credential-store files cannot be included in setups');
    const inspected = await inspectFile(asset.file.sourcePath);
    if (inspected.secret || inspected.size !== asset.file.size || inspected.sha256 !== asset.file.sha256 || process.platform !== 'win32' && asset.mode !== undefined && inspected.mode !== asset.mode) throw new Error('Setup source changed or contains credential-bearing content');
    asset.mode ??= inspected.mode;
    if (asset.path.toLowerCase().endsWith('.json')) await assertCleanJsonFile(asset);
  }
  return assets;
}

export async function directoryEntries(root) {
  const entries = [];
  for await (const entry of await fs.opendir(root)) entries.push(entry.name);
  return entries.sort(ordinal);
}

export async function collectAssets(root, entries = ['skills', 'rules', ...roots], prefix = '') {
  const assets = [];
  await noSymlinks(root);
  async function visit(relative) {
    const file = path.join(root, relative);
    let stat;
    try {
      stat = await fs.lstat(file);
    } catch (error) {
      if (error.code === 'ENOENT') return;
      throw error;
    }
    if (stat.isSymbolicLink()) throw new Error('Setup sources must not contain symlinks');
    if (stat.isDirectory()) {
      for (const entry of await directoryEntries(file)) await visit(`${relative}/${entry}`);
    } else assets.push(await assetFromFile(file, `${prefix}${relative}`));
  }
  for (const entry of entries) await visit(entry);
  return assets;
}

export function assetMetadata(assets) {
  return assets.map(asset => ({ path: asset.path, size: asset.file?.size ?? Buffer.byteLength(asset.content), sha256: asset.file?.sha256 ?? createHash('sha256').update(asset.content, 'utf8').digest('hex'), mode: (asset.mode ?? 0o644) & 0o777 }));
}

export function bundleDigest(yaml, assets) {
  const metadata = assets.every(asset => typeof asset.sha256 === 'string') ? assets : assetMetadata(assets);
  return createHash('sha256').update(JSON.stringify([yaml, metadata.map(asset => [asset.path, asset.size, asset.sha256, asset.mode])]), 'utf8').digest('hex');
}

export async function verifySavedBundle(configPath, metadata) {
  const yaml = await readText(configPath);
  const root = path.join(path.dirname(configPath), 'assets');
  if (!Array.isArray(metadata.assets) || !Array.isArray(metadata.assetMetadata) || metadata.assets.length !== metadata.assetMetadata.length) throw new Error('Invalid setup asset integrity metadata');
  const seen = new Set();
  const assets = [];
  for (let index = 0; index < metadata.assets.length; index++) {
    const relative = safeAssetPath(metadata.assets[index]);
    if (seen.has(relative.toLowerCase())) throw new Error('Duplicate setup asset path');
    seen.add(relative.toLowerCase());
    const expected = metadata.assetMetadata[index];
    if (expected?.path !== relative || !Number.isInteger(expected.mode) || expected.mode < 0 || expected.mode > 0o777) throw new Error('Invalid setup asset integrity metadata');
    const file = path.join(root, relative);
    const inspected = await inspectFile(file);
    if ((roots.has(relative) || path.basename(relative) === 'SKILL.md' || relative.startsWith('rules/') && /\.(?:md|mdc)$/i.test(relative)) && !inspected.text) throw new Error('Setup instruction entry must be UTF-8 text');
    if (inspected.secret || inspected.size !== expected.size || inspected.sha256 !== expected.sha256 || process.platform !== 'win32' && inspected.mode !== expected.mode) throw new Error('Setup asset changed since it was saved and reviewed');
    assets.push({ path: relative, content: '', mode: expected.mode, file: { sourcePath: file, size: inspected.size, sha256: inspected.sha256 } });
  }
  if (metadata.contentSha256 !== bundleDigest(yaml, metadata.assetMetadata)) throw new Error('Setup content integrity check failed');
  return { yaml, assets };
}

async function exists(file) {
  try {
    await fs.lstat(file);
    return true;
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
    return false;
  }
}

async function readFileBundle(file) {
  if (roots.has(path.basename(file))) return { yaml: '{}\n', name: `${path.basename(file)} setup`, assets: [await assetFromFile(file, path.basename(file))] };
  if (/^config\.ya?ml$/i.test(path.basename(file)) && await exists(path.join(path.dirname(file), 'setup.json'))) return readBundle(path.dirname(file));
  return { yaml: await readText(file), name: safeSetupName(path.basename(file, path.extname(file))), assets: [] };
}

async function readManifest(file) {
  let metadata;
  try { metadata = parseBundleJson(await readText(path.join(file, 'setup.json'))); } catch (error) { if (error.code !== 'ENOENT') throw new Error('Invalid setup manifest'); }
  if (metadata && (metadata.version !== 1 || !Array.isArray(metadata.assets))) throw new Error('Unsupported setup manifest');
  return metadata;
}

async function readConfigYaml(file, metadata) {
  const config = await exists(path.join(file, 'config.yml')) ? path.join(file, 'config.yml') : path.join(file, 'config.yaml');
  try {
    return await readText(config);
  } catch (error) {
    if (error.code !== 'ENOENT' || metadata) throw error;
    return '{}\n';
  }
}

async function collectBundleAssets(file, metadata) {
  if (metadata) return collectAssets(path.join(file, 'assets'));
  if (['skills', 'rules'].includes(path.basename(file))) return collectAssets(path.dirname(file), [path.basename(file)]);
  if (await exists(path.join(file, 'SKILL.md'))) return collectAssets(file, await directoryEntries(file), `skills/${path.basename(file)}/`);
  return collectAssets(file);
}

function applyWindowsModes(assets, assetMetadata) {
  for (const asset of assets) {
    const declaredMode = assetMetadata.find(item => item.path === asset.path)?.mode;
    if (!Number.isInteger(declaredMode) || declaredMode < 0 || declaredMode > 0o777) throw new Error('Invalid setup asset permissions');
    asset.mode = declaredMode;
  }
}

function manifestAssets(metadata, collected, yaml) {
  const declared = metadata.assets.map(safeAssetPath).sort(ordinal);
  if (new Set(declared.map(item => item.toLowerCase())).size !== declared.length || JSON.stringify(declared) !== JSON.stringify(collected.map(asset => asset.path).sort(ordinal))) throw new Error('Setup manifest assets do not match files');
  const byPath = new Map(collected.map(asset => [asset.path, asset]));
  const assets = metadata.assets.map(relative => byPath.get(relative));
  if (process.platform === 'win32' && Array.isArray(metadata.assetMetadata)) applyWindowsModes(assets, metadata.assetMetadata);
  if (metadata.contentSha256 !== undefined && (assets.some(asset => asset.omitted) || bundleDigest(yaml, assets) !== metadata.contentSha256)) throw new Error('Setup content does not match its saved manifest');
  return assets;
}

export async function readBundle(source) {
  const file = await noSymlinks(source);
  const stat = await fs.lstat(file);
  if (!stat.isDirectory()) return readFileBundle(file);
  const metadata = await readManifest(file);
  const yaml = await readConfigYaml(file, metadata);
  let assets = await collectBundleAssets(file, metadata);
  if (metadata) assets = manifestAssets(metadata, assets, yaml);
  return { yaml, assets, name: safeSetupName(metadata?.name, path.basename(file)) };
}

export function exportedSettings(settings, definitions) {
  const input = Object.create(null);
  for (const def of definitions) {
    if (settings.isConfigured(def)) put(input, def.segments, def.get(settings));
  }
  return input;
}
