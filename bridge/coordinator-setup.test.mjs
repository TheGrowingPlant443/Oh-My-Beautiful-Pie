import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import test from 'node:test';
import coordinatorSetup from './coordinator-setup.ts';

function fixture(role, run) {
  const root = mkdtempSync(path.join(tmpdir(), 'omp-private-setup-'));
  const keys = ['OMP_MANAGER_PRIVATE_SETUP_DIR', 'OMP_MANAGER_PRIVATE_SETUP_ROLE'];
  const previous = keys.map(key => process.env[key]);
  process.env[keys[0]] = root;
  process.env[keys[1]] = role;
  try {
    let handler;
    coordinatorSetup({ on(name, callback) { assert.equal(name, 'before_agent_start'); handler = callback; } });
    return Promise.resolve(run(root, handler)).finally(() => {
      keys.forEach((key, index) => { if (previous[index] === undefined) delete process.env[key]; else process.env[key] = previous[index]; });
      rmSync(root, { recursive: true, force: true });
    });
  } catch (error) {
    keys.forEach((key, index) => { if (previous[index] === undefined) delete process.env[key]; else process.env[key] = previous[index]; });
    rmSync(root, { recursive: true, force: true });
    throw error;
  }
}

for (const role of ['manager', 'architect']) {
  test(`${role} private instructions reload only for the main agent and deduplicate`, () => fixture(role, async (root, handler) => {
    const ctx = { agent: { kind: 'main' } };
    writeFileSync(path.join(root, 'AGENTS.md'), 'first instructions');
    writeFileSync(path.join(root, 'RULES.md'), 'private rules');
    const initial = await handler({ systemPrompt: ['base'] }, ctx);
    assert.equal(initial.systemPrompt.length, 3);
    assert.ok(initial.systemPrompt[1].startsWith(`<!-- omp-manager:${role}:private-setup:${root}:AGENTS.md -->`));
    assert.ok(initial.systemPrompt[1].includes(`# ${role === 'manager' ? 'Manager' : 'Architect'} setup: AGENTS.md`));
    writeFileSync(path.join(root, 'AGENTS.md'), 'updated instructions');
    const updated = await handler(initial, ctx);
    assert.equal(updated.systemPrompt.length, 3);
    assert.ok(updated.systemPrompt[1].includes('updated instructions'));
    assert.ok(!updated.systemPrompt.join('\n').includes('first instructions'));
    assert.equal(await handler(initial, { agent: { kind: 'subagent' } }), undefined);
  }));

  test(`${role} unreadable instructions notify and abort rather than continuing`, () => fixture(role, async (root, handler) => {
    mkdirSync(path.join(root, 'AGENTS.md'));
    const notices = [];
    let aborted = false;
    await assert.rejects(handler({ systemPrompt: ['base'] }, {
      agent: { kind: 'main' }, ui: { notify: (...args) => notices.push(args) },
      abort: async () => { aborted = true; },
    }));
    assert.equal(aborted, true);
    assert.equal(notices.length, 1);
    assert.equal(notices[0][1], 'error');
    assert.match(notices[0][0], new RegExp(`Cannot read ${role === 'manager' ? 'Manager' : 'Architect'} setup AGENTS.md`));
  }));
}

test('private setup rejects an unknown role', () => {
  assert.throws(() => fixture('worker', () => {}), /role must be manager or architect/);
});
