import test from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { inspectRepo, registerRepoInspectTool } from './omp-repo.mjs';

const hasGit = (() => { try { execFileSync('git', ['--version'], { stdio: 'ignore' }); return true; } catch { return false; } })();

function makeRepo() {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'omp-repo-'));
  const git = (who, ...args) => execFileSync('git', ['-C', dir, '-c', `user.name=${who}`, '-c', `user.email=${who}@example.com`, '-c', 'commit.gpgsign=false', '-c', 'core.autocrlf=false', ...args], { encoding: 'utf8' }).trim();
  const write = (name, body) => { mkdirSync(path.dirname(path.join(dir, name)), { recursive: true }); writeFileSync(path.join(dir, name), body); };
  git('Ann', 'init', '-q');
  write('a.txt', 'alpha\n'); git('Ann', 'add', '-A'); git('Ann', 'commit', '-q', '-m', 'add alpha');
  const first = git('Ann', 'rev-parse', 'HEAD');
  write('a.txt', 'alpha\nneedle line\n'); git('Bob', 'add', '-A'); git('Bob', 'commit', '-q', '-m', 'add needle');
  const second = git('Bob', 'rev-parse', 'HEAD');
  git('Ann', 'checkout', '-q', '-b', 'feature');
  write('sub/b.txt', 'feature file\n'); git('Ann', 'add', '-A'); git('Ann', 'commit', '-q', '-m', 'add feature');
  git('Ann', 'checkout', '-q', '-');
  git('Bob', 'merge', '--no-ff', '-q', '-m', 'Merge pull request #1 from x/feature', 'feature');
  const merge = git('Bob', 'rev-parse', 'HEAD');
  write('a.txt', 'alpha\nneedle line\ndirty\n');
  write('untracked.txt', 'new\n');
  return { dir, first, second, merge, git, write };
}

test('history, commits, diffs, blame and files can be read without changing anything', { skip: !hasGit && 'git is not installed' }, async () => {
  const { dir, first, second, merge, write } = makeRepo();
  try {
    const needle = await inspectRepo(dir, { action: 'log', code: 'needle' });
    assert.deepEqual(needle.split('\n').length, 1);
    assert.match(needle, new RegExp(`^- ${second.slice(0, 7)} \\d{4}-\\d\\d-\\d\\d Bob .*add needle`));
    assert.equal((await inspectRepo(dir, { action: 'log', query: 'ADD', limit: 1 })).split('\n').length, 1, 'message search is case-insensitive and limited');
    assert.match(await inspectRepo(dir, { action: 'log', merges: 'only' }), /Merge pull request #1/);
    assert.doesNotMatch(await inspectRepo(dir, { action: 'log', merges: 'exclude' }), /Merge pull request/);
    assert.match(await inspectRepo(dir, { action: 'log', author: 'ann', path: 'sub/b.txt' }), /add feature/);
    assert.equal(await inspectRepo(dir, { action: 'log', code: 'no-such-text' }), 'No matching commits.');

    const shown = await inspectRepo(dir, { action: 'show', commit: second });
    assert.ok(shown.includes('add needle') && shown.includes('+needle line'));
    assert.ok((await inspectRepo(dir, { action: 'show', commit: merge, stat: true })).includes('sub/b.txt'), 'a merge is shown against its first parent');
    const range = await inspectRepo(dir, { action: 'diff', base: first, target: second, stat: true });
    assert.ok(range.includes('a.txt') && !range.includes('sub/b.txt'));

    const changes = await inspectRepo(dir, { action: 'changes', stat: true });
    assert.ok(changes.includes('a.txt') && changes.includes('Untracked files (not in the diff):\nuntracked.txt'));
    assert.ok((await inspectRepo(dir, { action: 'changes', base: first })).includes('+needle line'), 'an explicit base compares with that commit');
    assert.ok((await inspectRepo(dir, { action: 'changes', path: 'a.txt' })).includes('+dirty'));

    assert.equal((await inspectRepo(dir, { action: 'blame', path: 'a.txt', start: 1, end: 2 })).trim().split('\n').length, 2);
    assert.equal(await inspectRepo(dir, { action: 'file', path: 'a.txt' }), 'alpha\nneedle line\ndirty\n');
    assert.equal(await inspectRepo(dir, { action: 'file', path: 'a.txt', ref: first }), 'alpha\n');
    assert.equal(await inspectRepo(dir, { action: 'file', path: 'sub\\b.txt' }), 'feature file\n', 'Windows separators are accepted');

    write('big.txt', 'x\n');
    execFileSync('git', ['-C', dir, 'add', 'big.txt']);
    write('big.txt', 'y'.repeat(130_000));
    const big = await inspectRepo(dir, { action: 'changes', path: 'big.txt', base: first });
    assert.ok(big.length < 101_000 && big.includes('[output truncated at 100000 characters'));
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('a repository without commits is readable and unsafe input is refused', { skip: !hasGit && 'git is not installed' }, async () => {
  const empty = mkdtempSync(path.join(os.tmpdir(), 'omp-repo-empty-'));
  const { dir, first } = makeRepo();
  try {
    execFileSync('git', ['-C', empty, 'init', '-q']);
    writeFileSync(path.join(empty, 'new.txt'), 'hello\n');
    execFileSync('git', ['-C', empty, 'add', 'new.txt']);
    assert.ok((await inspectRepo(empty, { action: 'changes' })).includes('+hello'));
    assert.equal(await inspectRepo(empty, { action: 'log' }), 'No matching commits.');

    const refused = [
      [{ action: 'show', commit: '--output=pwned' }, /Unknown commit or ref/],
      [{ action: 'diff', base: first, target: 'main;rm -rf' }, /Unknown commit or ref/],
      [{ action: 'file', path: '../x' }, /not a path inside the repository/],
      [{ action: 'file', path: 'sub/../../x' }, /not a path inside the repository/],
      [{ action: 'changes', path: '-x' }, /not a path inside the repository/],
      [{ action: 'changes', path: 'C:/Windows/win.ini' }, /not a path inside the repository/],
      [{ action: 'log', query: 'q'.repeat(201) }, /1\.\.200 characters/],
      [{ action: 'log', code: 'a', regex: 'b' }, /either code or regex/],
      [{ action: 'log', bogus: 1 }, /log does not accept "bogus"/],
      [{ action: 'show' }, /show needs "commit"/],
      [{ action: 'checkout', commit: first }, /unknown action/],
      [{ action: 'file', path: 'missing.txt' }, /File not found/],
    ];
    for (const [params, message] of refused) await assert.rejects(inspectRepo(dir, params), message, JSON.stringify(params));
    assert.ok(!existsSync(path.join(dir, 'pwned')) && !existsSync(path.join(process.cwd(), 'pwned')) && !existsSync(path.join(dir, '..', 'x')));
  } finally { rmSync(dir, { recursive: true, force: true }); rmSync(empty, { recursive: true, force: true }); }
});

test('the registered tool is read-only and reports problems as errors, not exceptions', { skip: !hasGit && 'git is not installed' }, async () => {
  const { dir } = makeRepo();
  try {
    let tool;
    const schema = () => ({ optional: () => schema(), int: () => schema(), min: () => schema(), max: () => schema() });
    const zod = { string: schema, boolean: schema, number: schema, enum: schema, object: shape => ({ shape }) };
    registerRepoInspectTool({ zod, registerTool: definition => { tool = definition; } }, dir);
    assert.equal(tool.name, 'repo_inspect');
    assert.equal(tool.approval({ action: 'log' }), 'read');
    const ok = await tool.execute('1', { action: 'log', code: 'needle' });
    assert.match(ok.content[0].text, /add needle/);
    assert.equal(ok.isError, undefined);
    const failed = await tool.execute('2', { action: 'show', commit: '--bad' });
    assert.equal(failed.isError, true);
    assert.match(failed.content[0].text, /Unknown commit or ref/);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
