import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import path from 'node:path';
import { installTerminalCommands, registerCommandHistoryTool, HELD_ERROR } from '../omp-terminal.mjs';

// Loaded as an explicit extension by the real OMP CLI in rpc mode. No model is called: the scenario drives the
// real native Bash/Eval tools of the captured session and observes the real bridge observer through its events.
export default async function terminalScenario(pi) {
  const root = process.env.BROWSER_TEST_ROOT;
  const events = [];
  const abortCalls = [];
  let session;
  // The scenario captures the live session first; the observer wraps on top and must restore to this wrapper.
  const originalInfos = pi.pi.AgentSession.prototype.getAllToolInfos;
  const originalBash = pi.pi.BashTool.prototype.execute;
  const originalEval = pi.pi.EvalTool.prototype.execute;
  function captureInfos(...args) { session = this; return originalInfos.apply(this, args); }
  pi.pi.AgentSession.prototype.getAllToolInfos = captureInfos;
  const observer = installTerminalCommands(pi, {
    emit: (kind, payload) => events.push({ kind, payload, at: Date.now() }),
    historyRoot: path.join(root, 'ledger'),
    sdkDir: process.env.OMP_SDK_DIR,
  });
  registerCommandHistoryTool(pi, observer);
  pi.on('session_start', (_event, ctx) => { observer.attachContext(ctx); void run(); });
  const text = result => result.content.filter(block => block.type === 'text').map(block => block.text).join('\n');
  const commands = () => events.filter(event => event.kind === 'terminal_command').map(event => event.payload.command);
  const finals = () => commands().filter(command => command.endedAt !== null);
  const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
  async function until(check, label, ms = 20000) {
    const end = Date.now() + ms;
    for (;;) {
      const value = await check();
      if (value) return value;
      if (Date.now() > end) throw new Error(`Timed out waiting for ${label}`);
      await sleep(50);
    }
  }
  const latest = id => commands().filter(command => command.executionId === id).at(-1);

  async function run() {
    let exitCode = 0;
    try {
      assert.ok(session, 'native session was not captured');
      const snapshot = observer.snapshot();
      assert.equal(snapshot.supported, true, `observer must capture the real session: ${snapshot.error}`);
      const bash = (params, id = 'direct-' + crypto.randomUUID()) => session.getToolByName('bash').execute(id, params, undefined, undefined, undefined);
      const evaluate = (params, id = 'direct-' + crypto.randomUUID()) => session.getToolByName('eval').execute(id, params, undefined, undefined, undefined);
      const originalAbort = session.abort;
      session.abort = function(options) { abortCalls.push(options); return originalAbort.call(this, options); };

      // 1. Streaming foreground Bash: first part is visible before completion, one end event, exact history.
      const firstMarker = 'printf first; sleep 2; printf second';
      const running = bash({ command: firstMarker });
      const first = await until(() => commands().find(command => command.preview === firstMarker && command.endedAt === null), 'running bash tab');
      assert.equal(first.tab, 2); assert.equal(first.kind, 'bash'); assert.equal(first.status, 'running');
      await until(() => events.some(event => event.kind === 'terminal_output' && event.payload.executionId === first.executionId && event.payload.text.includes('first') && !event.payload.text.includes('second')), 'streamed first output');
      const result = await running;
      assert.match(text(result), /second/);
      const done = latest(first.executionId);
      assert.equal(done.status, 'completed'); assert.equal(done.exitCode, 0); assert.equal(done.tab, null);
      assert.equal(commands().filter(command => command.executionId === first.executionId && command.endedAt !== null).length, 1, 'exactly one end event');
      const ledgerDir = path.join(root, 'ledger');
      const sessions = await readdir(ledgerDir);
      assert.equal(sessions.length, 1);
      const script = await readFile(path.join(ledgerDir, sessions[0], first.executionId, 'script.txt'), 'utf8');
      assert.equal(script, firstMarker);
      console.log('1 ok: streamed foreground bash, one end event, ledger script');

      // 2. Failure exit code.
      const failed = await bash({ command: 'exit 7' }).catch(error => error);
      const failure = finals().find(command => command.preview === 'exit 7');
      assert.ok(failure, 'failed command is recorded'); assert.equal(failure.status, 'failed'); assert.equal(failure.exitCode, 7);
      void failed;
      console.log('2 ok: exit code 7 => failed/7');

      // 3. Concurrent calls keep stable slots; slot is reused after the first exits.
      const slow = bash({ command: 'printf alpha; sleep 3; printf alpha-end' });
      const quick = bash({ command: 'printf beta; sleep 1; printf beta-end' });
      const slowRow = await until(() => commands().find(command => command.preview.startsWith('printf alpha;') && command.endedAt === null), 'alpha running');
      const quickRow = await until(() => commands().find(command => command.preview.startsWith('printf beta;') && command.endedAt === null), 'beta running');
      assert.deepEqual([slowRow.tab, quickRow.tab].sort(), [2, 3]);
      await quick;
      assert.equal(latest(quickRow.executionId).status, 'completed');
      const third = bash({ command: 'printf gamma; sleep 1' });
      const thirdRow = await until(() => commands().find(command => command.preview.startsWith('printf gamma') && command.endedAt === null), 'gamma running');
      assert.equal(thirdRow.tab, quickRow.tab, 'the freed slot is reused');
      assert.equal(latest(slowRow.executionId).tab, slowRow.tab, 'a live tab is never renumbered');
      await Promise.all([slow, third]);
      const alpha = events.filter(event => event.kind === 'terminal_output' && event.payload.executionId === slowRow.executionId).at(-1).payload.text;
      assert.ok(alpha.includes('alpha') && !alpha.includes('beta') && !alpha.includes('gamma'), 'outputs are isolated');
      console.log('3 ok: concurrent slots 2/3, reuse, isolation');

      // 4. JS eval with a title and nested concurrent native bash calls.
      const code = 'print("outer-start");\nconst [a, b] = await Promise.all([tool.bash({command:"printf nestedA"}), tool.bash({command:"printf nestedB"})]);\nprint("outer-end");\n'.repeat(1) + '// ' + 'x'.repeat(300);
      await evaluate({ language: 'js', code, title: 'Nested calls' });
      const outer = commands().find(command => command.kind === 'js' && command.description === 'Nested calls');
      assert.ok(outer, 'eval recorded with its title'); assert.equal(latest(outer.executionId).status, 'completed');
      assert.ok(outer.preview.length <= 256, 'preview is bounded');
      const outerScript = await readFile(path.join(ledgerDir, sessions[0], outer.executionId, 'script.txt'), 'utf8');
      assert.equal(outerScript, code, 'the full multi-line source is retained');
      const nested = finals().filter(command => command.kind === 'bash' && /^printf nested[AB]$/.test(command.preview));
      assert.equal(nested.length, 2, 'nested bash calls are separate executions');
      console.log('4 ok: eval + nested bash');

      // 5. Explicit background job outlives the initiating call and settles once.
      const background = await bash({ command: 'printf bgstart; sleep 2; printf bgend', async: true });
      assert.match(text(background), /background/i);
      const bgRow = await until(() => commands().find(command => command.preview.startsWith('printf bgstart') && command.endedAt === null), 'background running');
      await sleep(500);
      assert.equal(latest(bgRow.executionId).endedAt, null, 'returning from the tool call must not close a running job');
      await until(() => latest(bgRow.executionId).endedAt !== null, 'background settlement');
      assert.equal(latest(bgRow.executionId).status, 'completed');
      assert.equal(commands().filter(command => command.executionId === bgRow.executionId && command.endedAt !== null).length, 1);
      assert.match(events.filter(event => event.kind === 'terminal_output' && event.payload.executionId === bgRow.executionId).at(-1).payload.text, /bgend/);
      console.log('5 ok: backgrounded job lifetime');

      // 6. Named service stays live after readiness; Kill stops exactly it, interrupts the agent and holds new scripts.
      const heartbeat = path.join(root, 'heartbeat.txt').replaceAll('\\', '/');
      const serviceCommand = `node -e "setInterval(()=>require('fs').writeFileSync('${heartbeat}',String(Date.now())),200);console.log('service-up')"`;
      const started = await bash({ name: 'hb', command: serviceCommand, ready: { log: 'service-up', timeout: 20 } });
      assert.ok(started.details?.service, 'native service result');
      const serviceRow = await until(() => commands().find(command => command.preview.includes('setInterval') && command.endedAt === null), 'service tab');
      await sleep(800);
      assert.equal(latest(serviceRow.executionId).endedAt, null, 'service tab persists after the starting call returned');
      const beat1 = Number(await readFile(heartbeat, 'utf8'));
      await sleep(600);
      assert.ok(Number(await readFile(heartbeat, 'utf8')) > beat1, 'service is alive');
      const killed = await observer.kill(serviceRow.executionId);
      assert.equal(killed.killed, true);
      assert.equal(latest(serviceRow.executionId).status, 'cancelled');
      assert.equal(observer.snapshot().held, true);
      assert.ok(abortCalls.some(options => options?.reason === pi.pi.USER_INTERRUPT_LABEL), 'native abort uses the user-interrupt reason');
      const stopped = Number(await readFile(heartbeat, 'utf8'));
      await sleep(800);
      assert.equal(Number(await readFile(heartbeat, 'utf8')), stopped, 'the heartbeat stopped');
      const marker = path.join(root, 'must-not-exist.txt').replaceAll('\\', '/');
      await assert.rejects(bash({ command: `echo x > "${marker}"` }), { message: HELD_ERROR });
      await assert.rejects(evaluate({ language: 'js', code: `await Bun.write(${JSON.stringify(marker)}, 'x')` }), { message: HELD_ERROR });
      await assert.rejects(readFile(marker), 'held scripts never ran');
      await session.extensionRunner.emitInput('next', undefined, 'extension');
      assert.equal(observer.snapshot().held, true, 'extension/Manager input does not unlock');
      await session.extensionRunner.emitInput('hello', undefined, 'interactive');
      assert.equal(observer.snapshot().held, false, 'a new human prompt unlocks');
      await bash({ command: 'echo ok' });
      console.log('6 ok: service lifetime, kill, abort reason, hold and unlock');

      // 7. A kill on an already-ended execution is a harmless no-op that does not interrupt anything.
      const aborts = abortCalls.length;
      assert.deepEqual(await observer.kill(first.executionId), { killed: false });
      assert.equal(abortCalls.length, aborts);
      assert.equal(observer.snapshot().held, false);
      console.log('7 ok: stale kill is a no-op');

      // 8. Python eval keeps its source when a runtime exists.
      try {
        await evaluate({ language: 'py', code: 'print("py-out")', title: 'Python cell' });
        const python = commands().find(command => command.kind === 'py');
        assert.ok(python && latest(python.executionId).endedAt !== null);
        console.log('8 ok: python eval recorded');
      } catch (error) { if (error?.name === 'AssertionError') throw error; console.log('8 skipped: no python runtime', String(error.message).slice(0, 80)); }

      // 8b. A failed command keeps its output; the agent reads its own history through the command_history tool.
      const outputOf = id => readFile(path.join(ledgerDir, sessions[0], id, 'output.txt'), 'utf8');
      await bash({ command: 'echo before-fail; exit 7' }).catch(error => error);
      const failedBash = finals().find(command => command.preview === 'echo before-fail; exit 7');
      assert.ok(failedBash && failedBash.status === 'failed' && failedBash.hasOutput === true, 'failed bash row says its output was saved');
      assert.match(await outputOf(failedBash.executionId), /before-fail/);
      await evaluate({ language: 'js', code: "console.log('eval-out');\nthrow new Error('boom-eval');", title: 'Throws' }).catch(error => error);
      const failedEval = finals().find(command => command.description === 'Throws');
      assert.ok(failedEval && failedEval.status === 'failed' && failedEval.hasOutput === true, 'failed eval row says its output was saved');
      const evalOutput = await outputOf(failedEval.executionId);
      assert.ok(evalOutput.includes('eval-out') && evalOutput.includes('boom-eval'), `eval output keeps its output and error: ${evalOutput}`);
      await bash({ command: 'echo fine' });
      const fine = finals().find(command => command.preview === 'echo fine');
      assert.equal(fine.status, 'completed'); assert.equal(fine.hasOutput, false);
      await assert.rejects(outputOf(fine.executionId), 'completed commands keep no output file');
      const history = (id, params) => session.getToolByName('command_history').execute(id, params, undefined, undefined, undefined);
      const listed = text(await history('h1', { action: 'list', status: 'failed' }));
      assert.ok(listed.includes(failedBash.executionId) && listed.includes(failedEval.executionId) && listed.includes('output saved'), listed);
      assert.ok(!listed.includes(fine.executionId), 'the status filter excludes completed commands');
      const shown = text(await history('h2', { action: 'show', executionId: failedEval.executionId }));
      for (const part of ['--- script ---', "throw new Error('boom-eval')", '--- output ---', 'eval-out']) assert.ok(shown.includes(part), `show contains ${part}: ${shown}`);
      const bad = await history('h3', { action: 'show', executionId: '../x' });
      assert.equal(bad.isError, true);
      assert.ok(text(await history('h4', { action: 'show', executionId: fine.executionId })).includes('no output recorded'));
      assert.ok(text(await history('h5', { action: 'list', query: 'before-fail' })).includes(failedBash.executionId));
      console.log('8b ok: failed output persisted, command_history lists and shows it');

      // 9. Dispose restores every native method exactly, in reverse install order.
      observer.dispose();
      assert.equal(pi.pi.AgentSession.prototype.getAllToolInfos, captureInfos);
      assert.equal(pi.pi.BashTool.prototype.execute, originalBash, 'bash wrapper removed');
      assert.equal(pi.pi.EvalTool.prototype.execute, originalEval, 'eval wrapper removed');
      console.log('Terminal command observer verified');
    } catch (error) { process.stderr.write(String(error.stack || error) + '\n'); exitCode = 1; }
    finally {
      try { observer.dispose(); pi.pi.AgentSession.prototype.getAllToolInfos = originalInfos; await session?.dispose?.(); } finally { process.exit(exitCode); }
    }
  }
}
