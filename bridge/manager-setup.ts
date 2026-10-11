import { readFileSync } from 'node:fs';
import path from 'node:path';

export default function managerSetup(pi) {
  const root = process.env.OMP_MANAGER_SETUP_DIR;
  if (!root) throw new Error('OMP Manager setup directory is unavailable');
  const marker = `<!-- omp-manager:private-setup:${root}:`;
  pi.on('before_agent_start', async (event, ctx) => {
    if (ctx.agent?.kind !== 'main') return;
    if (!Array.isArray(event.systemPrompt)) throw new Error('OMP compatibility: systemPrompt must be a string array');
    const sections = [];
    for (const file of ['AGENTS.md', 'RULES.md', 'SKILLS.md']) {
      try {
        const content = readFileSync(path.join(root, file), 'utf8');
        if (content.trim()) sections.push(`${marker}${file} -->\n# Manager setup: ${file}\n${content}`);
      } catch (error) {
        if (error.code === 'ENOENT') continue;
        ctx.ui.notify(`Cannot read Manager setup ${file}: ${error.message}`, 'error');
        await ctx.abort();
        throw error;
      }
    }
    const base = event.systemPrompt.filter(part => !part.startsWith(marker));
    return { systemPrompt: [...base, ...sections] };
  });
}
