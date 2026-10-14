<script lang="ts">
  import { tick } from "svelte";
  import type { Instance } from "../api";
  let { projects, label, value = null, disabled = false, onselect }: {
    projects: Instance[]; label: string; value?: string | null;
    disabled?: boolean; onselect: (id: string) => void;
  } = $props();
  const id = $props.id();
  let trigger = $state<HTMLButtonElement>();
  let popover = $state<HTMLElement>();
  let searchInput = $state<HTMLInputElement>();
  let search = $state("");
  let expanded = $state(false);
  let selectionMade = false;
  let left = $state(12);
  let top = $state(12);
  let height = $state(420);
  const saved = $derived(projects.filter(row => row.kind === "instance"));
  const selected = $derived(saved.find(row => row.id === value));
  const matches = $derived(saved.filter(row => `${row.name}\n${row.projectPath}\n${row.profile || "Default profile"}`.toLowerCase().includes(search.trim().toLowerCase())));
  function position(): void {
    if (!trigger) return;
    const rect = trigger.getBoundingClientRect();
    const width = Math.min(460, window.innerWidth - 24);
    left = Math.max(12, Math.min(rect.left, window.innerWidth - width - 12));
    top = Math.max(12, Math.min(rect.bottom + 6, window.innerHeight - 132));
    height = Math.min(420, window.innerHeight - top - 12);
  }
  async function beforeToggle(event: Event): Promise<void> {
    if ((event as ToggleEvent).newState !== "open") return;
    search = "";
    position();
    await tick();
    if (popover?.matches(":popover-open")) searchInput?.focus();
  }
  function toggled(event: Event): void {
    expanded = (event as ToggleEvent).newState === "open";
    if (!expanded) {
      const selected = selectionMade;
      selectionMade = false;
      if (!selected || trigger?.closest("dialog[open]")) trigger?.focus();
    } else searchInput?.focus();
  }
  function choose(id: string): void {
    selectionMade = true;
    popover?.hidePopover();
    onselect(id);
  }
  function navigate(event: KeyboardEvent): void {
    if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
    const rows = [...popover!.querySelectorAll<HTMLButtonElement>(".project-row")];
    const index = rows.indexOf(document.activeElement as HTMLButtonElement);
    if (!rows.length || (index < 0 && event.key === "ArrowUp")) return;
    event.preventDefault();
    const next = index < 0 ? 0 : Math.max(0, Math.min(rows.length - 1, index + (event.key === "ArrowDown" ? 1 : -1)));
    rows[next].focus();
  }
</script>

<svelte:window onresize={() => { if (expanded) position(); }} />
<button type="button" bind:this={trigger} popovertarget={id} {disabled} aria-expanded={expanded} aria-haspopup="dialog">{label}{selected ? ` · ${selected.name}` : ""}</button>
<div bind:this={popover} {id} popover="auto" class="project-picker panel" role="dialog" aria-label={label} tabindex="-1" style:left={`${left}px`} style:top={`${top}px`} style:max-height={`${height}px`} onbeforetoggle={beforeToggle} ontoggle={toggled} onkeydown={navigate}>
  <label>Search projects<input bind:this={searchInput} type="search" bind:value={search} aria-label="Search projects" placeholder="Name, folder, or profile" /></label>
  <div class="results">
    {#each matches as row (row.id)}
      <button type="button" class="project-row" class:active={value === row.id} onclick={() => choose(row.id)}>
        <span><strong>{row.name}</strong><span class="dim">{row.profile || "Default profile"} · {row.status} · {row.id.slice(0, 8)}</span></span>
        <small class="mono dim">{row.projectPath}</small>
      </button>
    {:else}<p class="dim">{saved.length ? "No matching projects." : "No saved projects yet."}</p>{/each}
  </div>
</div>

<style>
  .project-picker { position: fixed; inset: auto; margin: 0; width: min(460px, calc(100vw - 24px)); padding: 12px; border: 1px solid var(--line); background: var(--panel); color: var(--text); box-shadow: 0 8px 30px #0008; }
  .project-picker:not(:popover-open) { display: none; }
  .project-picker:popover-open { display: flex; flex-direction: column; gap: 10px; }
  label { display: flex; flex-direction: column; gap: 6px; flex-shrink: 0; }
  .results { overflow: auto; min-height: 0; display: flex; flex-direction: column; gap: 6px; }
  .project-row { text-align: left; display: flex; flex-direction: column; gap: 5px; flex-shrink: 0; }
  .project-row > span { display: flex; flex-wrap: wrap; justify-content: space-between; gap: 8px; }
  small { overflow-wrap: anywhere; }
  p { margin: 6px 0; }
</style>
