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
manager.appendMessage({ role: 'user', content: 'Native workspace transcript sentinel', timestamp: Date.now() });
await manager.flush();
const sourceFile = manager.getSessionFile();
const sessionId = manager.getSessionId();
const artifacts = manager.getArtifactsDir();
await mkdir(artifacts, { recursive: true });
await writeFile(path.join(artifacts, 'sentinel.txt'), 'Native artifact sentinel');
await manager.close();
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
let reopened = await SessionManager.open(native.sessionFile, undefined, undefined, { throwIfMissing: true });
assert.equal(reopened.getSessionId(), sessionId);
assert.equal(reopened.getCwd(), destination);
assert.ok(reopened.getEntries().some(entry => entry.type === 'message' && entry.message.content === 'Native workspace transcript sentinel'));
assert.equal(await readFile(path.join(reopened.getArtifactsDir(), 'sentinel.txt'), 'utf8'), 'Native artifact sentinel');
await reopened.close();
const undone = await relocateSavedSession({ sdkDir, profile: 'workspace-fixture', sessionFile: native.sessionFile, targetFolder: rejected, cwd: destination }, async identity => {
  assert.equal(identity.cwd, rejected);
  return false;
});
assert.equal(undone.committed, false);
assert.equal(undone.identity.cwd, destination);
assert.equal(undone.identity.sessionFile, native.sessionFile);
await assert.rejects(relocateSavedSession({ sdkDir, profile: 'workspace-fixture', sessionFile: path.join(root, 'missing.jsonl'), targetFolder: rejected, cwd: destination }, async () => true));

// A second OS process, not another manager in the same process: native leases
// are process-owned and intentionally refcounted for same-process use.
const holder = spawn(process.env.BUN_BIN || 'bun', ['--eval', `
  const {SessionManager}=await import(${JSON.stringify(pathToFileURL(path.join(sdkDir, 'src/session/session-manager.ts')).href)});
  const manager=await SessionManager.open(${JSON.stringify(native.sessionFile)},undefined,undefined,{throwIfMissing:true});
  manager.appendMessage({role:'user',content:'Writer lease sentinel',timestamp:Date.now()});
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
reopened = await SessionManager.open(native.sessionFile, undefined, undefined, { throwIfMissing: true });
assert.equal(reopened.getCwd(), destination);
assert.equal(reopened.getSessionId(), sessionId);
await reopened.close();
console.log('Native workspace relocation verified');
