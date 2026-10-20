import { createHash, randomBytes, randomUUID } from 'node:crypto';
import { mkdir, stat } from 'node:fs/promises';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

export const MAX_FRAME = 64 * 1024 * 1024;
// One budget for spawn -> debugging endpoint -> CDP connect -> initialization, inside the supervisor's 30 s request bound.
export const LAUNCH_DEADLINE_MS = 20000;
const closedTab = 'Browser tab is closed; reopen it with browser.open';
const tabTimeout = 'Browser did not respond while opening the tab and was closed; run browser.open again';
const timeout = (promise, ms, message) => {
  let timer;
  return Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(message)), ms); })]).finally(() => clearTimeout(timer));
};

/** Failure with a stable machine-readable code; the message always starts with the code so it survives plain-text relays. */
export class BrowserError extends Error {
  constructor(code, message) {
    super(message.startsWith(code) ? message : `${code}: ${message}`);
    this.code = code;
  }
}

export function profileDirectory(root, id) {
  return path.join(root, 'terminal-browsers', createHash('sha256').update(id, 'utf8').digest('hex'), 'profile');
}

/** The installed Windows locations the SDK probes, in its order. Nothing is downloaded or executed. */
export function windowsChromiumCandidates(env = process.env) {
  const programFiles = env.ProgramFiles ?? 'C:\\Program Files';
  const programFilesX86 = env['ProgramFiles(x86)'] ?? 'C:\\Program Files (x86)';
  const local = env.LOCALAPPDATA;
  const join = path.win32.join;
  return [
    join(programFiles, 'Google\\Chrome\\Application\\chrome.exe'),
    join(programFilesX86, 'Google\\Chrome\\Application\\chrome.exe'),
    ...(local ? [join(local, 'Google\\Chrome\\Application\\chrome.exe')] : []),
    join(programFiles, 'Chromium\\Application\\chrome.exe'),
    ...(local ? [join(local, 'Chromium\\Application\\chrome.exe')] : []),
    join(programFiles, 'Microsoft\\Edge\\Application\\msedge.exe'),
    join(programFilesX86, 'Microsoft\\Edge\\Application\\msedge.exe'),
  ];
}
const isFile = async file => (await stat(file).catch(() => null))?.isFile() === true;

/**
 * Passive installed-only resolution: an explicit absolute PUPPETEER_EXECUTABLE_PATH, then the SDK's installed Windows
 * candidates, then an already existing Puppeteer cache executable. It never installs, downloads or runs a browser.
 */
export async function resolveInstalledChromium({ env = process.env, platform = process.platform, exists = isFile, cached = async () => null } = {}) {
  if (platform !== 'win32') throw new BrowserError('unsupported_platform', 'Owned terminal browsers are supported on Windows only');
  const explicit = env.PUPPETEER_EXECUTABLE_PATH;
  if (explicit) {
    if (!path.win32.isAbsolute(explicit) || !await exists(explicit)) {
      throw new BrowserError('browser_executable_missing', 'PUPPETEER_EXECUTABLE_PATH must be the absolute path of an installed Chrome, Chromium or Edge executable');
    }
    return explicit;
  }
  for (const candidate of windowsChromiumCandidates(env)) if (await exists(candidate)) return candidate;
  const cache = await cached();
  if (cache && await exists(cache)) return cache;
  throw new BrowserError('browser_executable_missing', 'No installed Chrome, Chromium or Edge was found. Install one (nothing is downloaded automatically) or set PUPPETEER_EXECUTABLE_PATH to its absolute path.');
}

/** Request/response channel to the native supervisor over the private helper stdio frames. */
export class NativeChannel {
  constructor(send) { this.send = send; this.pending = new Map(); this.closed = false; }
  request(action, payload) {
    if (this.closed) return Promise.reject(new BrowserError('browser_native_unavailable', 'The native browser supervisor is not connected'));
    const id = randomUUID();
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { this.pending.delete(id); reject(new BrowserError('browser_native_timeout', `${action} timed out`)); }, 60000);
      this.pending.set(id, { resolve, reject, timer });
      try { this.send({ type: 'native_request', id, action, payload }); }
      catch (error) { clearTimeout(timer); this.pending.delete(id); reject(error); }
    });
  }
  receive(frame) {
    const call = this.pending.get(frame.id);
    if (!call) return;
    this.pending.delete(frame.id); clearTimeout(call.timer);
    if (frame.error) call.reject(new BrowserError(String(frame.error.code ?? 'native_error'), String(frame.error.message ?? 'native browser action failed')));
    else call.resolve(frame.result);
  }
  close() {
    this.closed = true;
    for (const call of this.pending.values()) { clearTimeout(call.timer); call.reject(new BrowserError('browser_native_unavailable', 'The native browser supervisor disconnected')); }
    this.pending.clear();
  }
}

// One browser start. It owns the shared deadline and cancellation for spawn -> connect -> initialization, and records
// what native owns for it so exactly that job can be retired no matter where the start stops.
class LaunchOperation {
  constructor(owner, requestId, ms) {
    this.id = randomUUID(); this.instanceId = owner.id; this.generation = owner.generation; this.requestId = requestId;
    this.owned = null; this.browser = null; this.retiring = null; this.retired = false; this.gone = false; this.spawnSent = false;
    this.cancelled = false; this.settled = false; this.reason = null;
    this.aborted = new Promise((_, reject) => { this.reject = reject; });
    this.aborted.catch(() => {});
    this.timer = setTimeout(() => this.abort(new BrowserError('browser_launch_timeout', 'Chrome did not finish starting within 20 seconds and was closed; try Browser again.')), ms);
  }
  abort(error) {
    if (this.settled || this.cancelled) return;
    this.cancelled = true; this.reason = error; this.reject(error);
  }
  settle() { this.settled = true; clearTimeout(this.timer); }
  /** Awaits `promise` unless the operation is cancelled or past its deadline first; a late result is never published. */
  async run(promise) {
    const value = await Promise.race([promise, this.aborted]);
    if (this.cancelled) throw this.reason;
    return value;
  }
}


export class BrowserService {
  constructor(root, sdkDir, emit = () => {}, options = {}) {
    this.root = root; this.sdkDir = sdkDir; this.emit = emit;
    this.native = options.native ?? null;
    this.log = options.log ?? (() => {});
    this.helperSession = options.helperSession ?? randomBytes(16).toString('hex');
    this.resolveExecutable = options.resolveExecutable ?? (() => resolveInstalledChromium({ cached: () => this.cachedChromium() }));
    this.settings = { headless: false, embedded: true };
    this.owners = new Map(); this.capabilities = new Map(); this.epoch = Date.now();
    this.server = null; this.stopping = false;
    // Retirement state lives here, not on replaceable owner objects: a replacement generation must still wait for the
    // previous generation's browser to be gone, and a blocked cleanup must be retried before any new launch.
    this.turns = new Map(); this.exits = new Map(); this.blocked = new Map();
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
      headless: owner.headless, embedded: this.embedded(owner), pid: owner.native?.pid ?? null, error: owner.error ?? null };
  }
  state(owner, state, error = null) {
    owner.state = state; owner.error = error;
    this.emit(this.row(owner));
  }
  // Owner transitions (register / revoke / replacement) of one instance run one at a time; browser actions do not.
  serial(id, work) {
    const run = (this.turns.get(id) ?? Promise.resolve()).then(work);
    const tail = run.catch(() => {});
    this.turns.set(id, tail);
    tail.then(() => { if (this.turns.get(id) === tail) this.turns.delete(id); });
    return run;
  }
  live(owner, browser) { return owner.active && !owner.closing && !this.stopping && owner.browser === browser; }
  nativeCall(action, payload, ms) {
    if (!this.native) return Promise.reject(new BrowserError('browser_native_unavailable', 'Native browser ownership is unavailable'));
    return timeout(this.native.request(action, { helperSession: this.helperSession, ...payload }), ms, `${action} timed out`);
  }
  async dispatch(action, p = {}, requestId = null) {
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
      return this.serial(p.instanceId, async () => {
        if (this.stopping) throw new Error('browser service closed');
        const previous = this.owners.get(p.instanceId);
        if (previous?.active && previous.generation === p.generation) return null;
        // The old generation's browser job must be verified gone before its replacement can exist.
        if (previous) { previous.active = false; await this.closeOwner(previous); }
        if (this.stopping) throw new Error('browser service closed');
        this.owners.set(p.instanceId, { id: p.instanceId, generation: p.generation, active: true, epoch: 0, forceVisible: false,
          headless: this.settings.headless, state: 'closed', tabs: new Map(), pendingTabs: new Map(), browser: null, native: null, op: null, launching: null, closing: null });
        return null;
      });
    }
    if (action === 'snapshot') return [...this.owners.values()].filter(o => o.active).map(o => this.row(o));
    if (action === 'shutdown') {
      this.stopping = true;
      const owners = [...this.owners.values()]; for (const owner of owners) owner.active = false;
      await Promise.allSettled([...this.turns.values()]);
      const results = await Promise.allSettled(owners.map(o => this.closeOwner(o)));
      this.server?.stop(true);
      const failed = results.find(result => result.status === 'rejected');
      if (failed) throw failed.reason;
      return null;
    }
    if (action === 'revoke') {
      return this.serial(p.instanceId, async () => { const owner = this.owner(p.instanceId, p.generation); owner.active = false; await this.closeOwner(owner); return null; });
    }
    const owner = this.owner(p.instanceId, p.generation);
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
      try { await this.ensureBrowser(owner, requestId); this.owner(owner.id, owner.generation); await this.ensureVisiblePage(owner); this.owner(owner.id, owner.generation); }
      catch (error) { owner.forceVisible = previous; throw error; }
      this.state(owner, owner.state, owner.error ?? null);
      return this.row(owner);
    }
    if (p.target !== undefined && (typeof p.target !== 'string' || !p.target || p.target.length > 512)) throw new Error('invalid browser tab target');
    if (typeof p.name !== 'string' || !p.name) throw new Error('browser tab name is required');
    if (action !== 'ensureTab') throw new Error('unknown browser service action');
    if (owner.closing) throw new Error('Browser is closing; reopen it with browser.open');
    await this.ensureBrowser(owner, requestId);
    this.owner(owner.id, owner.generation);
    let tab = owner.tabs.get(p.name);
    if (!tab || tab.page.isClosed()) {
      let pending = owner.pendingTabs.get(p.name);
      if (!pending) {
        const browser = owner.browser;
        const op = owner.op;
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
          if (error.message === tabTimeout && op) await this.terminateOp(op).catch(blocked => this.log('tab-timeout retirement blocked', owner.id, String(blocked.message ?? blocked)));
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
  // Puppeteer's cache layout for an already present browser, computed without installing anything.
  async cachedChromium() {
    try {
      const load = async specifier => import(pathToFileURL(Bun.resolveSync(specifier, this.sdkDir)).href);
      const [browsers, utils, revisions] = await Promise.all([load('@oh-my-pi/pi-utils/browsers'), load('@oh-my-pi/pi-utils'), load('puppeteer-core/internal/revisions.js')]);
      const platform = browsers.detectBrowserPlatform();
      return platform ? browsers.computeExecutablePath({ buildId: revisions.PUPPETEER_REVISIONS.chrome, cacheDir: utils.getPuppeteerDir(), platform }) : null;
    } catch { return null; }
  }
  async browserArguments(puppeteer, owner, userDataDir) {
    // Puppeteer's own defaults (async in puppeteer-core 25), minus its startup URL and anything native admission refuses.
    const defaults = (await puppeteer.defaultArgs({ browser: 'chrome', headless: owner.headless, userDataDir, args: [] }))
      .filter(arg => arg.startsWith('--') && arg !== '--no-sandbox' && !/^--(?:remote-debugging-|user-data-dir=|no-startup-window)/.test(arg));
    return [...defaults, `--user-data-dir=${userDataDir}`, '--remote-debugging-address=127.0.0.1', '--remote-debugging-port=0',
      '--no-startup-window', '--hide-crash-restore-bubble',
      ...(this.embedded(owner) && !owner.headless ? ['--window-position=-32000,-32000'] : [])];
  }
  // One launch per owner: concurrent callers share it. A caller timing out never releases the browser or starts another.
  async ensureBrowser(owner, requestId = null) {
    if (owner.browser?.connected && owner.native) return;
    if (!owner.launching) {
      const op = new LaunchOperation(owner, requestId, LAUNCH_DEADLINE_MS);
      owner.op = op;
      owner.launching = this.launch(owner, op).finally(() => { op.settle(); owner.launching = null; });
    }
    return owner.launching;
  }
  async launch(owner, op) {
    owner.headless = owner.forceVisible ? false : this.settings.headless; owner.epoch = ++this.epoch; this.state(owner, 'opening');
    try {
      // A previous browser of this instance (any generation) must be gone, and a blocked cleanup retried, first.
      await op.run(this.settlePrevious(owner));
      this.helpers ??= import(pathToFileURL(path.join(this.sdkDir, 'src/tools/browser/launch.ts')).href);
      const { loadPuppeteer } = await op.run(this.helpers);
      const puppeteer = await op.run(loadPuppeteer());
      const executable = await op.run(this.resolveExecutable());
      const userDataDir = profileDirectory(this.root, owner.id);
      this.log('launch', owner.id, 'phase=spawn', path.basename(executable));
      const args = await op.run(this.browserArguments(puppeteer, owner, userDataDir));
      op.spawnSent = true;
      const spawned = this.nativeCall('browser_spawn', { sourceRequestId: op.requestId, operationId: op.id, instanceId: owner.id, generation: owner.generation,
        executable, args, profilePath: userDataDir }, LAUNCH_DEADLINE_MS + 5000);
      spawned.catch(() => {});
      op.owned = await op.run(spawned);
      this.log('launch', owner.id, 'phase=spawned', `pid=${op.owned.pid}`, `creation=${op.owned.creationTime100ns}`);
      const connecting = puppeteer.connect({ browserWSEndpoint: op.owned.webSocketUrl, defaultViewport: null, protocolTimeout: 8000 });
      // A connection finishing after the operation ended is disconnected, never published.
      connecting.then(late => { if (op.cancelled && op.browser !== late) Promise.resolve(late.disconnect()).catch(() => {}); }, () => {});
      const browser = await op.run(connecting);
      op.browser = browser;
      browser.once('disconnected', () => this.browserGone(owner, op, browser));
      const control = await op.run(browser.target().createCDPSession());
      const version = await op.run(control.send('Browser.getVersion'));
      const browserTargetId = (await op.run(control.send('Target.getTargetInfo'))).targetInfo.targetId;
      // Pages the profile restored at startup are closed through this owned connection before any tab is bound.
      for (const page of await op.run(browser.pages())) if (!page.isClosed()) await op.run(page.close()).catch(error => { if (op.cancelled) throw error; });
      await op.run(this.nativeCall('browser_ready', { operationId: op.id, instanceId: owner.id, generation: owner.generation, leaseId: op.owned.leaseId }, 10000));
      if (!owner.active || owner.closing || this.stopping || op.cancelled) throw new Error('stale_generation');
      owner.browser = browser;
      owner.native = { pid: op.owned.pid, creationTime100ns: op.owned.creationTime100ns, leaseId: op.owned.leaseId, operationId: op.id };
      owner.adoptedBlank = false; owner.control = control; owner.version = version; owner.browserTargetId = browserTargetId;
      // --no-startup-window may keep Chrome's background root alive after the human closes its last frame.
      // Serialize this check with page creation so a concurrent Browser/open request cannot lose its new page.
      browser.on?.('targetdestroyed', target => {
        if (target.type() !== 'page') return;
        this.exclusive(owner, async () => {
          if (owner.browser !== browser || owner.op !== op || owner.closing || !op.settled) return;
          const pages = await browser.pages();
          if (pages.some(page => !page.isClosed() && !page.url().startsWith('devtools://'))) return;
          await this.terminateOp(op);
        }).catch(error => this.log('last window retirement', owner.id, String(error.message ?? error)));
      });
      op.settle();
      this.log('launch', owner.id, 'phase=ready', String(version.product));
      this.state(owner, 'ready');
    } catch (error) {
      let message = String(error.message ?? error).replace(/(?:wss?|https?):\/\/(?:127\.0\.0\.1|\[::1\]|localhost)[^\s]*/g, '[browser endpoint]');
      op.abort(error);
      try { await this.terminateOp(op); }
      catch (blocked) { message += ` (cleanup is blocked and will be retried: ${String(blocked.message ?? blocked)})`; }
      this.log('launch', owner.id, 'phase=failed', message);
      if (message !== 'stale_generation') this.state(owner, 'error', message);
      else if (owner.active && !owner.closing) this.state(owner, 'closed');
      throw new Error(message);
    }
  }
  async settlePrevious(owner) {
    await this.exits.get(owner.id);
    const blocked = this.blocked.get(owner.id);
    if (blocked) await this.terminateOp(blocked);
  }
  // The browser's connection ended (user closed it, crash, or our own close): publish closed state and retire the
  // job, waiting briefly for a clean root exit so the profile is flushed.
  browserGone(owner, op, browser) {
    if (op.browser !== browser) return;
    op.gone = true;
    if (!op.settled) op.abort(new BrowserError('browser_disconnected', 'Chrome closed while it was starting'));
    if (owner.op === op && owner.browser === browser) {
      for (const tab of [...owner.tabs.values()]) this.invalidateTab(owner, tab);
      owner.browser = null; owner.native = null;
      this.state(owner, 'closed');
    }
    this.trackExit(op.instanceId, this.terminateOp(op));
  }
  invalidateTab(owner, tab) {
    if (owner.tabs.get(tab.name) === tab) owner.tabs.delete(tab.name);
    this.capabilities.delete(tab.capability);
    for (const proxy of [...tab.sockets]) proxy.close();
  }
  trackExit(id, promise) {
    const settled = promise.then(() => {}, error => this.log('retirement blocked', id, String(error.message ?? error)));
    this.exits.set(id, settled);
    settled.then(() => { if (this.exits.get(id) === settled) this.exits.delete(id); });
  }
  // Idempotent and single-flight per operation; waits only for that operation's native job, never for launching/closing.
  terminateOp(op) {
    if (op.retired) return Promise.resolve();
    // Nothing was ever asked of native for this operation: there is no job or lease to retire.
    if (!op.spawnSent && !op.browser) { op.retired = true; return Promise.resolve(); }
    if (op.retiring) return op.retiring;
    // Deferred one microtask: a synchronous `disconnected` emitted by browser.close() re-enters here and must join this promise.
    const run = Promise.resolve().then(async () => {
      let graceful = op.gone;
      const browser = op.browser;
      if (browser) {
        try { await timeout(browser.close(), 3000, 'browser close timed out'); graceful = true; } catch { /* the job is terminated below */ }
        try { await browser.disconnect(); } catch { /* already gone */ }
        op.browser = null;
      }
      await this.nativeCall('browser_retire', { operationId: op.id, instanceId: op.instanceId, generation: op.generation,
        leaseId: op.owned?.leaseId ?? null, graceMs: graceful ? 5000 : 0 }, 25000);
      op.retired = true;
      if (this.blocked.get(op.instanceId) === op) this.blocked.delete(op.instanceId);
    });
    op.retiring = run.catch(error => { op.retiring = null; this.blocked.set(op.instanceId, op); throw error; });
    return op.retiring;
  }
  // Admissions are invalidated, launch and pending tabs joined, then the owned job and any exit in flight are retired.
  closeOwner(owner) {
    if (owner.closing) return owner.closing;
    owner.closing = (async () => {
      for (const tab of [...owner.tabs.values()]) this.invalidateTab(owner, tab);
      owner.op?.abort(new Error('stale_generation'));
      await owner.launching?.catch(() => {});
      await Promise.allSettled([...owner.pendingTabs.values()]);
      await this.exits.get(owner.id);
      try {
        for (const op of new Set([owner.op, this.blocked.get(owner.id)])) if (op) await this.terminateOp(op);
      } catch (error) {
        owner.browser = null; owner.native = null;
        this.state(owner, 'error', `Browser cleanup is blocked: ${String(error.message ?? error)}`);
        throw error;
      }
      owner.browser = null; owner.native = null; this.state(owner, 'closed');
    })().finally(() => { owner.closing = null; });
    return owner.closing;
  }
  // A human "Browser" request must end with a normal page to show: none left after initialization means one blank page.
  ensureVisiblePage(owner) {
    const browser = owner.browser;
    return this.exclusive(owner, async () => {
      if (!this.live(owner, browser)) throw new Error('stale_generation');
      const pages = (await browser.pages()).filter(page => !page.isClosed() && !page.url().startsWith('devtools://'));
      if (pages.length) return;
      const page = await browser.newPage();
      if (!this.live(owner, browser)) { await page.close().catch(() => {}); throw new Error('stale_generation'); }
    });
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
  const [root,sdkDir,session]=process.argv.slice(2);
  if(!root||!sdkDir)throw new Error('browser service requires app root and SDK directory');
  // SDK cache initialization is private to this helper, not the real ~/.omp.
  const runtime=path.join(root,'terminal-browsers','runtime');await mkdir(runtime,{recursive:true});
  process.env.USERPROFILE=runtime;process.env.HOME=runtime;process.env.PI_CODING_AGENT_DIR=path.join(runtime,'agent');
  console.log=(...args)=>console.error(...args);
  const output=value=>process.stdout.write(JSON.stringify(value)+'\n');
  // The supervisor keeps this stream in terminal-browsers/browser-service.log for post-mortems.
  const log=(...parts)=>process.stderr.write(`${new Date().toISOString()} ${parts.join(' ')}\n`);
  // Each helper run has its own session; every native request carries it and old runs' frames are ignored natively.
  const helperSession=session||randomBytes(16).toString('hex');
  const native=new NativeChannel(output);
  const service=new BrowserService(root,sdkDir,state=>{log('state',state.instanceId,state.state,`pid=${state.pid}`,state.error??'');output({type:'state',data:state});},{native,log,helperSession});
  await service.start();output({type:'ready',helperSession});
  let chunks=[];let bytes=0;
  try {
    for await (const chunk of process.stdin) {
      let offset=0;
      while(offset<chunk.length){
        const newline=chunk.indexOf(10,offset);const end=newline<0?chunk.length:newline;
        const part=chunk.subarray(offset,end);bytes+=part.length;if(bytes>MAX_FRAME)throw new Error('browser frame exceeds transport limit');chunks.push(part);offset=end+1;
        if(newline<0)break;
        const frame=JSON.parse(Buffer.concat(chunks,bytes).toString('utf8'));chunks=[];bytes=0;
        // Native answers to this helper's own requests are demultiplexed from the supervisor's actions.
        if(frame.type==='native_response'){if(typeof frame.id==='string')native.receive(frame);continue;}
        if(typeof frame.id!=='string'||typeof frame.action!=='string')throw new Error('invalid browser frame');
        const started=Date.now();
        void service.dispatch(frame.action,frame.payload,frame.id).then(data=>{log(frame.action,'ok',`${Date.now()-started}ms`);output({type:'response',id:frame.id,ok:true,data});},error=>{log(frame.action,'failed',`${Date.now()-started}ms`,String(error.message??error));output({type:'response',id:frame.id,ok:false,error:String(error.message??error)});});
      }
    }
  } catch (error) { log('stdin loop failed',String(error?.stack??error)); process.exitCode=1; }
  log('shutting down');
  native.close();
  await service.dispatch('shutdown').catch(()=>{});
}
