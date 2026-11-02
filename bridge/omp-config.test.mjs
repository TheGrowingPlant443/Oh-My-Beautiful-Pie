import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { assetMetadata, bundleDigest, collectAssets, configPolicyFiles, mergeSetupAsset, readBundle, resolveMcpServer, safeAssetPath, sanitizeAssets, sanitizeConfig, validateAssetFiles, verifySavedBundle } from './config-bundle.mjs';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const definitions = [
  { id: 'modelRoles', segments: ['modelRoles'] },
  { id: 'modelPresets', segments: ['modelPresets'] },
  { id: 'theme', segments: ['theme'] },
  { id: 'tools.approvalMode', segments: ['tools', 'approvalMode'] },
  { id: 'auth.broker.token', segments: ['auth', 'broker', 'token'], isCredential: true },
  { id: 'extensions', segments: ['extensions'] },
  { id: 'telemetry.enabled', segments: ['telemetry', 'enabled'] },
];

test('portable configs keep model roles and safe settings without exporting credentials or executable hooks', () => {
  const secret = 'DO_NOT_EXPORT_THIS_SECRET';
  const result = sanitizeConfig({
    modelRoles: { default: 'openai/model', plan: 'openai/model', slow: null },
    theme: 'dark',
    tools: { approvalMode: 'always-ask' },
    modelPresets: { example: { modelRoles: { default: 'openai/model' }, headers: { Authorization: secret }, env: { PRIVATE_KEY: secret } } },
    auth: { broker: { token: secret } },
    extensions: ['./unreviewed-code.ts'],
    telemetry: { enabled: true },
    unknown: { secret },
  }, definitions);
  assert.deepEqual({ ...result.settings.modelRoles }, { default: 'openai/model', plan: 'openai/model', slow: null });
  assert.equal(result.settings.tools.approvalMode, 'always-ask');
  assert.equal(result.settings.theme, 'dark');
  assert.equal(JSON.stringify(result).includes(secret), false);
  assert.equal(Object.hasOwn(result.settings, 'extensions'), false);
  assert.equal(Object.hasOwn(result.settings, 'auth'), false);
  assert.deepEqual(result.omitted, ['auth.broker.token', 'extensions', 'modelPresets.example.env', 'modelPresets.example.headers', 'telemetry.enabled', 'unknown']);
});

test('empty input, invalid role selections, and recursive aliases do not become silent defaults', () => {
  for (const input of [null, [], 'not config', {}]) assert.throws(() => sanitizeConfig(input, definitions), /non-empty YAML mapping/);
  assert.throws(() => sanitizeConfig({ modelRoles: { default: 1 } }, definitions), /requires a model selection/);
  assert.throws(() => sanitizeConfig({ modelRoles: { default: '' } }, definitions), /requires a model selection/);
  assert.throws(() => sanitizeConfig({ extensions: ['unsafe.ts'] }, definitions), /No portable settings/);
  const cyclic = {}; cyclic.self = cyclic;
  assert.throws(() => sanitizeConfig({ modelPresets: cyclic }, definitions), /cyclic YAML aliases/);
});

test('untrusted prototype keys stay data exclusions rather than mutating objects', () => {
  const input = JSON.parse('{"modelRoles":{"default":"openai/model","__proto__":{"polluted":true}},"theme":"dark"}');
  const result = sanitizeConfig(input, definitions);
  assert.equal({}.polluted, undefined);
  assert.equal(Object.hasOwn(result.settings.modelRoles, '__proto__'), false);
  assert.deepEqual(result.omitted, ['modelRoles.__proto__']);
});

test('native custom skill paths cannot bypass reviewed copied assets', () => {
  const result = sanitizeConfig({ theme: 'dark', skills: { enabled: true, customDirectories: ['/outside/unreviewed'] } }, [...definitions, { id: 'skills', segments: ['skills'] }]);
  assert.equal(result.settings.skills.enabled, true);
  assert.equal(Object.hasOwn(result.settings.skills, 'customDirectories'), false);
  assert.ok(result.omitted.includes('skills.customDirectories'));
  assert.equal(JSON.stringify(result).includes('/outside/unreviewed'), false);
});

test('safe UTF-8 skill/rule assets are lossless and executable content is review-gated', () => {
  const assets = [
    { path: 'AGENTS.md', content: '\uFEFF# Æble 🍎\r\nUse local models.\r\n' },
    { path: 'rules/work.md', content: '# Work\nKeep changes small.\n' },
    { path: 'skills/build/SKILL.md', content: '# Build\nRead scripts/build.sh first.\n' },
    { path: 'skills/build/scripts/build.sh', content: '#!/bin/sh\nprintf \"hello\"\n' },
  ];
  const clean = sanitizeAssets(assets);
  assert.deepEqual(clean.assets, assets);
  assert.deepEqual(clean.omitted, []);
  assert.equal(clean.requiresExecutionReview, true);
  assert.equal(sanitizeAssets(assets.slice(0, 2)).requiresExecutionReview, false);
});

test('safe BOM-prefixed JSON remains byte-for-byte intact during review', () => {
  const content = '\uFEFF{"mcpServers":{"demo":{"command":"node"}}}\r\n';
  const result = sanitizeAssets([{ path: 'mcp.json', content }]);
  assert.equal(result.assets[0].content, content);
  assert.equal(result.requiresExecutionReview, true);
});

test('MCP commands remain visible while structured credentials and credential arguments never leave preview', () => {
  const secret = 'DO_NOT_PUBLISH_VALUE';
  const clean = sanitizeAssets([{ path: 'mcp.json', content: JSON.stringify({
    mcpServers: {
      example: { command: 'node', args: ['server.mjs', '--token', secret, '--port', '8000'], env: { API_KEY: secret }, headers: { Authorization: secret } },
      remote: { url: 'https://example.test/mcp', auth: { clientSecret: secret } },
    },
  }) }]);
  assert.equal(JSON.stringify(clean).includes(secret), false);
  const mcp = JSON.parse(clean.assets[0].content);
  assert.equal(mcp.mcpServers.example.command, 'node');
  assert.deepEqual(mcp.mcpServers.example.args, ['server.mjs', '--port', '8000']);
  assert.equal(Object.hasOwn(mcp.mcpServers.example, 'env'), false);
  assert.equal(clean.requiresExecutionReview, true);
  assert.ok(clean.omitted.includes('mcp.json: credential fields'));
});

test('opaque credentials are omitted without their values and unknown credential setting fields are stripped', () => {
  const secret = 'DO_NOT_PUBLISH_VALUE';
  for (const content of [`API_TOKEN=${secret}`, `auth: ${secret}`, `clientSecret = '${secret}'`, `command --client-secret ${secret}`, `const args = ['--client-secret', '${secret}']`, `curl -H 'Authorization: Bearer ${secret}'`, `https://user:${secret}@example.test`, `https://example.test?token=${secret}`, `-----BEGIN PRIVATE KEY-----\n${secret}`]) {
    const result = sanitizeAssets([{ path: 'skills/demo/run.sh', content }]);
    assert.equal(result.assets.length, 0);
    assert.equal(JSON.stringify(result).includes(secret), false);
    assert.ok(result.omitted.length);
  }
  const result = sanitizeConfig({ theme: 'dark', modelPresets: { demo: { clientSecret: secret, private_key: secret, safe: true } } }, definitions);
  assert.equal(JSON.stringify(result).includes(secret), false);
});

test('credential-shaped setting keys and asset paths never leak through omitted labels', () => {
  const secret = 'sk-THIS_IS_A_SECRET_CREDENTIAL';
  const result = sanitizeConfig({ theme: 'dark', [secret]: 'unknown', modelPresets: { demo: { [secret]: 'nested' } } }, definitions);
  assert.equal(JSON.stringify(result).includes(secret), false);
  assert.ok(result.omitted.includes('Credential-bearing setting key omitted'));
  const assets = sanitizeAssets([{ path: `skills/${secret}/config.json`, content: '{"token":"private"}' }]);
  assert.equal(JSON.stringify(assets).includes(secret), false);
  assert.equal(assets.assets.length, 0);
});

test('asset paths, duplicate aliases and non-text inline values reject hostile bundles without file/count caps', () => {
  for (const file of ['../AGENTS.md', '/AGENTS.md', 'skills/../../auth.json', 'skills/a\\b', 'skills//a', 'skills/./a', 'auth.json', 'skills/a:stream', 'skills/a\0b']) assert.throws(() => safeAssetPath(file), /path|roots/);
  assert.throws(() => sanitizeAssets([{ path: 'rules/a.md', content: 'a' }, { path: 'rules/A.md', content: 'b' }]), /Duplicate/);
  assert.throws(() => sanitizeAssets([{ path: 'rules/a.md', content: '\uD800' }]), /UTF-8/);
  assert.equal(sanitizeAssets([{ path: 'rules/a.md', content: 'x'.repeat(2 * 1024 * 1024) }]).assets[0].content.length, 2 * 1024 * 1024);
  assert.equal(sanitizeAssets(Array.from({ length: 600 }, (_, index) => ({ path: `rules/${index}.md`, content: 'safe' }))).assets.length, 600);
});

test('shared setup ordering uses frozen default provenance, including a late original default slot', () => {
  const root = path.resolve('manager'), snapshot = name => path.join(root, 'global-configs', name, 'config.yml');
  const a = snapshot('a'), b = snapshot('b'), c = snapshot('c'), m = snapshot('m');
  const x = path.resolve('native-x.yml'), y = path.resolve('native-y.yml');
  const files = [a, x, b, y];
  assert.deepEqual(configPolicyFiles(files, { root, defaultPath: a, selection: { path: c, source: 'session' } }), [a, x, c, y]);
  assert.deepEqual(configPolicyFiles(files, { root, defaultPath: a, selection: { path: m, source: 'model' } }), [a, m, x, y]);
  assert.deepEqual(configPolicyFiles(files, { root, defaultPath: a, selection: { path: a, source: 'default' } }), [a, x, y]);
  assert.deepEqual(configPolicyFiles([x, a, y], { root, defaultPath: a, selection: { path: c, source: 'session' } }), [a, x, c, y]);
  assert.deepEqual(configPolicyFiles([x, a, y], { root, defaultPath: a, selection: { path: a, source: 'default' } }), [a, x, y]);
  assert.deepEqual(configPolicyFiles([x, a, y], { root, defaultPath: a, selection: { path: a, source: 'session' } }), [a, x, a, y]);
});

test('setup asset unions replace exact paths but refuse case-only ambiguity', () => {
  const merged = new Map();
  mergeSetupAsset(merged, { path: 'rules/shared.md', content: 'A' });
  mergeSetupAsset(merged, { path: 'rules/shared.md', content: 'B' });
  assert.equal(merged.get('rules/shared.md').content, 'B');
  assert.throws(() => mergeSetupAsset(merged, { path: 'rules/Shared.md', content: 'C' }), /casing/);
});

test('known credential query parameters never enter previews or omission labels', () => {
  const secret = 'DO_NOT_EXPORT_QUERY';
  for (const key of ['auth', 'authorization', 'access_token', 'client_secret', 'signature', 'sig', 'api_key', 'password', 'secret']) {
    const content = `https://example.test/mcp?${key}=${secret}`;
    const result = sanitizeAssets([{ path: 'skills/demo/endpoint.txt', content }]);
    assert.equal(result.assets.length, 0);
    assert.equal(JSON.stringify(result).includes(secret), false);
    assert.throws(() => sanitizeConfig({ theme: content }, definitions), error => !error.message.includes(secret));
  }
});

test('small executable text keeps safe mode bits and mode-only changes invalidate review', { skip: process.platform === 'win32' ? 'POSIX executable bits are not physical Windows permissions' : false }, async t => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'omp-executable-assets-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  const native = path.join(root, 'native'), saved = path.join(root, 'saved');
  await fs.mkdir(path.join(native, 'skills/demo'), { recursive: true });
  await fs.writeFile(path.join(native, 'skills/demo/run.sh'), '#!/bin/sh\nexit 0\n');
  await fs.chmod(path.join(native, 'skills/demo/run.sh'), 0o4755);
  const preview = await readBundle(native);
  const script = preview.assets[0];
  assert.equal(script.content, '#!/bin/sh\nexit 0\n');
  assert.equal(script.file, undefined);
  assert.equal(script.mode, 0o755);
  assert.equal(sanitizeAssets([{ path: 'rules/helper.md', content: '#!/bin/sh\nexit 0\n', mode: 0o755 }]).requiresExecutionReview, true);
  await fs.mkdir(path.join(saved, 'assets/skills/demo'), { recursive: true });
  await fs.writeFile(path.join(saved, 'config.yml'), preview.yaml);
  const target = path.join(saved, 'assets', script.path);
  await fs.writeFile(target, script.content);
  await fs.chmod(target, script.mode);
  const metadata = { version: 1, name: 'Executable script', assets: [script.path], assetMetadata: assetMetadata(preview.assets), contentSha256: bundleDigest(preview.yaml, preview.assets) };
  await verifySavedBundle(path.join(saved, 'config.yml'), metadata);
  assert.equal((await fs.stat(target)).mode & 0o7777, 0o755);
  await fs.chmod(target, 0o644);
  await assert.rejects(verifySavedBundle(path.join(saved, 'config.yml'), metadata), /changed/);
});

test('relative MCP dependencies cannot escape or refer to omitted support files', () => {
  for (const command of ['./server.sh', '../outside.sh']) {
    assert.throws(() => sanitizeAssets([{ path: 'mcp.json', content: JSON.stringify({ mcpServers: { demo: { command } } }) }]), /relative dependencies/);
  }
  const assets = [{ path: 'mcp.json', content: JSON.stringify({ mcpServers: { demo: { command: 'node', cwd: 'skills/demo', args: ['./server.mjs'] } } }) }, { path: 'skills/demo/server.mjs', content: 'console.log(\"ready\");\n' }];
  assert.equal(sanitizeAssets(assets).assets.length, 2);
  const resolved = resolveMcpServer(JSON.parse(assets[0].content).mcpServers.demo, assets.map(asset => asset.path), '/portable/assets');
  assert.equal(resolved.command, 'node');
  assert.equal(resolved.cwd, path.join('/portable/assets', 'skills/demo'));
  assert.equal(resolved.args[0], path.join('/portable/assets', 'skills/demo/server.mjs'));
});

test('binary and large-text resources roundtrip by checked local references with no JSON byte payload', async t => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'omp-binary-assets-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  const native = path.join(root, 'native');
  await fs.mkdir(path.join(native, 'skills/demo'), { recursive: true });
  await fs.writeFile(path.join(native, 'skills/demo/SKILL.md'), '# Demo\n');
  const resourceBytes = 9 * 1024 * 1024;
  await fs.writeFile(path.join(native, 'skills/demo/model.bin'), Buffer.alloc(resourceBytes, 0x80));
  await fs.writeFile(path.join(native, 'skills/demo/template.txt'), 'x'.repeat(resourceBytes));
  for (let index = 0; index < 600; index++) await fs.writeFile(path.join(native, `skills/demo/template-${index}.txt`), `Template ${index}\n`);
  const preview = await readBundle(native);
  const resources = preview.assets.filter(asset => asset.file);
  assert.equal(resources.length, 2);
  assert.ok(resources.every(asset => asset.content === '' && asset.file.size === resourceBytes));
  assert.ok(JSON.stringify(resources).length < 2048);
  await validateAssetFiles(sanitizeAssets(preview.assets).assets);
  const saved = path.join(root, 'portable');
  await fs.mkdir(path.join(saved, 'assets/skills/demo'), { recursive: true });
  await fs.writeFile(path.join(saved, 'config.yml'), preview.yaml);
  for (const asset of preview.assets) {
    if (asset.file) await fs.copyFile(asset.file.sourcePath, path.join(saved, 'assets', asset.path));
    else await fs.writeFile(path.join(saved, 'assets', asset.path), asset.content);
    await fs.chmod(path.join(saved, 'assets', asset.path), asset.mode ?? 0o644);
  }
  const metadata = { version: 1, name: 'Binary resources', requiresExecutionReview: true, executableReview: false, assets: preview.assets.map(asset => asset.path), assetMetadata: assetMetadata(preview.assets), contentSha256: bundleDigest(preview.yaml, preview.assets) };
  await fs.writeFile(path.join(saved, 'setup.json'), JSON.stringify(metadata));
  const restored = await readBundle(saved);
  assert.deepEqual(assetMetadata(restored.assets), metadata.assetMetadata);
  assert.equal((await verifySavedBundle(path.join(saved, 'config.yml'), metadata)).assets.length, 603);
  await fs.writeFile(resources[0].file.sourcePath, 'changed since preview');
  await assert.rejects(validateAssetFiles(resources), /changed/);
  await fs.writeFile(path.join(saved, 'assets/skills/demo/model.bin'), 'changed after review');
  await assert.rejects(verifySavedBundle(path.join(saved, 'config.yml'), metadata), /changed/);
});

test('known credential stores and recognizable secret-bearing binary files are not published as file references', async t => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'omp-secret-assets-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  await fs.mkdir(path.join(root, 'skills/demo'), { recursive: true });
  await fs.writeFile(path.join(root, 'skills/demo/.netrc'), 'machine example.test login user password DO_NOT_EXPORT');
  await fs.writeFile(path.join(root, 'skills/demo/.npmrc'), '//example.test/:_authToken=DO_NOT_EXPORT');
  await fs.writeFile(path.join(root, 'skills/demo/data.bin'), Buffer.concat([Buffer.from([0xff, 0x00]), Buffer.from('API_TOKEN=DO_NOT_EXPORT')]));
  const cleaned = sanitizeAssets((await readBundle(root)).assets);
  assert.equal(cleaned.assets.length, 0);
  assert.equal(JSON.stringify(cleaned).includes('DO_NOT_EXPORT'), false);
  assert.ok(cleaned.omitted.length);
});

test('native folders and portable manifests roundtrip exact assets and meaningful names', async t => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'omp-bundle-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  const native = path.join(root, 'Native local setup');
  await fs.mkdir(path.join(native, 'skills', 'demo'), { recursive: true });
  const yaml = 'modelRoles:\n  default: ollama/model\n';
  const content = '\uFEFF# Æble 🍎\r\nRead carefully.\r\n';
  await fs.writeFile(path.join(native, 'config.yml'), yaml);
  await fs.writeFile(path.join(native, 'skills/demo/SKILL.md'), content);
  const imported = await readBundle(native);
  assert.equal(imported.name, 'Native local setup');
  assert.equal(imported.yaml, yaml);
  assert.deepEqual(imported.assets, [{ path: 'skills/demo/SKILL.md', content, mode: (await fs.stat(path.join(native, 'skills/demo/SKILL.md'))).mode & 0o777 }]);
  const portable = path.join(root, 'portable');
  await fs.mkdir(path.join(portable, 'assets/skills/demo'), { recursive: true });
  await fs.writeFile(path.join(portable, 'config.yml'), imported.yaml);
  await fs.writeFile(path.join(portable, 'assets/skills/demo/SKILL.md'), content);
  await fs.writeFile(path.join(portable, 'setup.json'), JSON.stringify({ version: 1, name: 'Daily local coding', executableReview: true, assets: ['skills/demo/SKILL.md'], assetMetadata: assetMetadata(imported.assets), contentSha256: bundleDigest(yaml, imported.assets) }));
  const restored = await readBundle(portable);
  assert.equal(restored.name, 'Daily local coding');
  assert.equal(restored.yaml, yaml);
  assert.deepEqual(restored.assets, imported.assets);
  assert.equal(sanitizeAssets(restored.assets).requiresExecutionReview, true);
  assert.deepEqual(await readBundle(path.join(portable, 'config.yml')), restored);
  await fs.writeFile(path.join(portable, 'assets/skills/demo/SKILL.md'), 'changed after review');
  await assert.rejects(readBundle(portable), /does not match/);
  await fs.writeFile(path.join(portable, 'assets/skills/demo/SKILL.md'), content);
  await fs.writeFile(path.join(portable, 'setup.json'), JSON.stringify({ version: 1, name: 'Bad', assets: ['../credentials'] }));
  await assert.rejects(readBundle(portable), /path|roots/);
  await fs.writeFile(path.join(portable, 'setup.json'), JSON.stringify({ version: 1, name: 'Bad', assets: [] }));
  await assert.rejects(readBundle(portable), /do not match/);
});

test('symlink roots/files and invalid UTF-8 cannot enter a bundle', async t => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'omp-bundle-hostile-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  await fs.mkdir(path.join(root, 'native'));
  await fs.writeFile(path.join(root, 'secret'), 'private');
  await fs.symlink(path.join(root, 'secret'), path.join(root, 'native/AGENTS.md'));
  await assert.rejects(collectAssets(path.join(root, 'native')), /symlinks/);
  await fs.symlink(path.join(root, 'native'), path.join(root, 'alias'));
  await assert.rejects(readBundle(path.join(root, 'alias')), /symlinks/);
  await fs.unlink(path.join(root, 'native/AGENTS.md'));
  await fs.writeFile(path.join(root, 'native/config.yml'), 'theme: dark\n');
  await fs.writeFile(path.join(root, 'native/AGENTS.md'), Buffer.from([0xff, 0xfe]));
  await assert.rejects(readBundle(path.join(root, 'native')), /UTF-8/);
});

test('asset-only native roots, standalone assets, skills/rules trees and individual skill folders wrap safely', async t => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'omp-assets-only-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  await fs.mkdir(path.join(root, 'skills/demo'), { recursive: true });
  await fs.mkdir(path.join(root, 'rules'));
  await fs.writeFile(path.join(root, 'AGENTS.md'), '# Local instructions\n');
  await fs.writeFile(path.join(root, 'RULES.md'), '# Local rules\n');
  await fs.writeFile(path.join(root, 'mcp.json'), '{"mcpServers":{"demo":{"command":"node"}}}');
  await fs.writeFile(path.join(root, 'skills/demo/SKILL.md'), '# Demo skill\n');
  await fs.writeFile(path.join(root, 'rules/style.md'), '# Style rule\n');
  const native = await readBundle(root);
  assert.equal(native.yaml.trim(), '{}');
  assert.equal(native.assets.length, 5);
  for (const file of ['AGENTS.md', 'RULES.md', 'mcp.json']) {
    const imported = await readBundle(path.join(root, file));
    assert.equal(imported.yaml.trim(), '{}');
    assert.equal(imported.assets[0].path, file);
  }
  for (const folder of ['skills', 'rules']) {
    const imported = await readBundle(path.join(root, folder));
    assert.ok(imported.assets.every(asset => asset.path.startsWith(`${folder}/`)));
    assert.equal(imported.assets.length, 1);
  }
  const skill = await readBundle(path.join(root, 'skills/demo'));
  assert.equal(skill.assets[0].path, 'skills/demo/SKILL.md');
  assert.deepEqual(Object.keys(sanitizeConfig({}, definitions, true).settings), []);
});

test('installed native settings parser roundtrips reviewed folder assets without trusting portable consent', {
  skip: process.env.OMP_SDK_DIR ? false : 'Set OMP_SDK_DIR to exercise the installed native settings parser',
}, async t => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'omp-config-native-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  const native = path.join(root, 'native');
  await fs.mkdir(path.join(native, 'skills/demo'), { recursive: true });
  await fs.mkdir(path.join(root, 'agent'));
  await fs.writeFile(path.join(native, 'config.yml'), 'modelRoles:\n  default: ollama/local\n');
  await fs.writeFile(path.join(native, 'skills/demo/SKILL.md'), '# Demo\nÆble 🍎\r\n');
  await fs.writeFile(path.join(native, 'mcp.json'), JSON.stringify({ mcpServers: { demo: { command: 'node', args: ['server.mjs'], env: { API_TOKEN: 'DO_NOT_EXPORT' } } } }));
  const bridge = fileURLToPath(new URL('./omp-config.mjs', import.meta.url));
  const invoke = (request, expectFailure = false) => {
    const result = spawnSync(process.env.BUN_BIN || 'bun', [bridge], {
      input: JSON.stringify({ ...request, sdkDir: process.env.OMP_SDK_DIR, cwd: root, profile: 'default' }),
      maxBuffer: Infinity,
      env: { ...process.env, PI_CODING_AGENT_DIR: path.join(root, 'agent'), PI_CONFIG_DIR: path.join(root, 'config'), PI_CONFIG_FILES: '', PI_COMPILED: '' }, encoding: 'utf8',
    });
    assert.equal(result.status, expectFailure ? 1 : 0, result.stderr || result.error?.message || result.stdout);
    const response = JSON.parse(result.stdout);
    assert.equal(response.ok, !expectFailure, response.error);
    return expectFailure ? response : response.data;
  };
  const preview = invoke({ action: 'import', path: native, name: 'Daily native setup' });
  assert.equal(preview.name, 'Daily native setup');
  assert.equal(preview.modelRoles.default, 'ollama/local');
  await fs.unlink(path.join(native, 'config.yml'));
  const assetsOnly = invoke({ action: 'import', path: native });
  assert.deepEqual(assetsOnly.modelRoles, {});
  assert.equal(assetsOnly.assets.length, 2);
  const standalone = invoke({ action: 'import', path: path.join(native, 'mcp.json') });
  assert.equal(standalone.assets[0].path, 'mcp.json');
  assert.equal(standalone.requiresExecutionReview, true);
  assert.equal(preview.requiresExecutionReview, true);
  assert.equal(JSON.stringify(preview).includes('DO_NOT_EXPORT'), false);
  assert.equal(JSON.parse(preview.assets.find(asset => asset.path === 'mcp.json').content).mcpServers.demo.command, 'node');
  const saved = path.join(root, 'portable');
  await fs.mkdir(path.join(saved, 'assets/skills/demo'), { recursive: true });
  await fs.writeFile(path.join(saved, 'config.yml'), preview.yaml);
  for (const asset of preview.assets) await fs.writeFile(path.join(saved, 'assets', asset.path), asset.content);
  await fs.writeFile(path.join(saved, 'setup.json'), JSON.stringify({ version: 1, name: preview.name, executableReview: true, assets: preview.assets.map(asset => asset.path), contentSha256: bundleDigest(preview.yaml, preview.assets) }));
  const restored = invoke({ action: 'import', path: saved });
  assert.equal(restored.name, preview.name);
  assert.equal(restored.yaml, preview.yaml);
  assert.deepEqual(restored.assets, preview.assets);
  assert.equal(restored.requiresExecutionReview, true);
  const catalogue = invoke({ action: 'models' });
  assert.equal(catalogue.error, null);
  assert.equal(catalogue.profile, 'default');
  assert.ok(catalogue.models.length > 0);
  assert.ok(catalogue.models.every(model => typeof model.selector === 'string' && model.selector.includes('/') && typeof model.name === 'string' && typeof model.available === 'boolean'));
  await fs.mkdir(path.join(root, 'agent/skills/source/scripts'), { recursive: true });
  await fs.mkdir(path.join(root, 'agent/rules'), { recursive: true });
  const outside = path.join(root, 'outside');
  await fs.mkdir(outside);
  await fs.writeFile(path.join(outside, 'SKILL.md'), '---\nname: outside-helper\ndescription: Native custom directory source\n---\n# Outside helper\n');
  await fs.mkdir(path.join(outside, 'templates'));
  await fs.writeFile(path.join(outside, 'templates/example.txt'), 'Standalone template bytes.\n');
  await fs.writeFile(path.join(root, 'agent/config.yml'), JSON.stringify({ modelRoles: { default: 'ollama/local' }, skills: { customDirectories: [outside] } }));
  await fs.writeFile(path.join(root, 'agent/AGENTS.md'), '# Global instructions\nKeep native configuration read-only.\n');
  await fs.writeFile(path.join(root, 'AGENTS.md'), '# Project instructions\nAlways stay local.\n');
  await fs.writeFile(path.join(root, 'agent/rules/source.md'), '# Source rule\nKeep supporting files.\n');
  await fs.writeFile(path.join(root, 'agent/skills/source/SKILL.md'), '---\nname: source-helper\ndescription: Preserve source supporting files\n---\n# Source\nRead scripts/run.sh before use.\n');
  const marker = path.join(root, 'must-not-execute');
  await fs.writeFile(path.join(root, 'agent/skills/source/scripts/run.sh'), `#!/bin/sh\n: > '${marker}'\n`, { mode: 0o755 });
  await fs.writeFile(path.join(root, 'agent/skills/source/model.bin'), Buffer.from([0xff, 0x00, 0x80]));
  await fs.writeFile(path.join(root, 'agent/mcp.json'), JSON.stringify({ mcpServers: { demo: { command: './skills/source/scripts/run.sh' } } }));
  const exported = invoke({ action: 'export' });
  assert.equal(exported.modelRoles.default, 'ollama/local');
  assert.equal(exported.requiresExecutionReview, true);
  assert.ok(exported.assets.some(asset => asset.path === 'skills/outside-helper/SKILL.md'));
  assert.equal(exported.assets.find(asset => asset.path === 'skills/outside-helper/templates/example.txt').content, 'Standalone template bytes.\n');
  assert.ok(exported.omitted.includes('skills.customDirectories'));
  for (const asset of ['skills/source/SKILL.md', 'skills/source/scripts/run.sh', 'skills/source/model.bin', 'rules/source.md', 'AGENTS.md', 'mcp.json']) assert.ok(exported.assets.some(item => item.path === asset), asset);
  assert.equal(exported.assets.find(asset => asset.path === 'skills/source/model.bin').file.size, 3);
  const contexts = exported.assets.filter(asset => asset.path === 'AGENTS.md' || asset.path.startsWith('rules/.omp-context/'));
  assert.ok(contexts.some(asset => asset.content.includes('# Global instructions')));
  assert.ok(contexts.some(asset => asset.content.includes('# Project instructions')));
  await assert.rejects(fs.stat(marker), { code: 'ENOENT' });
  const ordered = [];
  for (const [id, yaml, name, assets] of [
    ['a', 'modelRoles:\n  default: ollama/a\n  slow: ollama/a-only\n', 'Global A', [{ path: 'rules/a.md', content: '# A only\n' }, { path: 'rules/shared.md', content: '# Shared A\n' }]],
    ['b', 'modelRoles:\n  default: ollama/b\n', 'Explicit B', [{ path: 'rules/b.md', content: '# B only\n' }, { path: 'rules/shared.md', content: '# Shared B\n' }]],
  ]) {
    const folder = path.join(root, 'manager/global-configs', id);
    await fs.mkdir(path.join(folder, 'assets/rules'), { recursive: true });
    await fs.writeFile(path.join(folder, 'config.yml'), yaml);
    for (const asset of assets) {
      await fs.writeFile(path.join(folder, 'assets', asset.path), asset.content);
      await fs.chmod(path.join(folder, 'assets', asset.path), asset.mode ?? 0o644);
    }
    await fs.writeFile(path.join(folder, 'setup.json'), JSON.stringify({ version: 1, name, requiresExecutionReview: false, executableReview: false, assets: assets.map(asset => asset.path), assetMetadata: assetMetadata(assets), contentSha256: bundleDigest(yaml, assets) }));
    ordered.push(path.join(folder, 'config.yml'));
  }
  const x = path.join(root, 'native-x.yml'), y = path.join(root, 'native-y.yml');
  await fs.writeFile(x, 'modelRoles:\n  default: ollama/x\n  plan: ollama/x-only\n');
  await fs.writeFile(y, 'modelRoles:\n  default: ollama/y\n');
  const composed = invoke({ action: 'export', configFiles: [ordered[0], x, ordered[1], y], setupPaths: ordered, setupPath: ordered[1] });
  assert.equal(composed.name, 'Explicit B');
  assert.equal(composed.modelRoles.default, 'ollama/y');
  assert.equal(composed.modelRoles.slow, 'ollama/a-only');
  assert.equal(composed.modelRoles.plan, 'ollama/x-only');
  assert.ok(composed.assets.some(asset => asset.path === 'rules/a.md'));
  assert.ok(composed.assets.some(asset => asset.path === 'rules/b.md'));
  assert.equal(composed.assets.find(asset => asset.path === 'rules/shared.md').content, '# Shared B\n');
  assert.equal(composed.assets.some(asset => asset.path === 'skills/outside-helper/SKILL.md'), false);
  const manager = path.join(root, 'manager');
  const assignedFolder = path.join(manager, 'global-configs/m');
  const assignedAssets = [{ path: 'rules/m.md', content: '# Assigned M\n', mode: 0o644 }];
  const assignedYaml = 'modelRoles:\n  default: must-not-select-this-model\n';
  await fs.mkdir(path.join(assignedFolder, 'assets/rules'), { recursive: true });
  await fs.writeFile(path.join(assignedFolder, 'config.yml'), assignedYaml);
  await fs.writeFile(path.join(assignedFolder, 'assets/rules/m.md'), assignedAssets[0].content);
  await fs.chmod(path.join(assignedFolder, 'assets/rules/m.md'), 0o644);
  await fs.writeFile(path.join(assignedFolder, 'setup.json'), JSON.stringify({ version: 1, name: 'Assigned M', executableReview: false, requiresExecutionReview: false, assets: assignedAssets.map(asset => asset.path), assetMetadata: assetMetadata(assignedAssets), contentSha256: bundleDigest(assignedYaml, assignedAssets) }));
  const selector = catalogue.models[0].selector;
  await fs.writeFile(path.join(manager, 'model-configs.json'), JSON.stringify([{ model: selector, configPath: path.join(assignedFolder, 'config.yml') }]));
  const stopped = { configRoot: manager, configDefault: ordered[0], configSelection: null, configFiles: [ordered[0], x, ordered[1], y], model: selector, reconstructSelection: true };
  const admissionDirectory = path.join(manager, '.config-session-00000000-0000-4000-8000-000000000004');
  await fs.mkdir(admissionDirectory, { mode: 0o700 });
  const admission = invoke({ ...stopped, action: 'prepare_session', directory: admissionDirectory, helper: false });
  assert.equal(admission.model, selector);
  const automatic = invoke({ ...stopped, action: 'export' });
  assert.equal(automatic.name, 'Assigned M');
  assert.equal(automatic.modelRoles.default, 'ollama/y');
  assert.ok(automatic.assets.some(asset => asset.path === 'rules/a.md'));
  assert.ok(automatic.assets.some(asset => asset.path === 'rules/m.md'));
  assert.equal(automatic.assets.some(asset => asset.path === 'rules/b.md'), false);
  const explicit = invoke({ ...stopped, action: 'export', configSelection: ordered[1] });
  assert.equal(explicit.name, 'Explicit B');
  assert.equal(explicit.assets.some(asset => asset.path === 'rules/m.md'), false);
  await fs.writeFile(path.join(manager, 'model-configs.json'), '[]');
  const cleared = invoke({ ...stopped, action: 'export' });
  assert.equal(cleared.name, 'Global A');
  assert.equal(cleared.assets.some(asset => asset.path === 'rules/b.md'), false);
  for (const [suffix, model, thinking] of [
    ['00000000-0000-4000-8000-000000000001', selector, null],
    ['00000000-0000-4000-8000-000000000002', `${selector}:high`, 'high'],
  ]) {
    const directory = path.join(manager, `.config-session-${suffix}`);
    await fs.mkdir(directory, { mode: 0o700 });
    const prepared = invoke({ ...stopped, action: 'prepare_session', configSelection: ordered[1], directory, model, helper: false });
    assert.equal(prepared.model, selector);
    if (thinking === null) assert.equal(Object.hasOwn(prepared, 'thinkingLevel'), false);
    else assert.equal(prepared.thinkingLevel, thinking);
    const constructor = invoke({ action: 'export', configFiles: prepared.configFiles });
    assert.equal(constructor.modelRoles.slow, 'ollama/a-only');
    assert.equal(constructor.modelRoles.plan, 'ollama/x-only');
    assert.equal(constructor.modelRoles.default, 'ollama/y');
    if (process.platform !== 'win32') assert.equal((await fs.stat(directory)).mode & 0o777, 0o700);
  }
  const currentFolder = path.join(manager, 'global-configs/c');
  const currentYaml = 'modelRoles:\n  default: ollama/unwanted-E\n  plan: ollama/c-only\n  slow: ollama/c-only-slow\n';
  const currentAssets = [
    { path: 'rules/c.md', content: '# Current C\n', mode: 0o644 },
    { path: 'mcp.json', content: JSON.stringify({ mcpServers: { forbiddenUtilityMcp: { command: path.join(root, 'agent/skills/source/scripts/run.sh') } } }), mode: 0o644 },
  ];
  await fs.mkdir(path.join(currentFolder, 'assets/rules'), { recursive: true });
  await fs.writeFile(path.join(currentFolder, 'config.yml'), currentYaml);
  for (const asset of currentAssets) {
    const file = path.join(currentFolder, 'assets', asset.path);
    await fs.writeFile(file, asset.content);
    await fs.chmod(file, asset.mode);
  }
  await fs.writeFile(path.join(currentFolder, 'setup.json'), JSON.stringify({ version: 1, name: 'Current C', requiresExecutionReview: true, executableReview: false, assets: currentAssets.map(asset => asset.path), assetMetadata: assetMetadata(currentAssets), contentSha256: bundleDigest(currentYaml, currentAssets) }));
  const utilityDirectory = path.join(manager, '.config-session-00000000-0000-4000-8000-000000000005');
  await fs.mkdir(utilityDirectory, { mode: 0o700 });
  const utility = invoke({ ...stopped, action: 'prepare_session', directory: utilityDirectory, configFiles: [ordered[0], x, path.join(currentFolder, 'config.yml')], utility: true, authoritativeConfigFiles: true, configSelection: ordered[1], model: selector });
  assert.equal(utility.model, selector, 'Declared C.default E must not replace exact utility model D');
  assert.equal(utility.slotManifest.entries.some(entry => entry.source === ordered[1]), false, 'Active source C must not be replaced by stale original B');
  const utilitySnapshot = utility.slotManifest.settingsSnapshot;
  assert.ok(path.isAbsolute(utilitySnapshot));
  assert.ok(!path.relative(utilityDirectory, utilitySnapshot).startsWith('..'));
  const utilitySettings = JSON.parse(await fs.readFile(utilitySnapshot, 'utf8'));
  assert.equal(utilitySettings.version, 1);
  assert.equal(utilitySettings.cwd, root);
  assert.equal(utilitySettings.overrides.modelRoles.plan, 'ollama/c-only');
  assert.equal(JSON.stringify(utility).includes('ollama/c-only'), false, 'Private settings must not be embedded in native protocol output');
  if (process.platform !== 'win32') assert.equal((await fs.stat(utilitySnapshot)).mode & 0o777, 0o600);
  await assert.rejects(fs.stat(path.join(utilityDirectory, 'assets')), { code: 'ENOENT' });
  await assert.rejects(fs.stat(marker), { code: 'ENOENT' });
  const archivalModel = 'retired-ombp-provider/archived-model';
  assert.equal(catalogue.models.some(model => model.selector === archivalModel), false);
  const archive = path.join(root, 'unrelated-archive.jsonl');
  const timestamp = '2026-01-01T00:00:00.000Z';
  await fs.writeFile(archive, [
    { type: 'session', version: 3, id: '00000000-0000-4000-8000-000000000006', timestamp, cwd: root },
    { type: 'model_change', id: 'archive-model', parentId: null, timestamp, model: archivalModel, role: 'default' },
    { type: 'message', id: 'archive-user', parentId: 'archive-model', timestamp, message: { role: 'user', content: 'Archived context', timestamp: Date.parse(timestamp) } },
  ].map(entry => JSON.stringify(entry)).join('\n') + '\n');
  await fs.writeFile(path.join(manager, 'model-configs.json'), JSON.stringify([{ model: archivalModel, configPath: path.join(currentFolder, 'config.yml') }]));
  const archiveDirectory = path.join(manager, '.config-session-00000000-0000-4000-8000-000000000007');
  await fs.mkdir(archiveDirectory, { mode: 0o700 });
  const archivedUtility = invoke({ ...stopped, action: 'prepare_session', model: null, sessionPath: archive, directory: archiveDirectory, utility: true, renderOnly: true });
  assert.equal(archivedUtility.model, archivalModel);
  assert.equal(archivedUtility.local, false);
  const archivedSettings = JSON.parse(await fs.readFile(archivedUtility.slotManifest.settingsSnapshot, 'utf8'));
  assert.equal(archivedSettings.overrides.modelRoles.slow, 'ollama/c-only-slow');
  const rejectedDirectory = path.join(manager, '.config-session-00000000-0000-4000-8000-000000000008');
  await fs.mkdir(rejectedDirectory, { mode: 0o700 });
  const rejectedGeneration = invoke({ ...stopped, action: 'prepare_session', model: null, sessionPath: archive, directory: rejectedDirectory, utility: true }, true);
  assert.equal(rejectedGeneration.ok, false, 'Generation must not replace an unregistered archived D with configured default E');
  await fs.writeFile(path.join(manager, 'model-configs.json'), '[]');
  const disabledProviders = [...new Set(catalogue.models.map(model => model.selector.split('/')[0]))];
  await fs.writeFile(path.join(root, 'agent/config.yml'), JSON.stringify({ disabledProviders, modelRoles: { default: 'unavailable-native-default' } }));
  const noModelDirectory = path.join(manager, '.config-session-00000000-0000-4000-8000-000000000003');
  await fs.mkdir(noModelDirectory, { mode: 0o700 });
  const unresolved = invoke({ action: 'prepare_session', configRoot: manager, configDefault: null, configSelection: null, configFiles: [], model: null, directory: noModelDirectory, helper: false });
  assert.equal(unresolved.model, null);
  assert.equal(unresolved.local, false);
  assert.equal(Object.hasOwn(unresolved, 'thinkingLevel'), false);
  await assert.rejects(fs.stat(marker), { code: 'ENOENT' });
});
