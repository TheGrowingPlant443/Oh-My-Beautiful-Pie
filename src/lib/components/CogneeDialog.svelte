<script lang="ts">
  import { get } from "svelte/store";
  import { knowledge, knowledgeApi, type CogneeOpenResult } from "../knowledge";

  // Opens the Cognee web interface. No address, folder or other configuration is asked for: an explicit click revalidates the installed
  // interface and, when it can, starts this app's own frontend for the already running local Cognee backend and opens it in the system
  // browser. It never installs or downloads anything, never starts Cognee's backend, MCP server or Docker and never changes its stores.
  // Closing this popup only hides it: a start that was requested keeps going and its answer is shown when the popup is opened again.
  let { open, onClose }: { open: boolean; onClose: () => void } = $props();

  let dialog = $state<HTMLDialogElement>();
  let programmaticCloses = 0;
  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) dialog.showModal();
    else if (!open && dialog.open) { programmaticCloses++; dialog.close(); }
  });

  let caps = $state(get(knowledge).capabilities);
  $effect(() => knowledge.subscribe(value => { caps = value.capabilities; }));
  const cognee = $derived(caps?.cognee ?? null);

  // The in-flight request lives at component level (not tied to `open`), so closing and reopening the popup neither orphans nor repeats it.
  let busy = $state(false);
  let result = $state<CogneeOpenResult | null>(null);
  let failure = $state<string | null>(null);
  let launching: Promise<void> | null = null;

  function launch(): Promise<void> {
    if (launching) return launching;
    busy = true; result = null; failure = null;
    launching = knowledgeApi.openCognee()
      .then(answer => { result = answer; })
      .catch(error => { failure = String(error); })
      .finally(() => { busy = false; launching = null; });
    return launching;
  }

  const status = $derived.by(() => {
    if (!cognee) return "Cognee has not been looked for yet.";
    if (cognee.state === "unreadable") return `Cognee could not be fully inspected${cognee.message ? `: ${cognee.message}` : "."} This is not proof it is absent.`;
    if (cognee.state === "absent" || !cognee.installed) return "Cognee was not found on this computer.";
    const version = cognee.version ? ` ${cognee.version}` : "";
    if (cognee.ui.state === "ready") return `Cognee${version} is installed and its web interface can be started when you open it.`;
    return `Cognee${version} is installed. ${cognee.ui.message ?? "Its web interface cannot be started from here."}`;
  });
</script>
<dialog bind:this={dialog} aria-labelledby="cognee-title" onclose={() => { if (programmaticCloses) programmaticCloses--; else onClose(); }} oncancel={event => { event.preventDefault(); onClose(); }}>
  <section>
    <header><h3 id="cognee-title">Cognee</h3><button onclick={onClose}>Close</button></header>
    <div class="body">
      <p role="status" class:err={cognee?.state === "unreadable"}>{status}</p>
      {#if busy}
        <p role="status">Starting the Cognee interface… the first start can take up to a minute. You can close this window; it keeps going.</p>
      {/if}
      {#if result?.state === "opened"}
        <p role="status">Opened {result.url ?? "the Cognee interface"} in your browser.</p>
      {:else if result}
        <p class="err" role="alert">{result.message ?? "The Cognee interface could not be opened."}</p>
        {#if result.state === "openFailed" && result.url}<p class="dim">The interface is running at <code>{result.url}</code>; open that address in your browser yourself, or try again.</p>{/if}
      {/if}
      {#if failure}<p class="err" role="alert">{failure}</p>{/if}
      <p class="dim">Your Cognee backend must already be running; OMP Manager never starts it. Starting the interface may let it compile and write its own cache files, but nothing is installed or downloaded and Cognee's data and settings are not changed.</p>
      <div class="actions">
        <button disabled={busy || !cognee || cognee.state === "absent"} onclick={() => void launch()}>Open Cognee UI</button>
      </div>
    </div>
  </section>
</dialog>
<style>
  dialog { width: min(600px, 95vw); max-height: 85vh; box-sizing: border-box; border: 1px solid var(--line); padding: 0; border-radius: var(--radius); background: var(--panel); color: var(--text); }
  dialog::backdrop { background: rgba(4, 7, 12, .66); }
  section { display: flex; flex-direction: column; max-height: 85vh; }
  header { display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 14px 16px; border-bottom: 1px solid var(--line); }
  h3, p { margin: 0; }
  .body { overflow: auto; padding: 16px; display: flex; flex-direction: column; gap: 12px; overflow-wrap: anywhere; }
  .actions { display: flex; gap: 8px; }
  .dim { color: var(--text-dim); }
  .err { color: var(--err); }
</style>
