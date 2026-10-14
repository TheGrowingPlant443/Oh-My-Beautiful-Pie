import assert from 'node:assert/strict';
import path from 'node:path';
import {pathToFileURL} from 'node:url';
import {BrowserService} from '../omp-browser-service.mjs';

const root=process.env.BROWSER_TEST_ROOT;
process.env.USERPROFILE=root;
process.env.HOME=root;
process.env.PI_CODING_AGENT_DIR=path.join(root,'agent');
delete process.env.PI_COMPILED;
const frames=[];
const service=new BrowserService(root,process.env.OMP_SDK_DIR,frame=>frames.push(frame));
await service.start();
const global={headless:true,embedded:false};
try {
  const {loadPuppeteer}=await import(pathToFileURL(path.join(process.env.OMP_SDK_DIR,'src/tools/browser/launch.ts')).href);
  const puppeteer=await loadPuppeteer();
  await service.dispatch('configure',global);
  for(const instanceId of ['a','b'])await service.dispatch('register',{instanceId,generation:1});
  const open=async(instanceId,name='main')=>{
    const ticket=await service.dispatch('ensureTab',{instanceId,generation:1,name});
    const browser=await puppeteer.connect({browserURL:ticket.cdpUrl,defaultViewport:null,protocolTimeout:15000});
    const page=(await browser.pages())[0];
    return {ticket,browser,page};
  };
  const a=await open('a'), b=await open('b');
  await a.page.goto(process.env.BROWSER_FIXTURE_URL+'?a');
  await b.page.goto(process.env.BROWSER_FIXTURE_URL+'?b');
  const headlessPid=service.owner('a',1).browser.process().pid;
  const otherPid=service.owner('b',1).browser.process().pid;
  assert.equal(service.owner('a',1).headless,true);
  assert.equal(service.embedded(service.owner('a',1)),false);

  // Without consent a running headless browser is never touched.
  await assert.rejects(service.dispatch('openVisible',{instanceId:'a',generation:1,restartHeadless:false}),/^Error: browser_headless_restart_required$/);
  assert.equal(service.owner('a',1).browser.process().pid,headlessPid);
  assert.equal(service.owner('a',1).forceVisible,false);
  assert.equal(a.page.url(),process.env.BROWSER_FIXTURE_URL+'?a','refusing must keep the headless tabs');
  await assert.rejects(service.dispatch('openVisible',{instanceId:'a',generation:1,restartHeadless:'yes'}),/invalid browser visibility request/);
  await assert.rejects(service.dispatch('openVisible',{instanceId:'a',generation:9,restartHeadless:true}),/stale_generation/);
  await assert.rejects(service.dispatch('openVisible',{instanceId:'nobody',generation:1,restartHeadless:true}),/stale_generation/);

  // With consent only this owner restarts, headed and flagged for docking.
  const row=await service.dispatch('openVisible',{instanceId:'a',generation:1,restartHeadless:true});
  const visiblePid=service.owner('a',1).browser.process().pid;
  assert.notEqual(visiblePid,headlessPid);
  assert.equal(row.pid,visiblePid);
  assert.equal(row.headless,false);
  assert.equal(row.embedded,true);
  assert.equal(row.state,'ready');
  assert.equal((await fetch(a.ticket.cdpUrl+'/json/version',{method:'HEAD'})).status,403,'old tab capabilities must be revoked by the restart');
  assert.equal(service.owner('b',1).browser.process().pid,otherPid,'another owner must keep its browser');
  assert.deepEqual(service.settings,global,'a per-session request must not change global browser settings');
  assert.equal(b.page.url(),process.env.BROWSER_FIXTURE_URL+'?b','another owner must keep its tabs');
  assert.equal(service.embedded(service.owner('b',1)),false,'other owners keep the global embedded preference');

  // A headed browser is reused: same process, same pages, nothing navigated.
  const headed=await open('a');
  assert.equal((await headed.browser.pages()).length,1);
  await headed.page.goto(process.env.BROWSER_FIXTURE_URL+'?visible');
  const again=await service.dispatch('openVisible',{instanceId:'a',generation:1,restartHeadless:false});
  assert.equal(again.pid,visiblePid,'headed browsers must not be restarted');
  assert.equal(again.embedded,true);
  assert.equal(headed.page.url(),process.env.BROWSER_FIXTURE_URL+'?visible');
  assert.equal((await headed.browser.pages()).length,1,'reuse must not create or navigate tabs');
  assert.equal(frames.at(-1).embedded,true,'the forced presentation must be published to the supervisor');
  assert.equal(frames.at(-1).instanceId,'a');

  // Changing global settings supersedes the override without restarting the running browser.
  await service.dispatch('configure',global);
  assert.equal(service.owner('a',1).browser.process().pid,visiblePid);
  assert.equal(service.owner('a',1).forceVisible,false);
  assert.equal((await service.dispatch('snapshot')).find(row=>row.instanceId==='a').embedded,false);
  // Closing and reopening through the normal path follows the global preference again.
  headed.browser.disconnect();a.browser.disconnect();b.browser.disconnect();
  const closing=service.owner('a',1).browser;
  await closing.close();
  await new Promise(resolve=>setTimeout(resolve,500));
  assert.equal(service.owner('a',1).browser,null);
  const reopened=await open('a');
  assert.equal(service.owner('a',1).headless,true,'a reopened browser follows the global preference once the override is gone');
  reopened.browser.disconnect();

  // A browser started with the Browser button has no tab ticket; hand-opened pages must still be discoverable.
  await service.dispatch('register',{instanceId:'c',generation:1});
  assert.deepEqual(await service.dispatch('listTabs',{instanceId:'c',generation:1}),[]);
  assert.equal(service.owner('c',1).browser,null,'listing tabs must not launch Chrome');
  await service.dispatch('openVisible',{instanceId:'c',generation:1,restartHeadless:false});
  const chrome=service.owner('c',1).browser;
  const manualA=await chrome.newPage();await manualA.goto('data:text/html,<title>Manual A</title>a');
  const manualB=await chrome.newPage();await manualB.goto('data:text/html,<title>Manual B</title>b');
  const rows=await service.dispatch('listTabs',{instanceId:'c',generation:1});
  assert.deepEqual(rows.map(row=>row.name).sort(),['tab-1','tab-2','tab-3']);
  assert.deepEqual(rows.map(row=>row.title).filter(Boolean).sort(),['Manual A','Manual B']);
  for(const row of rows)assert.equal((await fetch(row.cdpUrl+'/json/version')).status,200);
  const repeat=await service.dispatch('listTabs',{instanceId:'c',generation:1});
  assert.deepEqual(repeat.map(row=>[row.name,row.tabEpoch]).sort(),rows.map(row=>[row.name,row.tabEpoch]).sort(),'listing twice must not bind pages twice');
  await manualB.close();
  assert.equal((await service.dispatch('listTabs',{instanceId:'c',generation:1})).length,2,'a closed page leaves the list');
  console.log('openVisible assertions passed');
} finally {
  await service.dispatch('shutdown');
}
