import assert from 'node:assert/strict';
import {fileURLToPath} from 'node:url';
import {spawn} from 'node:child_process';
import {access,readFile} from 'node:fs/promises';
import path from 'node:path';
import {installTerminalBrowserBridge} from '../omp-browser.mjs';
import {profileDirectory} from '../omp-browser-service.mjs';
import {TestNativeHost} from './native-host.mjs';

// Loaded as an explicit extension by the real OMP CLI. The human starts the browser with the Browser button (no
// agent browser.open yet) and opens tabs by hand: the agent must see them on its first browser.tabs().
export default async function prelaunchedScenario(pi) {
  const root=process.env.BROWSER_TEST_ROOT;
  let session;
  const originalInfos=pi.pi.AgentSession.prototype.getAllToolInfos;
  function captureInfos(...args){session=this;return originalInfos.apply(this,args);}
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
      assert.ok(session,'Main native OMP session was not captured');
      const tool=session.getToolByName('eval');
      async function cell(code){
        const result=await tool.execute('flow-'+crypto.randomUUID(),{language:'js',code,timeout:40});
        const text=result.content.filter(block=>block.type==='text').map(block=>block.text).join('\n');
        const match=/FLOW:(\{[^\n]*\})/.exec(text);
        assert.ok(match,`Missing browser observation: ${text}`);
        return JSON.parse(match[1]);
      }
      // No browser yet: nothing to list, and asking must not start one.
      const none=await cell('var names=null,error=null;try{names=(await browser.tabs()).map(t=>t.name);}catch(e){error=e.message;}print("FLOW:"+JSON.stringify({names,error}));');
      console.log('before launch',JSON.stringify(none));
      assert.ok(none.error!==null||none.names.length===0,'no tabs can exist before a browser does');
      await assert.rejects(access(portFile),'listing tabs must not launch Chrome');
      // The Browser button: the helper opens the headed browser; the agent has not called browser.open.
      const row=await control('openVisible',{instanceId:'terminal',generation:1,restartHeadless:false});
      assert.equal(row.state,'ready');
      const port=await chromePort();
      for(const title of ['Manual A','Manual B']){
        const created=await fetch(`http://127.0.0.1:${port}/json/new?${encodeURIComponent(`data:text/html,<title>${title}</title>${title}`)}`,{method:'PUT'});
        assert.equal(created.ok,true);
      }
      const seen=await cell('var rows=await browser.tabs();print("FLOW:"+JSON.stringify({rows:rows.map(t=>({name:t.name,title:t.title,kind:t.kind}))}));');
      console.log('after hand-opened tabs',JSON.stringify(seen));
      assert.deepEqual(seen.rows.map(r=>r.name).sort(),['tab-1','tab-2','tab-3'],'every hand-opened tab (and the launch page) must be attached');
      assert.deepEqual(seen.rows.map(r=>r.title).filter(Boolean).sort(),['Manual A','Manual B']);
      assert.ok(seen.rows.every(r=>r.kind==='connected'));
      const manual=seen.rows.find(r=>r.title==='Manual A').name;
      const read=await cell(`var mine=browser.tab(${JSON.stringify(manual)});print("FLOW:"+JSON.stringify({title:await mine.evaluate(()=>document.title)}));`);
      assert.equal(read.title,'Manual A','The agent must be able to control a hand-opened tab by name');
      // The agent's own tab still works next to them.
      const work=await cell('var tab=await browser.open({name:"work",url:"about:blank"});print("FLOW:"+JSON.stringify({names:(await browser.tabs()).map(t=>t.name).sort()}));');
      assert.deepEqual(work.names,['tab-1','tab-2','tab-3','work']);
      console.log('Pre-launched browser tabs are visible to the agent');
    } catch(error) {process.stderr.write(String(error.stack||error)+'\n');exitCode=1;}
    finally {
      try {if(session)await session.dispose();}
      finally {
        if(pi.pi.AgentSession.prototype.getAllToolInfos===captureInfos)pi.pi.AgentSession.prototype.getAllToolInfos=originalInfos;
        try{await control('shutdown');}catch{}
        helper.stdin.end();
      }
      process.exit(exitCode);
    }
  }
}
