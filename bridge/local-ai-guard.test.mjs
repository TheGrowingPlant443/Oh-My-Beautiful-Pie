import assert from 'node:assert/strict';
import { mkdtempSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

const guardUrl = new URL('./local-ai-guard.ts', import.meta.url).href;
function child(root, mode, events) {
  const script = `
    import guard from ${JSON.stringify(guardUrl)};
    class Registry { resolver() {} getApiKey() {} getApiKeyWithCredentialForProvider() {} }
    class Config { rawValue() { return undefined; } }
    const handlers = new Map();
    guard({ pi: { ModelRegistry: Registry, Settings: Config, settings: new Config() }, on(name, handler) { handlers.set(name, handler); } });
    for (const [event, id] of ${JSON.stringify(events)}) {
      await handlers.get(event)({}, { sessionManager: { getSessionId: () => id } });
    }
  `;
  return spawnSync(process.execPath, ['--experimental-strip-types', '--input-type=module', '-e', script], {
    env: { ...process.env, OMP_MANAGER_LOCAL_AI_SETUP_DIR: root, OMP_MANAGER_LOCAL_AI_SETUP_MODE: mode }, encoding: 'utf8', timeout: 10_000,
  });
}

test('forks and branches are marked without another turn and cannot reopen as ordinary conversations', () => {
  const root = mkdtempSync(path.join(tmpdir(), 'omp-local-guard-'));
  try {
    const result = child(root, 'local', [['session_start', 'original'], ['session_switch', 'fork'], ['session_branch', 'branch']]);
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(readdirSync(path.join(root, '.local-sessions')).sort(), ['branch.json', 'fork.json', 'original.json']);
    for (const id of ['fork', 'branch']) {
      assert.equal(child(root, 'ordinary', [['session_start', id]]).status, 78);
    }
    assert.equal(child(root, 'ordinary', [['session_start', 'clean']]).status, 0);
  } finally { rmSync(root, { recursive: true, force: true }); }
});
