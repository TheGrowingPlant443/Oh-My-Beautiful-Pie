import test from 'node:test';
import assert from 'node:assert/strict';
import { sanitizeConfig } from './omp-config.mjs';

const definitions = [
  { id: 'modelRoles', segments: ['modelRoles'] },
  { id: 'modelPresets', segments: ['modelPresets'] },
  { id: 'theme', segments: ['theme'] },
  { id: 'tools.approvalMode', segments: ['tools', 'approvalMode'] },
  { id: 'auth.broker.token', segments: ['auth', 'broker', 'token'], isCredential: true },
  { id: 'extensions', segments: ['extensions'] },
  { id: 'telemetry.enabled', segments: ['telemetry', 'enabled'] },
];

test('portable configs keep model roles and safe settings without exporting credentials or executable hooks', () => {
  const secret = 'DO_NOT_EXPORT_THIS_SECRET';
  const result = sanitizeConfig({
    modelRoles: { default: 'openai/model', plan: 'openai/model', slow: null },
    theme: 'dark',
    tools: { approvalMode: 'always-ask' },
    modelPresets: { example: { modelRoles: { default: 'openai/model' }, headers: { Authorization: secret }, env: { PRIVATE_KEY: secret } } },
    auth: { broker: { token: secret } },
    extensions: ['./unreviewed-code.ts'],
    telemetry: { enabled: true },
    unknown: { secret },
  }, definitions);
  assert.deepEqual({ ...result.settings.modelRoles }, { default: 'openai/model', plan: 'openai/model', slow: null });
  assert.equal(result.settings.tools.approvalMode, 'always-ask');
  assert.equal(result.settings.theme, 'dark');
  assert.equal(JSON.stringify(result).includes(secret), false);
  assert.equal(Object.hasOwn(result.settings, 'extensions'), false);
  assert.equal(Object.hasOwn(result.settings, 'auth'), false);
  assert.deepEqual(result.omitted, ['auth.broker.token', 'extensions', 'modelPresets.example.env', 'modelPresets.example.headers', 'telemetry.enabled', 'unknown']);
});

test('empty input, invalid role selections, and recursive aliases do not become silent defaults', () => {
  for (const input of [null, [], 'not config', {}]) assert.throws(() => sanitizeConfig(input, definitions), /non-empty YAML mapping/);
  assert.throws(() => sanitizeConfig({ modelRoles: { default: 1 } }, definitions), /requires a model selection/);
  assert.throws(() => sanitizeConfig({ modelRoles: { default: '' } }, definitions), /requires a model selection/);
  assert.throws(() => sanitizeConfig({ extensions: ['unsafe.ts'] }, definitions), /No portable settings/);
  const cyclic = {}; cyclic.self = cyclic;
  assert.throws(() => sanitizeConfig({ modelPresets: cyclic }, definitions), /cyclic YAML aliases/);
});

test('untrusted prototype keys stay data exclusions rather than mutating objects', () => {
  const input = JSON.parse('{"modelRoles":{"default":"openai/model","__proto__":{"polluted":true}},"theme":"dark"}');
  const result = sanitizeConfig(input, definitions);
  assert.equal({}.polluted, undefined);
  assert.equal(Object.hasOwn(result.settings.modelRoles, '__proto__'), false);
  assert.deepEqual(result.omitted, ['modelRoles.__proto__']);
});
