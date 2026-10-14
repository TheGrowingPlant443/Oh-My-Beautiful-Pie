import test from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import {BrowserService,profileDirectory} from './omp-browser-service.mjs';
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
  assert.deepEqual(await service.dispatch('snapshot'),[{instanceId:'a',generation:1,epoch:0,state:'closed',headless:false,embedded:true,pid:null,error:null}]);
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
  assert.deepEqual((await service.dispatch('snapshot')).map(row=>[row.instanceId,row.embedded]),[['a',true],['b',false]]);
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
