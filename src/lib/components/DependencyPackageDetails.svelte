<script lang="ts">
  import { ROOT, type DepNode } from "../dependency-graph";
  import { newestText, ownerText, packageMajorLag, projectVersionText, safePublicUrl, type PackageInsights } from "../package-insights";
  import { isCommonlyUsed, knownOwnerOf } from "../known-packages";
  import DependencyMetric from "./DependencyMetric.svelte";
  let { node, insights, error, onreview, onrefresh, onexplain, refreshing = false, inhouseReason, derivedNote, savedReview = false, savedUsage = false, explainDisabled = false }: {
    node: DepNode; insights?: PackageInsights; error?: string; onreview: () => void; onrefresh: () => void; onexplain: () => void;
    refreshing?: boolean; inhouseReason?: string; derivedNote?: string; savedReview?: boolean; savedUsage?: boolean; explainDisabled?: boolean;
  } = $props();
  const lag = $derived(packageMajorLag(node, insights));
  const knownOwner = $derived(knownOwnerOf(node, insights?.repositoryUrl, insights?.repository?.ownerLogin));
  const commonlyUsed = $derived(isCommonlyUsed(node));
</script>
{#if node.id !== ROOT}
  {#if inhouseReason !== undefined}
    <span>In-house — marked by The Architect: {inhouseReason}. Public lookups are disabled for this package.</span>
    <button disabled>Refresh</button><button disabled>Security review</button><button disabled>Explain how this package is used</button>
  {:else}
  <span class="description dim">{insights ? insights.description ?? "No description published by the registry." : "Loading…"}</span>
  <span>{projectVersionText(node)}</span>
  {#if node.resolvedVersions.length && node.declaredVersions.length}<span class="dim">Declared: {node.declaredVersions.join(", ")}</span>{/if}
  {#if derivedNote}<span class="derived">{derivedNote}</span>{/if}
  <span>Newest (registry): <strong>{newestText(insights, error)}</strong></span>
  {#if lag !== null && lag >= 2}<span class="lag">At least one resolved version is {lag} numeric major versions behind the registry release. This is version lag, not a security verdict.</span>{/if}
  {#if error}<span role="status">Metadata lookup failed: {error}</span>{/if}
  {#if insights}
    {#if safePublicUrl(insights.registryUrl)}<a href={safePublicUrl(insights.registryUrl)!} target="_blank" rel="noopener noreferrer">Official registry: {insights.registryUrl}</a>{/if}
    {#if insights.newestStatus !== "available"}<span class="dim">Newest release {newestText(insights).toLowerCase()}; see registry lookup notes below.</span>{/if}
    {#if insights.repository && safePublicUrl(insights.repository.url)}<a href={safePublicUrl(insights.repository.url)!} target="_blank" rel="noopener noreferrer">Repository: {insights.repository.url}</a>{/if}
  {/if}
  <DependencyMetric label="GitHub stars" metric={insights?.repository?.stars} pending={insights?.githubPending} loaded={!!insights} reason={insights?.repositoryReason} {error} />
  <DependencyMetric label="GitHub contributors (linked accounts)" metric={insights?.repository?.contributors} pending={insights?.githubPending} loaded={!!insights} reason={insights?.repositoryReason} {error} />
  <span>Owner: {insights ? ownerText(insights.repository) : error ? "unknown" : "Loading…"}{insights?.repository?.ownerLogin ? ` (${insights.repository.ownerLogin})` : ""}</span>
  {#if knownOwner}<span>Known owner: {knownOwner.line}</span>{/if}
  {#if commonlyUsed}<span class="dim">Commonly used: the name says what it does.</span>{/if}
  <small class="dim">All-time GitHub contributor accounts; may include bots and exclude unlinked contributors. Contributors are not confirmed maintainers. Organization ownership does not establish professional staffing.</small>
  <small class="dim">Actual maintainer count / professional status: not publicly verified</small>
  <DependencyMetric label="Official registry downloads" metric={insights?.downloads} {error} />
  {#if insights}
    <small class="dim">Last updated: registry {new Date(insights.checkedAt).toLocaleString()} · GitHub {insights.repository ? new Date(insights.repository.checkedAt).toLocaleString() : "not loaded"}</small>
    {#each insights.notes as note}<small class="dim">{note}</small>{/each}
  {/if}
  <button onclick={onreview}>{savedReview ? "Open saved review" : "Security review"}</button>
  <button onclick={onexplain} disabled={explainDisabled} title="Explains what this package does for this project. Reuses a saved explanation if one exists.">{savedUsage ? "Open usage explanation" : "Explain how this package is used"}</button>
  <button onclick={onrefresh} disabled={refreshing}>{refreshing ? "Refreshing…" : "Refresh"}</button>
  {/if}
{/if}
<style>
  .lag { color: #f85149; }
  .derived { color: #ff69b4; }
  .description { font-style: italic; }
  small { line-height: 1.4; }
</style>
