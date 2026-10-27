import assert from 'node:assert/strict';
import {fileURLToPath} from 'node:url';
import {spawn,execFileSync} from 'node:child_process';
import {readFile} from 'node:fs/promises';
import path from 'node:path';
import {installTerminalBrowserBridge} from '../omp-browser.mjs';
import {profileDirectory} from '../omp-browser-service.mjs';
import {TestNativeHost} from './native-host.mjs';

// Loaded as an explicit extension by the real OMP CLI. Chrome is closed the way a user closes it
// (WM_CLOSE, then a hard kill for app restart) and the same OMP session must be able to reopen it.
export default async function relaunchScenario(pi) {
  const root=process.env.BROWSER_TEST_ROOT;
  const host=new TestNativeHost(root);
  const captured={};
  const originalInfos=pi.pi.AgentSession.prototype.getAllToolInfos;
  function captureInfos(...args){captured.session=this;return originalInfos.apply(this,args);}
  pi.pi.AgentSession.prototype.getAllToolInfos=captureInfos;
  let helper,sequence=0,buffered='',pending=new Map();
  async function startHelper(){
    const child=spawn(process.env.BUN_BIN||'bun',[fileURLToPath(new URL('../omp-browser-service.mjs',import.meta.url)),root,process.env.OMP_SDK_DIR],{env:process.env,stdio:['pipe','pipe','inherit']});
    host.attach(child);
    helper=child;buffered='';pending=new Map();
    const ready=new Promise((resolve,reject)=>{child.once('error',reject);child.once('exit',()=>reject(new Error('Browser helper exited')));child.stdout.on('data',chunk=>{
      buffered+=chunk;
      for(;;){
        const end=buffered.indexOf('\n');if(end<0)break;
        const frame=JSON.parse(buffered.slice(0,end));buffered=buffered.slice(end+1);
        if(frame.type==='native_request')host.serve(child,frame);
        if(frame.type==='ready')resolve();
        if(frame.type==='response'){const call=pending.get(frame.id);pending.delete(frame.id);if(frame.ok)call?.resolve(frame.data);else call?.reject(new Error(frame.error));}
      }
    });});
    await ready;
    await control('configure',{headless:false,embedded:false});
    await control('register',{instanceId:'terminal',generation:1});
  }
  function control(action,payload={}){
    const id=String(++sequence);
    return new Promise((resolve,reject)=>{pending.set(id,{resolve,reject});helper.stdin.write(JSON.stringify({id,action,payload})+'\n');});
  }
  const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
  // Absolute Windows tool paths: a writable PATH entry must not substitute these.
  const windowsTool=relative=>path.win32.join(process.env.SystemRoot??'C:\\Windows','System32',relative);
  async function until(predicate,message){
    for(let i=0;i<100;i++){
      const value=await predicate();
      if(value){return value;}
      await sleep(100);
    }
    throw new Error(message);
  }
  async function pages(){
    const port=(await readFile(path.join(profileDirectory(root,'terminal'),'DevToolsActivePort'),'utf8')).split('\n')[0];
    return (await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()).filter(target=>target.type==='page').map(target=>target.url);
  }
  await startHelper();
  const disposeBrowser=installTerminalBrowserBridge(pi,async(action,payload)=>{
    if(action==='browser_prepare')return control('ensureTab',{instanceId:'terminal',generation:1,name:payload.name});
    if(action==='browser_close_tab')return control('closeTab',{instanceId:'terminal',generation:1,name:payload.name});
    if(action==='browser_close_all')return control('closeTab',{instanceId:'terminal',generation:1,name:null});
    if(action==='browser_tabs')return control('listTabs',{instanceId:'terminal',generation:1});
    throw new Error('unexpected browser action '+action);
  });
  pi.on('session_shutdown',(_event,ctx)=>{if(ctx.agent?.kind==='main')disposeBrowser();});
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
      const open='var tab=await browser.open({name:"browser",url:"about:blank",headed:true,persist:true});await tab.goto("data:text/html,<title>marker</title>remembered");print("FLOW:"+JSON.stringify({url:await tab.url(),names:(await browser.tabs()).map(t=>t.name)}));';
      const first=await cell(open);
      assert.deepEqual(first.names,['browser']);
      const [{pid}]=await control('snapshot');
      // Closing Chrome the way a user does must not leave its tabs to be restored on the next open.
      // .NET MainWindowHandle can pick Chrome's background owner window after --no-startup-window.
      // Close the visible browser frame belonging to this owned root, never an arbitrary Chrome window.
      execFileSync(windowsTool('WindowsPowerShell\\v1.0\\powershell.exe'),['-NoProfile','-Command',`
Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class OwnedChromeWindow {
  public delegate bool Visitor(IntPtr hwnd, IntPtr state);
  [DllImport("user32.dll")] public static extern bool EnumWindows(Visitor visitor, IntPtr state);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr hwnd, StringBuilder name, int size);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hwnd, uint message, IntPtr w, IntPtr l);
}
'@
$script:closed=0
[OwnedChromeWindow]::EnumWindows({
  param($hwnd,$state)
  [uint32]$ownerPid=0
  $null=[OwnedChromeWindow]::GetWindowThreadProcessId($hwnd,[ref]$ownerPid)
  $class=New-Object System.Text.StringBuilder 256
  $null=[OwnedChromeWindow]::GetClassName($hwnd,$class,256)
  if ($ownerPid -eq ${pid} -and [OwnedChromeWindow]::IsWindowVisible($hwnd) -and $class.ToString() -eq 'Chrome_WidgetWin_1') {
    if (![OwnedChromeWindow]::PostMessage($hwnd,0x0010,[IntPtr]::Zero,[IntPtr]::Zero)) { throw 'Could not close owned Chrome frame' }
    $script:closed++
  }
  return $true
},[IntPtr]::Zero) | Out-Null
if (!$script:closed) { throw 'Owned Chrome has no visible browser frame' }
`]);
      await until(async()=>(await control('snapshot'))[0].state==='closed','Closing Chrome did not mark the browser closed');
      const reopened=await cell(open);
      assert.deepEqual(reopened.names,['browser']);
      assert.deepEqual(await pages(),['data:text/html,<title>marker</title>remembered'],'Reopened Chrome must contain only the AI tab');
      // App restart: the helper and Chrome are killed together, then a new helper owns the same terminal id.
      const [{pid:second}]=await control('snapshot');
      execFileSync(windowsTool('taskkill.exe'),['/PID',String(helper.pid),'/T','/F'],{stdio:'ignore'});
      // The supervisor retires every browser job of a helper run that ended; the next run starts over the same profile.
      await host.waitForExit(second);
      await startHelper();
      const restarted=await cell(open);
      assert.deepEqual(restarted.names,['browser']);
      assert.deepEqual(await pages(),['data:text/html,<title>marker</title>remembered'],'Chrome restored tabs from the previous app run');
      assert.notEqual(second,(await control('snapshot'))[0].pid);
      console.log('Closed Chrome reopens with only the AI tab, also after an app restart');
    } catch(error) {console.error(error);exitCode=1;}
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
