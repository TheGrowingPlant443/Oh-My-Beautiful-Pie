import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// TAURI_DEV_HOST is set by `tauri dev` when targeting a network/phone host.
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [svelte()],
  // Tauri expects a fixed port; fail fast if it is busy.
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // Do not watch the Rust side; cargo rebuilds would spam HMR.
      ignored: ["**/src-tauri/**"],
    },
  },
  envPrefix: ["VITE_", "TAURI_ENV_"],
  build: {
    target: "es2022",
    outDir: "dist",
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
  },
});
