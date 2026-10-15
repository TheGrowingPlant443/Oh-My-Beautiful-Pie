import test from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { saveClassification } from './omp-architect.mjs';

const KEY = '0123456789abcdef';
const fresh = () => mkdtempSync(path.join(os.tmpdir(), 'omp-architect-'));
const read = dir => JSON.parse(readFileSync(path.join(dir, 'classifications', `${KEY}.json`), 'utf8'));

test('classifications merge by package id across calls', () => {
  const dir = fresh();
  try {
    assert.equal(saveClassification(dir, { key: KEY, items: [{ id: 'npm:a', basic: true, reason: 'core' }, { id: 'npm:b', basic: false }] }), 2);
    assert.equal(saveClassification(dir, { key: KEY, items: [{ id: 'npm:b', basic: true, reason: 'now basic' }, { id: 'npm:c', basic: false, reason: 'x'.repeat(500) }] }), 2);
    const saved = read(dir);
    assert.deepEqual(saved.items['npm:a'], { basic: true, reason: 'core' });
    assert.deepEqual(saved.items['npm:b'], { basic: true, reason: 'now basic' });
    assert.equal(saved.items['npm:c'].reason.length, 200);
    assert.equal(typeof saved.updatedAt, 'number');
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('invalid requests write nothing', () => {
  const dir = fresh();
  try {
    for (const bad of [
      { key: '../x', items: [{ id: 'a', basic: true }] },
      { key: KEY, items: [] },
      { key: KEY, items: [{ id: 'a', basic: 'yes' }] },
      { key: KEY, items: [{ id: 'a'.repeat(301), basic: true }] },
    ]) assert.throws(() => saveClassification(dir, bad));
    assert.equal(existsSync(path.join(dir, 'classifications')), false);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('a corrupt saved file is replaced instead of crashing', () => {
  const dir = fresh();
  try {
    mkdirSync(path.join(dir, 'classifications'));
    writeFileSync(path.join(dir, 'classifications', `${KEY}.json`), '{not json');
    saveClassification(dir, { key: KEY, items: [{ id: 'npm:a', basic: false }] });
    assert.deepEqual(Object.keys(read(dir).items), ['npm:a']);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
