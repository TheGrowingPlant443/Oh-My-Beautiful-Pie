import test from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdtemp, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { outstandingToolCalls, installWorkspaceChanges, WORKSPACE_COMPATIBILITY_ERROR } from './omp-workspace.mjs';
import { controlAllowed, workspaceChangeRole } from './omp-manager.mjs';

test('workspace controls are human-only even for a Manager with owned workers', () => {
  for (const action of ['workspace_change', 'cancel_workspace_change', 'workspace_plan_prompt']) {
    assert.equal(controlAllowed({ role: 'instance', owner: 'manager', action, source: 'manager' }), false);
    assert.equal(controlAllowed({ role: 'manager', owner: null, action, source: 'human' }), true);
  }
});

test('every ordinary agent may be relocated by the human but helpers and plan sessions never', () => {
  assert.equal(workspaceChangeRole({ role: 'instance', architect: false, helper: false, plan: false }), true);
  assert.equal(workspaceChangeRole({ role: 'manager', architect: false, helper: false, plan: false }), true);
  assert.equal(workspaceChangeRole({ role: 'instance', architect: true, helper: false, plan: false }), true);
  assert.equal(workspaceChangeRole({ role: 'instance', architect: false, helper: true, plan: false }), false);
  assert.equal(workspaceChangeRole({ role: 'instance', architect: false, helper: false, plan: true }), false);
  for (const role of ['instance', 'manager']) assert.equal(controlAllowed({ role, owner: 'manager', action: 'workspace_change', source: 'manager' }), false);
});

test('native batch accounting includes siblings not yet dispatched and waits for actual results', () => {
  const assistant = { role: 'assistant', content: [
    { type: 'toolCall', id: 'wait-call', name: 'wait' },
    { type: 'toolCall', id: 'delayed-eval', name: 'eval' },
  ] };
  const state = { messages: [assistant], pendingToolCalls: new Set(['wait-call']) };
  assert.deepEqual([...outstandingToolCalls(state)], ['wait-call', 'delayed-eval']);
  state.messages.push({ role: 'toolResult', toolCallId: 'delayed-eval' });
  assert.deepEqual([...outstandingToolCalls(state)], ['wait-call']);
  state.messages.push({ role: 'toolResult', toolCallId: 'wait-call' });
  // An end event without the synchronous native pending-set update cannot
  // falsely identify quiescence; both sources must agree.
  assert.deepEqual([...outstandingToolCalls(state)], ['wait-call']);
  state.pendingToolCalls.clear();
  assert.equal(outstandingToolCalls(state).size, 0);
});

test('missing native move hooks fail explicitly rather than adopting registry-only cwd', async () => {
  const workspace = installWorkspaceChanges({ pi: {} }, { emit() {}, request() {}, hasPendingDecisions: () => false });
  await assert.rejects(workspace.control('workspace_change', { requestId: 'request', targetFolder: path.resolve('destination') }), { message: WORKSPACE_COMPATIBILITY_ERROR });
  await workspace.dispose();
});

test('saved-session relocation uses the installed native writer lease, preserves transcript/artifacts, and rolls back rejected persistence', {
  skip: !process.env.OMP_SDK_DIR ? 'Set OMP_SDK_DIR for isolated native workspace integration' : false,
  timeout: 120000,
}, async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'omp-workspace-native-'));
  try {
    const { stdout } = await promisify(execFile)(process.env.BUN_BIN || 'bun', [fileURLToPath(new URL('./fixtures/workspace-saved-session.mjs', import.meta.url))], {
      env: { ...process.env, WORKSPACE_TEST_ROOT: root, HOME: root, USERPROFILE: root,
        PI_CODING_AGENT_DIR: path.join(root, 'agent'), PI_CONFIG_DIR: path.join(root, 'config'), OMP_PROFILE: 'workspace-fixture', PI_COMPILED: 'false' },
      timeout: 110000, maxBuffer: 1024 * 1024,
    });
    assert.match(stdout, /Native workspace relocation verified/);
  } finally { await rm(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 }); }
});
