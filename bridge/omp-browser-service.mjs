import { createHash, randomBytes } from 'node:crypto';
import { mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { spawn } from 'node:child_process';

export const MAX_FRAME = 64 * 1024 * 1024;
const closedTab = 'Browser tab is closed; reopen it with browser.open';
const tabTimeout = 'Browser did not respond while opening the tab and was closed; run browser.open again';
const timeout = (promise, ms, message) => {
  let timer;
  return Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(message)), ms); })]).finally(() => clearTimeout(timer));
};
export function profileDirectory(root, id) {
  return path.join(root, 'terminal-browsers', createHash('sha256').update(id, 'utf8').digest('hex'), 'profile');
}
// Cookies and logins stay in the terminal's profile, but open pages must not: a closed browser starts empty.
// The supervisor also kills Chrome on app/terminal stop; avoid Chrome's crash-restore prompt for that expected exit.
async function resetSession(userDataDir) {
  const defaults = path.join(userDataDir, 'Default');
  await Promise.all(['Sessions', 'Session Storage', 'Current Session', 'Current Tabs', 'Last Session', 'Last Tabs']
    .map(name => rm(path.join(defaults, name), { recursive: true, force: true, maxRetries: 20, retryDelay: 100 })));
  const file = path.join(defaults, 'Preferences');
  let preferences;
  try { preferences = JSON.parse(await readFile(file, 'utf8')); } catch { return; }
  if (!preferences || typeof preferences !== 'object' || Array.isArray(preferences)) return;
  preferences.profile = { ...(preferences.profile ?? {}), exit_type: 'Normal', exited_cleanly: true };
  await writeFile(file, JSON.stringify(preferences));
}
const killTree = process => process && process.exitCode === null && new Promise(resolve => {
  if (globalThis.process.platform !== 'win32') { process.kill('SIGKILL'); resolve(); return; }
  const killer = spawn('taskkill', ['/PID', String(process.pid), '/T', '/F'], { windowsHide: true, stdio: 'ignore' });
  killer.once('error', resolve); killer.once('exit', resolve);
});

export class BrowserService {
  constructor(root, sdkDir, emit = () => {}) {
    this.root = root; this.sdkDir = sdkDir; this.emit = emit;
    this.settings = { headless: false, embedded: true };
    this.owners = new Map(); this.capabilities = new Map(); this.epoch = Date.now();
    this.server = null; this.stopping = false;
  }
  async start() {
    this.server = Bun.serve({ hostname: '::1', port: 0,
      fetch: (req, server) => this.fetch(req, server),
      websocket: {
        open: ws => ws.data.proxy.open(ws),
        message: (ws, data) => ws.data.proxy.receive(data),
        close: ws => ws.data.proxy.close(),
        maxPayloadLength: MAX_FRAME,
      },
    });
  }
  owner(id, generation) {
    const owner = this.owners.get(id);
    if (!owner || !owner.active || owner.generation !== generation || this.stopping) throw new Error('stale_generation');
    return owner;
  }
  // A human "Browser" request docks one session's browser without touching the global Embedded preference.
  embedded(owner) { return owner.forceVisible || this.settings.embedded; }
  row(owner) {
    return { instanceId: owner.id, generation: owner.generation, epoch: owner.epoch, state: owner.state,
      headless: owner.headless, embedded: this.embedded(owner), pid: owner.browser?.process()?.pid ?? null, error: owner.error ?? null };
  }
  state(owner, state, error = null) {
    owner.state = state; owner.error = error;
    this.emit(this.row(owner));
  }
  async dispatch(action, p = {}) {
    if (this.stopping && action !== 'shutdown') throw new Error('browser service closed');
    if (action === 'configure') {
      if (typeof p.headless !== 'boolean' || typeof p.embedded !== 'boolean') throw new Error('invalid browser settings');
      this.settings = { headless: p.headless, embedded: p.embedded };
      // An explicit global change supersedes any per-session "Browser" request; running browsers keep their mode.
      for (const o of this.owners.values()) if (o.active) { o.forceVisible = false; this.state(o, o.state, o.error ?? null); }
      return null;
    }
    if (action === 'register') {
      if (typeof p.instanceId !== 'string' || !p.instanceId || !Number.isSafeInteger(p.generation) || p.generation < 0) throw new Error('invalid browser owner');
      const previous = this.owners.get(p.instanceId);
      if (previous?.active && previous.generation === p.generation) return null;
      if (previous) { previous.active = false; await this.closeOwner(previous); }
      this.owners.set(p.instanceId, { id: p.instanceId, generation: p.generation, active: true, epoch: 0, forceVisible: false,
        headless: this.settings.headless, state: 'closed', tabs: new Map(), pendingTabs: new Map(), browser: null, launching: null, closing: null });
      return null;
    }
    if (action === 'snapshot') return [...this.owners.values()].filter(o => o.active).map(o => this.row(o));
    if (action === 'shutdown') {
      this.stopping = true;
      const owners = [...this.owners.values()]; for (const owner of owners) owner.active = false;
      await Promise.all(owners.map(o => this.closeOwner(o)));
      this.server?.stop(true); return null;
    }
    const owner = this.owner(p.instanceId, p.generation);
    if (action === 'revoke') { owner.active = false; await this.closeOwner(owner); return null; }
    if (action === 'closeTab') {
      const tabs = p.name === null ? [...owner.tabs.values()] : [owner.tabs.get(p.name)].filter(Boolean);
      await Promise.all(tabs.map(async tab => { this.invalidateTab(owner, tab); await tab.page.close().catch(() => {}); }));
      return null;
    }
    // Ticket-free discovery for the adapter: never launches a browser, binds hand-opened pages as tab-N.
    if (action === 'listTabs') return owner.browser?.connected && !owner.closing ? this.listTabs(owner) : [];
    if (action === 'openVisible') {
      if (typeof p.restartHeadless !== 'boolean') throw new Error('invalid browser visibility request');
      await owner.closing; await owner.launching?.catch(() => {});
      this.owner(owner.id, owner.generation);
      if (owner.browser?.connected && owner.headless) {
        // Only this explicitly consented path may discard a running browser's tabs.
        if (!p.restartHeadless) throw new Error('browser_headless_restart_required');
        await this.closeOwner(owner); this.owner(owner.id, owner.generation);
      }
      const previous = owner.forceVisible;
      owner.forceVisible = true;
      try { await this.ensureBrowser(owner); this.owner(owner.id, owner.generation); }
      catch (error) { owner.forceVisible = previous; throw error; }
      this.state(owner, owner.state, owner.error ?? null);
      return this.row(owner);
    }
    if (p.target !== undefined && (typeof p.target !== 'string' || !p.target || p.target.length > 512)) throw new Error('invalid browser tab target');
    if (typeof p.name !== 'string' || !p.name) throw new Error('browser tab name is required');
    if (action !== 'ensureTab') throw new Error('unknown browser service action');
    if (owner.closing) throw new Error('Browser is closing; reopen it with browser.open');
    await this.ensureBrowser(owner);
    this.owner(owner.id, owner.generation);
    let tab = owner.tabs.get(p.name);
    if (!tab || tab.page.isClosed()) {
      let pending = owner.pendingTabs.get(p.name);
      if (!pending) {
        const browser = owner.browser;
        pending = timeout(this.exclusive(owner, async () => {
          let page;
          if (p.target !== undefined) {
            const hit = await this.pageByTarget(owner, p.target);
            // Renaming an automatically attached tab: the page stays open under the requested name.
            if (hit.tab) this.invalidateTab(owner, hit.tab);
            page = hit.page;
          } else {
            const mayAdopt = !owner.adoptedBlank;
            owner.adoptedBlank = true;
            const initial = mayAdopt ? await browser.pages() : [];
            page = initial.length === 1 && initial[0].url() === 'about:blank' ? initial[0] : await browser.newPage();
          }
          if (!owner.active || owner.browser !== browser || owner.closing) { if (p.target === undefined) await page.close(); throw new Error('stale_generation'); }
          return this.bindPage(owner, p.name, page, false);
        }), 20000, tabTimeout).catch(async error => {
          if (error.message === tabTimeout) await this.terminate(browser);
          throw error;
        }).finally(() => owner.pendingTabs.delete(p.name));
        owner.pendingTabs.set(p.name, pending);
      }
      tab = await pending;
    }
    if (!owner.active || owner.tabs.get(p.name) !== tab) throw new Error(closedTab);
    // browserURL resolves /json/version against the origin, discarding paths.
    return { epoch:owner.epoch, tabEpoch:tab.epoch, cdpUrl:`http://${tab.capability}.localhost:${this.server.port}` };
  }
  // Page creation and tab binding are serialized so a hand-opened tab is never bound twice.
  exclusive(owner, work) {
    const run = (owner.exclusive ?? Promise.resolve()).then(work);
    owner.exclusive = run.catch(() => {});
    return run;
  }
  async bindPage(owner, name, page, auto) {
    const session = await page.createCDPSession();
    const { targetInfo } = await session.send('Target.getTargetInfo'); await session.detach();
    const parentSession = page._client().parentSession();
    const {targetInfo:containerInfo} = await parentSession.send('Target.getTargetInfo');
    const tab = { name, auto, targetId:targetInfo.targetId, containerId:containerInfo.type==='tab'?containerInfo.targetId:null, page, epoch:++this.epoch, capability:randomBytes(24).toString('hex'), sockets:new Set(), owner };
    owner.tabs.set(name, tab); this.capabilities.set(tab.capability, tab);
    page.once('close', () => this.invalidateTab(owner, tab));
    return tab;
  }
  // Every tab in this terminal's Chrome belongs to its agent session: tabs the user opens by hand (or that
  // links open) are bound as tab-N the next time the agent looks.
  syncPages(owner) {
    return this.exclusive(owner, async () => {
      const bound = new Set([...owner.tabs.values()].map(tab => tab.page));
      for (const page of await owner.browser.pages()) {
        if (page.isClosed() || page.url().startsWith('devtools://') || bound.has(page)) continue;
        let name;
        do { name = `tab-${owner.autoTabs = (owner.autoTabs ?? 0) + 1}`; } while (owner.tabs.has(name));
        try { await this.bindPage(owner, name, page, true); } catch (error) { if (!page.isClosed()) throw error; }
      }
    });
  }
  async describePages(owner) {
    const bound = new Map([...owner.tabs.values()].map(tab => [tab.page, tab]));
    const pages = (await owner.browser.pages()).filter(page => !page.isClosed() && !page.url().startsWith('devtools://'));
    return Promise.all(pages.map(async page => {
      const [title, active] = await Promise.all([
        timeout(page.title(), 2000, 'title timed out').catch(() => ''),
        timeout(page.evaluate(() => document.visibilityState === 'visible'), 2000, 'visibility timed out').catch(() => false),
      ]);
      return { page, tab: bound.get(page) ?? null, url: page.url(), title, active };
    }));
  }
  async pageByTarget(owner, target) {
    const needle = target.toLowerCase();
    const rows = await this.describePages(owner);
    const hits = rows.filter(row => row.url.toLowerCase().includes(needle) || row.title.toLowerCase().includes(needle));
    const hit = hits.find(row => !row.tab || row.tab.auto);
    if (hit) return hit;
    if (hits.length) throw new Error(`The matching page is already attached as tab ${JSON.stringify(hits[0].tab.name)}`);
    throw new Error(`No browser page matched ${JSON.stringify(target)}. Available pages:\n${rows.map(row => `- ${row.title || '(untitled)'}  ${row.url}`).join('\n') || '(none)'}`);
  }
  async listTabs(owner) {
    await this.syncPages(owner);
    return (await this.describePages(owner)).filter(row => row.tab).map(({ page, tab, ...row }) => ({
      ...row, name: tab.name, auto: tab.auto, epoch: owner.epoch, tabEpoch: tab.epoch, cdpUrl: `http://${tab.capability}.localhost:${this.server.port}`,
    }));
  }
  async ensureBrowser(owner) {
    if (owner.browser?.connected) return;
    if (!owner.launching) owner.launching = (async () => {
      owner.headless = owner.forceVisible ? false : this.settings.headless; owner.epoch = ++this.epoch; this.state(owner, 'opening');
      let browser;
      try {
        this.helpers ??= import(pathToFileURL(path.join(this.sdkDir, 'src/tools/browser/launch.ts')).href);
        const {loadPuppeteer, ensureChromiumExecutable} = await this.helpers;
        const puppeteer = await loadPuppeteer();
        const executablePath = await ensureChromiumExecutable();
        // A browser the user just closed may still be flushing its profile; a new Chrome would hand off to it and exit.
        await owner.exiting;
        const userDataDir = profileDirectory(this.root, owner.id); await mkdir(userDataDir, { recursive:true }); await resetSession(userDataDir);
        browser = await puppeteer.launch({ executablePath, userDataDir, headless:owner.headless,
          defaultViewport:null, timeout:20000, protocolTimeout:8000,
          handleSIGINT:false, handleSIGTERM:false, handleSIGHUP:false,
          args:['--remote-debugging-address=127.0.0.1','--remote-debugging-port=0','--no-first-run',
            ...(this.embedded(owner) && !owner.headless ? ['--window-position=-32000,-32000'] : [])],
        });
        if (!owner.active || owner.closing || this.stopping) { await this.terminate(browser); throw new Error('stale_generation'); }
        owner.browser = browser;
        owner.adoptedBlank = false;
        owner.control = await browser.target().createCDPSession();
        owner.version = await owner.control.send('Browser.getVersion');
        owner.browserTargetId = (await owner.control.send('Target.getTargetInfo')).targetInfo.targetId;
        browser.once('disconnected', () => {
          if (owner.browser !== browser) return;
          for (const tab of [...owner.tabs.values()]) this.invalidateTab(owner, tab);
          owner.browser = null;
          owner.exiting = this.reap(browser);
          this.state(owner, 'closed');
        });
        this.state(owner, 'ready');
      } catch (error) {
        if (browser && owner.browser !== browser) await this.terminate(browser);
        this.state(owner, 'error', String(error.message ?? error).replace(/(?:ws|http):\/\/127\.0\.0\.1:[^\s]+/g, '[browser endpoint]'));
        throw new Error(owner.error);
      }
    })().finally(() => { owner.launching = null; });
    return owner.launching;
  }
  invalidateTab(owner, tab) {
    if (owner.tabs.get(tab.name) === tab) owner.tabs.delete(tab.name);
    this.capabilities.delete(tab.capability);
    for (const proxy of [...tab.sockets]) proxy.close();
  }
  async reap(browser) {
    const process = browser.process();
    if (!process || process.exitCode !== null) return;
    await timeout(new Promise(resolve => process.once('exit', resolve)), 5000, 'browser exit timed out').catch(() => killTree(process));
  }
  async terminate(browser) {
    try { await timeout(browser.close(), 5000, 'browser close timed out'); }
    catch { await killTree(browser.process()); }
  }
  async closeOwner(owner) {
    if (owner.closing) return owner.closing;
    owner.closing = (async () => {
      for (const tab of [...owner.tabs.values()]) this.invalidateTab(owner, tab);
      await owner.launching?.catch(() => {});
      await Promise.allSettled([...owner.pendingTabs.values()]);
      if (owner.browser) await this.terminate(owner.browser);
      owner.browser = null; this.state(owner, 'closed');
    })().finally(() => { owner.closing = null; });
    return owner.closing;
  }
  fetch(req, server) {
    const remote = server.requestIP(req)?.address;
    if (remote !== '127.0.0.1' && remote !== '::ffff:127.0.0.1' && remote !== '::1') return new Response('Forbidden',{status:403});
    const url = new URL(req.url);
    const match = /^\/cdp\/([a-f0-9]{48})\/(\d+)\/(\d+)\/devtools\/browser$/.exec(url.pathname);
    const hostCapability = /^([a-f0-9]{48})\.localhost$/.exec(url.hostname)?.[1];
    const discovery = (url.pathname === '/json/version' || url.pathname === '/json/list') && hostCapability;
    const tab = this.capabilities.get(discovery ? hostCapability : match?.[1]);
    if (!tab) return new Response('Forbidden',{status:403});
    const owner = tab.owner;
    if (!owner.active || !owner.browser?.connected || (!discovery && (String(owner.epoch)!==match[2] || String(tab.epoch)!==match[3]))) return new Response('Browser target is closed',{status:410});
    if (url.pathname === '/json/list') return this.listTabs(owner).then(rows => Response.json(rows), error => new Response(String(error.message ?? error), {status:502}));
    const endpoint = `/cdp/${tab.capability}/${owner.epoch}/${tab.epoch}/devtools/browser`;
    if (discovery) return Response.json({Browser:owner.version.product,'Protocol-Version':owner.version.protocolVersion,'User-Agent':owner.version.userAgent,'V8-Version':owner.version.jsVersion,webSocketDebuggerUrl:`ws://[::1]:${server.port}${endpoint}`});
    const proxy = new ScopedCdpProxy(tab);
    if (server.upgrade(req, {data:{proxy}})) return;
    return new Response('WebSocket upgrade required',{status:400});
  }
}

// Every connection sees one named page plus its own descendants, never peer pages.
class ScopedCdpProxy {
  constructor(tab) { this.tab=tab; this.allowed=new Set([tab.targetId,tab.owner.browserTargetId,...(tab.containerId?[tab.containerId]:[])]); this.frames=new Set([tab.targetId]);this.downloadGuids=new Set();this.sessions=new Map(); this.pending=new Map(); this.internalPending=new Map(); this.downstreamIds=new Set(); this.nextId=0; this.closed=false; this.creating=0;this.delayed=[]; }
  open(socket) {
    this.socket=socket; this.tab.sockets.add(this);
    this.upstream=new WebSocket(this.tab.owner.browser.wsEndpoint());
    this.opened = new Promise((resolve,reject) => { this.upstream.onopen=resolve; this.upstream.onerror=()=>reject(new Error('CDP connection failed')); });
    this.opened.catch(()=>this.close());
    this.upstream.onmessage=event=>{try{this.forward(JSON.parse(event.data));}catch{this.close();}};
    this.upstream.onclose=()=>this.close();
  }
  send(value) { if (!this.closed) this.socket.send(JSON.stringify(value)); }
  internal(method,params={},sessionId) {
    if(this.internalPending.size+this.pending.size>=1024)return Promise.reject(new Error('Too many pending CDP commands'));
    const id=++this.nextId;
    const result=new Promise((resolve,reject)=>{this.internalPending.set(id,{resolve,reject});this.upstream.send(JSON.stringify({id,method,params,...(sessionId?{sessionId}:{})}));});
    return timeout(result,10000,'CDP cleanup timed out').finally(()=>this.internalPending.delete(id));
  }
  permits(info,parentSession) {
    if (!info) return false;
    if (this.allowed.has(info.targetId)) return true;
    if ((info.openerId && this.allowed.has(info.openerId)) || (parentSession && this.sessions.has(parentSession) && this.sessions.get(parentSession)!==this.tab.owner.browserTargetId && ['iframe','worker','service_worker','shared_worker'].includes(info.type))) {
      this.allowed.add(info.targetId); return true;
    }
    return false;
  }
  async receive(data) {
    let message; let reserved=false;
    try {
      if (Buffer.byteLength(data)>MAX_FRAME) throw new Error('CDP frame exceeds transport limit');
      message=JSON.parse(String(data));
      if (!Number.isSafeInteger(message.id) || typeof message.method!=='string' || this.downstreamIds.has(message.id)) throw new Error('Invalid CDP request');
      if (this.downstreamIds.size>=1024) throw new Error('Too many pending CDP requests');
      this.downstreamIds.add(message.id);reserved=true;
      await this.opened;
      if(this.closed)return;
      const {method,params={},sessionId}=message;
      if (sessionId && !this.sessions.has(sessionId)) throw new Error('CDP session is outside this named tab');
      for(const key of ['targetId','openerId']) if(params[key] && !this.allowed.has(params[key])) throw new Error('CDP target is outside this named tab');
      if (params.sessionId && !this.sessions.has(params.sessionId)) throw new Error('CDP session is outside this named tab');
      if (params.targetId===this.tab.owner.browserTargetId && !['Target.attachToTarget','Target.getTargetInfo'].includes(method)) throw new Error('CDP browser target cannot be controlled through this named tab');
      if (['Target.sendMessageToTarget','Target.setRemoteLocations','Target.createBrowserContext','Target.disposeBrowserContext'].includes(method) || (method.startsWith('Browser.') && !['Browser.getVersion','Browser.getWindowForTarget','Browser.setDownloadBehavior'].includes(method))) throw new Error('CDP operation is outside this named tab');
      const browserScope = !sessionId || this.sessions.get(sessionId)===this.tab.owner.browserTargetId;
      if (browserScope && !['Target.getTargets','Target.getTargetInfo','Target.attachToTarget','Target.attachToBrowserTarget','Target.detachFromTarget','Target.setDiscoverTargets','Target.setAutoAttach','Target.createTarget','Target.closeTarget','Target.activateTarget','Target.getBrowserContexts','Browser.getVersion','Browser.getWindowForTarget','Browser.setDownloadBehavior','Schema.getDomains'].includes(method)) throw new Error('CDP browser operation is outside this named tab');
      if (method==='Target.getTargetInfo' && !params.targetId && !sessionId) { message={...message,params:{...params,targetId:this.tab.targetId}}; }
      if (method==='Browser.getWindowForTarget' && !params.targetId) message={...message,params:{...params,targetId:this.tab.targetId}};
      if (method==='Target.createTarget' && (params.browserContextId || params.newWindow)) throw new Error('CDP new window/context is outside this named tab');
      const id=++this.nextId;
      this.pending.set(id,{...message});this.downstreamIds.add(message.id);
      if(method==='Target.createTarget')this.creating++;
      this.upstream.send(JSON.stringify({...message,id}));
    } catch(error) { if(reserved)this.downstreamIds.delete(message.id);if(message?.id!==undefined)this.send({id:message.id,error:{code:-32000,message:String(error.message??error)}});else this.close(); }
  }
  forward(message) {
    if (this.closed) return;
    if (message.id!==undefined) {
      const internal=this.internalPending.get(message.id);
      if(internal){this.internalPending.delete(message.id);if(message.error)internal.reject(new Error(message.error.message));else internal.resolve(message.result);return;}
      const request=this.pending.get(message.id);if(!request)return;
      this.pending.delete(message.id);this.downstreamIds.delete(request.id);
      if (request.method==='Target.getTargets' && message.result) message.result.targetInfos=message.result.targetInfos.filter(info=>this.permits(info,request.sessionId));
      if (request.method==='Page.getFrameTree' && message.result?.frameTree) {
        const collect=tree=>{this.frames.add(tree.frame.id);for(const child of tree.childFrames??[])collect(child);};collect(message.result.frameTree);
      }
      if (request.method==='Target.getTargetInfo' && message.result && !this.permits(message.result.targetInfo,request.sessionId)) message={error:{code:-32000,message:'CDP target is outside this named tab'}};
      if (request.method==='Target.createTarget') {
        void (async()=>{
          try {
            if(message.result?.targetId){
              const target=await this.tab.owner.browser.waitForTarget(target=>target._targetId===message.result.targetId,{timeout:10000});
              const page=await target.page();if(!page)throw new Error('Created CDP target has no page');
              const parent=await page._client().parentSession().send('Target.getTargetInfo');
              this.allowed.add(message.result.targetId);
              if(parent.targetInfo.type==='tab')this.allowed.add(parent.targetInfo.targetId);
            }
          }catch{message={error:{code:-32000,message:'Could not bind the newly created browser target'}};}
          finally{
            this.creating--;
            if(!this.creating){const delayed=this.delayed;this.delayed=[];for(const event of delayed)this.forward(event);}
            this.send({...message,id:request.id});
          }
        })().catch(()=>this.close());
        return;
      }
      if (request.method==='Target.attachToBrowserTarget' && message.result?.sessionId) this.sessions.set(message.result.sessionId,this.tab.owner.browserTargetId);
      if (request.method==='Target.attachToTarget' && message.result?.sessionId) this.sessions.set(message.result.sessionId,request.params.targetId);
      this.send({...message,id:request.id});return;
    }
    const {method,params={},sessionId}=message;
    if(this.creating && method.startsWith('Target.')){
      if(method==='Target.attachedToTarget'&&params.waitingForDebugger)void this.internal('Runtime.runIfWaitingForDebugger',{},params.sessionId).catch(()=>this.close());
      if(this.delayed.length>=1024){this.close();return;}
      this.delayed.push(message);return;
    }
    if (method==='Target.attachedToTarget') {
      if (this.permits(params.targetInfo,sessionId)) this.sessions.set(params.sessionId,params.targetInfo.targetId);
      else {
        // Detaching before Runtime acknowledges resume can leave another page paused.
        void (async()=>{
          if(params.waitingForDebugger)await this.internal('Runtime.runIfWaitingForDebugger',{},params.sessionId);
          await this.internal('Target.detachFromTarget',{sessionId:params.sessionId},sessionId);
        })().catch(()=>this.close());
        return;
      }
    } else if (method==='Target.detachedFromTarget') {
      if(!this.sessions.has(params.sessionId))return;this.sessions.delete(params.sessionId);
    } else if (params.targetInfo && !this.permits(params.targetInfo,sessionId)) return;
    else if (params.targetId && !this.allowed.has(params.targetId)) return;
    if (sessionId && !this.sessions.has(sessionId)) return;
    if(method==='Browser.downloadWillBegin'){
      if(!this.frames.has(params.frameId))return;
      this.downloadGuids.add(params.guid);
    }else if(method==='Browser.downloadProgress'){
      if(!this.downloadGuids.has(params.guid))return;
      if(params.state==='completed'||params.state==='canceled')this.downloadGuids.delete(params.guid);
    }
    if(method==='Page.frameNavigated'&&params.frame)this.frames.add(params.frame.id);
    if(method==='Page.frameAttached')this.frames.add(params.frameId);
    if(method==='Page.frameDetached')this.frames.delete(params.frameId);
    if (method==='Target.targetDestroyed') this.allowed.delete(params.targetId);
    this.send(message);
  }
  close() {
    if(this.closed)return;this.closed=true;
    this.tab.sockets.delete(this);this.pending.clear();this.downstreamIds.clear();
    for(const call of this.internalPending.values())call.reject(new Error('CDP disconnected'));
    this.internalPending.clear();this.delayed=[];
    this.socket?.close();this.upstream?.close();
  }
}

if (import.meta.main) {
  const [root,sdkDir]=process.argv.slice(2);
  if(!root||!sdkDir)throw new Error('browser service requires app root and SDK directory');
  // SDK cache initialization is private to this helper, not the real ~/.omp.
  const runtime=path.join(root,'terminal-browsers','runtime');await mkdir(runtime,{recursive:true});
  process.env.USERPROFILE=runtime;process.env.HOME=runtime;process.env.PI_CODING_AGENT_DIR=path.join(runtime,'agent');
  console.log=(...args)=>console.error(...args);
  const output=value=>process.stdout.write(JSON.stringify(value)+'\n');
  // The supervisor keeps this stream in terminal-browsers/browser-service.log for post-mortems.
  const log=(...parts)=>process.stderr.write(`${new Date().toISOString()} ${parts.join(' ')}\n`);
  const service=new BrowserService(root,sdkDir,state=>{log('state',state.instanceId,state.state,`pid=${state.pid}`,state.error??'');output({type:'state',data:state});});
  await service.start();output({type:'ready'});
  let chunks=[];let bytes=0;
  try {
    for await (const chunk of process.stdin) {
      let offset=0;
      while(offset<chunk.length){
        const newline=chunk.indexOf(10,offset);const end=newline<0?chunk.length:newline;
        const part=chunk.subarray(offset,end);bytes+=part.length;if(bytes>MAX_FRAME)throw new Error('browser frame exceeds transport limit');chunks.push(part);offset=end+1;
        if(newline<0)break;
        const frame=JSON.parse(Buffer.concat(chunks,bytes).toString('utf8'));chunks=[];bytes=0;
        if(typeof frame.id!=='string'||typeof frame.action!=='string')throw new Error('invalid browser frame');
        const started=Date.now();
        void service.dispatch(frame.action,frame.payload).then(data=>{log(frame.action,'ok',`${Date.now()-started}ms`);output({type:'response',id:frame.id,ok:true,data});},error=>{log(frame.action,'failed',`${Date.now()-started}ms`,String(error.message??error));output({type:'response',id:frame.id,ok:false,error:String(error.message??error)});});
      }
    }
  } catch (error) { log('stdin loop failed',String(error?.stack??error)); process.exitCode=1; }
  log('shutting down');
  await service.dispatch('shutdown').catch(()=>{});
}
