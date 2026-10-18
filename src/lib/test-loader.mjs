import { mkdir, readFile, writeFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
import { pathToFileURL } from "node:url";

const root = new URL(`../../node_modules/.omp-test-cache/${process.pid}/`, import.meta.url);
const loaded = new Map();

/**
 * Imports a lib `.ts` module whose relative runtime imports are extensionless (as the bundler wants): each module is type-stripped
 * into a scratch `.mjs` under node_modules (so bare imports such as svelte/store still resolve) and its relative imports are
 * pointed at the stripped siblings. Type-only imports disappear with the types.
 */
export async function importTs(name) {
  const known = loaded.get(name);
  if (known) return known;
  const pending = (async () => {
    await mkdir(root, { recursive: true });
    let source = await readFile(new URL(`./${name}.ts`, import.meta.url), "utf8");
    const specifiers = [...source.matchAll(/^import\s+(?!type\b)[^;]*?from\s+"\.\/([\w-]+)";/gms)].map(match => match[1]);
    for (const dependency of new Set(specifiers)) {
      await importTs(dependency);
      source = source.replaceAll(`"./${dependency}"`, JSON.stringify(`./${dependency}.mjs`));
    }
    const target = new URL(`${name}.mjs`, root);
    await writeFile(target, stripTypeScriptTypes(source));
    return import(pathToFileURL(target.pathname.replace(/^\/(\w:)/, "$1")).href);
  })();
  loaded.set(name, pending);
  return pending;
}
