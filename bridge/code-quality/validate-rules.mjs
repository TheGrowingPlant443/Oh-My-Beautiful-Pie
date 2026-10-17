// Run inside the derivative image: node /opt/omp/validate-rules.mjs
// This is also a mandatory image build step; never accept an unvalidated or empty rule pack.
import { spawnSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
const root = process.env.OMP_ASSETS ?? '/opt/omp';
const lock = JSON.parse(readFileSync(`${root}/tools.lock.json`, 'utf8'));
function run(program, args) {
  const result = spawnSync(program, args, { encoding: 'utf8', maxBuffer: 16 * 1024 * 1024, env: { ...process.env, SEMGREP_SEND_METRICS: 'off', SEMGREP_ENABLE_VERSION_CHECK: '0' } });
  if (result.error || result.status !== 0) throw new Error(`${program} ${args.join(' ')} failed: ${result.error?.message ?? result.stderr ?? result.stdout}`);
  return `${result.stdout ?? ''}${result.stderr ?? ''}`;
}
const versions = {};
for (const [program, expected] of [['opengrep', lock.opengrep.version], ['semgrep', lock.semgrep.version]]) {
  const version = run(program, ['--version']).trim();
  if (!version.includes(expected)) throw new Error(`${program}: expected pinned ${expected}, got ${version}`);
  const help = run(program, ['scan', '--help']);
  for (const flag of ['--json', '--config', '--jobs', '--quiet']) {
    if (!help.includes(flag)) throw new Error(`${program}: unsupported required flag ${flag}`);
  }
  if (program === 'semgrep') for (const flag of ['--oss', '--metrics', '--disable-version-check']) {
    if (!help.includes(flag)) throw new Error(`${program}: unsupported required flag ${flag}`);
  }
  const validation = run(program, ['scan', '--validate', '--config', `${root}/rules/selected`, '--config', `${root}/rules/omp-quality.yml`, '--metrics=off', '--disable-version-check']);
  const fixtureResult = run(program, ['scan', '--test', '--config', `${root}/rules/omp-quality.yml`, `${root}/rules/fixtures`, '--metrics=off', '--disable-version-check']);
  versions[program] = { version, help, validation, fixtureResult, taintIntrafile: help.includes('--taint-intrafile') };
}
writeFileSync(`${root}/validation.json`, JSON.stringify({ version: 1, versions }, null, 2));
