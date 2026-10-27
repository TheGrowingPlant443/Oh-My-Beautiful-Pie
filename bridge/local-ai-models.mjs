import { isIP } from 'node:net';

// Endpoint policy, not a network sandbox: a loopback service can itself forward requests elsewhere.
export function isLocalEndpoint(endpoint) {
  try {
    const url = new URL(endpoint);
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password) return false;
    const host = url.hostname.replace(/^\[|\]$/g, '').toLowerCase();
    return host === 'localhost' || host === '::1' || (isIP(host) === 4 && host.split('.')[0] === '127');
  } catch { return false; }
}

// These native routes honor baseUrl; specialized hosted routes may replace it after model selection.
const DIRECT_APIS = new Set(['openai-completions', 'openai-responses']);
const REWRITING_PROVIDERS = new Set(['moonshot', 'sakana', 'xai', 'xai-oauth', 'github-copilot', 'alibaba-token-plan', 'alibaba-coding-plan']);
const COMPACTION_ENDPOINTS = ['endpoint', 'v2Endpoint', 'streamingEndpoint'];
function isLocalCompaction(route) {
  return route == null || ((route.api == null || route.api === 'openai-responses') &&
    COMPACTION_ENDPOINTS.every(key => route[key] == null || isLocalEndpoint(route[key])));
}
export function isLocalModel(model) {
  return !!model && model.transport == null && DIRECT_APIS.has(model.api) &&
    !REWRITING_PROVIDERS.has(model.provider) && isLocalEndpoint(model.baseUrl) &&
    isLocalCompaction(model.remoteCompaction);
}

const POLICY = Symbol.for('omp-manager.local-ai-model-policy');
export const LOCAL_MODEL_REQUIRED = 'local_ai_model_required: This conversation has Local AI setup. Use a loopback inference model, or start a fresh ordinary conversation for hosted models.';

function assertRequest(registry, target, options) {
  const provider = typeof target === 'string' ? target : target?.provider;
  const id = typeof target === 'string' ? options?.modelId : target?.id;
  const endpoint = typeof target === 'string' ? options?.baseUrl : target?.baseUrl;
  const configured = typeof provider === 'string' && typeof id === 'string' ? registry.find(provider, id) : undefined;
  if (!isLocalModel(configured) || target?.transport != null || !isLocalEndpoint(endpoint)) throw new Error(LOCAL_MODEL_REQUIRED);
  if (target?.api != null && target.api !== configured.api) throw new Error(LOCAL_MODEL_REQUIRED);
  if (!isLocalCompaction(target?.remoteCompaction)) throw new Error(LOCAL_MODEL_REQUIRED);
  if (new URL(endpoint).href !== new URL(configured.baseUrl).href) throw new Error(LOCAL_MODEL_REQUIRED);
}

function assertCompaction(value, key = '') {
  if ((key === 'remoteEndpoint' || COMPACTION_ENDPOINTS.includes(key)) && value != null && value !== '' && !isLocalEndpoint(value)) throw new Error(LOCAL_MODEL_REQUIRED);
  if (key === 'api' && value != null && value !== 'openai-responses') throw new Error(LOCAL_MODEL_REQUIRED);
  if (value && typeof value === 'object') {
    for (const [name, child] of Object.entries(value)) assertCompaction(child, name);
  }
}

/** Guard native inference, including independently constructed child registries and auth retries. Process-lifetime. */
export function installLocalModelPolicy(ModelRegistry, Settings, initialSettings) {
  const prototype = ModelRegistry?.prototype;
  const settingsPrototype = Settings?.prototype;
  if (!prototype || !settingsPrototype || !initialSettings) throw new Error('OMP compatibility: native model/settings registry is unavailable');
  // rawValue's descriptor plumbing reads segments/definition without applying schema defaults.
  assertCompaction(initialSettings.rawValue({ segments: ['compaction'], definition: {} }));
  if (prototype[POLICY]) return;
  const methods = ['resolver', 'getApiKey', 'getApiKeyWithCredentialForProvider'];
  const originals = new Map(methods.map(name => [name, Object.getOwnPropertyDescriptor(prototype, name)]));
  for (const [name, descriptor] of originals) {
    if (typeof descriptor?.value !== 'function' || !descriptor.configurable) throw new Error(`OMP compatibility: ModelRegistry.${name} cannot be guarded`);
  }
  const raw = Object.getOwnPropertyDescriptor(settingsPrototype, 'rawValue');
  if (typeof raw?.value !== 'function' || !raw.configurable) throw new Error('OMP compatibility: Settings.rawValue cannot be guarded');
  Object.defineProperty(settingsPrototype, 'rawValue', {
    ...raw, configurable: false, writable: false,
    value: function (setting) {
      const value = raw.value.call(this, setting);
      if (setting?.segments?.[0] === 'compaction') assertCompaction(value, setting.segments.at(-1));
      return value;
    },
  });
  for (const [name, descriptor] of originals) {
    const original = descriptor.value;
    Object.defineProperty(prototype, name, {
      ...descriptor, configurable: false, writable: false,
      value: function (target, ...args) {
        assertRequest(this, target, name === 'resolver' ? args[0] : args[1]);
        return original.call(this, target, ...args);
      },
    });
  }
  Object.defineProperty(prototype, POLICY, { value: true });
}
