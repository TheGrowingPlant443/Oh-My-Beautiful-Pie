<script lang="ts" module>
  // The read-only prerequisite probe runs once per app session; Recheck repeats it on demand.
  let probed = false;
</script>

<script lang="ts">
  import { untrack } from "svelte";
  import { api } from "../api";
  import { codeQualityClient } from "../code-quality-client";
  import { defaultQualityConfig, modeLabel, modeSelectors, qualityLocked, runtimeLabel, selectorMode, type QualityConfig, type QualityCredentialUpdate, type QualityRuntime } from "../code-quality";
  const service = codeQualityClient;
  const uid = $props.id();
  const quality = $derived($service.state);
  const locked = $derived(qualityLocked($service));
  let draft = $state<QualityConfig>(defaultQualityConfig());
  let analyzer = $state<"sonarqube" | "alternate">("sonarqube");
  let connection = $state<"community" | "enterprise">("community");
  let revision = $state(-1);
  let dirty = $state(false);
  let tokenAction = $state<"keep" | "replace" | "clear">("keep");
  let token = $state("");
  let installConsent = $state(false);
  let featuresConsent = $state(false);
  let rulesConsent = $state(false);
  let error = $state("");
  const mode = $derived(selectorMode(analyzer, connection));
  const credentialPresent = $derived(connection === "community" ? quality?.credentials.community : quality?.credentials.enterprise);
  const rulesIdentity = $derived(`${quality?.rules.manifestHash ?? ""}:${quality?.rules.licenseHash ?? ""}`);
  const runtimeIdentity = $derived(JSON.stringify(draft.runtime));
  $effect(() => { rulesIdentity; untrack(() => { rulesConsent = false; }); });
  $effect(() => { runtimeIdentity; untrack(() => { installConsent = false; }); });
  $effect(() => {
    if (!quality || quality.busy || probed || quality.prerequisites.length > 0) return;
    probed = true;
    untrack(() => { void service.mutate(() => api.checkCodeQualityPrerequisites()).catch(() => { probed = false; }); });
  });
  $effect(() => {
    const current = quality;
    if (!current) return;
    untrack(() => {
      if (revision < 0 || (!dirty && revision !== current.revision)) {
        draft = structuredClone(current.config);
        ({ analyzer, connection } = modeSelectors(current.config.mode));
        revision = current.revision;
      }
    });
  });
  function chooseRuntime(kind: QualityRuntime["kind"]): void {
    draft.runtime = kind === "podman" ? { kind, connection: "" } : kind === "dockerWsl" ? { kind, distro: "" } : { kind };
    dirty = true;
  }
  async function perform(action: () => Promise<unknown>): Promise<void> {
    error = "";
    try { await action(); } catch (failure) { error = String(failure); }
  }
  async function apply(): Promise<void> {
    const config: QualityConfig = { ...structuredClone($state.snapshot(draft)), mode };
    const credential: QualityCredentialUpdate | null = mode === "alternate" || tokenAction === "keep" ? null : { target: connection, action: tokenAction, ...(tokenAction === "replace" ? { token } : {}) };
    await perform(async () => {
      const current = await service.mutate(() => api.configureCodeQuality(config, revision, credential));
      token = ""; tokenAction = "keep"; dirty = false;
      draft = structuredClone(current.config); revision = current.revision; ({ analyzer, connection } = modeSelectors(current.config.mode));
    });
  }
</script>

<section class="quality-settings" aria-label="Code quality settings">
  <h3>Code quality</h3>
  {#if !quality}<p role="status">Reading code quality settings…</p>{/if}
  {#if error || $service.error}<p class="err" role="alert">{error || $service.error}</p>{/if}
  {#if quality}
    <p role="status"><strong>{quality.phase}</strong>{quality.busy ? " · operation in progress" : ""}{!quality.activated ? " · not yet activated" : ""}<br />Committed: {modeLabel(quality.config.mode)} · {runtimeLabel(quality.config.runtime)}<br />Effective: {modeLabel(quality.effectiveMode)}</p>
    {#if quality.requestedConfig}<p class="warn">Requested: {modeLabel(quality.requestedConfig.mode)} · {runtimeLabel(quality.requestedConfig.runtime)}</p>{/if}
    {#if quality.error}<p class="err" role="alert">{quality.error}</p>{/if}
    {#if quality.phase === "needsRestart"}<p class="warn">Restart Windows manually, then Recheck. OMP Manager never reboots Windows.</p>{/if}
    {#if quality.phase === "needsSetup"}<p class="warn">Setup is required. Review the prerequisite below; changing a selector does not install software.</p>{/if}
  {/if}
  <fieldset disabled={locked} oninput={() => dirty = true}>
    <legend>Analyzer and connection</legend>
    <label>Analyzer<select bind:value={analyzer}><option value="sonarqube">SonarQube</option><option value="alternate">MegaLinter + Opengrep + Semgrep CE</option></select></label>
    {#if analyzer === "sonarqube"}
      <label>Connection<select bind:value={connection}><option value="community">Community — managed locally</option><option value="enterprise">Enterprise — existing SSO server</option></select></label>
      {#if connection === "community"}
        <label>Community port<input type="number" min="1" max="65535" step="1" bind:value={draft.communityPort} /></label>
        <p>Server: {quality?.community.running === true ? "running" : quality?.community.running === false ? "stopped" : "unknown"} · {quality?.community.url ?? "not started"}</p>
        <button type="button" disabled={!quality?.community.url} onclick={() => perform(() => api.openCodeQualityDashboard())}>Open SonarQube setup</button>
        <small class="dim">On first login, complete SonarQube's password change and create a user token in My Account → Security. OMP Manager does not reset passwords.</small>
      {:else}
        <label>Enterprise URL<input type="url" placeholder="https://sonar.example.org/context" bind:value={draft.enterpriseUrl} /></label>
        <button type="button" disabled={!draft.enterpriseUrl.trim()} onclick={() => perform(() => api.openCodeQualitySsoLogin(draft.enterpriseUrl))}>Open SSO login</button>
        <small class="dim">Sign in using your external browser, then My Account → Security → create a user token. Browser cookies are not API credentials. OMP Manager never starts or stops Enterprise.</small>
      {/if}
      <p>Saved token for the committed connection: {credentialPresent ? "present" : "missing"}. Tokens need Browse + Execute Analysis; Community project creation also needs permission. A different server/context or local runtime has separate credentials.</p>
      <label>Token update<select bind:value={tokenAction}><option value="keep">Keep saved token</option><option value="replace">Replace token</option><option value="clear">Clear saved token</option></select></label>
      {#if tokenAction === "replace"}<label>User token<input type="password" autocomplete="new-password" spellcheck="false" bind:value={token} /></label>{/if}
      {#if tokenAction === "clear"}<small class="warn">Clear disables scans for this connection; it does not shut down unchanged Community.</small>{/if}
    {/if}
  </fieldset>
  <fieldset disabled={locked}>
    <legend>Container runtime</legend>
    <div class="quality-actions">
      {#each [{ kind: "podman", label: "Podman" }, { kind: "dockerDesktop", label: "Docker Desktop" }, { kind: "dockerWsl", label: "Docker in WSL" }] as option}
        <label class="quality-check"><input type="radio" name={`quality-runtime-${uid}`} checked={draft.runtime.kind === option.kind} onchange={() => chooseRuntime(option.kind as QualityRuntime["kind"])} />{option.label}</label>
      {/each}
    </div>
    {#if draft.runtime.kind === "dockerWsl"}
      <label>WSL distro<select value={draft.runtime.distro} onchange={e => { draft.runtime = { kind: "dockerWsl", distro: e.currentTarget.value }; dirty = true; }}><option value="">Select a distro</option>{#each quality?.options.wslDistros ?? [] as distro}<option value={distro}>{distro}</option>{/each}</select></label>
      <small class="dim">Docker runs in the exact selected distro as root using its own Linux socket, not a Desktop integration shim.</small>
    {:else if draft.runtime.kind === "podman"}
      <label>Podman connection<select value={draft.runtime.connection} onchange={e => { draft.runtime = { kind: "podman", connection: e.currentTarget.value }; dirty = true; }}><option value="">App machine · omp-manager-quality (WSL)</option>{#each quality?.options.podmanConnections ?? [] as item}<option value={item}>{item}</option>{/each}</select></label>
      <small class="dim">App connection: {quality?.options.podmanAppConnection ?? "not provisioned — use Install runtime"}. No ambient default connection is used.</small>
    {/if}
    <p class="warn">SonarQube history is separate per runtime. Switching retains existing named volumes and saved findings; it does not migrate or delete databases, stop shared engines or modify their global defaults.</p>
    {#each quality?.prerequisites ?? [] as prerequisite}
      <p><strong>{runtimeLabel(prerequisite.runtime)}</strong>: {prerequisite.ready ? "ready" : prerequisite.installed ? "installed, not ready" : "not installed"}{prerequisite.needsRestart ? " · restart required" : ""}<br />{prerequisite.message ?? ""}</p>
    {/each}
    <label class="quality-check"><input type="checkbox" bind:checked={installConsent} />I approve the selected runtime's interactive installer and license/elevation prompts. Docker-in-WSL installs Docker's signed repository/packages and starts its service as root. Podman creates/starts the app's WSL machine. No silent license acceptance.</label>
    {#if quality && quality.prerequisites.length > 0 && !quality.options.wingetAvailable && draft.runtime.kind !== "dockerWsl"}<p class="warn">winget is unavailable. Use the official <a href={draft.runtime.kind === "podman" ? "https://podman.io/docs/installation" : "https://docs.docker.com/desktop/setup/install/windows-install/"} target="_blank" rel="noopener noreferrer">runtime installer</a>, then Recheck.</p>{/if}
    <div class="quality-actions"><button type="button" disabled={!installConsent} onclick={() => perform(() => service.mutate(s => api.installCodeQualityRuntime(structuredClone($state.snapshot(draft.runtime)), s.revision, installConsent)))}>Install runtime</button></div>
    <label class="quality-check"><input type="checkbox" bind:checked={featuresConsent} />I approve the elevated fixed helper enabling Windows Subsystem for Linux and Virtual Machine Platform with no automatic restart. I will restart Windows myself if required.</label>
    <button type="button" disabled={!featuresConsent} onclick={() => perform(() => service.mutate(s => api.enableCodeQualityWindowsFeatures(s.revision, featuresConsent)))}>Enable required Windows features</button>
  </fieldset>
  {#if mode === "alternate" && quality}
    <fieldset disabled={locked}>
      <legend>Frozen rules license and provenance</legend>
      <p>Selected upstream rules: LGPL-2.1 + Commons Clause. Review these exact terms before any rule-pack download. This is separate from runtime and build approval; no commercial redistribution rights are asserted.</p>
      <small class="mono">Manifest: {quality.rules.manifestHash}<br />License: {quality.rules.licenseHash}</small>
      <pre class="quality-log">{quality.rules.licenseText}</pre>
      <label class="quality-check"><input type="checkbox" bind:checked={rulesConsent} />I acknowledge this pinned rules manifest and license.</label>
      <button type="button" disabled={!rulesConsent || quality.rules.acknowledged} onclick={() => perform(() => service.mutate(s => api.acknowledgeCodeQualityRules(s.rules.manifestHash, s.rules.licenseHash, s.revision, rulesConsent)))}>{quality.rules.acknowledged ? "Rules license acknowledged" : "Acknowledge rules license"}</button>
      <small class="dim">MegaLinter → Opengrep → Semgrep CE. Correctness, security, secrets and maintainability; no automatic fixes, cloud policies or formatting-only defaults. Actual versions, skipped language/rule coverage and individual linter outcomes appear with each run.</small>
    </fieldset>
  {/if}
  {#if dirty && quality && revision !== quality.revision}<p class="warn">Settings changed in another surface. Your draft is preserved; reload it before applying.</p><button disabled={locked} onclick={() => { if (quality) { draft = structuredClone(quality.config); revision = quality.revision; ({ analyzer, connection } = modeSelectors(quality.config.mode)); dirty = false; token = ""; tokenAction = "keep"; } }}>Reload committed settings</button>{/if}
  <div class="quality-actions">
    <button type="button" class="primary" disabled={locked || (mode === "alternate" && !quality?.rules.acknowledged) || (tokenAction === "replace" && !token)} onclick={apply}>{mode === "sonarqubeEnterprise" ? "Test / save connection" : "Apply changes"}</button>
    <button type="button" disabled={locked} onclick={() => perform(() => service.mutate(s => api.startCodeQualityService(s.revision)))}>Start / retry</button>
    <button type="button" disabled={locked} onclick={() => perform(() => service.mutate(() => api.checkCodeQualityPrerequisites()))}>Recheck</button>
  </div>
  {#if $service.uncertain}<p class="warn">Waiting for an authoritative service snapshot before unlocking controls.</p>{/if}
  {#if quality?.unresolved.length}<div class="warn"><strong>Unresolved ownership / server work</strong>{#each quality.unresolved as item}<p>{item}</p>{/each}</div>{/if}
  {#if quality?.diagnostics.length}<details><summary>Setup progress and diagnostics</summary><pre class="quality-log">{quality.diagnostics.join("\n")}</pre></details>{/if}
</section>
