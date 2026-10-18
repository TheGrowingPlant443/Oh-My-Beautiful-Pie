import test from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import managerBridge from './omp-manager.mjs';
import { TOOL_NAME, codeQualityParameters, codeQualityPayload, registerCodeQualityTool } from './omp-code-quality.mjs';

const ID = '123e4567-e89b-42d3-a456-426614174000';

// Every schema builder returns another builder, so any registration code can run against it.
const chain = () => new Proxy(() => chain(), { get: () => chain(), apply: () => chain() });
const permissiveZod = new Proxy({}, { get: () => chain });

test('payloads carry only the fields of their section and are validated before any transport', () => {
  assert.deepEqual(codeQualityPayload({ section: 'scans' }), { section: 'scans' });
  assert.deepEqual(codeQualityPayload({ section: 'summary', scanId: ID }), { section: 'summary', scanId: ID });
  assert.deepEqual(codeQualityPayload({ section: 'findings', scanId: ID, offset: 0, limit: 100, tool: 'ruff', severity: 'high', unset: undefined }),
    { section: 'findings', scanId: ID, offset: 0, limit: 100, tool: 'ruff', severity: 'high' });
  assert.deepEqual(codeQualityPayload({ section: 'artifact', scanId: ID, reportName: 'megalinter-report.json', offset: 65536 }),
    { section: 'artifact', scanId: ID, reportName: 'megalinter-report.json', offset: 65536 });
  const refused = [
    [{}, /section must be one of/], [null, /section must be one of/], [{ section: 'export' }, /section must be one of/],
    [{ section: 'scans', scanId: ID }, /scans does not accept scanId/],
    [{ section: 'scans', folder: 'C:/other' }, /does not accept folder/],
    [{ section: 'scans', instanceId: 'x' }, /does not accept instanceId/],
    [{ section: 'findings', scanId: ID, path: 'C:/x' }, /does not accept path/],
    [{ section: 'summary', scanId: ID, limit: 5 }, /summary does not accept limit/],
    [{ section: 'summary' }, /summary requires scanId/],
    [{ section: 'artifact', scanId: ID }, /artifact requires reportName/],
    [{ section: 'findings', scanId: 'not-a-uuid' }, /scanId must be a scan id/],
    [{ section: 'findings', scanId: ID, offset: -1 }, /offset must be a nonnegative safe integer/],
    [{ section: 'findings', scanId: ID, offset: 1.5 }, /offset must be/],
    [{ section: 'findings', scanId: ID, offset: Number.MAX_SAFE_INTEGER + 1 }, /offset must be/],
    [{ section: 'findings', scanId: ID, limit: 0 }, /limit must be an integer from 1 to 100/],
    [{ section: 'findings', scanId: ID, limit: 101 }, /limit must be an integer from 1 to 100/],
    [{ section: 'findings', scanId: ID, severity: 'bogus' }, /severity must be one of/],
    [{ section: 'findings', scanId: ID, tool: '' }, /tool must be 1\.\.200 characters/],
    [{ section: 'findings', scanId: ID, tool: 'x'.repeat(201) }, /tool must be 1\.\.200 characters/],
    ...['../x', 'a/b', 'a\\b', '..', 'a..b', ''].map(reportName => [{ section: 'artifact', scanId: ID, reportName }, /reportName must be a report name/]),
  ];
  for (const [params, message] of refused) assert.throws(() => codeQualityPayload(params), message, JSON.stringify(params));
});

test('the registered tool is flat, strict, read-only and has no way to name a repository', async () => {
  let tool, strict = false;
  const builder = () => ({ optional: builder, int: builder, min: builder, max: builder, regex: builder });
  const zod = { string: builder, number: builder, enum: builder, object: shape => ({ shape, strict() { strict = true; return this; } }) };
  const calls = [];
  registerCodeQualityTool({ zod, registerTool: definition => { tool = definition; } }, async (action, payload) => { calls.push([action, payload]); return { total: 0, scans: [] }; });
  assert.equal(tool.name, TOOL_NAME);
  assert.equal(tool.name, 'code_quality_report');
  assert.equal(tool.approval({ section: 'scans' }), 'read');
  assert.equal(strict, true);
  assert.deepEqual(Object.keys(tool.parameters.shape).sort(), ['limit', 'offset', 'reportName', 'scanId', 'section', 'severity', 'tool']);
  const ok = await tool.execute('1', { section: 'scans' });
  assert.equal(ok.isError, undefined);
  assert.deepEqual(JSON.parse(ok.content[0].text), { total: 0, scans: [] });
  assert.deepEqual(calls, [['code_quality', { section: 'scans' }]]);
  for (const params of [{ section: 'findings' }, { section: 'scans', folder: 'C:/x' }, { section: 'artifact', scanId: ID, reportName: '../x' }]) {
    const refused = await tool.execute('bad', params);
    assert.equal(refused.isError, true);
  }
  assert.equal(calls.length, 1, 'invalid calls never reach the app');
  registerCodeQualityTool({ zod, registerTool: definition => { tool = definition; } }, async () => { throw new Error('That scan does not belong to this repository'); });
  const denied = await tool.execute('2', { section: 'summary', scanId: ID });
  assert.equal(denied.isError, true);
  assert.match(denied.content[0].text, /does not belong to this repository/);
  const controller = new AbortController();
  controller.abort(new Error('cancelled'));
  await assert.rejects(tool.execute('3', { section: 'scans' }, controller.signal), /cancelled/);
});

test('every agent role registers the tool, including helpers and plan agents', () => {
  const keys = ['OMP_MANAGER_BRIDGE_ENDPOINT', 'OMP_MANAGER_BRIDGE_TOKEN', 'OMP_MANAGER_BRIDGE_INSTANCE_ID', 'OMP_MANAGER_BRIDGE_GENERATION',
    'OMP_MANAGER_BRIDGE_ROLE', 'OMP_MANAGER_HELPER_REPO', 'OMP_MANAGER_PLAN_SESSION', 'OMP_MANAGER_ARCHITECT_DIR', 'OMP_MANAGER_BROWSER_REQUIRED',
    'OMP_MANAGER_TERMINAL_DIR', 'OMP_MANAGER_SDK_DIR'];
  const saved = Object.fromEntries(keys.map(key => [key, process.env[key]]));
  const roles = {
    manager: { OMP_MANAGER_BRIDGE_ROLE: 'manager' },
    worker: { OMP_MANAGER_BRIDGE_ROLE: 'instance' },
    plan: { OMP_MANAGER_BRIDGE_ROLE: 'instance', OMP_MANAGER_PLAN_SESSION: '1' },
    helper: { OMP_MANAGER_BRIDGE_ROLE: 'instance', OMP_MANAGER_HELPER_REPO: process.cwd() },
  };
  try {
    for (const [name, env] of Object.entries(roles)) {
      for (const key of keys) delete process.env[key];
      Object.assign(process.env, { OMP_MANAGER_BRIDGE_ENDPOINT: '127.0.0.1:1', OMP_MANAGER_BRIDGE_TOKEN: 'token', OMP_MANAGER_BRIDGE_INSTANCE_ID: name, OMP_MANAGER_BRIDGE_GENERATION: '0' }, env);
      const names = [];
      managerBridge({ zod: permissiveZod, registerTool: definition => names.push(definition.name), on() {}, sendMessage() {}, sendUserMessage() {} });
      assert.ok(names.includes('code_quality_report'), `${name} registers code_quality_report (got ${names.join(', ')})`);
      assert.equal(names.filter(item => item === 'code_quality_report').length, 1, name);
      assert.equal(names.includes('manage_sessions'), name === 'manager', `${name}: manage_sessions stays Manager-only`);
      assert.equal(names.includes('repo_inspect'), name === 'helper', `${name}: repo_inspect stays helper-only`);
    }
  } finally {
    for (const key of keys) { if (saved[key] === undefined) delete process.env[key]; else process.env[key] = saved[key]; }
  }
});

const sdk = process.env.OMP_SDK_DIR;
const sdkTest = sdk ? {} : { skip: 'Set OMP_SDK_DIR to exercise installed pi.zod' };

test('the installed SDK validator rejects unknown keys and out-of-range values', sdkTest, async () => {
  const schema = codeQualityParameters(await import(pathToFileURL(path.join(sdk, '..', 'omptype', 'dist', 'js', 'zod.js')).href));
  for (const call of [{ section: 'scans' }, { section: 'summary', scanId: ID }, { section: 'findings', scanId: ID, offset: 0, limit: 1, severity: 'info', tool: 'ruff' },
    { section: 'artifact', scanId: ID, reportName: 't.json', offset: 0 }]) assert.deepEqual(schema.parse(call), call);
  for (const call of [{ section: 'scans', folder: 'C:/x' }, { section: 'scans', instanceId: 'x' }, { section: 'summary', scanId: 'nope' },
    { section: 'findings', scanId: ID, limit: 101 }, { section: 'findings', scanId: ID, limit: 0 }, { section: 'findings', scanId: ID, offset: -1 },
    { section: 'findings', scanId: ID, severity: 'bogus' }, { section: 'export' }, {}]) assert.throws(() => schema.parse(call), JSON.stringify(call));
});
