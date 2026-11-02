import { spawn, execFile } from 'node:child_process';
import { createHash } from 'node:crypto';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

// TEST HARNESS ONLY. Stands in for the Rust supervisor's private browser_spawn / browser_ready / browser_retire
// actions so the Bun helper can be exercised with real Chrome outside the Tauri app. It mirrors the native contract
// (one slot per instance, operation tombstones, helperSession scoping, loopback-only endpoint, contained retirement via
// taskkill /T on Windows and dedicated process groups on Unix), but it is NOT the proof of native retirement:
// src-tauri's browser::browser_supervisor::tests::native_tests::native_owned_browser_lifecycle is, with real leases and owned children.
const ENDPOINT = /DevTools listening on (ws:\/\/127\.0\.0\.1:[1-9]\d{0,4}\/devtools\/browser\/[0-9A-Za-z-]+)/;
const fail = (code, message) => Object.assign(new Error(message.startsWith(code) ? message : `${code}: ${message}`), { code });
// Absolute path: a writable PATH entry must not substitute taskkill.
const taskkill = path.win32.join(process.env.SystemRoot ?? String.raw`C:\Windows`, 'System32', 'taskkill.exe');
const retireProcessTree = pid => {
  if (process.platform === 'win32') return new Promise(resolve => execFile(taskkill, ['/PID', String(pid), '/T', '/F'], { windowsHide: true }, () => resolve()));
  try { process.kill(-pid, 'SIGKILL'); } catch (error) { if (error.code !== 'ESRCH') return Promise.reject(error); }
  return Promise.resolve();
};
const alive = pid => { try { process.kill(pid, 0); return true; } catch { return false; } };

export async function openManualTabs(port, urls) {
  const { loadPuppeteer } = await import(pathToFileURL(path.join(process.env.OMP_SDK_DIR, 'src/tools/browser/launch.ts')).href);
  const browser = await (await loadPuppeteer()).connect({ browserURL: `http://127.0.0.1:${port}`, defaultViewport: null, protocolTimeout: 15000 });
  try {
    for (const url of urls) {
      const page = await browser.newPage();
      await page.goto(url);
      await page.bringToFront();
    }
  } finally { await browser.disconnect(); }
}

export class TestNativeHost {
  constructor(root) { this.root = root; this.slots = new Map(); this.tombstones = new Set(); this.calls = []; }

  /** A request channel for a BrowserService that lives in this process. */
  channel() { return { request: (action, payload) => this.handle(action, payload) }; }

  /** When a helper process ends, every browser job its run owned is retired (the supervisor's EOF retirement). */
  attach(child) {
    child.nativeSessions = new Set();
    child.once('exit', () => { for (const session of child.nativeSessions) void this.retireSession(session); });
  }

  /** Serves one `native_request` frame of a spawned helper process and answers over its stdin. */
  serve(child, frame) {
    child.nativeSessions?.add(frame.payload?.helperSession);
    this.handle(frame.action, frame.payload).then(
      result => child.stdin.writable && child.stdin.write(JSON.stringify({ type: 'native_response', id: frame.id, result, error: null }) + '\n'),
      error => child.stdin.writable && child.stdin.write(JSON.stringify({ type: 'native_response', id: frame.id, result: null, error: { code: error.code ?? 'native_error', message: error.message } }) + '\n'),
    );
  }

  profile(instanceId) {
    return path.join(this.root, 'terminal-browsers', createHash('sha256').update(instanceId, 'utf8').digest('hex'), 'profile');
  }

  async handle(action, p) {
    this.calls.push(action);
    if (action === 'browser_spawn') return this.spawn(p);
    if (action === 'browser_ready') {
      const slot = this.slots.get(p.instanceId);
      if (!slot || slot.operationId !== p.operationId || slot.leaseId !== p.leaseId || slot.cancelled || !alive(slot.pid)) throw fail('stale_generation', 'stale_generation');
      slot.ready = true;
      return { ready: true };
    }
    if (action === 'browser_retire') return this.retire(p);
    throw fail('unknown_action', 'unknown native browser action');
  }

  async spawn(p) {
    if (this.tombstones.has(p.operationId)) throw fail('operation_cancelled', 'operation_cancelled');
    if (this.slots.has(p.instanceId)) throw fail('browser_profile_busy', 'a previous browser of this terminal has not finished closing');
    const normalize = value => process.platform === 'win32' ? path.win32.normalize(value).toLowerCase() : path.resolve(value);
    if (normalize(p.profilePath) !== normalize(this.profile(p.instanceId))) throw fail('browser_arguments_refused', 'profile path is not this terminal\'s profile');
    if (!Array.isArray(p.args) || p.args.some(arg => typeof arg !== 'string' || !arg.startsWith('--'))) throw fail('browser_arguments_refused', 'startup URLs and bare arguments are not permitted');
    for (const required of ['--remote-debugging-address=127.0.0.1', '--remote-debugging-port=0', '--no-startup-window']) if (!p.args.includes(required)) throw fail('browser_arguments_refused', `missing ${required}`);
    const slot = { instanceId: p.instanceId, generation: p.generation, operationId: p.operationId, session: p.helperSession, leaseId: crypto.randomUUID(), cancelled: false, ready: false, pid: 0, exited: false };
    this.slots.set(p.instanceId, slot);
    try {
      const child = spawn(p.executable, p.args, { stdio: ['ignore', 'ignore', 'pipe'], windowsHide: false, detached: process.platform !== 'win32' });
      slot.child = child; slot.pid = child.pid;
      slot.exit = new Promise(resolve => child.once('exit', () => { slot.exited = true; resolve(); }));
      let stderr = '';
      const endpoint = await new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(fail('browser_launch_timeout', 'Chrome did not open its debugging endpoint in time.')), 20000);
        child.stderr.on('data', chunk => { stderr += chunk; const match = ENDPOINT.exec(stderr); if (match) { clearTimeout(timer); resolve(match[1]); } });
        child.once('error', error => { clearTimeout(timer); reject(fail('browser_spawn_failed', String(error.message))); });
        child.once('exit', code => { clearTimeout(timer); reject(fail('browser_exited_before_ready', `Chrome exited (code ${code}) before opening its debugging endpoint. If another Chrome window is using this terminal's browser profile, close it normally and try again.`)); });
      });
      if (slot.cancelled) throw fail('operation_cancelled', 'operation_cancelled');
      return { leaseId: slot.leaseId, pid: child.pid, creationTime100ns: '0', webSocketUrl: endpoint };
    } catch (error) {
      await this.retireSlot(slot, 0);
      throw error;
    }
  }

  async retire(p) {
    this.tombstones.add(p.operationId);
    const slot = this.slots.get(p.instanceId);
    if (!slot || slot.operationId !== p.operationId) return { retired: true };
    if (p.leaseId && p.leaseId !== slot.leaseId) throw fail('lease_mismatch', 'lease does not belong to this operation');
    await this.retireSlot(slot, Math.min(Number(p.graceMs) || 0, 10000));
    return { retired: true };
  }

  async retireSlot(slot, graceMs) {
    slot.cancelled = true;
    if (slot.pid) {
      if (graceMs && !slot.exited) await Promise.race([slot.exit, new Promise(resolve => setTimeout(resolve, graceMs))]);
      await retireProcessTree(slot.pid);
      await slot.exit;
    }
    if (this.slots.get(slot.instanceId) === slot) this.slots.delete(slot.instanceId);
  }

  /** What the supervisor does when a helper run ends: every browser job that run owned is retired. */
  async retireSession(session) {
    await Promise.all([...this.slots.values()].filter(slot => slot.session === session).map(slot => this.retireSlot(slot, 0)));
  }
  async retireAll() { await Promise.all([...this.slots.values()].map(slot => this.retireSlot(slot, 0))); }
  /** Resolves once the Chrome root of `pid` has exited (test observation, not a PID-authority check). */
  async waitForExit(pid, ms = 10000) {
    const deadline = Date.now() + ms;
    while (alive(pid)) {
      if (Date.now() > deadline) throw new Error(`process ${pid} did not exit`);
      await new Promise(resolve => setTimeout(resolve, 50));
    }
  }
  isAlive(pid) { return alive(pid); }
}
