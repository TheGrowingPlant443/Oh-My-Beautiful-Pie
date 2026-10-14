const MARKER = Symbol.for('omp-manager.browser-adapter');
export const COMPATIBILITY_ERROR = 'OMP Manager browser integration is incompatible with this OMP version';
const OWNERSHIP_ERROR = 'OMP Manager owns this terminal\'s browser; use browser.open without an external app/relay/CDP override';
const CLOSED_ERROR = 'Browser tab is closed; reopen it with browser.open';

// The capability-scoped discovery endpoint is owned by the same helper as the CDP
// socket. Validate there, not through a second controller RPC before every read.
async function validateTab(ticket, context) {
  const deadline = AbortSignal.timeout(30000);
  const signal = context?.signal ? AbortSignal.any([context.signal, deadline]) : deadline;
  const response = await fetch(`${ticket.cdpUrl}/json/version`, { method: 'HEAD', signal });
  if (response.status === 403 || response.status === 410) throw new Error(CLOSED_ERROR);
  if (!response.ok) throw new Error(`Browser CDP validation failed: HTTP ${response.status}`);
}

// Every tab of this terminal's Chrome belongs to the agent session. The helper binds hand-opened tabs as tab-N
// when asked; null means the capability is closed.
async function listTabs(ticket, context) {
  const deadline = AbortSignal.timeout(30000);
  const signal = context?.signal ? AbortSignal.any([context.signal, deadline]) : deadline;
  const response = await fetch(`${ticket.cdpUrl}/json/list`, { signal });
  if (response.status === 403 || response.status === 410) return null;
  if (!response.ok) throw new Error(`Browser tab listing failed: HTTP ${response.status}`);
  return response.json();
}

export function installTerminalBrowserBridge(pi, request) {
  const prototype = pi.pi?.EvalTool?.prototype;
  const original = prototype && Object.getOwnPropertyDescriptor(prototype, 'execute');
  if (prototype?.[MARKER]) return;
  const definitions = new Set();
  const wrapped = new WeakMap();
  const tickets = new Map();
  const urls = new Set();
  function redact(value) {
    if (typeof value === 'string') {
      for (const url of urls) value = value.replaceAll(url, '[terminal browser CDP]').replaceAll(url.replace('http:', 'ws:'), '[terminal browser CDP]');
      return value;
    }
    if (Array.isArray(value)) return value.map(redact);
    if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([key, entry]) => [key, redact(entry)]));
    return value;
  }
  function wrap(definition) {
    if (wrapped.has(definition)) return;
    const descriptor = Object.getOwnPropertyDescriptor(definition, 'invoke');
    if (!descriptor?.writable || typeof descriptor.value !== 'function') throw new Error(COMPATIBILITY_ERROR);
    const invoke = descriptor.value;
    const callback = async function(parameters, context) {
      if (!parameters || typeof parameters !== 'object' || !['open','close','tabs','run','call'].includes(parameters.action) || (parameters.name !== undefined && (typeof parameters.name !== 'string' || !parameters.name))) return invoke.call(this, parameters, context);
      const name = parameters.name ?? 'main';
      const app = parameters.app;
      if (app && (app.path !== undefined || app.cdp_url !== undefined || app.relay === true || app.tern === true)) throw new Error(OWNERSHIP_ERROR);
      try {
        if (parameters.action === 'open') {
          if (context?.signal?.aborted) throw context.signal.reason;
          const ticket = await request('browser_prepare', { name, ...(app?.target === undefined ? {} : { target: app.target }) });
          if (!ticket || typeof ticket.cdpUrl !== 'string' || !Number.isSafeInteger(ticket.epoch) || !Number.isSafeInteger(ticket.tabEpoch)) throw new Error(COMPATIBILITY_ERROR);
          urls.add(ticket.cdpUrl);
          urls.add(new URL(ticket.cdpUrl).hostname.split('.')[0]);
          const old = tickets.get(name);
          if (old && (old.epoch !== ticket.epoch || old.tabEpoch !== ticket.tabEpoch)) await invoke.call(this, {action:'close',name,kill:false}, context);
          const {headed: _headed, ...ownedParameters} = parameters;
          // The capability exposes exactly one page and every page URL has a scheme, so ':' always selects it;
          // a target makes OMP bring that tab to the front before screenshots instead of failing on a hidden one.
          const result = await invoke.call(this, { ...ownedParameters, app:{cdp_url:ticket.cdpUrl,target:':'} }, context);
          tickets.set(name,ticket);
          return redact(result);
        }
        if (parameters.action === 'close') {
          const result = await invoke.call(this, {...parameters,kill:false}, context);
          await request(parameters.all ? 'browser_close_all' : 'browser_close_tab', parameters.all ? {} : {name});
          if (parameters.all) tickets.clear(); else tickets.delete(name);
          return redact(result);
        }
        const attachNewTabs = async () => {
          let rows = null;
          for (const ticket of tickets.values()) if ((rows = await listTabs(ticket, context))) break;
          if (!rows) {
            // No ticket yet (e.g. the human opened the browser with the Browser button) or all closed: ask the app.
            rows = await request('browser_tabs', {});
            if (!Array.isArray(rows)) throw new Error(COMPATIBILITY_ERROR);
          }
          for (const row of rows ?? []) {
            const known = tickets.get(row.name);
            if (known?.epoch === row.epoch && known.tabEpoch === row.tabEpoch) continue;
            urls.add(row.cdpUrl); urls.add(new URL(row.cdpUrl).hostname.split('.')[0]);
            if (known) await invoke.call(this, {action:'close',name:row.name,kill:false}, context);
            await invoke.call(this, {action:'open',name:row.name,persist:true,app:{cdp_url:row.cdpUrl,target:':'}}, context);
            tickets.set(row.name,{cdpUrl:row.cdpUrl,epoch:row.epoch,tabEpoch:row.tabEpoch});
          }
        };
        if (parameters.action === 'tabs') {
          for (const [tabName,ticket] of tickets) {
            try { await validateTab(ticket, context); }
            catch(error) {
              if (!String(error.message??error).includes(CLOSED_ERROR)) throw error;
              await invoke.call(this,{action:'close',name:tabName,kill:false},context);tickets.delete(tabName);
            }
          }
          await attachNewTabs();
        } else {
          if(!tickets.has(name))await attachNewTabs();
          const ticket=tickets.get(name);if(!ticket)throw new Error(CLOSED_ERROR);
          await validateTab(ticket, context);
        }
        return redact(await invoke.call(this,parameters,context));
      } catch(error) {
        if (error instanceof Error) { error.message=redact(error.message); if(error.stack)error.stack=redact(error.stack); }
        throw error;
      }
    };
    const documentation = definition.documentation;
    definition.invoke=callback;
    definition.documentation = `${documentation}\nOMP Manager owns an isolated browser for this terminal. App Settings control Headless and presentation. Human and automation input may conflict; neither pauses the other. Do not pass external app/relay/CDP overrides. Every tab in this browser belongs to your session: tabs the user opens by hand (or that links open) are attached automatically as tab-1, tab-2, ... the next time you call browser.tabs() or browser.tab(name); use browser.tabs() to see their url and title. browser.open({name:'docs', app:{target:'<url or title substring>'}}) re-attaches an existing page under a name of your choice (omit url, which would navigate it). Closing a tab closes its page. Screenshots bring the attached tab to the front.`;
    wrapped.set(definition,{descriptor,callback,documentation}); definitions.add(definition);
  }
  function incompatible() {
    pi.on('session_start', (_event,ctx) => { ctx.ui?.notify?.(COMPATIBILITY_ERROR,'error'); });
    pi.registerTool({name:'eval',label:'Eval',description:COMPATIBILITY_ERROR,parameters:pi.zod.object({language:pi.zod.enum(['py','js']),code:pi.zod.string()}).passthrough(),async execute(){throw new Error(COMPATIBILITY_ERROR);}});
  }
  if (!original?.writable || typeof original.value !== 'function') { incompatible(); return; }
  const execute = function(...args) {
    const session = this.session;
    if (!session || typeof session.getEvalPreludes !== 'function') throw new Error(COMPATIBILITY_ERROR);
    const preludes = session.getEvalPreludes();
    if (!Array.isArray(preludes)) throw new Error(COMPATIBILITY_ERROR);
    for (const definition of preludes) if (definition.name === 'browser') wrap(definition);
    return original.value.apply(this,args);
  };
  try {
    Object.defineProperty(prototype,'execute',{...original,value:execute});
    Object.defineProperty(prototype,MARKER,{value:execute,configurable:true});
  } catch { incompatible(); return; }
  pi.on('session_shutdown', (_event,ctx) => {
    if (ctx.agent?.kind !== 'main') return;
    if (Object.getOwnPropertyDescriptor(prototype,'execute')?.value === execute) Object.defineProperty(prototype,'execute',original);
    if (prototype[MARKER] === execute) delete prototype[MARKER];
    for (const definition of definitions) {
      const patch=wrapped.get(definition);
      if (definition.invoke === patch.callback) { Object.defineProperty(definition,'invoke',patch.descriptor); definition.documentation=patch.documentation; }
    }
    definitions.clear();tickets.clear();urls.clear();
  });
}
