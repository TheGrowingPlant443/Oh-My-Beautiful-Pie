<script lang="ts">
  import { onMount } from "svelte";
  import { api, type BrowserInfo } from "../api";
  import { reportError } from "../stores";
  let { instanceId, generation, info, hidden }: {
    instanceId: string; generation: number; info: BrowserInfo; hidden: boolean;
  } = $props();
  let holder = $state<HTMLDivElement>();
  const nativeReady = $derived(info.state === "ready" && info.pid !== null && !info.error);
  let lastKey = "";

  async function sync(): Promise<void> {
    if (!holder) return;
    const box = holder.getBoundingClientRect();
    const key = !nativeReady || hidden || box.width < 1 || box.height < 1
      ? "hidden" : [instanceId, generation, box.left, box.top, box.width, box.height].map(String).join(":");
    if (key === lastKey) return;
    lastKey = key;
    try {
      if (key === "hidden") await api.setBrowserView(null, null, null);
      else await api.setBrowserView(instanceId, generation, { x: box.left, y: box.top, width: box.width, height: box.height });
    } catch (error) { reportError("Position browser", error); lastKey = ""; }
  }
  $effect(() => { void nativeReady; void hidden; void instanceId; void generation; void sync(); });
  onMount(() => {
    const observer = new ResizeObserver(() => void sync());
    if (holder) observer.observe(holder);
    window.addEventListener("resize", sync);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", sync);
      void api.setBrowserView(null, null, null).catch(() => {});
    };
  });
</script>
<section class="browser" aria-label="Terminal browser">
  <div class="holder" bind:this={holder}>
    {#if info.error}<p class="err" role="alert">{info.error}</p>
    {:else if !nativeReady}<p class="dim" role="status">Opening browser…</p>{/if}
  </div>
</section>
<style>
  .browser { height: 100%; min-width: 0; display: flex; flex-direction: column; background: #101722; border: 1px solid var(--line); }
  .dim { color: var(--text-dim); font-size: 12px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .holder { flex: 1; min-height: 0; padding: 12px; overflow: auto; }
  p { margin: 0; overflow-wrap: anywhere; }
</style>
