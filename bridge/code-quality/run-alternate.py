"""Fixed in-container argv adapter. No shell, project hooks, remote configs or original tree."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path('/opt/omp')
WORK = Path('/workspace')
OUT = Path('/out')
OUT.mkdir(exist_ok=True)
os.environ.update({'SEMGREP_SEND_METRICS': 'off', 'SEMGREP_ENABLE_VERSION_CHECK': '0', 'DO_NOT_TRACK': '1', 'CHECKOV_SKIP_DOWNLOAD': 'true', 'CKV_SKIP_SECRETS': 'false'})

def has(patterns):
    return any(list(WORK.glob(pattern)) for pattern in patterns)

def fallback(source, dest, patterns):
    if not has(patterns):
        shutil.copyfile(ROOT / 'fallback' / source, WORK / dest)
        return True
    return False

def config_override(key, patterns):
    # Only choose existing individual configs. Do not replace a failing repo config.
    for pattern in patterns:
        matches = sorted(path for path in WORK.glob(pattern) if path.is_file())
        if matches:
            os.environ[key + '_CONFIG_FILE'] = str(matches[0])
            return

def capture(argv, name):
    # Keep machine reports separately bounded from the engine's continuously drained log tail.
    process = subprocess.Popen(argv, cwd=WORK, stdout=subprocess.PIPE)
    size = 0
    with (OUT / name).open('wb') as target:
        while True:
            block = process.stdout.read(65536)
            if not block:
                break
            size += len(block)
            if size > 64 * 1024 * 1024:
                process.kill()
                process.wait()
                raise RuntimeError('Machine report exceeded 64 MiB; narrow project exclusions.')
            target.write(block)
    return process.wait()

action = sys.argv[1]
if action == 'npm':
    if not (WORK / 'package-lock.json').is_file():
        raise RuntimeError('Prepare npm dependencies requires package-lock.json; other package managers need an approved custom image.')
    code = subprocess.call(['npm', 'ci', '--ignore-scripts', '--no-audit', '--no-fund'], cwd=WORK)
    if code:
        sys.exit(code)
    # Tar library preserves links. The host validator rejects links, traversal and >4 GiB;
    # npm packages using symlinks cannot escape through an apparently successful copy.
    # .bin launchers are unnecessary: the pinned image supplies the lint executables.
    bin_dir = WORK / 'node_modules' / '.bin'
    if bin_dir.exists():
        for link in bin_dir.iterdir():
            if link.is_symlink():
                resolved = link.resolve(strict=True)
                if not resolved.is_relative_to((WORK / 'node_modules').resolve()):
                    raise RuntimeError('npm dependency launcher escapes dependency tree')
                link.unlink()
    sys.exit(0)
elif action == 'megalinter':
    package = WORK / 'package.json'
    package_config = package.is_file() and 'eslintConfig' in json.loads(package.read_text(encoding='utf8'))
    if not package_config:
        fallback('eslint.config.cjs', 'eslint.config.cjs', ['eslint.config.*', '.eslintrc*'])
    for key in ['JAVASCRIPT_ES', 'TYPESCRIPT_ES', 'JSX_ESLINT', 'TSX_ESLINT']:
        config_override(key, ['eslint.config.js', 'eslint.config.mjs', 'eslint.config.cjs', 'eslint.config.ts', 'eslint.config.mts', 'eslint.config.cts', '.eslintrc*'])
    if not has(['.ruff.toml', 'ruff.toml']):
        pyproject = WORK / 'pyproject.toml'
        if not pyproject.exists() or '[tool.ruff' not in pyproject.read_text(encoding='utf8'):
            fallback('.ruff.toml', '.ruff.toml', ['.ruff.toml', 'ruff.toml'])
        else:
            os.environ['PYTHON_RUFF_CONFIG_FILE'] = str(pyproject)
    config_override('PYTHON_RUFF', ['.ruff.toml', 'ruff.toml'])
    fallback('java-pmd-ruleset.xml', 'java-pmd-ruleset.xml', ['java-pmd-ruleset.xml', 'ruleset.xml', '**/pmd*.xml'])
    config_override('JAVA_PMD', ['java-pmd-ruleset.xml', 'ruleset.xml', '**/pmd*.xml'])
    fallback('.hadolint.yaml', '.hadolint.yaml', ['.hadolint.yaml', '.hadolint.yml'])
    config_override('DOCKERFILE_HADOLINT', ['.hadolint.yaml', '.hadolint.yml'])
    fallback('.jscpd.json', '.jscpd.json', ['.jscpd.json', '.jscpd.yaml', '.jscpd.yml'])
    config_override('COPYPASTE_JSCPD', ['.jscpd.json', '.jscpd.yaml', '.jscpd.yml'])
    pyproject = WORK / 'pyproject.toml'
    if pyproject.is_file() and '[tool.bandit' in pyproject.read_text(encoding='utf8'):
        os.environ['PYTHON_BANDIT_CONFIG_FILE'] = str(pyproject)
    else:
        fallback('.bandit.yml', '.bandit.yml', ['.bandit', '.bandit.yml', 'bandit.yaml'])
        config_override('PYTHON_BANDIT', ['.bandit.yml', 'bandit.yaml'])
    fallback('.shellcheckrc', '.shellcheckrc', ['.shellcheckrc'])
    fallback('.checkov.yml', '.checkov.yml', ['.checkov.yml', '.checkov.yaml'])
    config_override('REPOSITORY_CHECKOV', ['.checkov.yml', '.checkov.yaml'])
    os.environ.update({'MEGALINTER_CONFIG': str(ROOT / 'megalinter.yml'), 'DEFAULT_WORKSPACE': str(WORK), 'GITHUB_WORKSPACE': str(WORK), 'APPLY_FIXES': 'none', 'VALIDATE_ALL_CODEBASE': 'true', 'PARALLEL': 'false', 'REPORT_OUTPUT_FOLDER': str(OUT), 'JSON_REPORTER': 'true', 'SARIF_REPORTER': 'true', 'PRE_COMMANDS': '[]', 'POST_COMMANDS': '[]', 'PLUGINS': '[]', 'EXTENDS': '[]'})
    os.chdir('/'); sys.exit(subprocess.call(['python', '-m', 'megalinter.run']))
elif action in ('opengrep', 'semgrep'):
    args = [action, 'scan']
    if action == 'semgrep':
        args += ['--oss', '--metrics=off', '--disable-version-check']
    else:
        capabilities = json.loads((ROOT / 'validation.json').read_text())['versions']['opengrep']
        if capabilities['taintIntrafile']:
            args += ['--taint-intrafile']
    args += ['--jobs=2', '--quiet', '--config', str(ROOT / 'rules' / 'selected'), '--config', str(ROOT / 'rules' / 'omp-quality.yml'), '--json', str(WORK)]
    sys.exit(capture(args, action + '.json'))
else:
    raise RuntimeError('Unknown fixed analyzer action')
