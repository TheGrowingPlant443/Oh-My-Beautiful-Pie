import assert from 'node:assert/strict';
import { mkdir, readFile, writeFile, access } from 'node:fs/promises';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { relocateSavedSession } from '../omp-workspace.mjs';

const root = process.env.WORKSPACE_TEST_ROOT;
assert.ok(root && process.env.OMP_SDK_DIR);
const sdkDir = process.env.OMP_SDK_DIR;
const dirs = await import(pathToFileURL(Bun.resolveSync('@oh-my-pi/pi-utils/dirs', sdkDir)).href);
dirs.setProfile('workspace-fixture');
const { SessionManager } = await import(pathToFileURL(path.join(sdkDir, 'src/session/session-manager.ts')).href);
const source = path.join(root, 'source');
const destination = path.join(root, 'destination');
const rejected = path.join(root, 'rejected');
await Promise.all([source, destination, rejected].map(folder => mkdir(folder, { recursive: true })));
const manager = SessionManager.create(source);
const conversation = [
  { role: 'user', content: 'Native workspace transcript sentinel', timestamp: Date.now() },
  { role: 'assistant', content: [{ type: 'text', text: 'Native workspace reply sentinel' }],
    api: 'openai-responses', provider: 'workspace-fixture', model: 'workspace-fixture',
    usage: { input: 1, output: 1, cacheRead: 0, cacheWrite: 0, totalTokens: 2,
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
    stopReason: 'stop', timestamp: Date.now() },
];
// Native journals stay lazy until the first assistant turn; flush alone does not create one.
for (const message of conversation) manager.appendMessage(message);
await manager.flush();
const sourceFile = manager.getSessionFile();
const sessionId = manager.getSessionId();
const artifacts = manager.getArtifactsDir();
await mkdir(artifacts, { recursive: true });
await writeFile(path.join(artifacts, 'sentinel.txt'), 'Native artifact sentinel');
await manager.close();
async function assertSavedConversation(sessionFile, cwd, messages) {
  const reopened = await SessionManager.open(sessionFile, undefined, undefined, { throwIfMissing: true });
  try {
    assert.equal(reopened.getSessionId(), sessionId);
    assert.equal(reopened.getCwd(), cwd);
    assert.deepEqual(reopened.getEntries().filter(entry => entry.type === 'message').map(entry => entry.message), messages);
    assert.equal(await readFile(path.join(reopened.getArtifactsDir(), 'sentinel.txt'), 'utf8'), 'Native artifact sentinel');
  } finally { await reopened.close(); }
}
await assertSavedConversation(sourceFile, source, conversation);
let native;
const moved = await relocateSavedSession({ sdkDir, profile: 'workspace-fixture', sessionFile: sourceFile, targetFolder: destination, cwd: source }, async identity => {
  assert.equal(identity.sessionId, sessionId);
  assert.equal(identity.cwd, destination);
  native = identity;
  await access(identity.sessionFile);
  return true;
});
assert.equal(moved.committed, true);
assert.notEqual(native.sessionFile, sourceFile);
await assertSavedConversation(native.sessionFile, destination, conversation);
const undone = await relocateSavedSession({ sdkDir, profile: 'workspace-fixture', sessionFile: native.sessionFile, targetFolder: rejected, cwd: destination }, async identity => {
  assert.equal(identity.cwd, rejected);
  return false;
});
assert.equal(undone.committed, false);
assert.equal(undone.identity.cwd, destination);
assert.equal(undone.identity.sessionFile, native.sessionFile);
await assertSavedConversation(native.sessionFile, destination, conversation);
await assert.rejects(relocateSavedSession({ sdkDir, profile: 'workspace-fixture', sessionFile: path.join(root, 'missing.jsonl'), targetFolder: rejected, cwd: destination }, async () => true));

// A second OS process, not another manager in the same process: native leases
// are process-owned and intentionally refcounted for same-process use.
const leaseMessage = { role: 'user', content: 'Writer lease sentinel', timestamp: Date.now() };
const holder = spawn(process.env.BUN_BIN || 'bun', ['--eval', `
  const {SessionManager}=await import(${JSON.stringify(pathToFileURL(path.join(sdkDir, 'src/session/session-manager.ts')).href)});
  const manager=await SessionManager.open(${JSON.stringify(native.sessionFile)},undefined,undefined,{throwIfMissing:true});
  manager.appendMessage(${JSON.stringify(leaseMessage)});
  await manager.flush();
  console.log('writer-ready');
  await Bun.stdin.text();
  await manager.close();
`], { env: process.env, stdio: ['pipe', 'pipe', 'pipe'] });
try {
  const ready = await Promise.race([once(holder.stdout, 'data').then(([bytes]) => String(bytes)), once(holder, 'exit').then(([code]) => { throw new Error(`Lease holder exited ${code}`); })]);
  assert.match(ready, /writer-ready/);
  await assert.rejects(relocateSavedSession({ sdkDir, profile: 'workspace-fixture', sessionFile: native.sessionFile, targetFolder: rejected, cwd: destination }, async () => true), /live|writes|writer|lease|lock/i);
} finally {
  holder.stdin.end();
  if (holder.exitCode === null) await once(holder, 'exit');
}
await assertSavedConversation(native.sessionFile, destination, [...conversation, leaseMessage]);
console.log('Native workspace relocation verified');
