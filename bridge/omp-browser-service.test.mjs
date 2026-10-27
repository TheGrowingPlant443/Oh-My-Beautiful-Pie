import test from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import {BrowserError,BrowserService,NativeChannel,profileDirectory,resolveInstalledChromium,windowsChromiumCandidates} from './omp-browser-service.mjs';
import {createServer} from 'node:http';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {fileURLToPath} from 'node:url';
import {mkdtemp,rm} from 'node:fs/promises';
import os from 'node:os';

test('restored terminal IDs cannot collide or traverse the persistent profile root',()=>{
  const root=path.resolve('browser-test-root');
  const ids=['a','A','../a','CON','a/../b','b'];
  const folders=ids.map(id=>profileDirectory(root,id));
  assert.equal(new Set(folders).size,ids.length);
  for(const folder of folders){
    const relative=path.relative(root,folder);
    assert.equal(path.isAbsolute(relative),false);
    assert.equal(relative.startsWith('..'),false);
  }
});

test('revoked and replaced generations cannot acquire another browser',async()=>{
  const service=new BrowserService(path.resolve('unused-fixture'),'unavailable-sdk');
  await service.dispatch('register',{instanceId:'a',generation:1});
  await service.dispatch('register',{instanceId:'a',generation:2});
  await assert.rejects(service.dispatch('ensureTab',{instanceId:'a',generation:1,name:'main'}),/stale_generation/);
  await assert.rejects(service.dispatch('ensureTab',{instanceId:'b',generation:2,name:'main'}),/stale_generation/);
  await service.dispatch('revoke',{instanceId:'a',generation:2});
  await assert.rejects(service.dispatch('ensureTab',{instanceId:'a',generation:2,name:'main'}),/stale_generation/);
  assert.deepEqual(await service.dispatch('snapshot'),[]);
  await service.dispatch('shutdown');
});

test('a per-session visible request validates its owner and yields to later global settings',async()=>{
  const frames=[];
  const service=new BrowserService(path.resolve('unused-fixture'),'unavailable-sdk',frame=>frames.push(frame));
  await service.dispatch('configure',{headless:false,embedded:false});
  await service.dispatch('register',{instanceId:'a',generation:1});
  await service.dispatch('register',{instanceId:'b',generation:1});
  await assert.rejects(service.dispatch('openVisible',{instanceId:'a',generation:1,restartHeadless:1}),/invalid browser visibility request/);
  await assert.rejects(service.dispatch('openVisible',{instanceId:'a',generation:2,restartHeadless:false}),/stale_generation/);
  assert.equal(service.owner('a',1).forceVisible,false);
  service.owner('a',1).forceVisible=true;
  assert.deepEqual((await service.dispatch('snapshot')).map(row=>[row.instanceId,row.embedded]),[['a',process.platform==='win32'],['b',false]]);
  frames.length=0;
  await service.dispatch('configure',{headless:false,embedded:false});
  assert.equal(service.owner('a',1).forceVisible,false);
  assert.deepEqual(frames.map(frame=>[frame.instanceId,frame.embedded]),[['a',false],['b',false]]);
  await service.dispatch('shutdown');
});

test('listing tabs never launches a browser and rejects stale or unknown owners',async()=>{
  const service=new BrowserService(path.resolve('unused-fixture'),'unavailable-sdk');
  await service.dispatch('register',{instanceId:'a',generation:1});
  assert.deepEqual(await service.dispatch('listTabs',{instanceId:'a',generation:1}),[]);
  assert.deepEqual((await service.dispatch('snapshot')).map(row=>[row.state,row.pid]),[['closed',null]]);
  await assert.rejects(service.dispatch('listTabs',{instanceId:'a',generation:2}),/stale_generation/);
  await assert.rejects(service.dispatch('listTabs',{instanceId:'nobody',generation:1}),/stale_generation/);
  await service.dispatch('shutdown');
});

test('real Chromium opens one terminal browser visibly without disturbing another', {
  skip: !process.env.OMP_SDK_DIR || !process.env.PUPPETEER_EXECUTABLE_PATH
    ? 'Set OMP_SDK_DIR and PUPPETEER_EXECUTABLE_PATH to run isolated native integration' : false,
  timeout:120000,
}, async()=>{
  const root=await mkdtemp(path.join(os.tmpdir(),'omp-browser-visible-'));
  const server=createServer((request,response)=>{response.writeHead(200,{'content-type':'text/html'});response.end('<!doctype html><title>visible</title>');});
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
  try {
    await promisify(execFile)(process.env.BUN_BIN || 'bun',[fileURLToPath(new URL('./fixtures/browser-open-visible-scenario.mjs',import.meta.url))],{
      env:{...process.env,BROWSER_TEST_ROOT:root,BROWSER_FIXTURE_URL:`http://127.0.0.1:${server.address().port}/`},
      timeout:110000,maxBuffer:1024*1024,
    });
  } finally {
    server.closeAllConnections();
    await new Promise(resolve=>server.close(resolve));
    await rm(root,{recursive:true,force:true,maxRetries:10,retryDelay:100});
  }
});

test('real Chromium isolates same-origin terminal profiles and identically named page titles', {
  skip: !process.env.OMP_SDK_DIR || !process.env.PUPPETEER_EXECUTABLE_PATH
    ? 'Set OMP_SDK_DIR and PUPPETEER_EXECUTABLE_PATH to run isolated native integration' : false,
  timeout:120000,
}, async()=>{
  const root=await mkdtemp(path.join(os.tmpdir(),'omp-browser-regression-'));
  const server=createServer((request,response)=>{
    if(request.url.startsWith('/download')){
      response.writeHead(200,{'content-type':'text/plain','content-disposition':'attachment; filename="fixture.txt"'});
      response.end('owned-download');return;
    }
    response.writeHead(200,{'content-type':'text/html'});
    response.end('<!doctype html><title>same-title</title><input id="shared"><iframe src="about:blank"></iframe><a id="download" href="/download">Download fixture</a>');
  });
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
  try {
    await promisify(execFile)(process.env.BUN_BIN || 'bun',[fileURLToPath(new URL('./fixtures/browser-service-scenario.mjs',import.meta.url))],{
      env:{...process.env,BROWSER_TEST_ROOT:root,BROWSER_FIXTURE_URL:`http://127.0.0.1:${server.address().port}/`},
      timeout:110000,maxBuffer:1024*1024,
    });
  } finally {
    server.closeAllConnections();
    await new Promise(resolve=>server.close(resolve));
    await rm(root,{recursive:true,force:true,maxRetries:10,retryDelay:100});
  }
});

test('browser executables are resolved passively from installed locations only',async()=>{
  const env={ProgramFiles:'C:\\PF',['ProgramFiles(x86)']:'C:\\PF86',LOCALAPPDATA:'C:\\Local'};
  assert.deepEqual(windowsChromiumCandidates(env),[
    'C:\\PF\\Google\\Chrome\\Application\\chrome.exe','C:\\PF86\\Google\\Chrome\\Application\\chrome.exe','C:\\Local\\Google\\Chrome\\Application\\chrome.exe',
    'C:\\PF\\Chromium\\Application\\chrome.exe','C:\\Local\\Chromium\\Application\\chrome.exe','C:\\PF\\Microsoft\\Edge\\Application\\msedge.exe','C:\\PF86\\Microsoft\\Edge\\Application\\msedge.exe']);
  const probed=[];
  const existing=new Set(['C:\\Local\\Google\\Chrome\\Application\\chrome.exe','C:\\PF\\Microsoft\\Edge\\Application\\msedge.exe']);
  const exists=async file=>{probed.push(file);return existing.has(file);};
  const never=async()=>{throw new Error('the cache must not be consulted when an installed browser exists');};
  assert.equal(await resolveInstalledChromium({env,platform:'win32',exists,cached:never}),'C:\\Local\\Google\\Chrome\\Application\\chrome.exe','first installed candidate in SDK order wins');
  assert.deepEqual(probed,windowsChromiumCandidates(env).slice(0,3));
  assert.equal(await resolveInstalledChromium({env:{...env,PUPPETEER_EXECUTABLE_PATH:'D:/tools/chrome.exe'},platform:'win32',exists:async()=>true,cached:never}),'D:/tools/chrome.exe','an explicit absolute path always wins');
  for(const explicit of ['chrome.exe','.\\chrome.exe'])await assert.rejects(resolveInstalledChromium({env:{...env,PUPPETEER_EXECUTABLE_PATH:explicit},platform:'win32',exists:async()=>true}),/browser_executable_missing/);
  await assert.rejects(resolveInstalledChromium({env:{...env,PUPPETEER_EXECUTABLE_PATH:'D:\\gone\\chrome.exe'},platform:'win32',exists:async()=>false,cached:never}),/browser_executable_missing/,'an explicit path is never silently replaced');
  assert.equal(await resolveInstalledChromium({env,platform:'win32',exists:async file=>file==='D:\\cache\\chrome.exe',cached:async()=>'D:\\cache\\chrome.exe'}),'D:\\cache\\chrome.exe','an already existing Puppeteer cache is the last resort');
  await assert.rejects(resolveInstalledChromium({env,platform:'win32',exists:async()=>false,cached:async()=>'D:\\cache\\chrome.exe'}),error=>error instanceof BrowserError&&error.code==='browser_executable_missing');
});

test('Unix resolution honors explicit browsers, absolute PATH order, Nix profiles and macOS app bundles',async()=>{
  const env={HOME:'/home/test',PATH:'relative/bin:/opt/preferred/bin:/usr/bin'};
  const available=new Set(['/opt/preferred/bin/chromium','/usr/bin/google-chrome']);
  const exists=async file=>available.has(file);
  const never=async()=>{throw new Error('installed browser must win over cache');};
  assert.equal(await resolveInstalledChromium({env,platform:'linux',exists,cached:never}),'/opt/preferred/bin/chromium');
  assert.equal(await resolveInstalledChromium({env:{HOME:'/home/test',PATH:''},platform:'linux',exists:async file=>file==='/home/test/.nix-profile/bin/chromium',cached:never}),'/home/test/.nix-profile/bin/chromium');
  const app='/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
  assert.equal(await resolveInstalledChromium({env:{PATH:''},platform:'darwin',exists:async file=>file===app,cached:never}),app);
  assert.equal(await resolveInstalledChromium({env:{...env,PUPPETEER_EXECUTABLE_PATH:'/opt/specific/chrome'},platform:'linux',exists:async file=>file==='/opt/specific/chrome',cached:never}),'/opt/specific/chrome');
  await assert.rejects(resolveInstalledChromium({env:{PUPPETEER_EXECUTABLE_PATH:'./chrome'},platform:'linux',exists:async()=>true}),error=>error.code==='browser_executable_missing');
});

test('the native channel correlates answers, keeps error codes and fails pending calls on close',async()=>{
  const sent=[];
  const channel=new NativeChannel(frame=>sent.push(frame));
  const ok=channel.request('browser_ready',{a:1});
  const bad=channel.request('browser_spawn',{});
  const orphan=channel.request('browser_retire',{});
  assert.deepEqual(sent.map(frame=>[frame.type,frame.action]),[['native_request','browser_ready'],['native_request','browser_spawn'],['native_request','browser_retire']]);
  channel.receive({type:'native_response',id:sent[0].id,result:{ready:true},error:null});
  channel.receive({type:'native_response',id:sent[1].id,result:null,error:{code:'browser_profile_busy',message:'browser_profile_busy: close Chrome'}});
  channel.receive({type:'native_response',id:'unknown',result:1,error:null});
  assert.deepEqual(await ok,{ready:true});
  await assert.rejects(bad,error=>error.code==='browser_profile_busy'&&error.message==='browser_profile_busy: close Chrome');
  channel.close();
  await assert.rejects(orphan,/disconnected/);
  await assert.rejects(channel.request('browser_ready',{}),/browser_native_unavailable/);
});

// A fake native supervisor and Puppeteer: the helper's launch transaction is exercised without a real browser.
function launchHarness({connect,natives={}}={}){
  const log=[],calls=[];
  const makeBrowser=()=>{
    const listeners={};
    return {connected:true,once(event,fn){listeners[event]=fn;},
      target:()=>({createCDPSession:async()=>({send:async method=>method==='Browser.getVersion'?{product:'Fake/1'}:{targetInfo:{targetId:'browser'}}})}),
      pages:async()=>[],
      async close(){log.push('browser.close');this.connected=false;listeners.disconnected?.();},
      async disconnect(){log.push('browser.disconnect');this.connected=false;}};
  };
  let spawned=0;
  const handlers={
    browser_spawn:async()=>({leaseId:`lease-${++spawned}`,pid:4242,creationTime100ns:'1',webSocketUrl:'ws://127.0.0.1:1/devtools/browser/x'}),
    browser_ready:async()=>({ready:true}),
    browser_retire:async()=>({retired:true}),
    ...natives,
  };
  const service=new BrowserService(path.resolve('unused-fixture'),'unavailable-sdk',undefined,{
    native:{request:async(action,payload)=>{calls.push({action,payload});return handlers[action](payload);}},
    resolveExecutable:async()=>'C:\\fake\\chrome.exe',
  });
  service.helpers=Promise.resolve({loadPuppeteer:async()=>({
    defaultArgs:({userDataDir})=>['--no-first-run','--enable-automation','--no-sandbox','--remote-debugging-port=9222','about:blank',`--user-data-dir=${userDataDir}`],
    connect:options=>{log.push('connect');return (connect??(async()=>makeBrowser()))(options);},
  })});
  return {service,calls,log,actions:()=>calls.map(call=>call.action)};
}
const tick=()=>new Promise(resolve=>setImmediate(resolve));
async function until(predicate){for(let i=0;i<200&&!predicate();i++)await tick();assert.ok(predicate(),'condition was not reached');}

test('a launch failing after the native spawn retires exactly its own browser job and publishes nothing',async()=>{
  const {service,calls,actions}=launchHarness({connect:async()=>{throw new Error('connect failed ws://127.0.0.1:9/devtools/browser/secret');}});
  await service.dispatch('register',{instanceId:'a',generation:1});
  await assert.rejects(service.dispatch('ensureTab',{instanceId:'a',generation:1,name:'main'},'request-1'),error=>/connect failed \[browser endpoint\]/.test(error.message)&&!/secret/.test(error.message));
  assert.deepEqual(actions(),['browser_spawn','browser_retire']);
  const [spawn,retire]=calls.map(call=>call.payload);
  assert.equal(spawn.sourceRequestId,'request-1');
  assert.equal(spawn.profilePath,profileDirectory(path.resolve('unused-fixture'),'a'));
  assert.equal(retire.operationId,spawn.operationId);assert.equal(retire.leaseId,'lease-1');assert.equal(retire.helperSession,spawn.helperSession);
  assert.equal(service.owner('a',1).native,null);assert.equal(service.owner('a',1).browser,null);
  assert.deepEqual((await service.dispatch('snapshot')).map(row=>[row.state,row.pid]),[['error',null]]);
  await service.dispatch('shutdown');
});

test('cancelling a start while native is still spawning retires that operation and never publishes the late result',async()=>{
  let release;
  const late=new Promise(resolve=>{release=resolve;});
  const {service,calls,log,actions}=launchHarness({natives:{browser_spawn:()=>late}});
  await service.dispatch('register',{instanceId:'a',generation:1});
  const opened=service.dispatch('ensureTab',{instanceId:'a',generation:1,name:'main'},'request-1').catch(error=>error);
  await until(()=>actions().includes('browser_spawn'));
  await service.dispatch('revoke',{instanceId:'a',generation:1});
  assert.match((await opened).message,/stale_generation/);
  const [spawn,retire]=calls.map(call=>call.payload);
  assert.deepEqual(actions(),['browser_spawn','browser_retire']);
  assert.equal(retire.operationId,spawn.operationId);assert.equal(retire.leaseId,null,'no lease is known yet: native cancels by operation');
  release({leaseId:'lease-late',pid:1,creationTime100ns:'1',webSocketUrl:'ws://127.0.0.1:1/devtools/browser/x'});
  await tick();await tick();
  assert.deepEqual(actions(),['browser_spawn','browser_retire'],'a late spawn result must not connect, become ready or be retired twice');
  assert.equal(log.includes('connect'),false);
  await service.dispatch('shutdown');
});

test('a start the native side rejects as stale disconnects its connection and retires instead of publishing',async()=>{
  const {service,calls,log,actions}=launchHarness({natives:{browser_ready:async()=>{throw new BrowserError('stale_generation','stale_generation');}}});
  await service.dispatch('register',{instanceId:'a',generation:1});
  await assert.rejects(service.dispatch('ensureTab',{instanceId:'a',generation:1,name:'main'},'request-1'),/stale_generation/);
  assert.deepEqual(actions(),['browser_spawn','browser_ready','browser_retire']);
  assert.ok(log.includes('browser.close')||log.includes('browser.disconnect'));
  assert.equal(calls[2].payload.leaseId,'lease-1');
  assert.equal(service.owner('a',1).browser,null);assert.equal(service.owner('a',1).native,null);
  assert.deepEqual((await service.dispatch('snapshot')).map(row=>row.state),['closed'],'a stale start is not reported as a browser error');
  await service.dispatch('shutdown');
});

test('a blocked retirement is retried before another browser is started, and a replacement waits for it',async()=>{
  let block=true,releaseRetire;
  const {service,calls,actions}=launchHarness({
    connect:async()=>{throw new Error('connect failed');},
    natives:{browser_retire:async()=>{if(block)throw new BrowserError('browser_cleanup_blocked','job still has processes');if(releaseRetire)await new Promise(resolve=>{releaseRetire=resolve;});return {retired:true};}},
  });
  await service.dispatch('register',{instanceId:'a',generation:1});
  await assert.rejects(service.dispatch('ensureTab',{instanceId:'a',generation:1,name:'main'},'r1'),/cleanup is blocked and will be retried/);
  assert.equal((await service.dispatch('snapshot'))[0].state,'error');
  // The next start first retries that same retirement; while it keeps failing no new browser is spawned.
  await assert.rejects(service.dispatch('ensureTab',{instanceId:'a',generation:1,name:'main'},'r2'),/browser_cleanup_blocked/);
  assert.deepEqual(actions(),['browser_spawn','browser_retire','browser_retire']);
  assert.equal(calls[1].payload.operationId,calls[2].payload.operationId);
  // A replacement generation must not exist until the previous generation's job is verified gone.
  releaseRetire=true;block=false;
  let replaced=false;
  const replacement=service.dispatch('register',{instanceId:'a',generation:2}).then(()=>{replaced=true;});
  await until(()=>actions().length===4);
  assert.equal(replaced,false,'registration of the new generation waits for the old generation\'s retirement');
  await assert.rejects(service.dispatch('ensureTab',{instanceId:'a',generation:2,name:'main'}),/stale_generation/);
  releaseRetire();
  await replacement;
  assert.equal(replaced,true);
  assert.equal(service.owner('a',2).generation,2);
  await service.dispatch('shutdown');
});
