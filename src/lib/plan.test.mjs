import { test } from 'node:test';
import assert from 'node:assert/strict';
import { planHeadlines } from './plan.ts';

test('markdown headings keep their line, level and trimmed title', () => {
  const text = ['# Goal', 'text', '', '## Steps ##', '1. one', '', '### Rollout  ', 'x'].join('\n');
  assert.deepEqual(planHeadlines(text), [
    { line: 0, level: 1, title: 'Goal' },
    { line: 3, level: 2, title: 'Steps' },
    { line: 6, level: 3, title: 'Rollout' },
  ]);
});

test('heading titles keep inner hashes and need a space after at most six markers', () => {
  const text = ['## a # b ##', '#### C#', '####### seven', '#nospace', '#   x   ###   '].join('\n');
  assert.deepEqual(planHeadlines(text), [
    { line: 0, level: 2, title: 'a # b' },
    { line: 1, level: 4, title: 'C' },
    { line: 4, level: 1, title: 'x' },
  ]);
});

test('a heading-looking line inside a code fence is not a headline', () => {
  const text = ['# Real', '```sh', '# comment', '```', '## Also real'].join('\r\n');
  assert.deepEqual(planHeadlines(text).map(h => h.title), ['Real', 'Also real']);
});

test('plans without headings use the first line of each paragraph', () => {
  const long = 'x'.repeat(130);
  const text = ['- first item', 'continues here', '', '2) second block', '', long].join('\n');
  assert.deepEqual(planHeadlines(text), [
    { line: 0, level: 1, title: 'first item' },
    { line: 3, level: 1, title: 'second block' },
    { line: 5, level: 1, title: `${'x'.repeat(100)}…` },
  ]);
});

test('blank plans have no headlines', () => {
  assert.deepEqual(planHeadlines(''), []);
  assert.deepEqual(planHeadlines('  \n\n '), []);
});
