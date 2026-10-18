<script lang="ts">
  import { onMount } from "svelte";
  import { api } from "../api";
  import { codeQualityClient as service } from "../code-quality-client";
  import { qualityLocked, QualityRevealLifetime, type QualityLoginDetails } from "../code-quality";
  let details = $state.raw<QualityLoginDetails | null>(null);
  let passwordVisible = $state(false), tokenVisible = $state(false), fetching = $state(false), error = $state("");
  let dialog = $state<HTMLDialogElement>();
  let programmaticCloses = 0;
  const lifetime = new QualityRevealLifetime(() => {
    details = null; passwordVisible = false; tokenVisible = false; fetching = false; error = "";
    if (dialog?.open) { programmaticCloses++; dialog.close(); }
  }, (fn, ms) => window.setTimeout(fn, ms), id => window.clearTimeout(id as number));
  async function reveal(): Promise<void> {
    const generation = lifetime.begin(); fetching = true;
    try {
      const result = await service.mutate(() => api.revealCodeQualityCredentials());
      if (!lifetime.accepts(generation)) return;
      details = result; dialog?.showModal(); lifetime.opened(generation);
    } catch { if (lifetime.accepts(generation)) error = "Could not read stored credentials. Finish setup or retry when code quality is idle."; }
    finally { if (lifetime.accepts(generation)) fetching = false; }
  }
  $effect(() => { if ($service.state?.ownership.status !== "owned") lifetime.close(); });
  onMount(() => {
    return lifetime.attach(window);
  });
</script>

{#if $service.state?.config.mode !== "alternate"}
  <button disabled={qualityLocked($service) || fetching} onclick={reveal}>Show SonarQube login and scan token</button>
  {#if error}<p class="err" role="alert">{error}</p>{/if}
{/if}
<dialog bind:this={dialog} aria-label="SonarQube login and scan token" oncancel={event => { event.preventDefault(); lifetime.close(); }} onclose={() => { if (programmaticCloses) programmaticCloses--; else lifetime.close(); }} onclick={event => { if (event.target === dialog) lifetime.close(); }}>
  {#if details}
    <section class="quality-settings">
      <h3>SonarQube login and scan token</h3>
      <p>Cleared when closed, when this window loses focus, or after 60 seconds.</p>
      <label>Server URL<input readonly value={details.serverUrl} /></label>
      <label>Username<input readonly value={details.login ?? "Not stored"} autocomplete="off" /></label>
      <label>Password<input readonly type={passwordVisible ? "text" : "password"} value={details.password ?? ""} placeholder="Not stored" autocomplete="off" /></label>
      <button disabled={!details.password} onclick={() => passwordVisible = !passwordVisible}>{passwordVisible ? "Hide" : "Show"} password</button>
      <label>Scan token<input readonly type={tokenVisible ? "text" : "password"} value={details.token ?? ""} placeholder="Not stored" autocomplete="off" /></label>
      <button disabled={!details.token} onclick={() => tokenVisible = !tokenVisible}>{tokenVisible ? "Hide" : "Show"} token</button>
      {#if details.notice}<p>{details.notice}</p>{/if}
      <div class="quality-actions"><button onclick={async () => { try { await api.openCodeQualityDashboard(); } catch { error = "Could not open SonarQube."; } }}>Open SonarQube</button><button onclick={() => lifetime.close()}>Close</button></div>
    </section>
  {/if}
</dialog>

<style>
  dialog { max-width: min(44rem, 90vw); max-height: 85vh; overflow: auto; color: inherit; background: var(--bg, #181a20); border: 1px solid #666; border-radius: 8px; }
  dialog::backdrop { background: #0009; }
</style>
