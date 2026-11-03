"use strict";
const byId = id => document.getElementById(id);
const token = location.hash.slice(1) || sessionStorage.getItem("control-token") || "";
sessionStorage.setItem("control-token", token);
history.replaceState(null, "", "/");
let state;
let busy = false;
let renderedPolicy = "";
let renderedHistory = "";
let catalog = {folders: [], programs: [], packages: [], language_servers: []};
let gpuChosen = false;

async function api(path, value) {
  const response = await fetch(`/api/${path}`, {
    method: value === undefined ? "GET" : "POST",
    headers: {Authorization: `Bearer ${token}`, "Content-Type": "application/json"},
    body: value === undefined ? undefined : JSON.stringify(value)
  });
  const data = await response.json();
  if (!response.ok) throw new Error(data.error);
  return data;
}

function message(error = "") { byId("message").textContent = error.message || error; }

async function action(run) {
  if (busy) return;
  busy = true;
  message();
  try { await run(); await refresh(); }
  catch (error) { message(error); }
  finally { busy = false; }
}

async function change(update) {
  const policy = structuredClone(state.policy);
  update(policy);
  await api("policy", policy);
}

function entry(list, label, detail, remove, enabled = true) {
  const row = document.createElement("li");
  const text = document.createElement("span");
  text.textContent = label;
  const small = document.createElement("small");
  small.textContent = detail;
  text.append(small);
  row.append(text);
  if (remove) {
    const button = document.createElement("button");
    button.className = "remove";
    button.textContent = enabled ? "−" : "Restore";
    button.setAttribute("aria-label", `${enabled ? "Revoke" : "Restore"} ${label}`);
    button.onclick = () => action(() => change(remove));
    row.append(button);
  }
  list.append(row);
  return row;
}

const REQUIREMENT_NOTES = {
  program: "Read-only Nix bundle; removal may break OMBP",
  "sandbox-browser": "Opens browsers in their own nested window; closes with the last browser",
  "host-omp": "Host files; container can change them; restart to revoke",
  "sonar-engine": "Only OMBP's SonarQube containers on host Podman; restart to revoke",
  folder: "Private sandbox files; restart to revoke",
};

function activeLaunch() {
  return state.runtime.running || state.runtime.starting;
}

function installedMode() {
  return activeLaunch() && state.runtime.launch
    ? state.runtime.mode === "container" && !!state.runtime.launch.installed_mode
    : byId("mode").value === "container" && state.policy.installed_mode;
}

function selectedPackages() {
  return activeLaunch() && state.runtime.launch ? state.runtime.launch.selected_packages : state.policy.selected_packages;
}

function openPackageDialog() {
  const dialog = byId("package-dialog");
  if (dialog.open) return;
  const groups = byId("package-groups");
  groups.replaceChildren();
  const selected = new Set(state.policy.selected_packages);
  const fieldsets = new Map();
  for (const choice of catalog.packages) {
    if (!fieldsets.has(choice.group)) {
      const fieldset = document.createElement("fieldset");
      const legend = document.createElement("legend");
      legend.textContent = choice.group;
      fieldset.append(legend);
      fieldsets.set(choice.group, fieldset);
      groups.append(fieldset);
    }
    const label = document.createElement("label");
    label.className = "check";
    label.title = (choice.executables || []).join(", ");
    const input = document.createElement("input");
    input.type = "checkbox";
    input.name = "package";
    input.value = choice.id;
    input.checked = selected.has(choice.id);
    input.disabled = choice.systems && !choice.systems.includes(catalog.system);
    const text = document.createElement("span");
    text.textContent = choice.label;
    const description = document.createElement("small");
    description.textContent = input.disabled
      ? `${choice.description} Available on ${choice.systems.join(", ")}.`
      : choice.description;
    text.append(description);
    label.append(input, text);
    fieldsets.get(choice.group).append(label);
  }
  byId("package-error").textContent = catalog.packages.length ? "" : "Package catalog unavailable. Reload the launcher to try again.";
  byId("package-save").disabled = !catalog.packages.length;
  dialog.showModal();
}

function showContents(title, note, groups) {
  byId("contents-title").textContent = title;
  byId("contents-note").textContent = note;
  byId("contents-list").replaceChildren(...groups.flatMap(({label, items}) => {
    const list = document.createElement("ul");
    list.append(...items.map(item => Object.assign(document.createElement("li"), {textContent: item})));
    if (!label) return [list];
    const heading = Object.assign(document.createElement("h3"), {textContent: `${label} (${items.length})`});
    return [heading, list];
  }));
  byId("contents-dialog").dataset.opener = title;
  byId("contents-dialog").showModal();
}

// Turns a list row's title into a button that lists what the bundle contains.
function contentsButton(row, title, note, groups) {
  const text = row.firstElementChild.firstChild;
  const open = document.createElement("button");
  open.type = "button";
  open.className = "contents-link";
  open.textContent = text.textContent;
  open.setAttribute("aria-haspopup", "dialog");
  open.title = "Show what this contains";
  open.dataset.contents = title;
  open.onclick = () => showContents(title, note, groups);
  text.replaceWith(open);
}

const CORE_IDS = new Set(["ombp", "sandbox-python", "sandbox-dbus", "sandbox-keyring", "sandbox-shell",
  "sandbox-coreutils", "sandbox-fonts", "tool-podman", "sandbox-dialog", "agent-cli", "sandbox-mesa"]);
const CORE_PARTS = ["Selected OMP/OMSP CLI", "OMBP with Bun, Node, Git and source-index Python", "Sandbox Python",
  "Bash/sh", "coreutils", "Private D-Bus", "Private keyring", "Fonts", "Podman client", "Zenity file picker",
  "Mesa (AMD launches)"];

function recordedPrograms(id) {
  return (state.requirements || []).find(item => item.kind === "program" && item.id === id && item.programs);
}

function renderRequirementList(kind, installed) {
  const list = byId(`${kind}-requirements`);
  list.replaceChildren();
  for (const requirement of state.requirements || []) {
    if (requirement.kind !== kind || installed && kind === "program") continue;
    if (installed && requirement.id === "nix-store") {
      entry(list, requirement.label, `${requirement.path} · Required private installed store; read-only`);
      continue;
    }
    const row = entry(list, requirement.label,
      `${requirement.enabled ? "Enabled" : "Disabled"} · ${requirement.path} · ${
        REQUIREMENT_NOTES[requirement.id] || REQUIREMENT_NOTES[kind]}`,
      policy => {
        const disabled = policy.disabled_requirements || [];
        policy.disabled_requirements = requirement.enabled
          ? [...new Set([...disabled, requirement.id])]
          : disabled.filter(id => id !== requirement.id);
      }, requirement.enabled);
    if (requirement.programs?.length > 1) contentsButton(row, requirement.label,
      `${requirement.programs.length} commands at the last Start`, [{items: requirement.programs}]);
  }
}

function renderInstalledPrograms() {
  const list = byId("program-requirements");
  const core = entry(list, "Required installed core", "OMP/OMSP, OMBP and sandbox runtime · always included");
  const recorded = (state.requirements || []).filter(item => item.kind === "program" && item.programs?.length
    && (CORE_IDS.has(item.id) || item.id.startsWith("bundled-")));
  contentsButton(core, "Required installed core", recorded.length
    ? "Commands installed at the last Start, by component"
    : "Exact commands are listed after the first Start",
    recorded.length ? recorded.map(item => ({label: item.label, items: item.programs})) : [{items: CORE_PARTS}]);
  const selected = new Set(selectedPackages());
  for (const choice of catalog.packages) {
    if (!selected.has(choice.id)) continue;
    const row = entry(list, choice.label, `${choice.group} · ${choice.description}`);
    const actual = recordedPrograms(`package-${choice.id}`);
    const items = actual ? actual.programs : choice.executables || [];
    if (items.length > 1) contentsButton(row, choice.label, actual
      ? `${items.length} commands installed at the last Start`
      : "Main commands; the full list appears after the next Start", [{items}]);
  }
}

function renderPermissions() {
  const installed = installedMode();
  byId("program-form").hidden = installed;
  byId("inspection-programs").hidden = installed;
  byId("installed-program-note").hidden = !installed;
  byId("installed-program-note").textContent = state.runtime.running
    ? "Current installed packages are read-only. Choose tools and restart to change them; saved selections may differ."
    : "Required core and selected tools will be installed privately on Start. Choose tools to change optional packages.";
  let programsHeading = "Required sandbox programs";
  if (installed) programsHeading = state.runtime.running ? "Current installed programs" : "Programs for next Start";
  byId("program-requirements-heading").textContent = programsHeading;
  for (const name of ["folders", "programs", "websites"]) byId(name).replaceChildren();
  for (const kind of ["folder", "program"]) renderRequirementList(kind, installed);
  if (installed) renderInstalledPrograms();
  for (const folder of state.policy.folders) {
    const row = entry(byId("folders"), folder.path, folder.write ? "Read + write + delete" : "Read only",
      policy => { policy.folders = policy.folders.filter(item => item.path !== folder.path); });
    const toggles = document.createElement("div");
    toggles.className = "dependency-modes";
    toggles.append(folderWriteToggle(folder));
    row.firstElementChild.append(toggles);
  }
  byId("write").checked = state.policy.folder_write_default;
  byId("auto-start").checked = !!state.policy.auto_start;
  state.policy.programs.forEach((program, index) => entry(byId("programs"), program,
    "Available in /allowed-programs", policy => policy.programs.splice(index, 1)));
  for (const website of state.policy.websites) renderWebsite(website);
  renderAutoApprove();
}

let autoSettingsOpen = false;

function renderAutoApprove() {
  const root = byId("auto-approve");
  const auto = state.policy.auto_approve;
  const toggle = document.createElement("label");
  toggle.className = "check";
  const input = document.createElement("input");
  input.type = "checkbox";
  input.checked = auto.enabled;
  input.onchange = () => action(() => change(policy => { policy.auto_approve.enabled = input.checked; }));
  const settings = document.createElement("button");
  settings.type = "button";
  settings.className = "secondary";
  settings.textContent = autoSettingsOpen ? "Close settings" : "Settings";
  settings.onclick = () => { autoSettingsOpen = !autoSettingsOpen; renderAutoApprove(); };
  toggle.append(input, `Auto-approve hosts containing: ${auto.sites.join(", ") || "(none)"}`);
  const panel = document.createElement("ul");
  panel.hidden = !autoSettingsOpen;
  for (const site of auto.sites) entry(panel, site, site.lastIndexOf(".") > 0
    ? `Hosts containing “${site.slice(0, site.lastIndexOf("."))}” and ending in “${site.slice(site.lastIndexOf("."))}”, look-alikes included`
    : `Any host containing “${site}”, look-alikes included`,
    policy => { policy.auto_approve.sites = policy.auto_approve.sites.filter(text => text !== site); });
  const form = document.createElement("form");
  form.className = "auto-add";
  const field = document.createElement("input");
  field.placeholder = "chatgpt or openai.com";
  field.setAttribute("aria-label", "Host text to auto-approve");
  field.required = true;
  const add = document.createElement("button");
  add.textContent = "Add";
  form.append(field, add);
  form.onsubmit = submit => {
    submit.preventDefault();
    const value = field.value.trim();
    action(() => change(policy => {
      policy.auto_approve.sites.push(value);
    }));
  };
  if (autoSettingsOpen) panel.append(form);
  root.replaceChildren(toggle, settings, panel);
}

function renderWebsite(website) {
  const row = entry(byId("websites"), website,
    state.policy.credential_origins.includes(website) ? "All subpaths · host keyring Authorization" : "All subpaths",
    policy => {
      policy.websites = policy.websites.filter(origin => origin !== website);
      policy.credential_origins = policy.credential_origins.filter(origin => origin !== website);
      for (const mode of [...Object.keys(DEPENDENCY_MODES), "exact_hosts"]) policy[mode] = policy[mode].filter(origin => origin !== website);
      delete policy.dependencies[website];
    });
  const toggles = document.createElement("div");
  toggles.className = "dependency-modes";
  toggles.append(subdomainToggle(website));
  for (const mode of Object.keys(DEPENDENCY_MODES)) toggles.append(dependencyToggle(website, mode));
  row.firstElementChild.append(toggles);
  const dependencies = state.policy.dependencies[website] || [];
  if (dependencies.length) entry(byId("websites"), `↳ ${dependencies.length} dependencies of ${website}`,
    dependencies.join(", "), policy => { delete policy.dependencies[website]; });
}

const DEPENDENCY_MODES = {
  all_dependencies: {
    label: "HTML/CSS",
    title: "Allow websites this site's own served HTML/CSS loads (scripts, styles, images, fonts, frames)",
    warning: "Pages that embed user-chosen resources (for example images from any host) widen this list."},
  header_dependencies: {
    label: "Header",
    title: "Allow any website requested with this site in its Referer/Origin header; catches script-loaded dependencies",
    warning: "Any program in the sandbox can fake that header, so while this is on it can reach ANY website."},
};

function dependencyToggle(website, mode) {
  const {label: name, title, warning} = DEPENDENCY_MODES[mode];
  const label = document.createElement("label");
  label.className = "check dependency";
  label.title = title;
  const input = document.createElement("input");
  input.type = "checkbox";
  input.checked = state.policy[mode].includes(website);
  input.setAttribute("aria-label", `Allow ${website} dependencies found by ${name}`);
  input.onchange = () => action(async () => {
    if (input.checked && !confirm(`${title}: ${website}?\n${warning}`)) {
      input.checked = false;
      return;
    }
    await change(policy => {
      policy[mode] = input.checked
        ? [...new Set([...policy[mode], website])]
        : policy[mode].filter(origin => origin !== website);
    });
  });
  label.append(input, name);
  return label;
}

function folderWriteToggle(folder) {
  const label = document.createElement("label");
  label.className = "check dependency";
  const input = document.createElement("input");
  input.type = "checkbox";
  input.checked = folder.write;
  input.setAttribute("aria-label", `Allow write and deletion in ${folder.path}`);
  input.onchange = () => action(() => change(policy => {
    for (const item of policy.folders) if (item.path === folder.path) item.write = input.checked;
  }));
  label.append(input, "Write");
  return label;
}

function subdomainToggle(website) {
  const label = document.createElement("label");
  label.className = "check dependency";
  label.title = "Also allow subdomains (ignoring a leading www.), e.g. api.github.com for github.com";
  const input = document.createElement("input");
  input.type = "checkbox";
  input.checked = !state.policy.exact_hosts.includes(website);
  input.setAttribute("aria-label", `Allow subdomains of ${website}`);
  input.onchange = () => action(() => change(policy => {
    policy.exact_hosts = input.checked
      ? policy.exact_hosts.filter(origin => origin !== website)
      : [...new Set([...policy.exact_hosts, website])];
  }));
  label.append(input, "Subdomains");
  return label;
}

async function allowEvent(event) {
  if (event.kind === "program" && installedMode()) {
    openPackageDialog();
    return;
  }
  const folderAccess = state.policy.folder_write_default ? "with write AND deletion" : "read-only";
  if (!confirm(`Allow ${event.kind}: ${event.target}?\nFolders are granted ${folderAccess}. Website subpaths are all allowed. Internet must also be enabled.`)) return;
  await change(policy => {
    if (event.kind === "website") policy.websites.push(event.target);
    else if (event.kind === "program") policy.programs.push(event.target);
    else policy.folders.push({path: event.target, write: policy.folder_write_default});
  });
  await api("history/forget", [event]);
}

const uncheckedEndpoints = new Set();
const openGroups = new Set();

function eventRow(event) {
  const row = document.createElement("div");
  row.className = "event";
  const detail = document.createElement("div");
  detail.textContent = event.target;
  const small = document.createElement("small");
  small.textContent = `${event.kind} · ${event.count} attempts · ${new Date(event.time * 1000).toLocaleString()} · ${event.reason}`;
  detail.append(small);
  const allow = document.createElement("button");
  allow.className = "allow";
  allow.textContent = "+";
  allow.setAttribute("aria-label", event.kind === "program" && installedMode()
    ? `Choose tools for the next Start; ${event.target} remains denied` : `Allow ${event.target}`);
  allow.onclick = () => action(() => allowEvent(event));
  row.append(detail, allow);
  return row;
}

async function allowGroup(site, events) {
  const targets = events.map(event => event.target);
  if (!confirm(`Allow ${targets.length} website(s) requested by ${site}?\n${targets.join("\n")}`)) return;
  await change(policy => {
    if (!policy.websites.includes(site)) policy.websites.push(site);
    policy.dependencies[site] = [...new Set([...(policy.dependencies[site] || []), ...targets])];
  });
  await api("history/forget", events);
}

function eventGroup(site, events) {
  const group = document.createElement("details");
  group.className = "event-group";
  group.open = openGroups.has(site);
  group.ontoggle = () => group.open ? openGroups.add(site) : openGroups.delete(site);
  const selected = events.filter(event => /^https?:\/\//.test(event.target)
    && !uncheckedEndpoints.has(`${site} ${event.target}`));
  const summary = document.createElement("summary");
  const title = document.createElement("span");
  title.textContent = site;
  const small = document.createElement("small");
  small.textContent = `${events.length} blocked website(s) · ${selected.length} selected`;
  title.append(small);
  const button = document.createElement("button");
  button.className = "allow";
  button.textContent = "+";
  button.disabled = selected.length === 0;
  button.setAttribute("aria-label", `Allow selected dependencies of ${site}`);
  button.onclick = click => { click.preventDefault(); action(() => allowGroup(site, selected)); };
  summary.append(title, button);
  group.append(summary);
  for (const event of events) {
    const label = document.createElement("label");
    label.className = "check";
    const input = document.createElement("input");
    input.type = "checkbox";
    input.checked = !uncheckedEndpoints.has(`${site} ${event.target}`);
    input.onchange = () => {
      if (input.checked) uncheckedEndpoints.delete(`${site} ${event.target}`);
      else uncheckedEndpoints.add(`${site} ${event.target}`);
      renderHistory();
    };
    const text = document.createElement("span");
    text.textContent = event.target;
    const detail = document.createElement("small");
    detail.textContent = `${event.count} attempts · ${event.reason}`;
    text.append(detail);
    label.append(input, text);
    group.append(label);
  }
  return group;
}

function renderHistory() {
  const root = byId("history");
  const servers = new Set(catalog.language_servers);
  const hide = state.policy.hide_language_server_denials;
  // Probes arrive as bare names, or store paths when the binary exists but is unapproved.
  const history = state.history.filter(event => !(hide && event.kind === "program"
    && servers.has(event.target.split("/").pop())));
  const hidden = state.history.length - history.length;
  byId("history-hidden").textContent = hidden ? `${hidden} language-server denial(s) hidden` : "";
  const groups = new Map();
  for (const event of history) {
    if (event.kind === "website" && event.site) {
      if (!groups.has(event.site)) groups.set(event.site, []);
      groups.get(event.site).push(event);
    }
  }
  const rows = [], shown = new Set();
  for (const event of history) {
    if (!(event.kind === "website" && event.site)) rows.push(eventRow(event));
    else if (!shown.has(event.site)) {
      shown.add(event.site);
      rows.push(eventGroup(event.site, groups.get(event.site)));
    }
  }
  root.replaceChildren(...rows);
}

function autocomplete(id, choices, delay = 0) {
  const input = byId(id), list = byId(`${id}-suggestions`);
  let active = -1, matches = [];
  function close() {
    list.hidden = true;
    input.setAttribute("aria-expanded", "false");
    input.removeAttribute("aria-activedescendant");
    active = -1;
  }
  function choose(index) {
    input.value = matches[index].value;
    close();
    input.focus();
  }
  async function render() {
    const query = input.value.trim().toLowerCase();
    const found = query ? await choices(query) : [];
    // A later keystroke or blur owns the list; drop this stale result.
    if (input.value.trim().toLowerCase() !== query || document.activeElement !== input) return;
    matches = found.filter(item => `${item.name} ${item.value}`.toLowerCase().includes(query));
    list.replaceChildren();
    active = -1;
    input.removeAttribute("aria-activedescendant");
    for (const [index, item] of matches.entries()) {
      const option = document.createElement("option");
      option.id = `${id}-suggestion-${index}`;
      option.value = item.value;
      option.textContent = `${item.name} — ${item.value}`;
      list.append(option);
    }
    list.selectedIndex = -1;
    list.size = Math.max(2, Math.min(matches.length, 5));
    list.hidden = !matches.length;
    input.setAttribute("aria-expanded", String(!!matches.length));
  }
  let timer;
  input.oninput = () => {
    // Old choices belong to the previous text; never let Enter pick them while the new search runs.
    matches = [];
    close();
    clearTimeout(timer);
    timer = setTimeout(() => render().catch(message), delay);
  };
  input.onblur = event => { if (event.relatedTarget !== list) close(); };
  list.onblur = close;
  list.onchange = () => { if (list.selectedIndex >= 0) choose(list.selectedIndex); };
  input.onkeydown = event => {
    if (event.key === "Escape") close();
    else if (!list.hidden && ["ArrowDown", "ArrowUp"].includes(event.key)) {
      event.preventDefault();
      const direction = event.key === "ArrowDown" ? 1 : -1;
      if (active < 0) active = direction === 1 ? 0 : matches.length - 1;
      else active = (active + direction + matches.length) % matches.length;
      list.selectedIndex = active;
      input.setAttribute("aria-activedescendant", list.children[active].id);
      list.children[active].scrollIntoView({block: "nearest"});
    } else if (!list.hidden && event.key === "Enter" && (active >= 0 || matches.length === 1)) {
      event.preventDefault();
      choose(Math.max(active, 0));
    }
  };
}

async function loadCatalog() {
  catalog = await api("catalog");
  if (!gpuChosen) byId("gpu").value = catalog.gpu;
  for (const folder of catalog.folders) {
    const option = document.createElement("option");
    option.value = folder.path;
    option.textContent = `${folder.name} · ${folder.path}`;
    byId("common-folder").append(option);
  }
  autocomplete("program", () => catalog.programs.map(item => ({name: item.name, value: item.path})));
  autocomplete("folder", async query => (await api(`folders?q=${encodeURIComponent(query)}`))
    .map(item => ({name: item.name, value: item.path})), 200);
}

function renderLists() {
  const effective = [installedMode(), selectedPackages(), !!state.runtime.running];
  const policyKey = JSON.stringify([state.policy, state.requirements, effective]);
  const historyKey = JSON.stringify([state.history, state.policy.dependencies, effective[0],
    state.policy.hide_language_server_denials, catalog.language_servers]);
  if (renderedPolicy !== policyKey) { renderPermissions(); renderedPolicy = policyKey; }
  if (renderedHistory !== historyKey) { renderHistory(); renderedHistory = historyKey; }
}

async function refresh() {
  state = await api("status");
  const running = state.runtime.running;
  byId("internet").checked = state.policy.internet;
  byId("allow-all-network").textContent = state.policy.allow_all_network
    ? "Stop allowing all network traffic" : "Allow all network traffic";
  byId("allow-all-network").classList.toggle("danger", state.policy.allow_all_network);
  byId("history-error").textContent = state.history_error ? `Denied history is only in memory: ${state.history_error}` : "";
  const mode = state.runtime.mode === "container" ? "Podman" : "Unrestricted";
  const busy = running || state.runtime.starting;
  byId("status").textContent = running ? `${mode} · ${state.runtime.launch.agent} · running` : "Stopped";
  if (state.runtime.starting) byId("status").textContent = "Building current OMBP…";
  byId("start").disabled = busy;
  byId("stop").disabled = !running;
  for (const id of ["mode", "gpu", "desktop", "source", "agent"]) byId(id).disabled = busy;
  if (running) {
    byId("mode").value = state.runtime.mode;
    byId("agent").value = state.runtime.launch.agent;
    byId("gpu").value = state.runtime.launch.gpu;
    byId("desktop").checked = state.runtime.launch.desktop;
    byId("source").value = state.runtime.launch.source;
  }
  byId("installed-mode").checked = !!state.policy.installed_mode;
  byId("hide-language-servers").checked = state.policy.hide_language_server_denials;
  renderMode();
  if (!byId("source").value) byId("source").value = state.source;
  renderLists();
  if (state.runtime.error) message(state.runtime.error);
}

byId("folder-form").onsubmit = event => {
  event.preventDefault();
  action(async () => {
    await change(policy => policy.folders.push({path: byId("folder").value.trim(), write: byId("write").checked}));
    byId("folder").value = "";
  });
};
byId("program-form").onsubmit = event => {
  event.preventDefault();
  action(async () => {
    const value = byId("program").value.trim();
    const match = catalog.programs.find(program =>
      value === `${program.name} · ${program.path}` || value.toLowerCase() === program.name.toLowerCase());
    await change(policy => policy.programs.push(match ? match.path : value));
    byId("program").value = "";
  });
};
byId("website-form").onsubmit = event => {
  event.preventDefault();
  action(async () => {
    await change(policy => {
      const value = byId("website").value.trim();
      const website = /^[a-z][a-z0-9+.-]*:\/\//i.test(value) ? value : `https://${value}`;
      policy.websites.push(website);
      if (byId("host-credential").checked) policy.credential_origins.push(website);
    });
    byId("website").value = "";
    byId("host-credential").checked = false;
  });
};
byId("internet").onchange = () => action(() => change(policy => { policy.internet = byId("internet").checked; }));
byId("write").onchange = () => action(() => change(policy => { policy.folder_write_default = byId("write").checked; }));
byId("installed-mode").onchange = () => action(() => change(policy => {
  policy.installed_mode = byId("installed-mode").checked;
}));
byId("choose-packages").onclick = openPackageDialog;
byId("package-cancel").onclick = () => byId("package-dialog").close();
byId("package-dialog").onclose = () => byId("package-groups").replaceChildren();
// A background refresh may have rebuilt the opener; return focus to its replacement.
byId("contents-dialog").onclose = () => [...document.querySelectorAll(".contents-link")]
  .find(button => button.dataset.contents === byId("contents-dialog").dataset.opener)?.focus();
byId("package-form").onsubmit = event => {
  event.preventDefault();
  if (state.runtime.starting) {
    byId("package-error").textContent = "Wait for this Start to finish before saving packages for the next launch.";
    return;
  }
  action(async () => {
    const selected = [...byId("package-groups").querySelectorAll("input:checked")].map(input => input.value);
    byId("package-save").disabled = true;
    byId("package-error").textContent = "";
    try {
      await change(policy => { policy.selected_packages = selected; });
      byId("package-dialog").close();
    } catch (error) {
      byId("package-error").textContent = error.message || error;
      throw error;
    } finally {
      byId("package-save").disabled = false;
    }
  });
};
byId("common-folder").onchange = () => {
  if (byId("common-folder").value) {
    byId("folder").value = byId("common-folder").value;
    byId("folder").focus();
  }
};
byId("clear-history").onclick = () => action(() => api("history/clear", {}));
byId("hide-language-servers").onchange = () => action(() => change(policy => {
  policy.hide_language_server_denials = byId("hide-language-servers").checked;
}));
byId("allow-all-network").onclick = () => action(async () => {
  const enable = !state.policy.allow_all_network;
  if (enable && !confirm("Allow ALL network traffic from the container?\nThe website whitelist is switched off: " +
      "anything inside can send your readable files to any public website. Private networks and host loopback stay blocked.")) return;
  await change(policy => { policy.allow_all_network = enable; });
});
byId("gpu").onchange = () => { gpuChosen = true; };
function renderMode() {
  const normal = byId("mode").value === "normal";
  byId("installed-controls").hidden = normal;
  byId("installed-mode").disabled = normal || !state || !!activeLaunch();
  byId("choose-packages").disabled = normal || !state || state.runtime.starting
    || !state.policy.installed_mode || !catalog.packages.length;
  byId("package-save").disabled = !state || state.runtime.starting || !catalog.packages.length;
  byId("boundary").textContent = byId("mode").value === "normal"
    ? "Unrestricted host launch: folder, program and website permissions below DO NOT APPLY. OMBP uses your normal host configuration and credentials."
    : "Container mode: OMBP and every OMP child run together. Only approved host resources are exposed. Network requests must pass the website gateway.";
}
byId("mode").onchange = () => { renderMode(); if (state) renderLists(); };
function launchSettings() {
  return {source: byId("source").value.trim(), mode: byId("mode").value,
    desktop: byId("desktop").checked, gpu: byId("gpu").value, agent: byId("agent").value};
}
const UNRESTRICTED = "Start unrestricted on the host? None of these access controls will apply.";
byId("start").onclick = () => action(async () => {
  if (byId("mode").value === "normal" && !confirm(UNRESTRICTED)) return;
  byId("start").disabled = true;
  byId("status").textContent = "Building current OMBP…";
  const settings = launchSettings();
  try {
    await api("start", settings);
    // Automatic start reuses the settings of the latest manual start.
    if (state.policy.auto_start) await change(policy => { policy.auto_start = settings; });
  } finally {
    byId("start").disabled = false;
    byId("status").textContent = "Stopped";
  }
});
byId("auto-start").onchange = () => action(async () => {
  const settings = launchSettings();
  if (byId("auto-start").checked && settings.mode === "normal" && !confirm(`Every launcher start: ${UNRESTRICTED}`)) {
    byId("auto-start").checked = false;
    return;
  }
  await change(policy => { policy.auto_start = byId("auto-start").checked ? settings : null; });
});
byId("stop").onclick = () => action(() => api("stop", {}));
try { await loadCatalog(); }
catch (error) { message(error); }
try { await refresh(); }
catch (error) { message(error); }
setInterval(() => { if (!busy) refresh().catch(message); }, 2000);
