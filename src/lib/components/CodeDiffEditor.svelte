<script lang="ts">
  import { onMount } from "svelte";
  import type { DiffContent } from "../api";
  import { errorText } from "../stores";
  // The parent re-creates this component (keyed) whenever a different file or a reload is shown, so the
  // models below are only built from the initial content.
  let { content, onChange, onSave }: { content: DiffContent; onChange: (text: string) => void; onSave: () => void } = $props();
  const SIDE_BY_SIDE_MIN = 1000;
  let host = $state<HTMLDivElement>();
  let status = $state("Loading editor…");
  onMount(() => {
    let disposed = false;
    let teardown: (() => void) | undefined;
    // Dynamic on purpose: Monaco is ~3 MB and only the Code diff page needs it, so startup does not parse it.
    void import("../diff-editor").then(({ monaco, languageFor }) => {
      if (disposed) return;
      const element = host!;
      const language = languageFor(content.path);
      const original = monaco.editor.createModel(content.original ?? "", language);
      const modified = monaco.editor.createModel(content.modified ?? "", language);
      modified.setEOL(monaco.editor.EndOfLineSequence.LF);
      const editor = monaco.editor.createDiffEditor(element, {
        theme: "vs-dark",
        originalEditable: false,
        readOnly: content.readOnlyReason !== null,
        lineNumbers: "on",
        automaticLayout: true,
        renderSideBySide: element.clientWidth >= SIDE_BY_SIDE_MIN,
        scrollBeyondLastLine: false,
        minimap: { enabled: false },
        ariaLabel: `Code diff of ${content.path}`,
      });
      editor.setModel({ original, modified });
      const changes = modified.onDidChangeContent(() => onChange(modified.getValue()));
      editor.getModifiedEditor().addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS, () => onSave());
      const observer = new ResizeObserver(() => editor.updateOptions({ renderSideBySide: element.clientWidth >= SIDE_BY_SIDE_MIN }));
      observer.observe(element);
      status = "";
      teardown = () => {
        observer.disconnect();
        changes.dispose();
        // The widget must release its models before they are disposed.
        editor.setModel(null);
        editor.dispose();
        original.dispose();
        modified.dispose();
      };
    }).catch(error => { status = `The editor could not be loaded: ${errorText(error)}`; });
    return () => { disposed = true; teardown?.(); };
  });
</script>
<div class="holder">
  {#if status}<p class="status dim" role="status">{status}</p>{/if}
  <div class="editor" bind:this={host} role="group" aria-label="Code diff editor"></div>
</div>
<style>
  .holder { position: relative; width: 100%; height: 100%; min-height: 240px; }
  .editor { position: absolute; inset: 0; }
  .status { position: absolute; margin: 12px; z-index: 1; }
</style>
