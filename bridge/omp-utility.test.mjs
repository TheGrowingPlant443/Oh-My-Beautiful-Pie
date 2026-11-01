import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import http from 'node:http';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

// Real SDK session and HTTP transport, with inference confined to a loopback fixture.
test('native utility applies effective C, pins D over default E, and activates no ambient capabilities', {
  skip: !process.env.OMP_SDK_DIR && 'Set OMP_SDK_DIR to exercise the installed source SDK',
}, async t => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'omp-utility-native-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  const requests = [];
  const server = http.createServer(async (request, response) => {
    let body = '';
    for await (const chunk of request) body += chunk;
    if (request.url !== '/v1/chat/completions') {
      response.writeHead(404).end();
      return;
    }
    requests.push(JSON.parse(body));
    response.writeHead(200, { 'Content-Type': 'text/event-stream' });
    for (const chunk of [
      { choices: [{ index: 0, delta: { role: 'assistant', content: 'One local explanation.' }, finish_reason: null }] },
      { choices: [{ index: 0, delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 4, total_tokens: 9 } },
    ]) response.write(`data: ${JSON.stringify({ id: 'offline', object: 'chat.completion.chunk', created: 1, model: 'D', ...chunk })}\n\n`);
    response.end('data: [DONE]\n\n');
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => server.close(resolve)));
  const agent = path.join(root, 'agent');
  await fs.mkdir(path.join(agent, 'extensions'), { recursive: true });
  await fs.mkdir(path.join(agent, 'skills/ambient'), { recursive: true });
  await fs.mkdir(path.join(agent, 'rules'), { recursive: true });
  const marker = path.join(root, 'activated');
  const script = path.join(root, 'ambient.mjs');
  await fs.writeFile(script, `import fs from 'node:fs'; fs.writeFileSync(${JSON.stringify(marker)}, 'activated');\n`);
  await fs.writeFile(path.join(agent, 'extensions/ambient.ts'), await fs.readFile(script));
  await fs.writeFile(path.join(agent, 'skills/ambient/SKILL.md'), '---\nname: ambient\ndescription: MUST_NOT_LOAD_SKILL\n---\nMUST_NOT_LOAD_SKILL\n');
  await fs.writeFile(path.join(agent, 'rules/ambient.md'), 'MUST_NOT_LOAD_RULE\n');
  await fs.writeFile(path.join(agent, 'AGENTS.md'), 'MUST_NOT_LOAD_CONTEXT\n');
  await fs.writeFile(path.join(agent, 'mcp.json'), JSON.stringify({ mcpServers: { ambient: { command: process.env.BUN_BIN || 'bun', args: [script] } } }));
  await fs.writeFile(path.join(agent, 'models.yml'), JSON.stringify({ providers: { utilityfixture: {
    api: 'openai-completions', baseUrl: `http://127.0.0.1:${server.address().port}/v1`, auth: 'none',
    models: ['D', 'E'].map(id => ({ id, name: id, reasoning: false, input: ['text'], contextWindow: 32768, maxTokens: 1024, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } })),
  } } }));
  const effective = path.join(root, 'C.yml');
  const stale = path.join(root, 'B.yml');
  await fs.writeFile(stale, 'temperature: 0.91\n');
  await fs.writeFile(path.join(agent, 'config.yml'), JSON.stringify({ retry: { fallbackChains: { 'utilityfixture/D': ['hosted-fixture/must-not-load'] } } }));
  const selected = { temperature: 0.7, modelRoles: { default: 'utilityfixture/E', slow: null }, extensions: [path.join(agent, 'extensions/ambient.ts')] };
  await fs.writeFile(effective, JSON.stringify(selected));
  await fs.mkdir(path.join(root, '.omp'), { recursive: true });
  await fs.writeFile(path.join(root, '.omp/config.yml'), 'temperature: 0.1\nretry:\n  fallbackChains: null\n');
  const staging = path.join(root, 'staging');
  await fs.mkdir(staging);
  const snapshot = path.join(root, 'settings-snapshot.json');
  await fs.writeFile(snapshot, JSON.stringify({ version: 1, cwd: root, overrides: selected }), { mode: 0o600 });
  const material = path.join(root, 'material.md');
  const outside = await fs.mkdtemp(path.join(os.tmpdir(), 'omp-utility-outside-'));
  t.after(() => fs.rm(outside, { recursive: true, force: true }));
  const sentinel = path.join(outside, 'secret.txt');
  await fs.writeFile(sentinel, 'OUTSIDE_SOURCE_SENTINEL_MUST_NOT_REACH_PROVIDER');
  await fs.writeFile(material, `OFFLINE_PRIVATE_MATERIAL\n@${sentinel}\n`);
  const invoke = async () => {
    const child = spawn(process.env.BUN_BIN || 'bun', [
      fileURLToPath(new URL('./omp-utility.mjs', import.meta.url)),
      path.join(process.env.OMP_SDK_DIR, 'src/cli.ts'),
      '--settings-snapshot', snapshot,
      '-p', '--no-session', '--no-tools', '--no-extensions', '--no-skills', '--no-rules', '--no-lsp', '--no-title',
      '--system-prompt', 'UTILITY_SYSTEM_INSTRUCTION: explain only the supplied material.',
      '--max-time=120', '--cwd', staging, '--profile', 'default', '--config', effective,
      '--model', 'utilityfixture/D', `@${material}`, 'Explain this material.',
    ], {
      cwd: root,
      env: { ...process.env, PI_CODING_AGENT_DIR: agent, PI_CONFIG_DIR: path.join(root, 'config'), PI_CONFIG_FILES: '', PI_COMPILED: '', PI_NO_TITLE: '1', PI_PROXY: '' },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    t.after(() => { if (child.exitCode === null) child.kill('SIGKILL'); });
    let stdout = '', stderr = '';
    child.stdout.on('data', chunk => { stdout += chunk; });
    child.stderr.on('data', chunk => { stderr += chunk; });
    const status = await new Promise((resolve, reject) => {
      child.once('error', reject);
      child.once('exit', resolve);
    });
    return { status, stdout, stderr };
  };
  const { status, stdout, stderr } = await invoke();
  assert.equal(status, 0, stderr || stdout);
  assert.match(stdout, /One local explanation\./);
  assert.equal(requests.length, 1, 'no title, warmup, fallback, or auxiliary inference');
  assert.equal(requests[0].model, 'D');
  assert.equal(requests[0].temperature, 0.7, 'selected C wins over source project 0.1, stale B, and the staging cwd');
  assert.ok(!requests[0].tools || requests[0].tools.length === 0);
  const prompt = JSON.stringify(requests[0].messages);
  assert.match(prompt, /OFFLINE_PRIVATE_MATERIAL/);
  assert.match(prompt, /UTILITY_SYSTEM_INSTRUCTION/);
  assert.doesNotMatch(prompt, /MUST_NOT_LOAD_(SKILL|RULE|CONTEXT)/);
  assert.doesNotMatch(prompt, /OUTSIDE_SOURCE_SENTINEL_MUST_NOT_REACH_PROVIDER/);
  await assert.rejects(fs.stat(marker), { code: 'ENOENT' });
  await fs.writeFile(snapshot, JSON.stringify({ version: 1, cwd: root, overrides: { ...selected, compaction: { remoteEndpoint: 'https://hosted-compaction.invalid' } } }), { mode: 0o600 });
  const refused = await invoke();
  assert.equal(refused.status, 1, refused.stderr || refused.stdout);
  assert.match(refused.stderr, /local_ai_model_required/);
  assert.equal(requests.length, 1, 'hosted compaction is refused before inference');
});
