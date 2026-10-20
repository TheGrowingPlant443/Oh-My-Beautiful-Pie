import test from 'node:test';
import assert from 'node:assert/strict';
import { get } from 'svelte/store';
import { createWindowVisibility } from './window-visibility-core.ts';

const flush = () => new Promise(resolve => setImmediate(resolve));

function fakeSource() {
  const source = {
    native: { visible: true, minimized: false },
    fail: false,
    docHidden: false,
    nativeListeners: new Set(),
    docListeners: new Set(),
    reads: 0,
    gates: [],
    async nativeState() {
      source.reads++;
      if (source.fail) throw new Error('window gone');
      if (source.gates.length) await source.gates.shift();
      return { ...source.native };
    },
    async watch(listener) { source.nativeListeners.add(listener); return () => source.nativeListeners.delete(listener); },
    documentHidden: () => source.docHidden,
    watchDocument(listener) { source.docListeners.add(listener); return () => source.docListeners.delete(listener); },
    emitNative: () => source.nativeListeners.forEach(listener => listener()),
    emitDocument: () => source.docListeners.forEach(listener => listener()),
  };
  return source;
}
const watch = store => { const seen = []; const stop = store.subscribe(value => seen.push(value)); return { seen, stop }; };

test('the initial native state is read once subscribed and counts minimized as not visible', async () => {
  const source = fakeSource();
  source.native.minimized = true;
  const store = createWindowVisibility(source);
  const { seen, stop } = watch(store);
  assert.deepEqual(seen, [false], 'unknown until the first read settles: no spurious visible sample');
  await flush();
  assert.equal(get(store), false);
  assert.deepEqual(seen, [false], 'still not visible');
  stop();
});

test('native transitions and document visibility re-evaluate the flag', async () => {
  const source = fakeSource();
  const store = createWindowVisibility(source);
  const { seen, stop } = watch(store);
  await flush();
  assert.equal(get(store), true);
  source.native.minimized = true;
  source.emitNative();
  await flush();
  assert.equal(get(store), false, 'minimize reported by a resize/move/focus event');
  source.native.minimized = false;
  source.emitNative();
  await flush();
  assert.equal(get(store), true);
  source.docHidden = true;
  source.emitDocument();
  assert.equal(get(store), false, 'a hidden document applies immediately');
  await flush();
  source.docHidden = false;
  source.emitDocument();
  await flush();
  assert.equal(get(store), true);
  source.native.visible = false;
  source.emitNative();
  await flush();
  assert.equal(get(store), false, 'a hidden native window');
  assert.ok(seen.length >= 5);
  stop();
});

test('an unreadable native state defaults to visible and never throws', async () => {
  const source = fakeSource();
  source.fail = true;
  const store = createWindowVisibility(source);
  const { stop } = watch(store);
  await flush();
  assert.equal(get(store), true);
  source.docHidden = true;
  source.emitDocument();
  await flush();
  assert.equal(get(store), false, 'the document still applies');
  stop();
});

test('a failing watch subscription is tolerated', async () => {
  const source = fakeSource();
  source.watch = async () => { throw new Error('no events'); };
  source.watchDocument = () => { throw new Error('no document'); };
  const store = createWindowVisibility(source);
  const { stop } = watch(store);
  await flush();
  assert.equal(get(store), true);
  stop();
});

test('an older native read that settles late cannot overwrite a newer one', async () => {
  const source = fakeSource();
  const store = createWindowVisibility(source);
  const { stop } = watch(store);
  await flush();
  let release;
  source.gates.push(new Promise(resolve => { release = resolve; }));
  source.native.minimized = true;
  source.emitNative();
  await flush();
  source.native.minimized = false;
  source.emitNative();
  await flush();
  release();
  await flush();
  assert.equal(get(store), true, 'the newest read wins');
  stop();
});

test('unsubscribing removes both listeners and ignores in-flight reads', async () => {
  const source = fakeSource();
  const store = createWindowVisibility(source);
  const { stop } = watch(store);
  await flush();
  assert.equal(source.nativeListeners.size, 1);
  assert.equal(source.docListeners.size, 1);
  stop();
  assert.equal(source.nativeListeners.size, 0);
  assert.equal(source.docListeners.size, 0);
});
