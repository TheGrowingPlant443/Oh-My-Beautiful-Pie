<script lang="ts">
  import type { ProjectSourceEvidence } from "../package-insights";
  let { evidence, label = "Local source evidence" }: { evidence: readonly ProjectSourceEvidence[]; label?: string } = $props();
  const range = (item: ProjectSourceEvidence): string => item.lineStart === item.lineEnd ? `${item.path}:${item.lineStart}` : `${item.path}:${item.lineStart}-${item.lineEnd}`;
</script>
{#if evidence.length}
  <div class="evidence">
    <strong>{label}</strong>
    <ul>{#each evidence as item}<li><code>{range(item)}</code><br />{item.explanation}</li>{/each}</ul>
  </div>
{/if}
<style>
  .evidence { display: flex; flex-direction: column; gap: 4px; }
  ul { margin: 0; padding-left: 20px; }
  code { font-family: var(--mono, ui-monospace, monospace); }
</style>
