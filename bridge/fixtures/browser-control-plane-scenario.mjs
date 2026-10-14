import assert from 'node:assert/strict';
import {fileURLToPath} from 'node:url';
import {spawn} from 'node:child_process';
import {installTerminalBrowserBridge} from '../omp-browser.mjs';

// Loaded as an explicit extension by the real OMP CLI, not a second source SDK.
export default async function controlPlaneScenario(pi) {
  const root=process.env.BROWSER_TEST_ROOT;
  let session;
  const originalInfos=pi.pi.AgentSession.prototype.getAllToolInfos;
  function captureInfos(...args){session=this;return originalInfos.apply(this,args);}
  pi.pi.AgentSession.prototype.getAllToolInfos=captureInfos;
  const helper=spawn(process.env.BUN_BIN||'bun',[fileURLToPath(new URL('../omp-browser-service.mjs',import.meta.url)),root,process.env.OMP_SDK_DIR],{env:process.env,stdio:['pipe','pipe','inherit']});
  let sequence=0;
  let buffered='';
  const pending=new Map();
  let readyResolve,readyReject;
  const ready=new Promise((resolve,reject)=>{readyResolve=resolve;readyReject=reject;});
  helper.on('error',readyReject);
  helper.on('exit',()=>{readyReject(new Error('Browser helper exited'));for(const call of pending.values())call.reject(new Error('Browser helper exited'));pending.clear();});
  helper.stdout.on('data',chunk=>{
    buffered+=chunk;
    for(;;){
      const end=buffered.indexOf('\n');if(end<0)break;
      const frame=JSON.parse(buffered.slice(0,end));buffered=buffered.slice(end+1);
      if(frame.type==='ready')readyResolve();
      if(frame.type==='response'){
        const call=pending.get(frame.id);pending.delete(frame.id);
        if(frame.ok)call?.resolve(frame.data);else call?.reject(new Error(frame.error));
      }
    }
  });
  function control(action,payload={}){
    const id=String(++sequence);
    return new Promise((resolve,reject)=>{pending.set(id,{resolve,reject});helper.stdin.write(JSON.stringify({id,action,payload})+'\n');});
  }
  await ready;
  await control('configure',{headless:true,embedded:false});
  await control('register',{instanceId:'terminal',generation:1});
  let controllerOnline=true;
  installTerminalBrowserBridge(pi,async(action,payload)=>{
    if(action==='browser_tabs')return controllerOnline?control('listTabs',{instanceId:'terminal',generation:1}):[];
    if(!controllerOnline)throw new Error('Controller replies are unavailable after browser allocation');
    assert.equal(action,'browser_prepare');
    const ticket=await control('ensureTab',{instanceId:'terminal',generation:1,name:payload.name});
    controllerOnline=false;
    return ticket;
  });
  pi.on('session_start',()=>{void run();});
  async function run(){
    let exitCode=0;
    try {
      pi.getAllTools();
      assert.ok(session,'Main native OMP session was not captured');
      const tool=session.getToolByName('eval');
      async function cell(code){
        const result=await tool.execute('flow-'+crypto.randomUUID(),{language:'js',code,timeout:15});
        const text=result.content.filter(block=>block.type==='text').map(block=>block.text).join('\n');
        const match=/FLOW:(\{[^\n]*\})/.exec(text);
        assert.ok(match,`Missing browser observation: ${text}`);
        return JSON.parse(match[1]);
      }
      assert.deepEqual(await cell('var tab=await browser.open({name:"browser",url:"about:blank",headed:true,persist:true});var rows=await browser.tabs();print("FLOW:"+JSON.stringify({url:await tab.url(),names:rows.map(t=>t.name),kind:rows[0]?.kind}));'),{url:'about:blank',names:['browser'],kind:'connected'});
      const [status]=await control('snapshot');
      assert.equal(status.state,'ready');
      assert.ok(status.pid,'The owned helper, not an unmanaged OMP browser, must own Chrome');
      assert.deepEqual(await cell('await tab.evaluate(()=>{document.body.innerHTML="<input id=shared>";});await tab.fill("#shared","native CDP");print("FLOW:"+JSON.stringify({value:await tab.value("#shared"),names:(await browser.tabs()).map(t=>t.name)}));'),{value:'native CDP',names:['browser']});
      await control('closeTab',{instanceId:'terminal',generation:1,name:'browser'});
      const closed=await cell('var closedMessage=null;try{await tab.url();}catch(error){closedMessage=error.message;}print("FLOW:"+JSON.stringify({closedMessage}));');
      assert.equal(closed.closedMessage,'Browser tab is closed; reopen it with browser.open');
      assert.deepEqual(await cell('print("FLOW:"+JSON.stringify({names:(await browser.tabs()).map(t=>t.name)}));'),{names:[]});
      console.log('Native URL, tabs, DOM and closed-tab behavior passed with controller replies unavailable');
    } catch(error) {console.error(error);exitCode=1;}
    finally {
      try {if(session)await session.dispose();}
      finally {
        if(pi.pi.AgentSession.prototype.getAllToolInfos===captureInfos)pi.pi.AgentSession.prototype.getAllToolInfos=originalInfos;
        await control('shutdown');helper.stdin.end();
      }
      process.exit(exitCode);
    }
  }
}
