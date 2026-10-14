<script lang="ts">
  import { open as pickFile } from "@tauri-apps/plugin-dialog";
  import { api, type SavedConfigs } from "../api";
  import { errorText } from "../stores";
  type Source = "current" | "snapshot" | "native" | "custom" | "none";
  let { mode, value = $bindable(""), active, disabled = false, label, onchange }: {
    mode: "import" | "overlay";
    value?: string;
    active: boolean;
    disabled?: boolean;
    label: string;
    onchange?: (path: string, source: Source) => void;
  } = $props();
  let data = $state<SavedConfigs | null>(null);
  let loadError = $state("");
  let customPath = $state("");
  $effect(() => {
    if (active) void load();
    else customPath = "";
  });
  async function load(): Promise<void> {
    loadError = "";
    try { data = await api.listSavedConfigs(); }
    catch (e) { loadError = errorText(e); }
  }
  function sourceOf(path: string): Source {
    if (!path) return "none";
    if (path === customPath) return "custom";
    if (data && path === data.current) return "current";
    if (data && path === data.nativeGlobal) return "native";
    if (data?.snapshots.some(s => s.path === path)) return "snapshot";
    return "custom";
  }
  async function change(e: Event & { currentTarget: HTMLSelectElement }): Promise<void> {
    const select = e.currentTarget;
    const chosen = select.value;
    if (chosen === "__browse__") {
      try {
        const path = await pickFile({ directory: false, multiple: false, title: "Choose OMP config", filters: [{ name: "OMP YAML config", extensions: ["yml", "yaml"] }] });
        if (typeof path === "string") { customPath = path; value = path; onchange?.(path, "custom"); return; }
      } catch (error) { loadError = errorText(error); }
      select.value = value;
      return;
    }
    value = chosen;
    onchange?.(chosen, sourceOf(chosen));
  }
</script>

<label>{label}
  <select {value} {disabled} onchange={change}>
    {#if mode === "import"}<option value="" disabled>Select a config…</option>{:else}<option value="">No extra config</option>{/if}
    {#each data?.snapshots ?? [] as snapshot (snapshot.path)}
      <option value={snapshot.path}>Saved {new Date(snapshot.modifiedMs).toLocaleString()}{snapshot.path === data?.current ? " · current default" : ""}</option>
    {/each}
    {#if mode === "import" && data?.nativeGlobal}<option value={data.nativeGlobal}>Native global config · {data.nativeGlobal}</option>{/if}
    {#if customPath}<option value={customPath}>Custom · {customPath}</option>{/if}
    <option value="__browse__">Browse for a file…</option>
  </select>
</label>
{#if loadError}<small class="err">{loadError}</small>{/if}
{#if data?.error}<small class="err">{data.error}</small>{/if}
{#if value}<small class="mono dim">{value}</small>{/if}

<style>
  label { display: flex; flex-direction: column; gap: 6px; }
</style>
