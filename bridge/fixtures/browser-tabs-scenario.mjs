import assert from 'node:assert/strict';
import {fileURLToPath} from 'node:url';
import {spawn} from 'node:child_process';
import {readFile} from 'node:fs/promises';
import path from 'node:path';
import {installTerminalBrowserBridge} from '../omp-browser.mjs';
import {profileDirectory} from '../omp-browser-service.mjs';

// Loaded as an explicit extension by the real OMP CLI. A tab opened by hand in the terminal's Chrome must be
// attached to the agent session automatically, controllable, renameable, and must not break the AI's own tab.
export default async function tabsScenario(pi) {
  const root=process.env.BROWSER_TEST_ROOT;
  let session;
  const originalInfos=pi.pi.AgentSession.prototype.getAllToolInfos;
  function captureInfos(...args){session=this;return originalInfos.apply(this,args);}
  pi.pi.AgentSession.prototype.getAllToolInfos=captureInfos;
  const helper=spawn(process.env.BUN_BIN||'bun',[fileURLToPath(new URL('../omp-browser-service.mjs',import.meta.url)),root,process.env.OMP_SDK_DIR],{env:process.env,stdio:['pipe','pipe','inherit']});
  let sequence=0,buffered='';
  const pending=new Map();
  const ready=new Promise((resolve,reject)=>{helper.once('error',reject);helper.once('exit',()=>reject(new Error('Browser helper exited')));helper.stdout.on('data',chunk=>{
    buffered+=chunk;
    for(;;){
      const end=buffered.indexOf('\n');if(end<0)break;
      const frame=JSON.parse(buffered.slice(0,end));buffered=buffered.slice(end+1);
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
  installTerminalBrowserBridge(pi,async(action,payload)=>{
    if(action==='browser_prepare')return control('ensureTab',{instanceId:'terminal',generation:1,name:payload.name,...(payload.target===undefined?{}:{target:payload.target})});
    if(action==='browser_close_tab')return control('closeTab',{instanceId:'terminal',generation:1,name:payload.name});
    if(action==='browser_close_all')return control('closeTab',{instanceId:'terminal',generation:1,name:null});
    if(action==='browser_tabs')return control('listTabs',{instanceId:'terminal',generation:1});
    throw new Error('unexpected browser action '+action);
  });
  async function chromePages(){
    const port=(await readFile(path.join(profileDirectory(root,'terminal'),'DevToolsActivePort'),'utf8')).split('\n')[0];
    return {port,list:(await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()).filter(target=>target.type==='page')};
  }
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
      const opened=await cell('var tab=await browser.open({name:"browser",url:"about:blank",headed:true,persist:true});await tab.goto("data:text/html,<title>AI tab</title>mine");print("FLOW:"+JSON.stringify({names:(await browser.tabs()).map(t=>t.name)}));');
      assert.deepEqual(opened.names,['browser']);
      // The user opens a tab by hand; Chrome activates it, hiding the AI's own tab.
      const {port}=await chromePages();
      const created=await fetch(`http://127.0.0.1:${port}/json/new?${encodeURIComponent('data:text/html,<title>User tab</title>theirs')}`,{method:'PUT'});
      assert.equal(created.ok,true);
      const seen=await cell('var rows=await browser.tabs();print("FLOW:"+JSON.stringify({rows:rows.map(t=>({name:t.name,title:t.title,kind:t.kind}))}));');
      assert.deepEqual(seen.rows.map(row=>row.name),['browser','tab-1'],'The hand-opened tab must be attached automatically');
      assert.equal(seen.rows[1].title,'User tab');
      assert.equal(seen.rows[1].kind,'connected');
      const shot=await cell('var failure=null;try{await tab.screenshot();}catch(error){failure=error.message;}print("FLOW:"+JSON.stringify({failure}));');
      assert.equal(shot.failure,null,'The AI tab must be brought to the front for screenshots');
      const driven=await cell('var mine=browser.tab("tab-1");await mine.evaluate(()=>{document.body.innerHTML="<input id=x>";});await mine.fill("#x","controlled");print("FLOW:"+JSON.stringify({value:await mine.value("#x"),own:await tab.url()}));');
      assert.equal(driven.value,'controlled','The AI must be able to control the hand-opened tab');
      assert.match(driven.own,/AI(%20| )tab/,'The AI tab must be untouched');
      const renamed=await cell('var other=await browser.open({name:"user",app:{target:"User tab"}});print("FLOW:"+JSON.stringify({title:await other.evaluate(()=>document.title),rows:(await browser.tabs()).map(t=>t.name)}));');
      assert.equal(renamed.title,'User tab');
      assert.deepEqual(renamed.rows,['browser','user'],'Re-attaching under a name must replace the automatic tab');
      const missing=await cell('var message=null;try{await browser.open({name:"nope",app:{target:"no such page"}});}catch(error){message=error.message;}print("FLOW:"+JSON.stringify({message}));');
      assert.match(missing.message,/No browser page matched "no such page"\. Available pages:/);
      await cell('await other.close();print("FLOW:"+JSON.stringify({done:true}));');
      assert.equal((await chromePages()).list.length,1,'Closing an attached tab closes its page');
      const after=await cell('print("FLOW:"+JSON.stringify({rows:(await browser.tabs()).map(t=>t.name)}));');
      assert.deepEqual(after.rows,['browser']);
      console.log('Hand-opened tabs are attached automatically and controllable');
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
