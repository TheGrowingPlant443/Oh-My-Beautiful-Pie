import { test } from 'node:test';
import assert from 'node:assert/strict';
import { buildTree, visibleRows } from './diff-tree.ts';

const file = (path, group = null) => ({ path, oldPath: null, status: 'M', group });
const names = rows => rows.map(row => `${'  '.repeat(row.depth)}${row.kind === 'folder' ? row.name + '/' : row.name}`);

test('paths nest under folders, folders sort before files and counts include descendants', () => {
  const tree = buildTree([file('zeta.txt'), file('src/lib/b.ts'), file('src/a.ts'), file('src/lib/a.ts'), file('docs/guide.md'), file('README.md')], '');
  assert.deepEqual(names(visibleRows(tree, new Set())), [
    'docs/', '  guide.md',
    'src/', '  lib/', '    a.ts', '    b.ts', '  a.ts',
    'README.md', 'zeta.txt',
  ]);
  const src = tree.find(node => node.name === 'src');
  assert.equal(src.count, 3);
  assert.equal(src.children.find(node => node.name === 'lib').count, 2);
});

test('collapsing a folder hides its descendants but not its siblings', () => {
  const tree = buildTree([file('src/lib/a.ts'), file('src/b.ts'), file('docs/c.md')], 'changes');
  const collapsed = new Set(['changes/src']);
  assert.deepEqual(names(visibleRows(tree, collapsed)), ['docs/', '  c.md', 'src/']);
  assert.deepEqual(names(visibleRows(tree, new Set(['changes/src/lib']))), ['docs/', '  c.md', 'src/', '  lib/', '  b.ts']);
});

test('the same folder in two groups has distinct keys, so collapsing one leaves the other open', () => {
  const staged = buildTree([file('src/a.ts', 'staged')], 'staged');
  const changes = buildTree([file('src/a.ts', 'changes')], 'changes');
  assert.notEqual(staged[0].key, changes[0].key);
  const collapsed = new Set([staged[0].key]);
  assert.equal(visibleRows(staged, collapsed).length, 1);
  assert.equal(visibleRows(changes, collapsed).length, 2);
});

test('rows keep the file they represent', () => {
  const entry = file('a/b/c.ts', 'changes');
  const rows = visibleRows(buildTree([entry], 'changes'), new Set());
  assert.equal(rows.at(-1).file, entry);
  assert.equal(rows.at(-1).depth, 2);
});
