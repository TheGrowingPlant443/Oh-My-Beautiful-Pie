import test from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { cp, mkdir, mkdtemp, readFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';
import { configPolicyFiles } from './omp-config-policy.mjs';

test('ordered setup slots retain defaults and native overlay precedence across selection and restart', () => {
  const root = path.resolve('app');
  const setup = name => path.join(root, 'global-configs', name, 'config.yml');
  const a = setup('A'), b = setup('B'), c = setup('C'), m = setup('M');
  const x = path.join(root, 'X.yml'), y = path.join(root, 'Y.yml');
  const select = (files, source, selected, defaultPath = a) => configPolicyFiles(files, { root, defaultPath, selection: { source, path: selected } });
  assert.deepEqual(select([a, x, b, y], 'session', b), [a, x, b, y]);
  assert.deepEqual(select([a, x, b, y], 'session', c), [a, x, c, y]);
  assert.deepEqual(select([a, x, b, y], 'model', m), [a, m, x, y]);
  assert.deepEqual(select([a, x, b, y], 'default', a), [a, x, y]);
  assert.deepEqual(select([a, x, y], 'session', c), [a, c, x, y]);
  assert.deepEqual(select([a, x, y], 'session', a), [a, x, y]);
  assert.deepEqual(select([x, a, y], 'session', c), [a, x, c, y]);
  assert.deepEqual(select([x, a, y], 'session', a), [a, x, a, y]);
  assert.deepEqual(select([x, a, y], 'default', a), [a, x, y]);
  assert.deepEqual(select([b, x], 'native', null, null), [x]);
});

const sdk = process.env.OMP_SDK_DIR || process.env.OMP_MANAGER_SDK_DIR;
test('native Settings, actual model changes and scoped setup resources stay offline and preserve session intent', {
  skip: sdk ? false : 'Set OMP_SDK_DIR for the installed native SDK regression', timeout: 150000,
}, async () => {
  const root = await mkdtemp(path.join(os.homedir(), '.omp-config-policy-'));
  try {
    await mkdir(path.join(root, 'agent'), { recursive: true });
    const { version } = JSON.parse(await readFile(path.join(sdk, 'package.json'), 'utf8'));
    const nativeRoot = path.join(root, 'natives');
    await cp(path.join(process.env.PI_NATIVES_DIR || path.join(os.homedir(), '.omp', 'natives'), version), path.join(nativeRoot, version), { recursive: true });
    const config = path.relative(os.homedir(), path.join(root, 'config'));
    assert.ok(!config.split(path.sep).includes('..'));
    const { stdout } = await promisify(execFile)(process.env.BUN_BIN || 'bun', [fileURLToPath(new URL('./fixtures/config-policy-sdk.mjs', import.meta.url))], {
      env: { ...process.env, OMP_SDK_DIR: sdk, OMP_MANAGER_SDK_DIR: sdk, CONFIG_POLICY_TEST_ROOT: root,
        PI_CODING_AGENT_DIR: path.join(root, 'agent'), PI_CONFIG_DIR: config, PI_NATIVES_DIR: nativeRoot,
        PI_CONFIG_FILES: '', OMP_PROFILE: 'default', PI_COMPILED: '', OMP_MANAGER_HELPER_REPO: '' },
      timeout: 140000, maxBuffer: 1024 * 1024,
    });
    assert.match(stdout, /Installed SDK setup policy Settings\/model_changed\/assets\/idle\/intent verified/);
  } finally { await rm(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 }); }
});
