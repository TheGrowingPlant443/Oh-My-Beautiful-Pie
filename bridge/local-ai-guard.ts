import { existsSync, mkdirSync, readFileSync, writeFileSync, lstatSync } from 'node:fs';
import path from 'node:path';
import { installLocalModelPolicy, isLocalModel, LOCAL_MODEL_REQUIRED } from './local-ai-models.mjs';

export default function localAiGuard(api) {
  const root = process.env.OMP_MANAGER_LOCAL_AI_SETUP_DIR;
  const enabled = process.env.OMP_MANAGER_LOCAL_AI_SETUP_MODE === 'local';
  function fatal(error) {
    process.stderr.write(`OMP Manager Local AI setup: ${error.message ?? 'incompatible runtime'}\n`);
    // Extension factory/hook errors are otherwise caught by OMP; never continue without the guard.
    process.exit(78);
  }
  if (!root) return fatal(new Error('setup directory is unavailable'));
  if (enabled) {
    try { installLocalModelPolicy(api.pi?.ModelRegistry, api.pi?.Settings, api.pi?.settings); } catch (error) { fatal(error); }
  }
  function checkSession(ctx) {
    try {
      const id = ctx.sessionManager.getSessionId();
      if (typeof id !== 'string' || !/^[a-zA-Z0-9-]+$/.test(id)) throw new Error('native session identity is unavailable');
      const directory = path.join(root, '.local-sessions');
      const marker = path.join(directory, `${id}.json`);
      if (existsSync(marker)) {
        if (!lstatSync(marker).isFile() || lstatSync(marker).isSymbolicLink()) throw new Error('local conversation marker is not a regular file');
        const saved = JSON.parse(readFileSync(marker, 'utf8'));
        if (saved.version !== 1 || saved.local !== true) throw new Error('invalid local conversation marker');
        if (!enabled) throw new Error(LOCAL_MODEL_REQUIRED);
      } else if (enabled) {
        mkdirSync(directory, { recursive: true, mode: 0o700 });
        writeFileSync(marker, JSON.stringify({ version: 1, local: true }), { flag: 'wx', mode: 0o600 });
      }
    } catch (error) { fatal(error); }
  }
  api.on('session_start', async (_event, ctx) => checkSession(ctx));
  api.on('session_switch', async (_event, ctx) => checkSession(ctx));
  api.on('session_branch', async (_event, ctx) => checkSession(ctx));
  api.on('before_agent_start', async (_event, ctx) => checkSession(ctx));
  api.on('before_provider_request', async (_event, ctx) => {
    checkSession(ctx);
    // Covers native request paths with explicit API-key callbacks as well as normal registry resolution.
    if (enabled && !isLocalModel(ctx.model)) fatal(new Error(LOCAL_MODEL_REQUIRED));
  });
  globalThis[Symbol.for('omp-manager.local-ai-guard-ready')] = Object.freeze({ root, enabled, checkSession });
}
