import assert from 'node:assert/strict';
import test from 'node:test';
import { isLocalModel, installLocalModelPolicy } from './local-ai-models.mjs';

const local = { api: 'openai-completions', provider: 'self-hosted', id: 'coder', baseUrl: 'http://127.0.0.1:18082/v1' };
const hosted = { api: 'openai-completions', provider: 'local-looking-name', id: 'coder', baseUrl: 'https://api.example.com/v1' };

test('locality follows the resolved endpoint rather than provider labels or free pricing', () => {
  for (const baseUrl of ['http://127.0.0.1:18082/v1', 'https://127.2.3.4/v1', 'http://[::1]:8080/v1', 'http://localhost:11434/v1']) {
    assert.equal(isLocalModel({ ...local, baseUrl }), true, baseUrl);
  }
  for (const baseUrl of ['http://0.0.0.0:8080', 'http://192.168.1.2/v1', 'http://localhost.example.com/v1', 'https://127.0.0.1.example.com', 'file:///local', 'http://user:password@localhost', 'not a URL']) {
    assert.equal(isLocalModel({ ...local, provider: 'local', baseUrl, cost: { input: 0 } }), false, baseUrl);
  }
  assert.equal(isLocalModel({ ...local, transport: 'pi-native' }), false);
  assert.equal(isLocalModel(hosted), false);
  assert.equal(isLocalModel({ ...local, api: 'google-antigravity' }), false);
  assert.equal(isLocalModel({ ...local, provider: 'github-copilot' }), false);
  for (const remoteCompaction of [
    { endpoint: hosted.baseUrl }, { v2StreamingEnabled: true, v2Endpoint: hosted.baseUrl },
    { v2StreamingEnabled: true, streamingEndpoint: hosted.baseUrl }, { api: 'azure-openai-responses' },
  ]) assert.equal(isLocalModel({ ...local, remoteCompaction }), false);
  assert.equal(isLocalModel({ ...local, remoteCompaction: { api: 'openai-responses', v2Endpoint: local.baseUrl } }), true);
});

function registryFixture(compaction = {}) {
  const requests = [];
  class Registry {
    find(provider, id) { return [local, hosted].find(model => model.provider === provider && model.id === id); }
    resolver(model) { requests.push(model); return () => this.getApiKeyWithCredentialForProvider(model.provider, '', { baseUrl: model.baseUrl, modelId: model.id }); }
    getApiKey(model) { requests.push(model); return 'local-key'; }
    getApiKeyWithCredentialForProvider(provider, _session, options) { requests.push({ provider, ...options }); return 'local-key'; }
  }
  class Config {
    constructor() { this.compaction = compaction; }
    rawValue(setting) { return setting.segments.reduce((value, segment) => value?.[segment], { compaction: this.compaction }); }
  }
  const config = new Config();
  installLocalModelPolicy(Registry, Config, config);
  return { Registry, requests, Config, config };
}

test('local sessions deny hosted fallback and independently constructed child registries before credentials', () => {
  const { Registry, requests } = registryFixture();
  for (const registry of [new Registry(), new Registry()]) {
    assert.throws(() => registry.resolver(hosted), /local_ai_model_required/);
    assert.throws(() => registry.getApiKey(hosted), /local_ai_model_required/);
    assert.throws(() => registry.getApiKeyWithCredentialForProvider(hosted.provider, '', { baseUrl: hosted.baseUrl, modelId: hosted.id }), /local_ai_model_required/);
  }
  assert.deepEqual(requests, []);
});

test('retry-time endpoint replacement and provider-only credential requests cannot bypass the guard', () => {
  const { Registry, requests } = registryFixture();
  const registry = new Registry();
  const resolve = registry.resolver(local);
  resolve();
  const before = requests.length;
  assert.throws(() => registry.getApiKeyWithCredentialForProvider(local.provider, '', { baseUrl: hosted.baseUrl, modelId: local.id }), /local_ai_model_required/);
  assert.throws(() => registry.getApiKeyWithCredentialForProvider(local.provider), /local_ai_model_required/);
  assert.throws(() => registry.resolver({ ...local, transport: 'pi-native' }), /local_ai_model_required/);
  for (const remoteCompaction of [
    { endpoint: hosted.baseUrl }, { v2Endpoint: hosted.baseUrl }, { streamingEndpoint: hosted.baseUrl }, { api: 'azure-openai-responses' },
  ]) assert.throws(() => registry.getApiKey({ ...local, remoteCompaction }), /local_ai_model_required/);
  assert.equal(requests.length, before);
  assert.equal(registry.getApiKey(local), 'local-key');
  assert.throws(() => { Registry.prototype.resolver = () => 'bypass'; }, TypeError);
});

test('incompatible native registry cannot install a partial policy', () => {
  class Incompatible { getApiKey() {} }
  assert.throws(() => installLocalModelPolicy(Incompatible), /OMP compatibility/);
  assert.equal(Object.getOwnPropertyDescriptor(Incompatible.prototype, 'getApiKey').configurable, true);
});

test('remote compaction overrides are rejected at startup and when child settings change', () => {
  assert.throws(() => registryFixture({ remoteEndpoint: hosted.baseUrl }), /local_ai_model_required/);
  const { Config, config } = registryFixture({ remoteEndpoint: local.baseUrl });
  const handle = { segments: ['compaction', 'remoteEndpoint'], definition: {} };
  assert.equal(config.rawValue(handle), local.baseUrl);
  config.compaction.remoteEndpoint = hosted.baseUrl;
  assert.throws(() => config.rawValue(handle), /local_ai_model_required/);
  const child = new Config();
  child.compaction = { methods: { remote: { remoteEndpoint: hosted.baseUrl } } };
  assert.throws(() => child.rawValue({ segments: ['compaction', 'methods'], definition: {} }), /local_ai_model_required/);
});
