import assert from 'node:assert/strict';
import {fileURLToPath} from 'node:url';
import {spawn} from 'node:child_process';
import {access,readFile} from 'node:fs/promises';
import path from 'node:path';
import {installTerminalBrowserBridge} from '../omp-browser.mjs';
import {profileDirectory} from '../omp-browser-service.mjs';
import {TestNativeHost,openManualTabs} from './native-host.mjs';

// Loaded by the pinned OMP CLI: targets/tabs must discover the Browser button's headed Chrome,
// not resolve a native relay/Tern target or launch an unrelated headless browser.
export default async function prelaunchedScenario(pi) {
  const root=process.env.BROWSER_TEST_ROOT;
  const captured={};
  const originalInfos=pi.pi.AgentSession.prototype.getAllToolInfos;
  function captureInfos(...args){captured.session=this;return originalInfos.apply(this,args);}
  pi.pi.AgentSession.prototype.getAllToolInfos=captureInfos;
  const host=new TestNativeHost(root);
  const helper=spawn(process.env.BUN_BIN||'bun',[fileURLToPath(new URL('../omp-browser-service.mjs',import.meta.url)),root,process.env.OMP_SDK_DIR],{env:process.env,stdio:['pipe','pipe','inherit']});
  host.attach(helper);
  let sequence=0,buffered='';
  const pending=new Map();
  const ready=new Promise((resolve,reject)=>{helper.once('error',reject);helper.once('exit',()=>reject(new Error('Browser helper exited')));helper.stdout.on('data',chunk=>{
    buffered+=chunk;
    for(;;){
      const end=buffered.indexOf('\n');if(end<0)break;
      const frame=JSON.parse(buffered.slice(0,end));buffered=buffered.slice(end+1);
      if(frame.type==='native_request')host.serve(helper,frame);
      if(frame.type==='ready')resolve();
      if(frame.type==='response'){const call=pending.get(frame.id);pending.delete(frame.id);if(frame.ok)call?.resolve(frame.data);else call?.reject(new Error(frame.error));}
    }
  });});
  function control(action,payload={}){
    const id=String(++sequence);
    return new Promise((resolve,reject)=>{pending.set(id,{resolve,reject});helper.stdin.write(JSON.stringify({id,action,payload})+'\n');});
  }
  await ready;
  await control('configure',{headless:false,embedded:true});
  await control('register',{instanceId:'terminal',generation:1});
  const disposeBrowser=installTerminalBrowserBridge(pi,async(action,payload)=>{
    if(action==='browser_prepare')return control('ensureTab',{instanceId:'terminal',generation:1,name:payload.name,...(payload.target===undefined?{}:{target:payload.target})});
    if(action==='browser_close_tab')return control('closeTab',{instanceId:'terminal',generation:1,name:payload.name});
    if(action==='browser_close_all')return control('closeTab',{instanceId:'terminal',generation:1,name:null});
    if(action==='browser_tabs')return control('listTabs',{instanceId:'terminal',generation:1});
    throw new Error('unexpected browser action '+action);
  });
  pi.on('session_shutdown',(_event,ctx)=>{if(ctx.agent?.kind==='main')disposeBrowser();});
  const portFile=path.join(profileDirectory(root,'terminal'),'DevToolsActivePort');
  async function chromePort(){return (await readFile(portFile,'utf8')).split('\n')[0];}
  pi.on('session_start',()=>{void run();});
  async function run(){
    let exitCode=0;
    try {
      pi.getAllTools();
      assert.ok(captured.session,'Main native OMP session was not captured');
      const tool=captured.session.getToolByName('eval');
      async function cell(code){
        const result=await tool.execute('flow-'+crypto.randomUUID(),{language:'js',code,timeout:40});
        const text=result.content.filter(block=>block.type==='text').map(block=>block.text).join('\n');
        const match=/FLOW:(\{[^\n]*\})/.exec(text);
        assert.ok(match,`Missing browser observation: ${text}`);
        return JSON.parse(match[1]);
      }
      // Both supported SDK discovery calls must be empty, successful, and side-effect-free before launch.
      const none=await cell('var targets=await browser.targets({});var rows=await browser.tabs();print("FLOW:"+JSON.stringify({targets,rows}));');
      assert.deepEqual(none,{targets:[],rows:[]});
      assert.equal(host.calls.includes('browser_spawn'),false);
      await assert.rejects(access(portFile),'discovery must not launch Chrome');
      // The Browser button: the helper opens the headed browser; the agent has not called browser.open.
      const row=await control('openVisible',{instanceId:'terminal',generation:1,restartHeadless:false});
      assert.equal(row.state,'ready');
      assert.equal(row.headless,false);
      const pid=row.pid;
      const port=await chromePort();
      const manualTitles=['Manual A','Manual B'];
      await openManualTabs(port,manualTitles.map(title=>`data:text/html,<title>${title}</title>${title}`));
      const pageIds=async()=>new Set((await(await fetch(`http://127.0.0.1:${port}/json/list`)).json()).filter(page=>page.type==='page').map(page=>page.id));
      const originalPages=await pageIds();
      const seen=await cell('var targets=await browser.targets({});console.log(targets);var rows=await browser.tabs();console.log(rows);print("FLOW:"+JSON.stringify({targets,rows:rows.map(t=>({name:t.name,title:t.title,kind:t.kind}))}));');
      const manualTargets=seen.targets.filter(target=>manualTitles.includes(target.title));
      assert.deepEqual(manualTargets.map(target=>target.title).sort(),manualTitles);
      assert.ok(manualTargets.every(target=>target.target===target.url&&typeof target.shown==='boolean'));
      const manualRows=seen.rows.filter(row=>manualTitles.includes(row.title));
      assert.deepEqual(manualRows.map(row=>row.title).sort(),manualTitles);
      assert.ok(manualRows.every(row=>row.kind==='connected'));
      assert.ok(manualTargets.every(target=>manualRows.some(row=>row.name===target.tab&&row.title===target.title)));
      assert.deepEqual(await pageIds(),originalPages,'discovery must not create or replace a page');
      const manual=manualTargets.find(target=>target.title==='Manual A');
      const read=await cell(`var mine=browser.tab(${JSON.stringify(manual.tab)});await mine.evaluate(()=>{document.body.dataset.agent="attached";});print("FLOW:"+JSON.stringify({title:await mine.title(),marker:await mine.evaluate(()=>document.body.dataset.agent)}));`);
      assert.deepEqual(read,{title:'Manual A',marker:'attached'},'The agent must control the button-opened page by target tab name');
      const adopted=await cell(`var adopted=await browser.open({name:"manual",app:{target:${JSON.stringify(manual.target)}}});print("FLOW:"+JSON.stringify({title:await adopted.title(),marker:await adopted.evaluate(()=>document.body.dataset.agent)}));`);
      assert.deepEqual(adopted,{title:'Manual A',marker:'attached'},'A discovered target must adopt the existing page, without navigating it');
      assert.deepEqual(await pageIds(),originalPages,'adoption must preserve the button-opened pages');
      manualRows.find(row=>row.name===manual.tab).name='manual';
      const refused=await cell('var errors=[];for(var app of [{relay:true},{tern:true},{cdp_url:"http://127.0.0.1:1"},{path:"chrome"}]){try{await browser.targets({app});}catch(error){errors.push(error.message);}}print("FLOW:"+JSON.stringify({errors}));');
      assert.equal(refused.errors.length,4);
      assert.ok(refused.errors.every(error=>error.includes("OMP Manager owns this terminal")));
      // The agent's own tab still works next to them.
      const work=await cell('var tab=await browser.open({name:"work",url:"about:blank"});print("FLOW:"+JSON.stringify({names:(await browser.tabs()).map(t=>t.name).sort()}));');
      assert.ok(work.names.includes('work'));
      assert.ok(manualRows.every(row=>work.names.includes(row.name)),'opening an agent tab must preserve manual-tab references');
      const [current]=await control('snapshot');
      assert.equal(current.pid,pid,'discovery/adoption must retain the Browser button process');
      assert.equal(current.headless,false);
      assert.equal(host.calls.filter(action=>action==='browser_spawn').length,1,'only the Browser button may launch Chrome');
      console.log('Pinned SDK targets/tabs discover and control the Browser button browser without another launch');
    } catch(error) {process.stderr.write(String(error.stack||error)+'\n');exitCode=1;}
    finally {
      try {if(captured.session)await captured.session.dispose();}
      finally {
        if(pi.pi.AgentSession.prototype.getAllToolInfos===captureInfos)pi.pi.AgentSession.prototype.getAllToolInfos=originalInfos;
        try{await control('shutdown');}catch{}
        helper.stdin.end();
      }
      process.exit(exitCode);
    }
  }
}
