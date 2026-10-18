import { readFileSync } from 'node:fs';
import path from 'node:path';

export default function coordinatorSetup(pi) {
  const root = process.env.OMP_MANAGER_PRIVATE_SETUP_DIR;
  const role = process.env.OMP_MANAGER_PRIVATE_SETUP_ROLE;
  if (!root) throw new Error('OMP private setup directory is unavailable');
  if (role !== 'manager' && role !== 'architect') throw new Error('OMP private setup role must be manager or architect');
  const label = role === 'manager' ? 'Manager' : 'Architect';
  const marker = `<!-- omp-manager:${role}:private-setup:${root}:`;
  pi.on('before_agent_start', async (event, ctx) => {
    if (ctx.agent?.kind !== 'main') return;
    if (!Array.isArray(event.systemPrompt)) throw new Error('OMP compatibility: systemPrompt must be a string array');
    const sections = [];
    for (const file of ['AGENTS.md', 'RULES.md', 'SKILLS.md']) {
      try {
        const content = readFileSync(path.join(root, file), 'utf8');
        if (content.trim()) sections.push(`${marker}${file} -->\n# ${label} setup: ${file}\n${content}`);
      } catch (error) {
        if (error.code === 'ENOENT') continue;
        ctx.ui.notify(`Cannot read ${label} setup ${file}: ${error.message}`, 'error');
        await ctx.abort();
        throw error;
      }
    }
    const base = event.systemPrompt.filter(part => !part.startsWith(marker));
    return { systemPrompt: [...base, ...sections] };
  });
}
