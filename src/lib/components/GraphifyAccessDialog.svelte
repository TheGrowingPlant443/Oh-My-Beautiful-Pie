<script lang="ts">
  import { open as pickFolderDialog } from "@tauri-apps/plugin-dialog";
  import { untrack } from "svelte";
  import { createGraphifyAccess } from "../graphify-access";
  import { knowledge, knowledgeApi, graphifyAction, projectFolder, type KnowledgeCapabilities } from "../knowledge";

  // Shown instead of the graph view when the shown project has no usable Graphify output (absent, foreign, malformed, unreadable, or not
  // probed yet). It explains exactly what was found and lets the human choose an existing output folder. The backend accepts it only when
  // its `.graphify_root` names this project; nothing is copied, edited or run, and no HTML is opened. Cancelling changes nothing.
  //
  // `capabilities` must be the answer accepted for `folder` (`capabilitiesFor(...)`), or null while it is unknown; `loading` tells that a probe is running.
  let { open, folder, capabilities, loading = false, onClose, onOpenGraph }: {
    open: boolean;
    folder: string | null;
    capabilities: KnowledgeCapabilities | null;
    loading?: boolean;
    onClose: () => void;
    /** Opens the graph view of `folder` (called after a chosen output was registered and admitted, or when the output is now valid). */
    onOpenGraph: (folder: string) => void;
  } = $props();

  let dialog = $state<HTMLDialogElement>();
  let programmaticCloses = 0;
  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) dialog.showModal();
    else if (!open && dialog.open) { programmaticCloses++; dialog.close(); }
  });

  const access = createGraphifyAccess({
    api: knowledgeApi,
    pick: async () => {
      const picked = await pickFolderDialog({ directory: true, multiple: false, title: "Choose the existing Graphify output folder (graphify-out) for this project" });
      return typeof picked === "string" ? picked : null;
    },
    // Read live after every await: props are getters, so this is the project shown now.
    current: () => ({ folder, projectKey: capabilities?.projectKey ?? null }),
    adopt: value => knowledge.adopt(value),
    navigate: target => { onClose(); onOpenGraph(target); },
  });
  let view = $state({ busy: false, error: null as string | null });
  $effect(() => access.subscribe(value => { view = value; }));
  // A project switch, closing or unmounting ends the running operation's right to change anything (this also covers A to B to A).
  $effect(() => {
    void folder;
    void open;
    untrack(() => access.invalidate());
    return () => access.invalidate();
  });

  const graphify = $derived(capabilities?.graphify ?? null);
  const action = $derived(graphifyAction(capabilities));
  const target = $derived(projectFolder(folder));
  const detail = $derived.by(() => {
    if (!target) return "Choose a project first.";
    if (!graphify) return loading ? "Checking this project's Graphify output…" : "This project's Graphify output has not been checked yet.";
    switch (graphify.state) {
      case "absent": return "This project has no Graphify output: there is no graphify-out/graph.json in it, and no output has been chosen for it.";
      case "foreign": return graphify.message ?? "The Graphify output found for this project was built for another project, so it is not shown.";
      case "malformed": return graphify.message ?? "The Graphify output of this project is not readable as a graph.";
      case "unavailable": return graphify.message ?? "The Graphify output of this project cannot be inspected.";
      case "unverified": return graphify.message ?? "A Graphify output exists, but it carries no project marker, so it is shown as unverified.";
      default: return "A Graphify output for this project is available.";
    }
  });
</script>
<dialog bind:this={dialog} aria-labelledby="graphify-access-title" onclose={() => { if (programmaticCloses) programmaticCloses--; else onClose(); }} oncancel={event => { event.preventDefault(); onClose(); }}>
  <section>
    <header><h3 id="graphify-access-title">Graphify</h3><button onclick={onClose}>Close</button></header>
    <div class="body">
      {#if target}<p class="dim mono" title="Project">{target}</p>{/if}
      <p role="status" class:err={graphify !== null && graphify.state !== "absent" && action === "access"}>{detail}</p>
      {#if graphify?.sourceRoot && action === "access"}<p class="dim">The output claims to have been built for <code>{graphify.sourceRoot}</code>. To view it as its own graph, open that folder as a project; this app never edits the marker or adopts a copy by name.</p>{/if}
      {#if view.busy}<p role="status">Waiting for your choice and checking the folder…</p>{/if}
      {#if view.error}<p class="err" role="alert">{view.error}</p>{/if}
      <p class="dim">An existing output is accepted only when its <code>.graphify_root</code> names this project. Graphify is not run and no graph.html or remote script is opened.</p>
      <div class="actions">
        {#if action === "view" && target}<button class="primary" onclick={() => { onClose(); onOpenGraph(target); }}>Open Graphify graph</button>{/if}
        <button disabled={view.busy || !target} onclick={() => { if (target) void access.choose(target, capabilities?.projectKey ?? null); }}>Choose existing Graphify output…</button>
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
  .actions { display: flex; flex-wrap: wrap; gap: 8px; }
  .dim { color: var(--text-dim); }
  .err { color: var(--err); }
  .mono { font-family: var(--mono, ui-monospace, monospace); font-size: 12px; }
</style>
