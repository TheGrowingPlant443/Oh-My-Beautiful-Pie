import { readFileSync } from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

export default async function localAiSetup(api) {
  const root = process.env.OMP_MANAGER_LOCAL_AI_SETUP_DIR;
  if (!root || process.env.OMP_MANAGER_LOCAL_AI_SETUP_MODE !== 'local') throw new Error('Local AI setup requires its guarded local process');
  // The app materializes this plugin in a runtime-selected root, not beside its source-module dependencies.
  const { isLocalModel } = await import(pathToFileURL(path.join(root, '..', 'local-ai-models.mjs')).href);
  const marker = `<!-- omp-manager:local-ai:${root}:`;
  api.on('before_agent_start', async (event, ctx) => {
    if (!Array.isArray(event.systemPrompt)) throw new Error('OMP compatibility: systemPrompt must be a string array');
    const base = event.systemPrompt.filter(part => !part.startsWith(marker));
    if (!isLocalModel(ctx.model)) return { systemPrompt: base };
    const sections = [];
    for (const file of ['AGENTS.md', 'RULES.md', 'SKILLS.md']) {
      try {
        const content = readFileSync(path.join(root, file), 'utf8');
        if (content.trim()) sections.push(`${marker}${file} -->\n# Local AI models setup: ${file}\n${content}`);
      } catch (error) {
        if (error.code === 'ENOENT') continue;
        ctx.abort();
        throw new Error(`Cannot read Local AI setup ${file}: ${error.message}`);
      }
    }
    return { systemPrompt: [...base, ...sections] };
  });
}
