import { AsyncLocalStorage } from 'node:async_hooks';
import { createHash, randomUUID } from 'node:crypto';
import { mkdirSync, readFileSync, readdirSync, renameSync, rmSync, statSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

export const TERMINAL_COMPATIBILITY_ERROR = 'OMP Manager terminal integration is incompatible with this OMP version';
export const HELD_ERROR = 'Command execution paused by Kill process; wait for a new human prompt';
export const OUTPUT_LIMIT = 256 * 1024;
const MARKER = Symbol.for('omp-manager.terminal-observer');
const FLUSH_MS = 100;
const SERVICE_POLL_MS = 250;
// Shorter than the app's 30 s native-control deadline so a stuck process is reported rather than timed out.
const KILL_DEADLINE_MS = 25000;
const PREVIEW_CHARS = 256;
const LIVE = new Set(['running', 'stopping']);
const EXECUTION_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const META_LIMIT = 1024 * 1024;
const SHOW_LIMIT = 65536;

export function sha256(value) { return createHash('sha256').update(String(value)).digest('hex'); }

// One line of whitespace-flattened code, capped by Unicode characters; the full script stays in script.txt.
export function previewOf(script) {
  return Array.from(String(script).replace(/\s+/gu, ' ').trim()).slice(0, PREVIEW_CHARS).join('');
}

// Keep the last OUTPUT_LIMIT UTF-8 bytes without splitting a character.
export function capTail(text) {
  if (text.length * 3 <= OUTPUT_LIMIT) return { text, truncated: false };
  const buffer = Buffer.from(text, 'utf8');
  if (buffer.length <= OUTPUT_LIMIT) return { text, truncated: false };
  let start = buffer.length - OUTPUT_LIMIT;
  while (start < buffer.length && (buffer[start] & 0xc0) === 0x80) start++;
  return { text: buffer.subarray(start).toString('utf8'), truncated: true };
}

function updateText(update) {
  const blocks = update?.content;
  if (!Array.isArray(blocks)) return undefined;
  const parts = blocks.filter(block => block?.type === 'text' && typeof block.text === 'string').map(block => block.text);
  return parts.length ? parts.join('\n') : undefined;
}

function writableMethod(prototype, name) {
  const descriptor = prototype && Object.getOwnPropertyDescriptor(prototype, name);
  return descriptor?.writable && typeof descriptor.value === 'function' ? descriptor : undefined;
}

function exitCodeOf(details) {
  const code = details?.exitCode;
  return Number.isSafeInteger(code) ? code : null;
}

function summary(exec) {
  return {
    executionId: exec.id, sessionId: exec.rootSessionId, sourceSessionId: exec.sourceSessionId, toolCallId: exec.toolCallId,
    agentId: exec.agentId, kind: exec.kind, description: exec.description, preview: exec.preview, cwd: exec.cwd,
    startedAt: exec.startedAt, endedAt: exec.endedAt, status: exec.status, exitCode: exec.exitCode,
    tab: LIVE.has(exec.status) ? exec.tab : null, historical: false, hasOutput: exec.hasOutput,
  };
}

function setOutput(exec, text) {
  if (exec.final || typeof text !== 'string' || text === exec.rawText) return;
  exec.rawText = text;
  const capped = capTail(text);
  exec.text = capped.text;
  exec.truncated = capped.truncated;
  exec.revision++;
  exec.dirty = true;
}

function appendOutput(exec, chunk) {
  if (exec.final) return;
  const addition = typeof chunk === 'string' ? chunk : Buffer.from(chunk).toString('utf8');
  if (!addition) return;
  const capped = capTail(exec.text + addition);
  exec.rawText = capped.text;
  exec.text = capped.text;
  exec.truncated = exec.truncated || capped.truncated;
  exec.revision++;
  exec.dirty = true;
}

function interactiveContext(ctx, exec) {
  const ui = ctx?.ui;
  if (!ui || typeof ui.custom !== 'function') return ctx;
  const custom = function(factory, ...rest) {
    const wrapped = (...args) => {
      const component = factory(...args);
      if (component && typeof component.appendOutput === 'function') {
        const original = component.appendOutput;
        exec.interactive = true;
        component.appendOutput = function(chunk) { appendOutput(exec, chunk); return original.call(this, chunk); };
        exec.restoreOverlay = () => { delete component.appendOutput; exec.restoreOverlay = undefined; };
      }
      return component;
    };
    return ui.custom(wrapped, ...rest);
  };
  return Object.create(ctx, { ui: { value: Object.create(ui, { custom: { value: custom } }) } });
}

const sameService = (daemon, service) => !!daemon && daemon.id === service.id && daemon.startedAt === service.startedAt && daemon.owner === service.owner;

function serviceOutcome(killRequested, daemon) {
  const code = Number.isSafeInteger(daemon.exitCode) ? daemon.exitCode : null;
  if (killRequested) return { status: 'cancelled', code };
  return { status: daemon.state === 'failed' || (code !== null && code !== 0) ? 'failed' : 'completed', code };
}

function jobStatus(exec, job) {
  if (exec.killRequested || job.status === 'cancelled') return 'cancelled';
  return job.status === 'failed' ? 'failed' : 'completed';
}

function resultStatus(exec, failed) {
  if (exec.killRequested) return 'cancelled';
  return failed ? 'failed' : 'completed';
}

const savedOutput = (row, read) => (row.hasOutput ? read('output.txt') : null);

const exitNote = row => (row.exitCode !== null && row.exitCode !== undefined && row.exitCode !== 0 ? ` (exit ${row.exitCode})` : '');
const isoOf = time => new Date(time).toISOString();

function historyLine(row) {
  const preview = Array.from(row.preview).slice(0, 120).join('');
  const saved = row.hasOutput ? ' | output saved' : '';
  return `- ${row.executionId} | ${isoOf(row.startedAt)} | ${row.status}${exitNote(row)} | ${row.kind} | ${row.description ?? ''} | ${preview}${saved}`;
}

export function formatHistory({ rows, total }) {
  if (!rows.length) return 'No commands recorded in this conversation yet.';
  const showing = total > rows.length ? `, showing ${rows.length}` : '';
  return [`${total} command(s) in this conversation (newest first${showing})`, ...rows.map(historyLine)].join('\n');
}

function endedText(row, running) {
  if (row.endedAt !== null) return isoOf(row.endedAt);
  return running ? 'still running' : 'unknown';
}

function scriptText(script) {
  if (script === null) return '(script unavailable)';
  return script.length > SHOW_LIMIT ? `${script.slice(0, SHOW_LIMIT)}\n[script truncated; showing the first ${SHOW_LIMIT} characters]` : script;
}

function outputText(output) {
  if (output === null) return '(no output recorded: OMP Manager keeps the output of failed commands only)';
  return output.length > SHOW_LIMIT ? `[output truncated; showing the last ${SHOW_LIMIT} characters]\n${output.slice(-SHOW_LIMIT)}` : output;
}

// part: 'script' | 'output' | 'both'. Script is cut to its first SHOW_LIMIT characters, output to its last.
export function formatEntry({ summary: row, script, output }, part = 'both') {
  const running = LIVE.has(row.status);
  const lines = [
    `Command ${row.executionId}`,
    `Status: ${row.status}${exitNote(row)}`,
    `Kind: ${row.kind}`,
    `Started: ${isoOf(row.startedAt)}`,
    `Ended: ${endedText(row, running)}`,
    `Directory: ${row.cwd}`,
  ];
  if (row.description) lines.push(`Description: ${row.description}`);
  if (part !== 'output') lines.push('--- script ---', scriptText(script));
  if (part !== 'script') lines.push(running ? '--- output (live, still running) ---' : '--- output ---', outputText(output));
  return lines.join('\n');
}

// Read-only model tool over the observer's ledger. Its calls never pass through Bash/Eval, so they are not recorded.
export function registerCommandHistoryTool(pi, observer) {
  pi.registerTool({
    name: 'command_history',
    label: 'Command History',
    loadMode: 'essential',
    description: 'Your own Bash/Eval command history in this conversation, as recorded by OMP Manager. action "list" shows commands newest first (optional filters: status = running|stopping|completed|failed|cancelled|interrupted, query = text found in the description or code, limit 1-100, default 20) with their ids. action "show" with an executionId returns that command\'s exact script and, for failed commands, the saved output (completed commands do not keep output). Use it to recall what you already ran or why a command failed. Read-only; it never runs anything.',
    parameters: pi.zod.object({
      action: pi.zod.enum(['list', 'show']),
      limit: pi.zod.number().int().min(1).max(100).optional(),
      status: pi.zod.enum(['running', 'stopping', 'completed', 'failed', 'cancelled', 'interrupted']).optional(),
      query: pi.zod.string().optional(),
      executionId: pi.zod.string().optional(),
      part: pi.zod.enum(['script', 'output', 'both']).optional(),
    }),
    approval: () => 'read',
    async execute(_id, params, signal) {
      if (signal?.aborted) throw signal.reason;
      try {
        const text = params.action === 'list'
          ? formatHistory(observer.history({ limit: params.limit, status: params.status, query: params.query }))
          : formatEntry(observer.entry(params.executionId ?? ''), params.part ?? 'both');
        return { content: [{ type: 'text', text }] };
      } catch (error) {
        return { content: [{ type: 'text', text: String(error.message ?? error) }], isError: true };
      }
    },
  });
}

// Observes real native Bash/Eval execution. It never replaces the tools or scrapes the agent screen: it wraps
// the prototype execute methods inside OMP's approval layer, so approval, cancellation and results are native.
export function installTerminalCommands(pi, options) {
  const { emit, canSend = () => true, historyRoot, sdkDir } = options;
  const BashTool = pi.pi?.BashTool;
  const EvalTool = pi.pi?.EvalTool;
  const AgentSession = pi.pi?.AgentSession;
  const bashDescriptor = writableMethod(BashTool?.prototype, 'execute');
  const evalDescriptor = writableMethod(EvalTool?.prototype, 'execute');
  const infoDescriptor = writableMethod(AgentSession?.prototype, 'getAllToolInfos');
  const interruptLabel = pi.pi?.USER_INTERRUPT_LABEL;
  const compatible = !!bashDescriptor && !!evalDescriptor && !!infoDescriptor
    && typeof AgentSession.prototype.abort === 'function' && typeof interruptLabel === 'string';
  const noop = {
    supported: false, attachContext() {}, snapshot: () => ({ supported: false, error: TERMINAL_COMPATIBILITY_ERROR, held: false, active: [] }), resync() {},
    kill: async () => { throw new Error(TERMINAL_COMPATIBILITY_ERROR); }, clearHoldForHuman() {}, dispose() {},
    history() { throw new Error(TERMINAL_COMPATIBILITY_ERROR); }, entry() { throw new Error(TERMINAL_COMPATIBILITY_ERROR); },
  };
  if (BashTool?.prototype?.[MARKER]) return { ...BashTool.prototype[MARKER], dispose() {} };
  if (!compatible) return noop;

  const als = new AsyncLocalStorage();
  const live = new Map();
  const intents = new Map();
  const liveSessions = new Set();
  const patchedManagers = new Map();
  let context;
  let held = false;
  let ledgerError = null;
  let serviceError = null;
  let flushTimer;
  let serviceTimer;
  let servicePolling = false;
  let servicesModule;
  let disposed = false;

  const rootSessionId = () => context?.sessionManager?.getSessionId?.();
  const rootSession = () => [...liveSessions].find(session => session.sessionId === rootSessionId());
  const rootManager = () => rootSession()?.asyncJobManager;

  function tracked(session) {
    const root = rootSessionId();
    if (!session || !root) return false;
    if (session.getSessionId?.() === root) return true;
    const manager = session.asyncJobManager;
    return !!manager && manager === rootManager();
  }

  function ledgerDirectory(exec) { return path.join(historyRoot, sha256(exec.rootSessionId), exec.id); }

  function writeLedgerFile(exec, name, data) {
    const target = path.join(exec.directory, name);
    const temporary = `${target}.${process.pid}.${randomUUID()}.tmp`;
    try {
      writeFileSync(temporary, data);
      renameSync(temporary, target);
      return true;
    } catch (error) {
      rmSync(temporary, { force: true });
      ledgerError = `Terminal history could not be written: ${error.message ?? error}`;
      return false;
    }
  }

  function writeMeta(exec) { writeLedgerFile(exec, 'meta.json', JSON.stringify(summary(exec))); }

  function lowestFreeTab() {
    const used = new Set([...live.values()].map(exec => exec.tab));
    let tab = 2;
    while (used.has(tab)) tab++;
    return tab;
  }

  function sendCommand(exec) { try { emit('terminal_command', { command: summary(exec) }); } catch {} }

  function sendOutput(exec) {
    if (!exec.dirty || !canSend()) return;
    try {
      emit('terminal_output', { executionId: exec.id, revision: exec.revision, text: exec.text, truncated: exec.truncated });
      exec.dirty = false;
    } catch {}
  }

  function pollJobs() {
    for (const exec of live.values()) {
      const progress = exec.job?.progressText;
      if (typeof progress === 'string' && !exec.interactive) setOutput(exec, progress);
    }
  }

  function ensureFlushTimer() {
    if (flushTimer || disposed) return;
    flushTimer = setInterval(() => {
      pollJobs();
      for (const exec of live.values()) sendOutput(exec);
      if (!live.size) { clearInterval(flushTimer); flushTimer = undefined; }
    }, FLUSH_MS);
    flushTimer.unref?.();
  }

  async function loadServices() {
    if (!servicesModule) {
      servicesModule = import(pathToFileURL(path.join(sdkDir, 'src', 'launch', 'services.ts')).href).then(module => {
        for (const name of ['findService', 'serviceLogs', 'stopService']) if (typeof module[name] !== 'function') throw new Error(TERMINAL_COMPATIBILITY_ERROR);
        return module;
      });
      servicesModule.catch(error => { serviceError = String(error?.message ?? error); });
    }
    return servicesModule;
  }

  async function pollService(exec) {
    const services = await loadServices();
    const daemon = await services.findService(exec.service.session, exec.service.name);
    if (!sameService(daemon, exec.service)) {
      settle(exec, exec.killRequested ? 'cancelled' : 'interrupted', null);
      return;
    }
    setOutput(exec, await services.serviceLogs(exec.service.session, exec.service.name));
    if (daemon.state === 'exited' || daemon.state === 'failed') {
      const { status, code } = serviceOutcome(exec.killRequested, daemon);
      settle(exec, status, code);
    }
  }

  async function pollServices() {
    if (servicePolling) return;
    servicePolling = true;
    try {
      for (const exec of live.values()) {
        if (!exec.service || exec.final) continue;
        try { await pollService(exec); } catch (error) { serviceError = String(error?.message ?? error); }
      }
    } finally { servicePolling = false; }
  }

  function ensureServiceTimer() {
    if (serviceTimer || disposed) return;
    serviceTimer = setInterval(() => {
      void pollServices().finally(() => {
        if (![...live.values()].some(exec => exec.service)) { clearInterval(serviceTimer); serviceTimer = undefined; }
      });
    }, SERVICE_POLL_MS);
    serviceTimer.unref?.();
  }

  // Only failed commands keep their final output (already capped to the last OUTPUT_LIMIT bytes).
  function persistOutput(exec) {
    if (exec.text) exec.hasOutput = writeLedgerFile(exec, 'output.txt', exec.text);
  }

  function settle(exec, status, exitCode) {
    if (exec.final) return;
    pollJobs();
    exec.final = true;
    exec.status = status;
    exec.exitCode = exitCode;
    exec.endedAt = Date.now();
    exec.dirty = true;
    sendOutput(exec);
    exec.restoreOverlay?.();
    live.delete(exec.id);
    if (status === 'failed') persistOutput(exec);
    writeMeta(exec);
    sendCommand(exec);
    exec.resolveSettled();
  }

  function begin(kind, tool, toolCallId, params) {
    const session = tool.session;
    const script = kind === 'bash' ? String(params?.command ?? '') : String(params?.code ?? '');
    const sessionCwd = session.cwd ?? process.cwd();
    const cwd = kind === 'bash' && typeof params?.cwd === 'string' && params.cwd ? path.resolve(sessionCwd, params.cwd) : sessionCwd;
    let description = intents.get(toolCallId) ?? (typeof params?.title === 'string' && params.title.trim() ? params.title.trim() : null);
    if (!description && typeof tool.intent === 'function') {
      try { const value = tool.intent(params); if (typeof value === 'string' && value.trim()) description = value.trim(); } catch {}
    }
    const settled = Promise.withResolvers();
    const exec = {
      id: randomUUID(), kind, toolCallId, script, description, preview: previewOf(script), cwd,
      rootSessionId: rootSessionId(), sourceSessionId: session.getSessionId?.() ?? rootSessionId(), agentId: session.getAgentId?.() ?? 'main',
      startedAt: Date.now(), endedAt: null, status: 'running', exitCode: null, tab: lowestFreeTab(),
      text: '', rawText: '', truncated: false, hasOutput: false, revision: 0, dirty: false, final: false,
      controller: new AbortController(), session, manager: undefined, job: undefined, jobId: undefined, service: undefined,
      killRequested: false, interactive: false, settled: settled.promise, resolveSettled: settled.resolve,
    };
    exec.directory = ledgerDirectory(exec);
    try {
      mkdirSync(exec.directory, { recursive: true });
      writeFileSync(path.join(exec.directory, 'script.txt'), script, { flag: 'wx' });
    } catch (error) { ledgerError = `Terminal history could not be written: ${error.message ?? error}`; }
    live.set(exec.id, exec);
    writeMeta(exec);
    sendCommand(exec);
    ensureFlushTimer();
    return exec;
  }

  function settleFromJob(exec) {
    const job = exec.job;
    const code = exitCodeOf(job.latestDetails);
    setOutput(exec, job.resultText ?? job.errorText ?? job.progressText ?? exec.rawText);
    const status = jobStatus(exec, job);
    settle(exec, status, code ?? (job.status === 'completed' && exec.kind === 'bash' ? 0 : null));
  }

  // The captured native session also owns the shared job manager; patch each manager instance once so a job
  // registered while an observed call runs is linked to that call and outlives the tool return.
  function patchManager(manager) {
    if (!manager || patchedManagers.has(manager)) return;
    const descriptor = Object.getOwnPropertyDescriptor(manager, 'register');
    const original = manager.register;
    if (typeof original !== 'function') return;
    const wrapper = function(type, label, run, registerOptions) {
      const id = original.call(this, type, label, run, registerOptions);
      const exec = als.getStore()?.exec;
      if (exec && (type === 'bash' || type === 'eval') && !exec.jobId) {
        exec.manager = this; exec.jobId = id; exec.job = this.getJob(id);
        exec.job?.promise.then(() => { if (exec.backgrounded) settleFromJob(exec); });
      }
      return id;
    };
    Object.defineProperty(manager, 'register', { configurable: true, writable: true, enumerable: descriptor?.enumerable ?? false, value: wrapper });
    patchedManagers.set(manager, { descriptor, wrapper });
  }

  async function adoptService(exec, result) {
    const service = result?.details?.service;
    if (!service || typeof service.name !== 'string') return false;
    try {
      const services = await loadServices();
      const daemon = await services.findService(exec.session, service.name);
      if (!daemon) return false;
      exec.service = { name: service.name, session: exec.session, id: daemon.id, startedAt: daemon.startedAt, owner: daemon.owner };
      ensureServiceTimer();
      void pollServices();
      return true;
    } catch (error) {
      serviceError = String(error?.message ?? error);
      return false;
    }
  }

  function makeWrapper(kind, descriptor) {
    const original = descriptor.value;
    return function(toolCallId, params, signal, onUpdate, ctx) {
      if (disposed || !tracked(this.session)) return original.call(this, toolCallId, params, signal, onUpdate, ctx);
      if (held) throw new Error(HELD_ERROR);
      const language = params?.language === 'py' || params?.language === 'python' ? 'py' : 'js';
      const exec = begin(kind === 'bash' ? 'bash' : language, this, toolCallId, params);
      patchManager(this.session.asyncJobManager);
      const combined = signal ? AbortSignal.any([signal, exec.controller.signal]) : exec.controller.signal;
      const tee = update => {
        const text = updateText(update);
        if (text !== undefined && !exec.interactive) setOutput(exec, text);
        return onUpdate?.(update);
      };
      const callContext = kind === 'bash' && params?.pty === true ? interactiveContext(ctx, exec) : ctx;
      let running;
      try { running = als.run({ exec }, () => original.call(this, toolCallId, params, combined, tee, callContext)); }
      catch (error) { running = Promise.reject(error); }
      return running.then(async result => {
        if (result?.details?.async?.state === 'running' && exec.job) {
          // Backgrounded: the tool call returned but the native job is still the process; it owns the lifetime.
          exec.backgrounded = true;
          if (exec.job.endTime !== undefined) settleFromJob(exec);
          return result;
        }
        const text = updateText(result);
        if (text !== undefined && !exec.interactive) setOutput(exec, text);
        if (kind === 'bash' && await adoptService(exec, result)) return result;
        const failed = result?.isError === true;
        const code = exitCodeOf(result?.details);
        const status = resultStatus(exec, failed);
        settle(exec, status, code ?? (kind === 'bash' && !failed ? 0 : null));
        return result;
      }, error => {
        const cancelled = exec.killRequested || signal?.aborted || exec.controller.signal.aborted;
        if (!cancelled) {
          // A tool that throws returns no result text, so the thrown message is the only failure output.
          const failure = String(error?.message ?? error);
          setOutput(exec, exec.rawText && !exec.rawText.includes(failure) ? `${exec.rawText}\n${failure}` : failure);
        }
        settle(exec, cancelled ? 'cancelled' : 'failed', null);
        throw error;
      });
    };
  }

  const restorers = [];
  function patch(prototype, name, descriptor, value, markerValue) {
    Object.defineProperty(prototype, name, { ...descriptor, value });
    restorers.push(() => { if (Object.getOwnPropertyDescriptor(prototype, name)?.value === value) Object.defineProperty(prototype, name, descriptor); });
    if (markerValue) {
      Object.defineProperty(prototype, MARKER, { value: markerValue, configurable: true });
      restorers.push(() => { if (prototype[MARKER] === markerValue) delete prototype[MARKER]; });
    }
  }

  const bashWrapper = makeWrapper('bash', bashDescriptor);
  const evalWrapper = makeWrapper('eval', evalDescriptor);
  const captureInfos = function(...args) { liveSessions.add(this); return infoDescriptor.value.apply(this, args); };

  async function abortRoot() {
    const session = rootSession();
    if (!session) throw new Error('Native OMP session was not captured; cannot interrupt the agent');
    await session.abort({ reason: interruptLabel });
  }

  function setHeld(value) {
    if (held === value) return;
    held = value;
    try { emit('terminal_hold', { held }); } catch {}
  }

  async function kill(executionId) {
    const exec = live.get(executionId);
    if (!exec || exec.final) return { killed: false };
    setHeld(true);
    exec.killRequested = true;
    exec.status = 'stopping';
    sendCommand(exec);
    if (exec.service) {
      try {
        const services = await loadServices();
        const daemon = await services.findService(exec.service.session, exec.service.name);
        if (sameService(daemon, exec.service)) await services.stopService(exec.service.session, exec.service.name);
      } catch (error) { serviceError = String(error?.message ?? error); }
    }
    if (exec.manager && exec.jobId) exec.manager.cancel(exec.jobId, { ownerId: exec.job?.ownerId });
    exec.controller.abort(new Error('Killed from OMP Manager'));
    const deadline = Date.now() + KILL_DEADLINE_MS;
    const timeout = new Promise(resolve => setTimeout(() => resolve('timeout'), KILL_DEADLINE_MS).unref?.());
    await Promise.race([abortRoot().catch(() => undefined), timeout]);
    if (!exec.final) await Promise.race([exec.settled, new Promise(resolve => setTimeout(() => resolve('timeout'), Math.max(0, deadline - Date.now())).unref?.())]);
    if (!exec.final) throw new Error('Process is still stopping; it stays listed until it actually exits');
    return { killed: true };
  }

  function snapshotError(rootReady) {
    if (disposed) return null;
    return rootReady ? ledgerError ?? serviceError : TERMINAL_COMPATIBILITY_ERROR;
  }

  function snapshot() {
    const rootReady = !!rootSession();
    return {
      supported: !disposed && rootReady,
      error: snapshotError(rootReady),
      held,
      active: [...live.values()].map(summary),
    };
  }

  function resync() {
    setHeld(held);
    try { emit('terminal_hold', { held }); } catch {}
    for (const exec of live.values()) { sendCommand(exec); exec.dirty = true; sendOutput(exec); }
  }

  function sessionLedger() {
    const root = rootSessionId();
    if (!root) throw new Error('No OMP session is active yet');
    return path.join(historyRoot, sha256(root));
  }

  // Live rows come from memory; stored rows from meta.json. A stored row that never ended was cut off by a restart.
  function storedRow(directory, executionId) {
    const liveExec = live.get(executionId);
    if (liveExec) return summary(liveExec);
    try {
      const file = path.join(directory, executionId, 'meta.json');
      if (statSync(file).size > META_LIMIT) return null;
      const row = JSON.parse(readFileSync(file, 'utf8'));
      if (row?.executionId !== executionId) return null;
      return { ...row, hasOutput: row.hasOutput === true, status: row.endedAt === null || row.endedAt === undefined ? 'interrupted' : row.status };
    } catch { return null; }
  }

  function history({ limit = 20, status, query } = {}) {
    const directory = sessionLedger();
    let names;
    try { names = readdirSync(directory); } catch { return { rows: [], total: 0 }; }
    const needle = typeof query === 'string' ? query.trim().toLowerCase() : '';
    const rows = [];
    for (const name of names) {
      if (!EXECUTION_ID.test(name)) continue;
      const row = storedRow(directory, name);
      if (!row || (status && row.status !== status)) continue;
      if (needle && !`${row.description ?? ''}\n${row.preview ?? ''}`.toLowerCase().includes(needle)) continue;
      rows.push(row);
    }
    rows.sort((a, b) => b.startedAt - a.startedAt || (a.executionId < b.executionId ? -1 : 1));
    return { rows: rows.slice(0, Math.min(100, Math.max(1, Math.trunc(limit) || 20))), total: rows.length };
  }

  function entry(executionId) {
    if (!EXECUTION_ID.test(executionId)) throw new Error('executionId must be an id returned by action "list"');
    const directory = sessionLedger();
    const row = storedRow(directory, executionId);
    if (!row) throw new Error('No command with that id in this conversation');
    const liveExec = live.get(executionId);
    const read = file => { try { return readFileSync(path.join(directory, executionId, file), 'utf8'); } catch { return null; } };
    return {
      summary: row,
      script: liveExec ? liveExec.script : read('script.txt'),
      output: liveExec ? liveExec.text : savedOutput(row, read),
    };
  }

  const observer = {
    supported: true,
    attachContext(ctx) {
      context = ctx;
      try { pi.getAllTools?.(); } catch {}
    },
    snapshot, resync, kill, history, entry,
    clearHoldForHuman() { setHeld(false); },
    dispose() {
      if (disposed) return;
      for (const exec of live.values()) settle(exec, 'interrupted', null);
      disposed = true;
      clearInterval(flushTimer); clearInterval(serviceTimer);
      for (const [manager, { descriptor, wrapper }] of patchedManagers) {
        if (manager.register === wrapper) { if (descriptor) Object.defineProperty(manager, 'register', descriptor); else delete manager.register; }
      }
      patchedManagers.clear();
      for (const restore of restorers.toReversed()) restore();
      liveSessions.clear(); intents.clear();
    },
  };

  try {
    patch(AgentSession.prototype, 'getAllToolInfos', infoDescriptor, captureInfos);
    patch(BashTool.prototype, 'execute', bashDescriptor, bashWrapper, observer);
    patch(EvalTool.prototype, 'execute', evalDescriptor, evalWrapper);
  } catch {
    for (const restore of restorers.toReversed()) restore();
    return noop;
  }

  pi.on('tool_execution_start', event => {
    if (typeof event.intent === 'string' && event.intent.trim() && (event.toolName === 'bash' || event.toolName === 'eval')) {
      intents.set(event.toolCallId, event.intent.trim());
      if (intents.size > 500) intents.delete(intents.keys().next().value);
    }
  });
  pi.on('tool_call', event => (held && (event.toolName === 'bash' || event.toolName === 'eval') ? { block: true, reason: HELD_ERROR } : undefined));
  pi.on('input', event => {
    if ((event.source === 'interactive' || event.source === 'rpc') && typeof event.text === 'string' && event.text.trim()) setHeld(false);
  });
  return observer;
}
