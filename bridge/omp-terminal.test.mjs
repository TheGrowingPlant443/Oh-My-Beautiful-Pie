import test from 'node:test';
import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {mkdtemp,rm} from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {capTail,previewOf,OUTPUT_LIMIT,installTerminalCommands,TERMINAL_COMPATIBILITY_ERROR} from './omp-terminal.mjs';
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
});

const nativeSkip = !process.env.OMP_SDK_DIR ? 'Set OMP_SDK_DIR to run isolated native integration' : false;
test('native Bash/Eval execution is streamed, kept alive through background jobs and services, and killed with an agent interrupt', { skip: nativeSkip, timeout: 240000 }, async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'omp-terminal-commands-'));
  try {
    const {stdout} = await promisify(execFile)(process.env.OMP_BIN || 'omp', [
      '--mode','rpc','--no-ui','--no-session','--no-title','--no-extensions','--no-skills','--no-rules','--no-lsp','--auto-approve','--cwd',root,
      '--extension',fileURLToPath(new URL('./fixtures/terminal-commands-scenario.mjs', import.meta.url)),
    ], {
      env:{...process.env,BROWSER_TEST_ROOT:root,USERPROFILE:root,HOME:root,PI_CODING_AGENT_DIR:path.join(root,'agent'),PI_NATIVES_DIR:process.env.PI_NATIVES_DIR||path.join(os.homedir(),'.omp','natives')},
      timeout: 230000, maxBuffer: 4 * 1024 * 1024,
    });
    assert.match(stdout, /Terminal command observer verified/);
  } finally { await rm(root,{recursive:true,force:true,maxRetries:10,retryDelay:100}); }
});
