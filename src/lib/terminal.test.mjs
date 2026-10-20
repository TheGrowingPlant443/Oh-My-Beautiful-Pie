import { test } from 'node:test';
import assert from 'node:assert/strict';
import { terminalTabs, nextSelection, outputWrite, stripAnsi, shellKey, commandKey, HISTORY_KEY, DECISIONS_KEY, MONITOR_KEY } from './terminal.ts';

function command(id, tab, startedAt, description = null) {
  return { executionId: id, sessionId: 's', sourceSessionId: 's', toolCallId: id, agentId: 'main', kind: 'bash', description, preview: `run ${id}`, cwd: '.', startedAt, endedAt: null, status: 'running', exitCode: null, tab, historical: false, hasOutput: false };
}
function owner(id, name, active, shell = false) {
  return { instanceId: id, name, generation: 1, sessionId: 's', supported: true, error: null, held: false, revision: 1, active, userShell: shell ? { shellId: 'x', cwd: '.', cols: 80, rows: 24, exited: false, exitCode: null } : null };
}

test('a session pane lists the user shell, history, then live commands by slot', () => {
  const tabs = terminalTabs('a', [owner('a', 'Alpha', [command('c3', 3, 30), command('c2', 2, 40, 'Build')], true)]);
  assert.deepEqual(tabs.map(tab => tab.label), ['0 · Terminal', '1 · History', 'Outbound & Processes', '2 · Build', '3 · run c3']);
  assert.deepEqual(terminalTabs('a', [owner('a', 'Alpha', [])]).map(tab => tab.key), [HISTORY_KEY, MONITOR_KEY]);
});

test('the aggregate page labels every tab with its owner and never invents a shell', () => {
  const tabs = terminalTabs(null, [owner('a', 'Alpha', [command('c1', 2, 20)]), owner('b', 'Beta', [command('c2', 2, 10)], true)]);
  assert.deepEqual(tabs.map(tab => tab.label), ['History', 'Decisions', 'Beta · Tab 2 · run c2', 'Alpha · Tab 2 · run c1', 'Beta · User terminal']);
});

test('global decisions is permanent, never appears per-agent, and cannot steal Follow selection', () => {
  const tabs = terminalTabs(null, []);
  assert.deepEqual(tabs.map(tab => tab.key), [HISTORY_KEY, DECISIONS_KEY]);
  assert.equal(nextSelection(tabs, new Set([HISTORY_KEY]), HISTORY_KEY, true), HISTORY_KEY);
  assert.equal(nextSelection(tabs, new Set(), DECISIONS_KEY, true), DECISIONS_KEY);
  assert.equal(terminalTabs('a', []).some(tab => tab.kind === 'decisions'), false);
  const commands = terminalTabs(null, [owner('a', 'Alpha', [command('c', 2, 20)])]);
  assert.equal(nextSelection(commands, new Set(tabs.map(tab => tab.key)), DECISIONS_KEY, true), commandKey('a', 'c'));
});

test('follow selects only newly started commands; without it the selection stays and falls back to history', () => {
  const before = terminalTabs('a', [owner('a', 'Alpha', [command('c1', 2, 10)], true)]);
  const after = terminalTabs('a', [owner('a', 'Alpha', [command('c1', 2, 10), command('c2', 3, 20)], true)]);
  const known = new Set(before.map(tab => tab.key));
  assert.equal(nextSelection(after, known, shellKey('a'), false), shellKey('a'));
  assert.equal(nextSelection(after, known, shellKey('a'), true), commandKey('a', 'c2'));
  assert.equal(nextSelection(after, new Set(after.map(tab => tab.key)), shellKey('a'), true), shellKey('a'), 'output ticks do not move the selection');
  const ended = terminalTabs('a', [owner('a', 'Alpha', [command('c1', 2, 10)], true)]);
  assert.equal(nextSelection(ended, new Set(after.map(tab => tab.key)), commandKey('a', 'c2'), false), HISTORY_KEY);
  assert.equal(nextSelection(ended, new Set(after.map(tab => tab.key)), commandKey('a', 'c2'), true), commandKey('a', 'c1'));
  const closedShell = terminalTabs('a', [owner('a', 'Alpha', [], false)]);
  assert.equal(nextSelection(closedShell, new Set(), shellKey('a'), true), HISTORY_KEY, 'a closed user tab falls back to history');
  assert.equal(nextSelection(before, new Set(), '', false), shellKey('a'), 'a fresh pane starts on the shell when there is one');
});

test('output tails append only when they extend the shown text', () => {
  const frame = text => ({ executionId: 'x', revision: 1, text, truncated: false });
  assert.deepEqual(outputWrite('ab', frame('abcd')), { reset: false, data: 'cd' });
  assert.deepEqual(outputWrite('abcd', frame('cdef')), { reset: true, data: 'cdef' });
  assert.deepEqual(outputWrite('', frame('x')), { reset: false, data: 'x' });
});

test('saved failure output is shown without colour escape sequences', () => {
  assert.equal(stripAnsi('\u001b[31mred\u001b[0m text'), 'red text');
  assert.equal(stripAnsi('plain\n  at x (file.js:1:2)'), 'plain\n  at x (file.js:1:2)');
});

test('the monitor tab is permanent per agent, unnumbered, and never part of the global Terminals page', () => {
  const stopped = terminalTabs('a', []);
  assert.deepEqual(stopped.map(tab => tab.key), [HISTORY_KEY, MONITOR_KEY], 'present with no owner and no shell');
  const monitor = stopped.find(tab => tab.kind === 'monitor');
  assert.equal(monitor.label, 'Outbound & Processes');
  assert.equal(monitor.instanceId, 'a');
  const busy = terminalTabs('a', [owner('a', 'Alpha', [command('c1', 2, 10)], true)]);
  assert.deepEqual(busy.map(tab => tab.key), [shellKey('a'), HISTORY_KEY, MONITOR_KEY, commandKey('a', 'c1')], 'after History, before live commands');
  assert.equal(terminalTabs(null, [owner('a', 'Alpha', [command('c1', 2, 10)], true)]).some(tab => tab.kind === 'monitor'), false);
  assert.equal(terminalTabs(null, []).some(tab => tab.key === MONITOR_KEY), false);
});

test('Follow and fresh-pane selection never choose the monitor, and a selected monitor stays selected', () => {
  const before = terminalTabs('a', [owner('a', 'Alpha', [], true)]);
  const after = terminalTabs('a', [owner('a', 'Alpha', [command('c1', 2, 10)], true)]);
  const known = new Set(before.map(tab => tab.key));
  assert.equal(nextSelection(after, known, MONITOR_KEY, false), MONITOR_KEY);
  assert.equal(nextSelection(after, new Set(after.map(tab => tab.key)), MONITOR_KEY, true), MONITOR_KEY, 'output ticks leave it');
  assert.equal(nextSelection(after, known, MONITOR_KEY, true), commandKey('a', 'c1'), 'Follow moves to a new command only');
  assert.equal(nextSelection(terminalTabs('a', []), new Set(), '', true), HISTORY_KEY);
  assert.equal(nextSelection(terminalTabs(null, []), new Set(), MONITOR_KEY, false), HISTORY_KEY, 'a stale monitor selection cannot apply on the global page');
});
