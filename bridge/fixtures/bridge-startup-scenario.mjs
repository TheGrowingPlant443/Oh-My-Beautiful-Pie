import assert from 'node:assert/strict';
import { readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import managerBridge from '../omp-manager.mjs';

export default function bridgeStartup(pi) {
  const noticeMode = process.env.BRIDGE_MANAGER_NOTICE === '1';
  let session;
  const sessions = new Set();
  const prototype = pi.pi.AgentSession.prototype;
  const original = Object.getOwnPropertyDescriptor(prototype, 'getAllToolInfos');
  Object.defineProperty(prototype, 'getAllToolInfos', { ...original, value: function (...args) {
    sessions.add(this);
    return original.value.apply(this, args);
  } });
  managerBridge(pi);
  pi.on('session_start', (_event, ctx) => {
    // Native init unwinds after session_start; checking from inside its handler deadlocks that boundary.
    setTimeout(async () => {
      try {
        const sessionFile = ctx.sessionManager.getSessionFile() ?? null;
        const workspace = pi.pi.InteractiveMode.prototype[Symbol.for('omp-manager.workspace-host')];
        const observer = pi.pi.BashTool.prototype[Symbol.for('omp-manager.terminal-observer')];
        const capabilities = workspace.nativeUiCapabilities();
        const eager = process.env.BRIDGE_EXPECT_EAGER === '1';
        assert.equal(capabilities.agentHub, eager, 'cached startup must capture the native host');
        assert.equal(capabilities.nativeDecisions, eager, 'capture must precede native UI forwarding');
        assert.equal(workspace.nativeGuardAvailable(), eager);
        if (eager) {
          pi.getAllTools();
          session = [...sessions].find(candidate => candidate.sessionId === ctx.sessionManager.getSessionId());
          assert.ok(session);
          if (noticeMode) {
            const notices = ctx.sessionManager.getBranch().filter(entry => entry.type === 'custom_message' && entry.customType === 'omp-manager.ready');
            assert.equal(notices.length, 1, 'a saved Manager conversation must retain one startup notice');
            await ctx.sessionManager.ensureOnDisk();
          } else {
            const result = await session.getToolByName('eval').execute('startup-eval', {
              language: 'js', code: 'print("bridge startup command")', timeout: 10,
            });
            assert.equal(result.isError === true, false);
            const row = observer.history().rows.find(row => row.toolCallId === 'startup-eval');
            assert.ok(row, 'real Eval execution must reach command history');
            assert.equal(row.status, 'completed');
            assert.equal(row.kind, 'js');
          }
          await session.dispose();
        }
        await writeFile(process.env.BRIDGE_STARTUP_RESULT, JSON.stringify({ eager, capabilities, sessionFile }));
        process.exit(0);
      } catch (error) {
        await session?.dispose().catch(() => {});
        await writeFile(process.env.BRIDGE_STARTUP_RESULT, JSON.stringify({ error: String(error.stack ?? error) }));
        process.exit(1);
      }
    }, 0);
  });
}

if (import.meta.main) {
  const root = process.env.BRIDGE_STARTUP_ROOT;
  const noticeMode = process.env.BRIDGE_MANAGER_NOTICE === '1';
  let sessionFile = null;
  // First load records OMP's verdict; the unchanged module is deferred on the next launch unless the flag is present.
  for (const [index, flag, eager] of (noticeMode ? [[0, true, true], [1, true, true]] : [[0, true, true], [1, false, false], [2, true, true]])) {
    const result = path.join(root, `startup-${index}.json`);
    let output = '';
    const child = Bun.spawn([process.env.OMP_BIN, '--no-extensions', '--extension', fileURLToPath(import.meta.url),
      ...(flag ? ['--omp-manager-bridge'] : []), ...(noticeMode ? [] : ['--no-session']),
      ...(sessionFile ? ['--resume', sessionFile] : []),
      '--no-skills', '--no-rules', '--no-lsp', '--no-title', '--auto-approve', '--cwd', root], {
      env: { ...process.env, BRIDGE_EXPECT_EAGER: eager ? '1' : '0', BRIDGE_STARTUP_RESULT: result },
      terminal: { cols: 100, rows: 30, data(_terminal, bytes) { output = (output + Buffer.from(bytes).toString()).slice(-16384); } },
    });
    const timer = setTimeout(() => child.kill(), 30000);
    try {
      const status = await child.exited;
      const observation = JSON.parse(await readFile(result, 'utf8').catch(() => '{}'));
      assert.equal(status, 0, observation.error ?? output);
      assert.equal(observation.eager, eager);
      if (noticeMode) {
        assert.ok(observation.sessionFile, 'Manager announcement must persist its conversation');
        sessionFile = observation.sessionFile;
      }
    } finally { clearTimeout(timer); child.terminal.close(); }
  }
  console.log(noticeMode ? 'Manager startup notice survives reopening without duplication' : 'Cached native bridge startup verified');
}
