<script lang="ts">
  import type { SourceIndexStatus } from "../api";
  import type { CoverageState } from "../system-overview";

  let { status, coverage, onLoad }: { status: SourceIndexStatus | null; coverage: CoverageState; onLoad: (more: boolean) => void } = $props();
  const summary = $derived(status?.coverage ?? null);
  const unavailable = $derived((status?.capabilities.profiles ?? []).filter(profile => !profile.available));
</script>
<details class="coverage" ontoggle={event => { if ((event.currentTarget as HTMLDetailsElement).open && coverage.status === "idle") onLoad(false); }}>
  <summary>Coverage and parser capabilities{summary ? ` · ${summary.parsedFiles.toLocaleString()} of ${summary.admittedFiles.toLocaleString()} files parsed` : ""}</summary>
  {#if status}
    {#if status.capabilities.hint}<p class="warn" role="alert">{status.capabilities.hint}</p>{/if}
    {#if status.capabilities.probed}
      <p class="dim">Runtime: {status.capabilities.runtime.source}{status.capabilities.runtime.version ? ` · Python ${status.capabilities.runtime.version}` : ""}{status.capabilities.runtime.treeSitter ? ` · Tree-sitter ${status.capabilities.runtime.treeSitter}` : ""}{status.capabilities.runtime.error ? ` · ${status.capabilities.runtime.error}` : ""}</p>
    {/if}
    {#if unavailable.length}
      <p class="dim">Unavailable grammars (files in these languages are listed but not parsed): {unavailable.map(profile => `${profile.language}${profile.error ? ` (${profile.error})` : ""}`).join("; ")}</p>
    {/if}
    {#if summary}
      <p>Files: {summary.admittedFiles.toLocaleString()} admitted · {summary.parsedFiles.toLocaleString()} parsed · {summary.partialFiles.toLocaleString()} partial · {summary.failedFiles.toLocaleString()} failed · {summary.skippedFiles.toLocaleString()} skipped · {summary.unsupportedFiles.toLocaleString()} unsupported</p>
      <p>Callsites: {summary.encounteredSites.toLocaleString()} found · {summary.resolvedSites.toLocaleString()} resolved · {summary.unresolvedSites.toLocaleString()} unresolved · {summary.dynamicSites.toLocaleString()} dynamic</p>
      {#if summary.partial}<p class="warn" role="status">Coverage is partial{summary.lastFullyProcessedFile ? `; last fully processed file: ${summary.lastFullyProcessedFile}` : ""}.</p>{/if}
    {/if}
  {/if}
  {#if coverage.status === "loading" && coverage.items.length === 0}<p role="status">Loading coverage…</p>{/if}
  {#if coverage.error}<p class="err" role="alert">{coverage.error}</p>{/if}
  <ul>
    {#each coverage.items as row, index (index)}
      {#if row.kind === "language"}<li>{row.language}: {row.files} files · {row.parsed} parsed · {row.partial} partial · {row.failed} failed · {row.unsupported} unsupported{row.diagnostics.length ? ` — ${row.diagnostics.join("; ")}` : ""}</li>
      {:else if row.kind === "file"}<li><code>{row.path}</code> — {row.status}{row.language ? ` (${row.language})` : ""}{row.diagnostics.length ? `: ${row.diagnostics.join("; ")}` : ""}</li>
      {:else if row.kind === "limitation"}<li>{row.text}</li>{/if}
    {/each}
  </ul>
  {#if coverage.nextOffset !== null}<button disabled={coverage.status === "loading"} onclick={() => onLoad(true)}>Load more ({coverage.items.length.toLocaleString()} of {coverage.total.toLocaleString()})</button>{/if}
</details>
<style>
  .coverage { border: 1px solid var(--line); border-radius: 8px; padding: 6px 10px; background: var(--panel); font-size: 12px; max-height: 240px; overflow: auto; }
  summary { cursor: pointer; }
  p { margin: 4px 0; overflow-wrap: anywhere; }
  ul { margin: 4px 0; padding-left: 18px; }
  .dim { color: var(--text-dim); }
  .warn { color: var(--warn); }
  .err { color: var(--err); }
</style>
