<script lang="ts">
  import { api } from "../api";
  import { codeQualityClient as service } from "../code-quality-client";
  let { enterOnAcquire = true }: { enterOnAcquire?: boolean } = $props();
  let dialog = $state<HTMLDialogElement>();
  let error = $state("");
  const quality = $derived($service.state);
  const transferring = $derived($service.pending || $service.uncertain || quality?.ownership.status === "transferring");
  async function takeControl(): Promise<void> {
    dialog?.close(); error = "";
    try {
      await service.mutate(() => api.takeOverCodeQuality(true), false, true);
      if (enterOnAcquire && service.snapshot.state?.ownership.status === "owned") await service.mutate(() => api.enterCodeQuality());
    } catch (failure) { error = String(failure); }
  }
</script>

{#if quality && quality.ownership.status !== "owned"}
  <section class="quality-section" aria-label="Code quality ownership">
    <h3>{quality.ownership.status === "handedOff" ? "Control transferred to another window" : transferring ? "Transferring code quality control…" : "Another OMP Manager controls code quality"}</h3>
    <p class="warn">This window cannot change code quality while another process controls it. Saved reports remain available. Closing the owner lets this window recover control automatically.</p>
    <button disabled={transferring} onclick={() => dialog?.showModal()}>Take control here</button>
    <dialog bind:this={dialog} aria-label="Confirm code quality handover" class="panel quality-section" onclick={event => { if (event.target === dialog) dialog?.close(); }}>
      <p>Another OMP Manager controls code quality. Taking control will cancel its active scan, stop its managed SonarQube containers and transfer control to this window. The Podman machine, stored data and other Architect sessions will not be deleted. Continue?</p>
      <button onclick={() => dialog?.close()}>Cancel</button><button class="primary" onclick={takeControl}>Take control here</button>
    </dialog>
    {#if error}<p class="err" role="alert">{error}</p>{/if}
  </section>
{/if}

<style>
  dialog:not([open]) { display: none; }
  dialog { max-width: min(42rem, 90vw); color: inherit; }
  dialog::backdrop { background: #0009; }
</style>
