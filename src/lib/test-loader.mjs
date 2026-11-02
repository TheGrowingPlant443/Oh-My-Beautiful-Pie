import { registerHooks } from "node:module";

const library = new URL("./", import.meta.url).href;

// Resolve bundler-style imports in place so coverage names original TypeScript, not generated copies.
registerHooks({
  resolve(specifier, context, nextResolve) {
    if (context.parentURL?.startsWith(library) && /^\.\/[\w-]+$/u.test(specifier)) {
      return nextResolve(`${specifier}.ts`, context);
    }
    return nextResolve(specifier, context);
  },
});

export async function importTs(name) {
  return import(new URL(`./${name}.ts`, import.meta.url).href);
}
