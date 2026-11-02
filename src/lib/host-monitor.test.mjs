import test from 'node:test';
import assert from 'node:assert/strict';
import { get } from 'svelte/store';
import { createHostMonitor, monitorProducerCurrent, SAMPLE_INTERVAL_MS, PAGE_SIZE, errorCode, sameContext, metricLabel, sectionNotes, formatEndpoint, directionLabel, ownerLabel } from './host-monitor.ts';

const flush = () => new Promise(resolve => setImmediate(resolve));
const deferred = () => Promise.withResolvers();
const PATH = 'C:\\work\\repo';
const CTX = { instanceId: 'a', generation: 1, projectPath: PATH };

function fakeClock() {
  let time = 1000;
  let nextId = 1;
  const timers = new Map();
  return {
    now: () => time,
    setTimeout(fn, ms) { const id = nextId++; timers.set(id, { at: time + ms, fn }); return id; },
    clearTimeout(id) { timers.delete(id); },
    pending: () => timers.size,
    async advance(ms) {
      const target = time + ms;
      for (;;) {
        const due = [...timers.entries()].filter(([, timer]) => timer.at <= target).sort((a, b) => a[1].at - b[1].at)[0];
        if (!due) break;
        timers.delete(due[0]);
        time = Math.max(time, due[1].at);
        due[1].fn();
        await flush();
      }
      time = target;
      await flush();
    },
  };
}

const section = (status = 'ok', error = null) => ({ status, error });
const summaryOf = (viewId, n, extra = {}) => ({
  viewId, snapshotId: `snap-${n}`, instanceId: 'a', generation: 1, scope: 'wholeComputer', startedAt: n, completedAt: n + 1, durationMs: 1,
  processCount: 10 + n, externalTcpCount: n, confirmedOutboundCount: 0, unknownDirectionCount: n, udpEndpointCount: 2, limitedProcessCount: 1,
  sections: { processes: section(), tcp4: section(), tcp6: section(), udp4: section(), udp6: section() }, ...extra,
});

function harness({ context = CTX } = {}) {
  const clock = fakeClock();
  const calls = { open: [], collect: [], read: [], close: [], explain: [], pause: [], cancel: [], focus: [] };
  const hooks = {};
  let seq = 0, views = 0, active = 0, maxActive = 0;
  const transport = {
    openHostMonitor: async (instanceId, generation) => {
      calls.open.push([instanceId, generation]);
      const n = ++views;
      await hooks.open?.(n);
      return { viewId: `view-${n}`, context: hooks.openContext ?? { instanceId, generation, projectPath: (hooks.path ?? PATH).replace(/\\/g, '/') } };
    },
    collectHostMonitor: async (viewId, displayed) => {
      const n = ++seq;
      calls.collect.push({ viewId, displayed, at: clock.now(), n });
      active++; maxActive = Math.max(maxActive, active);
      try { await hooks.collect?.(n, viewId); } finally { active--; }
      return summaryOf(viewId, n, hooks.summaryExtra ?? {});
    },
    readHostMonitor: async (snapshotId, query) => {
      calls.read.push({ snapshotId, query });
      await hooks.read?.(snapshotId, query);
      const total = hooks.total ?? 5;
      const offset = Math.min(query.offset, hooks.maxOffset ?? query.offset);
      return { snapshotId, section: query.kind, total, offset, nextOffset: null, items: [] };
    },
    closeHostMonitor: async viewId => { calls.close.push(viewId); },
    explainHostMonitor: async snapshotId => { calls.explain.push(snapshotId); return hooks.explain ? await hooks.explain(snapshotId) : sent(snapshotId); },
    explainHostMonitorAfterSrsPause: async (snapshotId, jobId, attemptId) => { calls.pause.push([snapshotId, jobId, attemptId]); return hooks.pause ? await hooks.pause(snapshotId) : sent(snapshotId); },
    cancelHostMonitorExplanation: async snapshotId => { calls.cancel.push(snapshotId); return true; },
  };
  const monitor = createHostMonitor({ transport, clock, focusArchitect: async (producer, current) => { await hooks.focus?.(); if (!current()) return false; calls.focus.push(producer.instanceId); return hooks.focusAllowed ?? true; } });
  if (context) monitor.setContext(context);
  return { monitor, clock, calls, hooks, transport, state: () => get(monitor.state), maxActive: () => maxActive, show: async () => { monitor.setVisible(true); await flush(); } };
}
const producer = id => ({ instanceId: id, generation: 1, launchId: 'l', sessionId: 's' });
const sent = snapshotId => ({ requestId: 'r1', snapshotId, producer: producer('arch'), promptSent: true, error: null, blockedBy: null });

test('a visible pane samples at once, then every two seconds after the previous start without overlap', async () => {
  const h = harness();
  assert.equal(h.calls.open.length, 0, 'nothing happens while hidden');
  await h.show();
  assert.deepEqual(h.calls.collect.map(call => call.at), [1000]);
  assert.equal(h.state().summary.snapshotId, 'snap-1');
  await h.clock.advance(SAMPLE_INTERVAL_MS - 1);
  assert.equal(h.calls.collect.length, 1);
  await h.clock.advance(1);
  assert.deepEqual(h.calls.collect.map(call => call.at), [1000, 3000]);
  await h.clock.advance(SAMPLE_INTERVAL_MS * 3);
  assert.deepEqual(h.calls.collect.map(call => call.at), [1000, 3000, 5000, 7000, 9000]);
  assert.equal(h.maxActive(), 1);
  assert.equal(h.calls.open.length, 1, 'one view serves the whole visible session');
});

test('a slow collection delays the next start until it finishes and never queues a catch-up', async () => {
  const h = harness();
  const gate = deferred();
  h.hooks.collect = n => n === 2 ? gate.promise : undefined;
  await h.show();
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.calls.collect.length, 2);
  await h.clock.advance(SAMPLE_INTERVAL_MS * 3);
  assert.equal(h.calls.collect.length, 2, 'still waiting: no overlap');
  assert.equal(h.maxActive(), 1);
  gate.resolve();
  await flush();
  assert.equal(h.calls.collect.length, 3, 'next start happens right after completion (already more than 2 s after the previous start)');
  assert.equal(h.calls.collect[2].at, 9000);
  await h.clock.advance(SAMPLE_INTERVAL_MS - 1);
  assert.equal(h.calls.collect.length, 3);
  await h.clock.advance(1);
  assert.equal(h.calls.collect.length, 4);
});

test('a failed collection keeps the last good snapshot with a stale label, and the next success clears it', async () => {
  const h = harness();
  h.hooks.collect = n => { if (n === 2) throw 'Access denied reading the table'; };
  await h.show();
  const good = h.state().summary;
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  const failed = h.state();
  assert.equal(failed.summary, good, 'last-good summary retained');
  assert.equal(failed.page.snapshotId, 'snap-1', 'and its page');
  assert.match(failed.stale.message, /Access denied/);
  assert.equal(failed.collecting, false);
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.state().stale, null);
  assert.equal(h.state().summary.snapshotId, 'snap-3');
});

test('every collect acknowledges the last displayed snapshot; a failed candidate page keeps the old display id', async () => {
  const h = harness();
  h.hooks.read = snapshotId => { if (snapshotId === 'snap-2') throw 'page unavailable'; };
  await h.show();
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.state().summary.snapshotId, 'snap-1', 'candidate not swapped in');
  assert.match(h.state().stale.message, /page unavailable/);
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.deepEqual(h.calls.collect.map(call => call.displayed), [null, 'snap-1', 'snap-1']);
  assert.equal(h.state().summary.snapshotId, 'snap-3', 'a later candidate replaces the failed one');
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.calls.collect[3].displayed, 'snap-3');
});

test('summary and page are replaced together from one snapshot', async () => {
  const h = harness();
  await h.show();
  const gate = deferred();
  h.hooks.read = snapshotId => snapshotId === 'snap-2' ? gate.promise : undefined;
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.state().summary.snapshotId, 'snap-1', 'candidate summary is not shown before its page');
  assert.equal(h.state().page.snapshotId, 'snap-1');
  gate.resolve();
  await flush();
  assert.equal(h.state().summary.snapshotId, 'snap-2');
  assert.equal(h.state().page.snapshotId, 'snap-2');
});

test('changing the query while a candidate loads re-reads the candidate with the new query', async () => {
  const h = harness();
  await h.show();
  const gate = deferred();
  let first = true;
  h.hooks.read = snapshotId => { if (snapshotId === 'snap-2' && first) { first = false; return gate.promise; } };
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  h.monitor.showSection('processes');
  await flush();
  gate.resolve();
  await flush();
  const state = h.state();
  assert.equal(state.summary.snapshotId, 'snap-2');
  assert.equal(state.page.snapshotId, 'snap-2');
  assert.equal(state.page.section, 'processes', 'the swapped page answers the current section');
});

test('paging, section switches, filters and search read the displayed snapshot and never collect', async () => {
  const h = harness();
  await h.show();
  const collects = h.calls.collect.length;
  const reads = h.calls.read.length;
  h.monitor.showSection('processes');
  h.monitor.loadPage(PAGE_SIZE);
  h.monitor.setText('chrome');
  h.monitor.setTcpView('listeners');
  h.monitor.showSection('udp');
  h.monitor.showSection('tcp', 'external');
  await flush();
  assert.equal(h.calls.collect.length, collects);
  assert.equal(h.calls.read.length, reads + 6);
  assert.ok(h.calls.read.slice(reads).every(call => call.snapshotId === 'snap-1'));
  const queries = h.calls.read.slice(reads).map(call => call.query);
  assert.deepEqual(queries[0], { kind: 'processes', text: '', offset: 0, limit: PAGE_SIZE });
  assert.deepEqual(queries[1], { kind: 'processes', text: '', offset: PAGE_SIZE, limit: PAGE_SIZE });
  assert.deepEqual(queries[2], { kind: 'processes', text: 'chrome', offset: 0, limit: PAGE_SIZE }, 'search restarts at the first page');
  assert.deepEqual(queries[3], { kind: 'tcp', view: 'listeners', text: 'chrome', offset: 0, limit: PAGE_SIZE });
  assert.deepEqual(queries[4], { kind: 'udp', text: '', offset: 0, limit: PAGE_SIZE });
  assert.deepEqual(queries[5], { kind: 'tcp', view: 'external', text: '', offset: 0, limit: PAGE_SIZE });
});

test('the chosen section, search and page survive snapshot updates', async () => {
  const h = harness();
  await h.show();
  h.monitor.showSection('processes');
  h.monitor.setText('node');
  await flush();
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  const state = h.state();
  assert.equal(state.section, 'processes');
  assert.equal(state.text, 'node');
  const last = h.calls.read.at(-1);
  assert.equal(last.snapshotId, 'snap-2');
  assert.deepEqual(last.query, { kind: 'processes', text: 'node', offset: 0, limit: PAGE_SIZE });
});

test('an offset beyond a shrunken count adopts the backend\'s effective offset for the next snapshot', async () => {
  const h = harness();
  await h.show();
  h.monitor.showSection('processes');
  h.monitor.loadPage(300);
  await flush();
  assert.equal(h.state().offset, 300);
  h.hooks.total = 130;
  h.hooks.maxOffset = 100;
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.state().page.offset, 100);
  assert.equal(h.state().offset, 100);
  assert.equal(h.calls.read.at(-1).query.offset, 300, 'asked for the old offset, got the last valid page');
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.calls.read.at(-1).query.offset, 100, 'later snapshots start from the clamped offset');
});

test('hiding closes the view, stops the timer and drops a late collection result', async () => {
  const h = harness();
  const gate = deferred();
  h.hooks.collect = () => gate.promise;
  await h.show();
  assert.equal(h.state().collecting, true);
  h.monitor.setVisible(false);
  assert.deepEqual(h.calls.close, ['view-1']);
  assert.equal(h.clock.pending(), 0);
  gate.resolve();
  await flush();
  assert.equal(h.state().summary, null, 'late result never published');
  assert.equal(h.calls.read.length, 0, 'and its page is never loaded');
  await h.clock.advance(SAMPLE_INTERVAL_MS * 3);
  assert.equal(h.calls.collect.length, 1);
  assert.equal(h.state().collecting, false);
});

test('returning opens a fresh view and samples once immediately', async () => {
  const h = harness();
  await h.show();
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  h.monitor.setVisible(false);
  await h.clock.advance(SAMPLE_INTERVAL_MS * 5);
  assert.equal(h.calls.collect.length, 2, 'no collection while hidden');
  assert.equal(h.state().summary.snapshotId, 'snap-2', 'last snapshot stays on screen while paused');
  const before = h.clock.now();
  await h.show();
  assert.equal(h.calls.open.length, 2);
  assert.deepEqual([h.calls.collect[2].viewId, h.calls.collect[2].displayed, h.calls.collect[2].at], ['view-2', null, before]);
});

test('a context change closes the old view, fences its late work and samples for the new context', async () => {
  const h = harness();
  await h.show();
  const gate = deferred();
  h.hooks.collect = n => n === 2 ? gate.promise : undefined;
  h.hooks.path = 'C:/other';
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.calls.collect.length, 2);
  h.monitor.setContext({ instanceId: 'b', generation: 3, projectPath: 'C:/other' });
  assert.deepEqual(h.calls.close, ['view-1']);
  assert.equal(h.state().summary, null, 'the old agent\'s snapshot is gone');
  await flush();
  assert.deepEqual(h.calls.open.at(-1), ['b', 3]);
  gate.resolve();
  await flush();
  assert.notEqual(h.state().summary?.snapshotId, 'snap-2', 'the old view\'s late candidate is dropped');
  assert.equal(h.state().context.instanceId, 'b');
});

test('an identical context does not restart sampling', async () => {
  const h = harness();
  await h.show();
  h.monitor.setContext({ ...CTX, projectPath: 'c:/work/repo/' });
  await flush();
  assert.equal(h.calls.open.length, 1);
  assert.deepEqual(h.calls.close, []);
});

test('a successful open that arrives after hiding is closed at once and nothing is collected', async () => {
  const h = harness();
  const gate = deferred();
  h.hooks.open = () => gate.promise;
  await h.show();
  h.monitor.setVisible(false);
  gate.resolve();
  await flush();
  assert.deepEqual(h.calls.close, ['view-1']);
  assert.equal(h.calls.collect.length, 0);
});

test('a backend context that differs from the pane is rejected and its view closed', async () => {
  const h = harness();
  h.hooks.openContext = { instanceId: 'a', generation: 2, projectPath: PATH };
  await h.show();
  assert.deepEqual(h.calls.close, ['view-1']);
  assert.equal(h.calls.collect.length, 0);
  assert.match(h.state().stale.message, /different agent/);
  h.hooks.openContext = undefined;
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.calls.collect.length, 1, 'a later open with the right context works');
});

test('a snapshot for another generation is discarded', async () => {
  const h = harness();
  h.hooks.summaryExtra = { generation: 2 };
  await h.show();
  assert.equal(h.state().summary, null);
  assert.match(h.state().stale.message, /different monitor context/);
});

test('monitor_frozen stops sampling and Refresh retries once', async () => {
  const h = harness();
  h.hooks.collect = n => { if (n === 2) throw 'monitor_frozen'; };
  await h.show();
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.state().frozen, true);
  assert.equal(h.state().summary.snapshotId, 'snap-1');
  assert.equal(h.state().stale, null, 'frozen is not an error');
  await h.clock.advance(SAMPLE_INTERVAL_MS * 4);
  assert.equal(h.calls.collect.length, 2, 'no sampling while frozen');
  h.monitor.refresh();
  await flush();
  assert.equal(h.state().frozen, false);
  assert.equal(h.calls.collect.length, 3);
  assert.equal(h.state().summary.snapshotId, 'snap-3');
});

test('unsupported_platform stops sampling and closes the view', async () => {
  const h = harness();
  h.hooks.collect = () => { throw 'unsupported_platform'; };
  await h.show();
  assert.equal(h.state().unsupported, true);
  assert.deepEqual(h.calls.close, ['view-1']);
  await h.clock.advance(SAMPLE_INTERVAL_MS * 3);
  assert.equal(h.calls.collect.length, 1);
});

test('view_closed reopens a fresh view and acknowledges nothing from the old one', async () => {
  const h = harness();
  h.hooks.collect = n => { if (n === 2) throw 'view_closed'; };
  await h.show();
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.state().stale, null);
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.calls.open.length, 2);
  assert.deepEqual([h.calls.collect[2].viewId, h.calls.collect[2].displayed], ['view-2', null]);
  assert.equal(h.state().summary.snapshotId, 'snap-3');
});

test('monitor_cancelled and collection_in_progress are benign and sampling continues without a label', async () => {
  const h = harness();
  h.hooks.collect = n => { if (n === 1) throw 'monitor_cancelled'; if (n === 2) throw 'collection_in_progress: busy'; };
  await h.show();
  assert.equal(h.state().stale, null);
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.state().stale, null);
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.state().summary.snapshotId, 'snap-3');
});

test('snapshot_expired drops the display id so the next collect starts a fresh display', async () => {
  const h = harness();
  h.hooks.collect = n => { if (n === 2) throw 'snapshot_expired'; };
  await h.show();
  await h.clock.advance(SAMPLE_INTERVAL_MS * 2);
  assert.deepEqual(h.calls.collect.map(call => call.displayed), [null, 'snap-1', null]);
  assert.equal(h.state().stale, null, 'recovered');
});

test('Refresh honours the minimum start interval', async () => {
  const h = harness();
  await h.show();
  h.monitor.refresh();
  await flush();
  assert.equal(h.calls.collect.length, 1, 'too soon after the immediate sample');
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.calls.collect.length, 2, 'the refresh and the timer share one start');
});

test('dispose closes the view and ignores later calls', async () => {
  const h = harness();
  await h.show();
  h.monitor.dispose();
  assert.deepEqual(h.calls.close, ['view-1']);
  h.monitor.setVisible(true);
  h.monitor.refresh();
  await h.clock.advance(SAMPLE_INTERVAL_MS * 3);
  assert.equal(h.calls.collect.length, 1);
});

// ---------- Explain ----------
test('a retired or replaced Architect receipt cannot focus another native conversation', () => {
  const p = producer('arch');
  const architect = { id: 'arch', generation: 1, status: 'running', bridgeReady: true };
  const owner = { generation: 1, sessionId: 's' };
  assert.equal(monitorProducerCurrent(p, architect, owner), true);
  for (const changed of [{ ...architect, id: 'other' }, { ...architect, generation: 2 }, { ...architect, status: 'stopped' }, { ...architect, bridgeReady: false }, null]) {
    assert.equal(monitorProducerCurrent(p, changed, owner), false);
  }
  for (const changed of [{ ...owner, generation: 2 }, { ...owner, sessionId: 'replacement' }, null]) {
    assert.equal(monitorProducerCurrent(p, architect, changed), false);
  }
});

test('a refused focus leaves the admitted snapshot frozen without claiming chat navigation', async () => {
  const h = harness();
  h.hooks.focusAllowed = false;
  await h.show();
  await h.monitor.explainDisplayed({ architectId: 'arch' });
  assert.equal(h.state().explain.phase, 'admitted');
  assert.equal(h.state().explain.focused, false);
  assert.equal(h.state().frozen, true);
});

test('navigation during an awaited producer check prevents late focus', async () => {
  const h = harness();
  const gate = deferred();
  h.hooks.focus = () => gate.promise;
  await h.show();
  const asking = h.monitor.explainDisplayed({ architectId: 'arch' });
  await flush();
  h.monitor.setVisible(false);
  gate.resolve();
  await asking;
  assert.deepEqual(h.calls.focus, []);
});


test('explain sends only the displayed snapshot, freezes sampling and focuses the intended Architect once', async () => {
  const h = harness();
  await h.show();
  const gate = deferred();
  h.hooks.collect = n => n === 2 ? gate.promise : undefined;
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.calls.collect.length, 2, 'candidate snap-2 in flight');
  await h.monitor.explainDisplayed({ architectId: 'arch' });
  assert.deepEqual(h.calls.explain, ['snap-1'], 'the displayed snapshot, not the candidate');
  assert.deepEqual(h.calls.focus, ['arch']);
  assert.equal(h.state().explain.phase, 'admitted');
  assert.equal(h.state().frozen, true);
  gate.resolve();
  await flush();
  assert.equal(h.state().summary.snapshotId, 'snap-1', 'the candidate cannot replace the explained snapshot');
  await h.clock.advance(SAMPLE_INTERVAL_MS * 3);
  assert.equal(h.calls.collect.length, 2, 'no sampling while the explanation holds the display');
  await h.monitor.explainDisplayed({ architectId: 'arch' });
  assert.equal(h.calls.explain.length, 1, 'never sent twice');
});

test('focus needs a matching producer and a still-current origin', async () => {
  for (const [name, origin, answer] of [
    ['other producer', { architectId: 'arch' }, { ...sent('snap-1'), producer: producer('someone-else') }],
    ['no producer', { architectId: 'arch' }, { ...sent('snap-1'), producer: null }],
    ['no intended architect', { architectId: null }, sent('snap-1')],
  ]) {
    const h = harness();
    h.hooks.explain = () => answer;
    await h.show();
    await h.monitor.explainDisplayed(origin);
    assert.deepEqual(h.calls.focus, [], name);
    assert.equal(h.state().explain.phase, 'admitted', name);
  }
  const stale = harness();
  const gate = deferred();
  stale.hooks.explain = () => gate.promise;
  await stale.show();
  let valid = true;
  const asking = stale.monitor.explainDisplayed({ architectId: 'arch', current: () => valid });
  await flush();
  valid = false;
  gate.resolve(sent('snap-1'));
  await asking;
  assert.deepEqual(stale.calls.focus, [], 'the parent moved on while the request was in flight');
  const hidden = harness();
  const hiddenGate = deferred();
  hidden.hooks.explain = () => hiddenGate.promise;
  await hidden.show();
  const pending = hidden.monitor.explainDisplayed({ architectId: 'arch' });
  await flush();
  hidden.monitor.setVisible(false);
  hiddenGate.resolve(sent('snap-1'));
  await pending;
  assert.deepEqual(hidden.calls.focus, [], 'hiding fences the origin');
});

test('an uncertain delivery is never resent and keeps the display frozen', async () => {
  const h = harness();
  h.hooks.explain = () => ({ requestId: 'r9', snapshotId: 'snap-1', producer: null, promptSent: null, error: { code: 'delivery_uncertain', message: 'no receipt' }, blockedBy: null });
  await h.show();
  await h.monitor.explainDisplayed({ architectId: 'arch' });
  assert.equal(h.state().explain.phase, 'uncertain');
  assert.equal(h.state().frozen, true);
  await h.monitor.explainDisplayed({ architectId: 'arch' });
  assert.equal(h.calls.explain.length, 1);
  assert.deepEqual(h.calls.focus, []);
  await h.clock.advance(SAMPLE_INTERVAL_MS * 3);
  assert.equal(h.calls.collect.length, 1);
});

test('a rejected explain command is treated as uncertain, not as an invitation to resend', async () => {
  const h = harness();
  h.hooks.explain = () => { throw 'ipc broke'; };
  await h.show();
  await h.monitor.explainDisplayed({ architectId: 'arch' });
  assert.equal(h.state().explain.phase, 'uncertain');
  await h.monitor.explainDisplayed({ architectId: 'arch' });
  assert.equal(h.calls.explain.length, 1);
});

test('a provably unsent request thaws the display, resumes sampling and allows one new explicit request', async () => {
  const h = harness();
  h.hooks.explain = () => ({ requestId: 'r2', snapshotId: 'snap-1', producer: null, promptSent: false, error: { code: 'architect_missing', message: 'No Architect' }, blockedBy: null });
  await h.show();
  await h.monitor.explainDisplayed({ architectId: 'arch' });
  assert.equal(h.state().explain.phase, 'failed');
  assert.equal(h.state().explain.message, 'No Architect');
  assert.equal(h.state().frozen, false);
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.calls.collect.length, 2, 'sampling resumed');
  assert.equal(h.state().explain.phase, 'failed', 'the failure stays until dismissed');
  h.hooks.explain = undefined;
  await h.monitor.explainDisplayed({ architectId: 'arch' });
  assert.deepEqual(h.calls.explain, ['snap-1', 'snap-2'], 'one explicit new request for the then displayed snapshot');
  assert.deepEqual(h.calls.focus, ['arch']);
});

test('an SRS conflict offers Pause and continue, which re-asks with the same snapshot and exact job', async () => {
  const h = harness();
  h.hooks.explain = () => ({ requestId: 'r3', snapshotId: 'snap-1', producer: null, promptSent: false, error: { code: 'srs_active', message: 'SRS owns the Architect' }, blockedBy: { jobId: 'job-1', attemptId: 'att-1' } });
  await h.show();
  await h.monitor.explainDisplayed({ architectId: 'arch' });
  const blocked = h.state().explain;
  assert.deepEqual([blocked.phase, blocked.jobId, blocked.attemptId], ['blocked', 'job-1', 'att-1']);
  assert.equal(h.state().frozen, true, 'the acknowledged display stays frozen');
  await h.clock.advance(SAMPLE_INTERVAL_MS * 2);
  assert.equal(h.calls.collect.length, 1);
  await h.monitor.continueAfterSrsPause();
  assert.deepEqual(h.calls.pause, [['snap-1', 'job-1', 'att-1']]);
  assert.deepEqual(h.calls.focus, ['arch']);
  assert.equal(h.state().explain.phase, 'admitted');
  await h.monitor.continueAfterSrsPause();
  assert.equal(h.calls.pause.length, 1, 'only valid while blocked');
});

test('dismissing a blocked continuation cancels it and resumes sampling', async () => {
  const h = harness();
  h.hooks.explain = () => ({ requestId: 'r3', snapshotId: 'snap-1', producer: null, promptSent: false, error: { code: 'srs_active', message: 'busy' }, blockedBy: { jobId: 'j', attemptId: 'a' } });
  await h.show();
  await h.monitor.explainDisplayed({ architectId: 'arch' });
  h.monitor.dismissExplanation();
  assert.deepEqual(h.calls.cancel, ['snap-1']);
  assert.equal(h.state().explain.phase, 'idle');
  assert.equal(h.state().frozen, false);
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.calls.collect.length, 2);
  assert.deepEqual(h.calls.pause, []);
});

test('hiding, a context change or disposal cancels a blocked continuation', async () => {
  for (const leave of [m => m.setVisible(false), m => m.setContext({ instanceId: 'z', generation: 1, projectPath: 'C:/z' }), m => m.dispose()]) {
    const h = harness();
    h.hooks.explain = () => ({ requestId: 'r3', snapshotId: 'snap-1', producer: null, promptSent: false, error: { code: 'srs_active', message: 'busy' }, blockedBy: { jobId: 'j', attemptId: 'a' } });
    await h.show();
    await h.monitor.explainDisplayed({ architectId: 'arch' });
    leave(h.monitor);
    assert.deepEqual(h.calls.cancel, ['snap-1']);
    await h.monitor.continueAfterSrsPause();
    assert.deepEqual(h.calls.pause, [], 'nothing continues after the origin is gone');
  }
});

test('a blocked answer that arrives after the origin left is cancelled immediately', async () => {
  const h = harness();
  const gate = deferred();
  h.hooks.explain = () => gate.promise;
  await h.show();
  const asking = h.monitor.explainDisplayed({ architectId: 'arch' });
  await flush();
  h.monitor.setVisible(false);
  assert.deepEqual(h.calls.cancel, [], 'nothing blocked yet');
  gate.resolve({ requestId: 'r4', snapshotId: 'snap-1', producer: null, promptSent: false, error: { code: 'srs_active', message: 'busy' }, blockedBy: { jobId: 'j', attemptId: 'a' } });
  await asking;
  assert.deepEqual(h.calls.cancel, ['snap-1']);
  assert.equal(h.state().explain.phase, 'idle');
});

test('Refresh after an admitted explanation retries and clears the notice once the backend thawed', async () => {
  const h = harness();
  await h.show();
  await h.monitor.explainDisplayed({ architectId: 'arch' });
  h.hooks.collect = n => { if (n === 2) throw 'monitor_frozen'; };
  h.monitor.refresh();
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.state().frozen, true);
  assert.equal(h.state().explain.phase, 'admitted');
  h.monitor.refresh();
  await h.clock.advance(SAMPLE_INTERVAL_MS);
  assert.equal(h.state().frozen, false);
  assert.equal(h.state().explain.phase, 'idle');
  assert.equal(h.state().summary.snapshotId, 'snap-3');
});

test('explain needs a visible pane with a displayed snapshot', async () => {
  const hiddenPane = harness();
  await hiddenPane.monitor.explainDisplayed({ architectId: 'arch' });
  const gate = deferred();
  const noDisplay = harness();
  noDisplay.hooks.collect = () => gate.promise;
  await noDisplay.show();
  await noDisplay.monitor.explainDisplayed({ architectId: 'arch' });
  assert.deepEqual([hiddenPane.calls.explain.length, noDisplay.calls.explain.length], [0, 0]);
  assert.equal(noDisplay.state().frozen, false);
  gate.resolve();
});

// ---------- Pure helpers ----------

test('backend error codes are parsed from bare or prefixed strings only', () => {
  assert.equal(errorCode('monitor_frozen'), 'monitor_frozen');
  assert.equal(errorCode('view_closed: reopen'), 'view_closed');
  assert.equal(errorCode(new Error('snapshot_expired')), 'snapshot_expired');
  assert.equal(errorCode('Access is denied. (os error 5)'), null);
});

test('contexts compare by id, generation and normalised folder', () => {
  assert.equal(sameContext(CTX, { ...CTX, projectPath: 'c:/WORK/repo/' }), true);
  assert.equal(sameContext(CTX, { ...CTX, projectPath: '\\\\?\\C:\\work\\repo' }), true);
  assert.equal(sameContext(CTX, { ...CTX, generation: 2 }), false);
  assert.equal(sameContext(null, null), true);
  assert.equal(sameContext(CTX, null), false);
});

test('context paths with long separator runs normalise without changing the folder', () => {
  assert.equal(sameContext(CTX, { ...CTX, projectPath: 'C:\\work\\repo' + '\\'.repeat(100_000) }), true);
  assert.equal(sameContext(CTX, { ...CTX, projectPath: 'C:/work/other' }), false);
});

test('counts render unavailable, partial and exact values', () => {
  const base = summaryOf('v', 1);
  assert.equal(metricLabel(base, 'processes'), '11');
  assert.equal(metricLabel({ ...base, processCount: null }, 'processes'), 'unavailable');
  assert.equal(metricLabel({ ...base, sections: { ...base.sections, tcp6: section('partial', 'x') } }, 'external'), 'at least 1 (partial)');
  assert.equal(metricLabel({ ...base, sections: { ...base.sections, tcp6: section('partial', 'x') } }, 'processes'), '11', 'only contributing sections matter');
  assert.equal(metricLabel({ ...base, sections: { ...base.sections, udp4: section('unavailable', 'x') } }, 'udp'), 'at least 2 (partial)');
  assert.deepEqual(sectionNotes({ ...base, sections: { ...base.sections, tcp4: section('unavailable', 'denied'), udp6: section('partial', null) } }), ['IPv4 TCP unavailable: denied', 'IPv6 UDP partially read']);
  assert.deepEqual(sectionNotes(base), []);
});

test('endpoints keep IPv6 brackets and scope ids; direction wording never claims more than the evidence', () => {
  assert.equal(formatEndpoint({ address: '93.184.216.34', port: 443, scopeId: null }), '93.184.216.34:443');
  assert.equal(formatEndpoint({ address: 'fe80::1', port: 80, scopeId: 12 }), '[fe80::1%12]:80');
  assert.equal(formatEndpoint({ address: '::1', port: 8080, scopeId: 0 }), '[::1]:8080');
  assert.equal(formatEndpoint(null), '—');
  assert.match(directionLabel({ protocol: 'tcp', direction: 'unknown', directionEvidence: 'unavailable' }), /unknown/i);
  assert.match(directionLabel({ protocol: 'tcp', direction: 'outbound', directionEvidence: 'synSent' }), /attempt/i);
  assert.match(directionLabel({ protocol: 'udp', direction: 'unknown', directionEvidence: 'unavailable' }), /no peer/);
  assert.equal(ownerLabel({ instanceId: 'a', kind: 'browserRoot' }, 'a'), 'Agent browser');
  assert.equal(ownerLabel({ instanceId: 'b', kind: 'descendant' }, 'a'), 'Agent child (other agent)');
});
