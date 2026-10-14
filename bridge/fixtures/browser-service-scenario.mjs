import assert from 'node:assert/strict';
import path from 'node:path';
import {pathToFileURL} from 'node:url';
import {BrowserService} from '../omp-browser-service.mjs';
import {mkdir,readFile} from 'node:fs/promises';
function completedDownload(session) {
  return new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>{session.off('Browser.downloadProgress',progress);reject(new Error('Owned download did not complete'));},10000);
    function progress(event){if(event.state==='completed'){clearTimeout(timer);session.off('Browser.downloadProgress',progress);resolve(event);}}
    session.on('Browser.downloadProgress',progress);
  });
}

const root=process.env.BROWSER_TEST_ROOT;
process.env.USERPROFILE=root;
process.env.HOME=root;
process.env.PI_CODING_AGENT_DIR=path.join(root,'agent');
delete process.env.PI_COMPILED;
const service=new BrowserService(root,process.env.OMP_SDK_DIR);
await service.start();
try {
  const {loadPuppeteer}=await import(pathToFileURL(path.join(process.env.OMP_SDK_DIR,'src/tools/browser/launch.ts')).href);
  const puppeteer=await loadPuppeteer();
  await service.dispatch('configure',{headless:true,embedded:false});
  for(const instanceId of ['a','b'])await service.dispatch('register',{instanceId,generation:1});
  const [a,duplicate,b,second]=await Promise.all([['a','main'],['a','main'],['b','main'],['a','second']].map(([instanceId,name])=>service.dispatch('ensureTab',{instanceId,generation:1,name})));
  assert.equal(a.cdpUrl,duplicate.cdpUrl,'simultaneous named opens must be single flight');
  const connections=[];
  for(const ticket of [a,second,b])connections.push(await puppeteer.connect({browserURL:ticket.cdpUrl,defaultViewport:null,protocolTimeout:15000}));
  const pages=await Promise.all(connections.map(async browser=>{
    const pages=await browser.pages();
    assert.equal(pages.length,1,'CDP discovery must not expose a peer named page');
    await pages[0].goto(process.env.BROWSER_FIXTURE_URL);
    return pages[0];
  }));
  await pages[0].evaluate(()=>{document.cookie='owner=a;path=/';localStorage.setItem('owner','a');});
  assert.equal(await pages[2].evaluate(()=>localStorage.getItem('owner')),null,'same-origin storage must not cross terminal profiles');
  assert.equal(await pages[1].evaluate(()=>localStorage.getItem('owner')),'a','named pages in one terminal retain its shared login');
  await pages[1].type('#shared','second');
  assert.equal(await pages[0].$eval('#shared',e=>e.value),'');
  assert.equal(await pages[1].$eval('#shared',e=>e.value),'second');
  const peer=await pages[1].createCDPSession();
  const {targetInfo}=await peer.send('Target.getTargetInfo');
  const own=await pages[0].createCDPSession();
  await assert.rejects(own.send('Target.closeTarget',{targetId:targetInfo.targetId}),/outside this named tab/);
  await assert.rejects(own.send('Browser.close'),/outside this named tab/);
  const browserSession=await connections[0].target().createCDPSession();
  const peerBrowserSession=await connections[1].target().createCDPSession();
  await assert.rejects(browserSession.send('Target.closeTarget',{targetId:service.owner('a',1).browserTargetId}),/cannot be controlled/);
  await assert.rejects(browserSession.send('Target.attachToTarget',{targetId:targetInfo.targetId,flatten:true}),/outside this named tab/);
  const downloads=path.join(root,'downloads');await mkdir(downloads);
  await browserSession.send('Browser.setDownloadBehavior',{behavior:'allow',downloadPath:downloads,eventsEnabled:true});
  const observed=[];
  browserSession.on('Browser.downloadWillBegin',event=>observed.push(event.url));
  let complete=completedDownload(browserSession);
  await pages[0].$eval('#download',e=>{e.href+='?owner=own';e.click();});await complete;
  assert.equal(await readFile(path.join(downloads,'fixture.txt'),'utf8'),'owned-download');
  await peerBrowserSession.send('Browser.setDownloadBehavior',{behavior:'allow',downloadPath:downloads,eventsEnabled:true});
  complete=completedDownload(peerBrowserSession);
  await pages[1].$eval('#download',e=>{e.href+='?owner=peer';e.click();});await complete;
  assert.deepEqual(observed,[new URL('/download?owner=own',process.env.BROWSER_FIXTURE_URL).href],'browser sessions must not expose peer download events');
  await browserSession.detach();await peerBrowserSession.detach();
  const extra=await connections[0].newPage();
  await extra.goto(process.env.BROWSER_FIXTURE_URL);
  await extra.type('#shared','raw-created');
  assert.equal(await extra.$eval('#shared',e=>e.value),'raw-created');
  await extra.close();
  const browserA=service.owner('a',1).browser;
  const browserB=service.owner('b',1).browser;
  const pid=browserA.process().pid;
  assert.notEqual(pid,browserB.process().pid);
  await service.dispatch('configure',{headless:false,embedded:false});
  assert.equal(service.owner('a',1).browser.process().pid,pid,'headless preference must not restart an active browser');
  assert.equal(service.owner('a',1).headless,true);
  for(const browser of connections)browser.disconnect();
  await browserA.close();
  assert.notEqual(browserA.process().exitCode,null,'browser-only close must reap its root process');
  assert.equal(browserB.connected,true,'closing A must not stop B');
  assert.equal((await fetch(a.cdpUrl+'/json/version',{method:'HEAD'})).status,403,'closed tabs must revoke their CDP discovery capability');
  await service.dispatch('configure',{headless:true,embedded:false});
  const reopened=await service.dispatch('ensureTab',{instanceId:'a',generation:1,name:'main'});
  assert.notEqual(reopened.epoch,a.epoch);
  const restored=await puppeteer.connect({browserURL:reopened.cdpUrl,defaultViewport:null});
  const page=(await restored.pages())[0];
  await page.goto(process.env.BROWSER_FIXTURE_URL);
  assert.equal(await page.evaluate(()=>localStorage.getItem('owner')),'a','browser restart must retain the terminal profile');
  restored.disconnect();
  await service.dispatch('revoke',{instanceId:'a',generation:1});
  await assert.rejects(service.dispatch('ensureTab',{instanceId:'a',generation:1,name:'main'}),/stale_generation/);
  assert.equal((await fetch(a.cdpUrl+'/json/version')).status,403);
  console.log('Real browser isolation and lifecycle assertions passed');
} finally {
  await service.dispatch('shutdown');
}
