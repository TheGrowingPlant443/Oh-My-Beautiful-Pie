import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs';
import path from 'node:path';

// The Architect's only app-specific tool: it records which direct dependencies of a project are basic (built in) and
// which were chosen. The app reads the same file (src-tauri/src/architect.rs), keyed by a hash of the project folder.
export const KEY = /^[0-9a-f]{16}$/;
const MAX_ITEMS = 2000;
const MAX_ID = 300;
const MAX_REASON = 200;

export function saveClassification(dir, { key, items }) {
  if (typeof key !== 'string' || !KEY.test(key)) throw new Error('invalid key');
  if (!Array.isArray(items) || items.length < 1 || items.length > MAX_ITEMS) throw new Error('invalid item');
  for (const item of items) {
    if (!item || typeof item.id !== 'string' || item.id.length < 1 || item.id.length > MAX_ID || typeof item.basic !== 'boolean') throw new Error('invalid item');
    if (item.reason !== undefined && typeof item.reason !== 'string') throw new Error('invalid item');
  }
  const folder = path.join(dir, 'classifications');
  const file = path.join(folder, `${key}.json`);
  let saved = { items: {} };
  if (existsSync(file)) {
    try {
      const parsed = JSON.parse(readFileSync(file, 'utf8'));
      if (parsed && typeof parsed === 'object' && parsed.items && typeof parsed.items === 'object' && !Array.isArray(parsed.items)) saved = { items: parsed.items };
    } catch { /* a corrupt file is replaced */ }
  }
  for (const item of items) saved.items[item.id] = { basic: item.basic, reason: (item.reason ?? '').slice(0, MAX_REASON) };
  saved.updatedAt = Date.now();
  mkdirSync(folder, { recursive: true });
  const temporary = `${file}.${process.pid}.tmp`;
  writeFileSync(temporary, JSON.stringify(saved));
  renameSync(temporary, file);
  return items.length;
}

export function registerClassifyTool(pi, dir) {
  pi.registerTool({
    name: 'classify_dependencies',
    label: 'Classify Dependencies',
    loadMode: 'essential',
    description: 'Record which direct dependencies of a project are BASIC (built in: framework core, language/runtime foundations, type definitions, universally required packages that were not a real design choice) and which were CHOSEN for this project (a deliberate library choice such as a charting, state or UI library). Call it with the key given in the request and one item per package id you were asked about; you may call it several times with parts of the list. Only this tool saves your answer.',
    parameters: pi.zod.object({
      key: pi.zod.string(),
      items: pi.zod.array(pi.zod.object({ id: pi.zod.string(), basic: pi.zod.boolean(), reason: pi.zod.string().optional() })),
    }),
    approval: () => 'read',
    async execute(_id, params, signal) {
      if (signal?.aborted) throw signal.reason;
      try { return { content: [{ type: 'text', text: `Saved ${saveClassification(dir, params)} classifications.` }] }; }
      catch (error) { return { content: [{ type: 'text', text: String(error.message ?? error) }], isError: true }; }
    },
  });
}
