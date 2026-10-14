import test from 'node:test';
import assert from 'node:assert/strict';
import {installTerminalBrowserBridge,COMPATIBILITY_ERROR} from './omp-browser.mjs';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {mkdtemp,rm} from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

test('an incompatible session cannot execute native eval with an unwrapped shared browser',()=>{
  class NativeEval { execute(){throw new Error('unsafe native eval executed');} }
  installTerminalBrowserBridge({pi:{EvalTool:NativeEval},on(){}},()=>{throw new Error('browser request must not run');});
  assert.throws(()=>new NativeEval().execute('call',{language:'js',code:'browser.open()'}),{message:COMPATIBILITY_ERROR});
});

test('an immutable live browser callback fails closed before native code executes',()=>{
  const definition={name:'browser'};
  Object.defineProperty(definition,'invoke',{value:()=>{throw new Error('shared browser executed');},writable:false});
  class NativeEval {
    constructor(){this.session={getEvalPreludes:()=>[definition]};}
    execute(){throw new Error('unsafe native eval executed');}
  }
  installTerminalBrowserBridge({pi:{EvalTool:NativeEval},on(){}},()=>{throw new Error('browser request must not run');});
  assert.throws(()=>new NativeEval().execute('call',{language:'js',code:'browser.open()'}),{message:COMPATIBILITY_ERROR});
});

const nativeSkip = !process.env.OMP_SDK_DIR || !process.env.PUPPETEER_EXECUTABLE_PATH
  ? 'Set OMP_SDK_DIR and PUPPETEER_EXECUTABLE_PATH to run isolated native integration' : false;
async function runNativeScenario(name, prefix) {
  const root=await mkdtemp(path.join(os.tmpdir(),prefix));
  try {
    await promisify(execFile)(process.env.OMP_BIN || 'omp',[
      '--mode','rpc','--no-ui','--no-session','--no-title','--no-extensions','--no-skills','--no-rules','--no-lsp','--auto-approve','--cwd',root,
      '--extension',fileURLToPath(new URL(`./fixtures/${name}`,import.meta.url)),
    ],{
      env:{...process.env,BROWSER_TEST_ROOT:root,USERPROFILE:root,HOME:root,PI_CODING_AGENT_DIR:path.join(root,'agent'),PI_NATIVES_DIR:process.env.PI_NATIVES_DIR||path.join(os.homedir(),'.omp','natives')},timeout:170000,maxBuffer:1024*1024,
    });
  } finally {
    await rm(root,{recursive:true,force:true,maxRetries:10,retryDelay:100});
  }
}

test('native browser reads survive controller reply loss but reject closed CDP targets', { skip: nativeSkip, timeout:180000 },
  () => runNativeScenario('browser-control-plane-scenario.mjs','omp-browser-control-plane-'));

test('a browser the user closed reopens for the same session without restoring old tabs, also after an app restart', { skip: nativeSkip, timeout:180000 },
  () => runNativeScenario('browser-relaunch-scenario.mjs','omp-browser-relaunch-'));

test('tabs the user opens by hand are attached to the agent session automatically and can be controlled', { skip: nativeSkip, timeout:180000 },
  () => runNativeScenario('browser-tabs-scenario.mjs','omp-browser-tabs-'));

test('tabs opened by hand in a browser the human started with the Browser button are visible to the agent', { skip: nativeSkip, timeout:180000 },
  () => runNativeScenario('browser-prelaunched-scenario.mjs','omp-browser-prelaunched-'));
