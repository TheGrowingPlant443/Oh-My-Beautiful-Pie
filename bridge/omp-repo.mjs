import { execFile } from 'node:child_process';
import { accessSync, constants, readFileSync, realpathSync, statSync } from 'node:fs';
import path from 'node:path';

// Read-only access to one Git repository for OMP Manager's helper agents. There is no shell and no write command:
// every action builds a fixed `git` argument list from validated values (refs resolved to hashes, paths after `--`,
// free text only in attached `--flag=value` form).
const OUTPUT_LIMIT = 100_000;
const FILE_LIMIT = 1024 * 1024;
const REF = /^[A-Za-z0-9._/@~^{}-]{1,200}$/;
const FIELDS = {
  changes: { allowed: ['base', 'path', 'stat'], required: [] },
  log: { allowed: ['query', 'code', 'regex', 'author', 'since', 'until', 'path', 'ref', 'merges', 'limit'], required: [] },
  show: { allowed: ['commit', 'path', 'stat'], required: ['commit'] },
  diff: { allowed: ['base', 'target', 'path', 'stat'], required: ['base', 'target'] },
  blame: { allowed: ['path', 'ref', 'start', 'end'], required: ['path'] },
  file: { allowed: ['path', 'ref'], required: ['path'] },
};

// Only absolute PATH entries count, so a relative or empty entry (the working directory) never supplies `git`.
function gitProgram() {
  const name = process.platform === 'win32' ? 'git.exe' : 'git';
  for (const entry of (process.env.PATH ?? '').split(path.delimiter)) {
    if (!path.isAbsolute(entry)) continue;
    const candidate = path.join(entry, name);
    try {
      if (!statSync(candidate).isFile()) continue;
      accessSync(candidate, constants.X_OK);
      return candidate;
    } catch {}
  }
  throw new Error('Git is not installed or is not on PATH');
}

function git(repo, args) {
  const base = ['-C', repo, '--no-pager', '--no-optional-locks', '--literal-pathspecs', '-c', 'core.quotepath=false', '-c', 'core.fsmonitor=false'];
  const env = { ...process.env, GIT_OPTIONAL_LOCKS: '0', GIT_TERMINAL_PROMPT: '0' };
  for (const name of ['GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE']) delete env[name];
  return new Promise((resolve, reject) => {
    const program = gitProgram();
    const child = execFile(program, [...base, ...args], { maxBuffer: 8 * 1024 * 1024, timeout: 30000, windowsHide: true, env }, (error, stdout, stderr) => {
      if (!error) return resolve(stdout);
      if (error.code === 'ERR_CHILD_PROCESS_STDIO_MAXBUFFER') return reject(new Error('The output is too large; narrow it with path, limit or stat'));
      reject(new Error(String(stderr).trim() || error.message));
    });
    child.stdin.end();
  });
}

function bad(message) { return new Error(`invalid_repo_inspect: ${message}`); }

function free(value, name) {
  if (typeof value !== 'string' || !value || value.length > 200 || value.includes('\0')) throw bad(`${name} must be 1..200 characters`);
  return value;
}

function cleanPath(value) {
  if (typeof value !== 'string' || !value || value.includes('\0')) throw bad('path must be a non-empty relative path');
  const normal = path.posix.normalize(value.replaceAll('\\', '/'));
  if (normal === '.' || normal === '..' || normal.startsWith('../') || normal.startsWith('/') || /^[A-Za-z]:/.test(normal) || normal.startsWith('-')) {
    throw bad(`path "${value}" is not a path inside the repository`);
  }
  return normal;
}

async function commitOf(repo, ref) {
  if (typeof ref !== 'string' || !REF.test(ref) || ref.startsWith('-')) throw new Error(`Unknown commit or ref: ${ref}`);
  try { return (await git(repo, ['rev-parse', '--verify', '--quiet', '--end-of-options', `${ref}^{commit}`])).trim(); }
  catch { throw new Error(`Unknown commit or ref: ${ref}`); }
}

async function headOrEmptyTree(repo) {
  try { return (await git(repo, ['rev-parse', '--verify', '--quiet', 'HEAD^{commit}'])).trim(); }
  catch { return (await git(repo, ['hash-object', '-t', 'tree', '--stdin'])).trim(); }
}

function limited(text) {
  return text.length > OUTPUT_LIMIT
    ? `${text.slice(0, OUTPUT_LIMIT)}\n[output truncated at ${OUTPUT_LIMIT} characters; narrow it with path, limit or stat]`
    : text;
}

function check(action, params) {
  const fields = FIELDS[action];
  if (!fields) throw bad(`unknown action "${action}"`);
  for (const [key, value] of Object.entries(params)) {
    if (key === 'action' || value === undefined || value === null) continue;
    if (!fields.allowed.includes(key)) throw bad(`${action} does not accept "${key}"`);
  }
  for (const key of fields.required) if (params[key] === undefined || params[key] === null) throw bad(`${action} needs "${key}"`);
}

// Always ends the argument list: a path (never an option) can only follow the `--`.
function tail(params) { return params.path === undefined ? ['--'] : ['--', cleanPath(params.path)]; }

const ACTIONS = {
  async changes(repo, params) {
    const base = params.base === undefined ? await headOrEmptyTree(repo) : await commitOf(repo, params.base);
    const diff = await git(repo, ['diff', '--no-ext-diff', '--no-textconv', '-M', ...(params.stat ? ['--stat=200'] : []), base, ...tail(params)]);
    const untracked = (await git(repo, ['ls-files', '--others', '--exclude-standard', '-z'])).split('\0').filter(Boolean).slice(0, 200);
    return untracked.length ? `${diff}\nUntracked files (not in the diff):\n${untracked.join('\n')}` : diff;
  },
  async log(repo, params) {
    if (params.code !== undefined && params.regex !== undefined) throw bad('log takes either code or regex, not both');
    const limit = Math.min(100, Math.max(1, Math.trunc(params.limit ?? 30)));
    const args = ['log', params.ref === undefined ? '--all' : await commitOf(repo, params.ref), '--date=short', '--format=%h%x1f%ad%x1f%an%x1f%D%x1f%s', '-n', String(limit)];
    if (params.query !== undefined || params.author !== undefined) args.push('--fixed-strings', '--regexp-ignore-case');
    if (params.query !== undefined) args.push(`--grep=${free(params.query, 'query')}`);
    if (params.author !== undefined) args.push(`--author=${free(params.author, 'author')}`);
    if (params.code !== undefined) args.push(`-S${free(params.code, 'code')}`);
    if (params.regex !== undefined) args.push(`-G${free(params.regex, 'regex')}`);
    if (params.since !== undefined) args.push(`--since=${free(params.since, 'since')}`);
    if (params.until !== undefined) args.push(`--until=${free(params.until, 'until')}`);
    if (params.merges === 'only') args.push('--merges');
    else if (params.merges === 'exclude') args.push('--no-merges');
    const lines = (await git(repo, [...args, ...tail(params)])).split('\n').filter(Boolean).map(line => {
      const [hash, date, author, refs, subject] = line.split('\x1f');
      const refsText = refs ? ` [${refs}]` : '';
      return `- ${hash} ${date} ${author}${refsText} ${subject}`;
    });
    return lines.length ? lines.join('\n') : 'No matching commits.';
  },
  async show(repo, params) {
    const hash = await commitOf(repo, params.commit);
    return git(repo, ['show', '--no-ext-diff', '--no-textconv', '-M', '-m', '--first-parent', '--format=fuller', params.stat ? '--stat' : '--patch', hash, ...tail(params)]);
  },
  async diff(repo, params) {
    const [base, target] = [await commitOf(repo, params.base), await commitOf(repo, params.target)];
    return git(repo, ['diff', '--no-ext-diff', '--no-textconv', '-M', ...(params.stat ? ['--stat=200'] : []), base, target, ...tail(params)]);
  },
  async blame(repo, params) {
    const file = cleanPath(params.path);
    const start = Math.max(1, Math.trunc(params.start ?? 1));
    const end = Math.min(start + 399, Math.trunc(params.end ?? start + 199));
    if (end < start) throw bad('blame needs end >= start');
    return git(repo, ['blame', '-L', `${start},${end}`, ...(params.ref === undefined ? [] : [await commitOf(repo, params.ref)]), '--', file]);
  },
  async file(repo, params) {
    const file = cleanPath(params.path);
    if (params.ref !== undefined) return git(repo, ['show', `${await commitOf(repo, params.ref)}:${file}`]);
    const root = realpathSync(repo);
    let real;
    try { real = realpathSync(path.resolve(root, file)); } catch { throw new Error(`File not found: ${file}`); }
    const relative = path.relative(root, real);
    if (relative.startsWith('..') || path.isAbsolute(relative)) throw bad(`path "${file}" leaves the repository`);
    const info = statSync(real);
    if (!info.isFile() || info.size > FILE_LIMIT) throw new Error(`${file} is not a regular file of at most ${FILE_LIMIT} bytes`);
    return readFileSync(real, 'utf8');
  },
};

/** Runs one repo_inspect action against `repo`; returns text (capped) or throws an Error with the reason. */
export async function inspectRepo(repo, params) {
  check(params?.action, params);
  return limited(await ACTIONS[params.action](repo, params));
}

export function registerRepoInspectTool(pi, repo) {
  const text = pi.zod.string().optional();
  pi.registerTool({
    name: 'repo_inspect',
    label: 'Repo Inspect',
    loadMode: 'essential',
    description: 'Read-only access to this Git repository (besides the read-only code_quality_report, the only thing you can do). Actions: changes (current working-tree diff against base, default HEAD; stat:true for the file list), log (search history by message query, code added/removed, regex, author, dates, path, merges; limit 1-100), show (one commit: message and diff), diff (between two commits), blame (path, optional line range), file (working-tree file, or at a ref). Commits are given as hashes, branch names or tags.',
    parameters: pi.zod.object({
      action: pi.zod.enum(['changes', 'log', 'show', 'diff', 'blame', 'file']),
      base: text, target: text, commit: text, ref: text, path: text, query: text, code: text, regex: text, author: text, since: text, until: text,
      stat: pi.zod.boolean().optional(),
      merges: pi.zod.enum(['include', 'only', 'exclude']).optional(),
      limit: pi.zod.number().int().min(1).max(100000).optional(),
      start: pi.zod.number().int().min(1).max(100000).optional(),
      end: pi.zod.number().int().min(1).max(100000).optional(),
    }),
    approval: () => 'read',
    async execute(_id, params, signal) {
      if (signal?.aborted) throw signal.reason;
      try { return { content: [{ type: 'text', text: await inspectRepo(repo, params) }] }; }
      catch (error) { return { content: [{ type: 'text', text: String(error.message ?? error) }], isError: true }; }
    },
  });
}
