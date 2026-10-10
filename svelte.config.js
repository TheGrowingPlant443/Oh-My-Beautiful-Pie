import { vitePreprocess } from "@sveltejs/vite-plugin-svelte";

export default {
  // Required for `lang="ts"` script blocks and svelte-check.
  preprocess: vitePreprocess(),
  compilerOptions: {
    runes: true,
  },
};
