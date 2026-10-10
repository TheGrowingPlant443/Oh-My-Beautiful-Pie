<script lang="ts">
  /** Live plan reader: manual refresh plus an optional poll. */
  import { api } from "../api";
  import { errorText, reportError } from "../stores";

  interface Props {
    instanceId: string;
    pollMs?: number;
  }

  let { instanceId, pollMs = 6000 }: Props = $props();

  let text = $state<string | null>(null);
  let loading = $state(false);
  let loaded = $state(false);
  let error = $state<string | null>(null);
  let auto = $state(true);
  let copied = $state(false);

  async function load(): Promise<void> {
    loading = true;
    try {
      text = await api.readPlan(instanceId);
      error = null;
    } catch (err) {
      error = errorText(err);
      reportError("read plan failed", err);
    } finally {
      loading = false;
      loaded = true;
    }
  }

  $effect(() => {
    void load();
    if (!auto) return;
    const timer = setInterval(() => void load(), pollMs);
    return () => clearInterval(timer);
  });

  async function copy(): Promise<void> {
    if (!text) return;
    try {
      await navigator.clipboard.writeText(text);
      copied = true;
      setTimeout(() => (copied = false), 1400);
    } catch (err) {
      reportError("copy plan failed", err);
    }
  }
</script>

<section class="panel">
  <header>
    <h3>Plan</h3>
    {#if loaded && text === null}<span class="pill">none</span>
    {:else if text}<span class="pill dim">{text.length} chars</span>{/if}
    <span class="spacer"></span>
    <button class="ghost tiny" class:active={auto} onclick={() => (auto = !auto)}>
      {auto ? "Auto" : "Manual"}
    </button>
    <button class="ghost tiny" disabled={loading} onclick={() => void load()}>Refresh</button>
    <button class="ghost tiny" disabled={text === null} onclick={() => void copy()}>
      {copied ? "Copied" : "Copy"}
    </button>
  </header>

  <div class="body">
    {#if error}
      <p class="err msg">{error}</p>
    {:else if text === null}
      <p class="empty">{loading ? "Reading plan…" : "No plan for this session yet."}</p>
    {:else}
      <pre class="plan mono">{text}</pre>
    {/if}
  </div>
</section>

<style>
  h3 {
    margin: 0;
    font-size: 12px;
    letter-spacing: .07em;
    text-transform: uppercase;
    color: var(--text-dim);
  }
  .tiny { font-size: 12px; padding: 3px 7px; }
  .msg { margin: 0; font-size: 12px; }
  .plan {
    margin: 0;
    font-size: 12px;
    line-height: 1.55;
    white-space: pre-wrap;
    word-break: break-word;
    color: var(--text);
  }
</style>
