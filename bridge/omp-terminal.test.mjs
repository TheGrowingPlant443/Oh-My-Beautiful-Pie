import test from 'node:test';
import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {mkdir,mkdtemp,rm,writeFile} from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {capTail,previewOf,OUTPUT_LIMIT,installTerminalCommands,TERMINAL_COMPATIBILITY_ERROR,formatHistory,formatEntry} from './omp-terminal.mjs';
import {controlAllowed} from './omp-manager.mjs';

test('output tails keep the newest bytes and never split a multi-byte character', () => {
  const big = 'é'.repeat(OUTPUT_LIMIT) ;
  const capped = capTail(big);
  assert.equal(capped.truncated, true);
  assert.ok(Buffer.byteLength(capped.text) <= OUTPUT_LIMIT);
  assert.ok(!capped.text.includes('\ufffd'));
  assert.equal(capTail('short').truncated, false);
});

test('previews are one flattened line capped at 256 characters', () => {
  assert.equal(previewOf('a\n  b\t\tc'), 'a b c');
  assert.equal(Array.from(previewOf('😀'.repeat(400))).length, 256);
});

test('only the human can kill a terminal command', () => {
  assert.equal(controlAllowed({role:'instance',owner:'manager',action:'terminal_kill',source:'manager'}), false);
  assert.equal(controlAllowed({role:'manager',owner:null,action:'terminal_kill',source:undefined}), false);
  assert.equal(controlAllowed({role:'instance',owner:null,action:'terminal_kill',source:'human'}), true);
});

test('an incompatible runtime reports the integration error instead of a fake stream', async () => {
  const observer = installTerminalCommands({pi:{}}, {emit(){}, historyRoot:'x', sdkDir:'y'});
  assert.equal(observer.snapshot().supported, false);
  assert.equal(observer.snapshot().error, TERMINAL_COMPATIBILITY_ERROR);
  await assert.rejects(observer.kill('id'), {message: TERMINAL_COMPATIBILITY_ERROR});
  assert.throws(() => observer.history(), {message: TERMINAL_COMPATIBILITY_ERROR});
  assert.throws(() => observer.entry('x'), {message: TERMINAL_COMPATIBILITY_ERROR});
});

const row = (executionId, patch = {}) => ({executionId, kind:'bash', description:null, preview:'echo hi', cwd:'C:/p', startedAt:Date.UTC(2026,9,11,12), endedAt:Date.UTC(2026,9,11,12,0,1), status:'completed', exitCode:0, hasOutput:false, ...patch});

test('the history list names every command with its status, exit code and whether output was saved', () => {
  assert.equal(formatHistory({rows:[], total:0}), 'No commands recorded in this conversation yet.');
  const text = formatHistory({total:3, rows:[
    row('a', {status:'failed', exitCode:7, hasOutput:true, description:'Build'}),
    row('b'),
  ]});
  assert.deepEqual(text.split('\n'), [
    '3 command(s) in this conversation (newest first, showing 2)',
    '- a | 2026-10-11T12:00:00.000Z | failed (exit 7) | bash | Build | echo hi | output saved',
    '- b | 2026-10-11T12:00:00.000Z | completed | bash |  | echo hi',
  ]);
});

test('a shown command has its script from the start and its output from the end, cut at 65536 characters', () => {
  const failed = row('f', {status:'failed', exitCode:1, hasOutput:true, description:'Why'});
  const both = formatEntry({summary:failed, script:'exit 1', output:'boom'});
  assert.match(both, /^Command f\nStatus: failed \(exit 1\)\nKind: bash\nStarted: 2026-10-11T12:00:00\.000Z\nEnded: 2026-10-11T12:00:01\.000Z\nDirectory: C:\/p\nDescription: Why\n--- script ---\nexit 1\n--- output ---\nboom$/);
  assert.ok(!formatEntry({summary:failed, script:'exit 1', output:'boom'}, 'script').includes('--- output ---'));
  assert.ok(!formatEntry({summary:failed, script:'exit 1', output:'boom'}, 'output').includes('--- script ---'));
  const long = formatEntry({summary:failed, script:`HEAD${'x'.repeat(70000)}`, output:`${'y'.repeat(70000)}TAIL`});
  assert.ok(long.includes('HEAD') && long.includes('[script truncated; showing the first 65536 characters]'));
  assert.ok(long.includes('[output truncated; showing the last 65536 characters]\n') && long.endsWith('TAIL'));
  assert.ok(formatEntry({summary:row('c'), script:'echo', output:null}).includes('no output recorded'));
  assert.ok(formatEntry({summary:row('r', {status:'running', endedAt:null, exitCode:null}), script:'sleep', output:'so far'}).includes('--- output (live, still running) ---\nso far'));
});

const nativeSkip = !process.env.OMP_SDK_DIR ? 'Set OMP_SDK_DIR to run isolated native integration' : false;
test('native Bash/Eval execution is streamed, kept alive through background jobs and services, and killed with an agent interrupt', { skip: nativeSkip, timeout: 240000 }, async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'omp-terminal-commands-'));
  try {
    const agent = path.join(root, 'agent');
    await mkdir(agent);
    await writeFile(path.join(agent, 'models.yml'), `providers:
  terminal-offline:
    api: openai-completions
    baseUrl: http://127.0.0.1:9/v1
    apiKey: terminal-fixture-key
    models:
      - id: fixture
`);
    await writeFile(path.join(agent, 'config.yml'), 'modelRoles:\n  default: terminal-offline/fixture\n');
    const {stdout} = await promisify(execFile)(process.env.OMP_BIN || 'omp', [
      '--mode','rpc','--no-ui','--no-session','--no-title','--no-extensions','--no-skills','--no-rules','--no-lsp','--auto-approve','--cwd',root,
      '--extension',fileURLToPath(new URL('./fixtures/terminal-commands-scenario.mjs', import.meta.url)),
    ], {
      env:{
        ...process.env,
        BROWSER_TEST_ROOT:root, USERPROFILE:root, HOME:root,
        PI_CODING_AGENT_DIR:agent, PI_CONFIG_DIR:'.omp', PI_CONFIG_FILES:'', OMP_PROFILE:'default',
        XDG_CONFIG_HOME:path.join(root,'config'), XDG_DATA_HOME:path.join(root,'data'),
        XDG_STATE_HOME:path.join(root,'state'), XDG_CACHE_HOME:path.join(root,'cache'),
        PI_NATIVES_DIR:process.env.PI_NATIVES_DIR||path.join(os.homedir(),'.omp','natives'),
      },
      timeout: 230000, maxBuffer: 4 * 1024 * 1024,
    });
    assert.match(stdout, /Terminal command observer verified/);
  } finally { await rm(root,{recursive:true,force:true,maxRetries:10,retryDelay:100}); }
});
