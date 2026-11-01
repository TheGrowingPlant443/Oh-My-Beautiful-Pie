"use strict";
const byId = id => document.getElementById(id);
const token = location.hash.slice(1) || sessionStorage.getItem("control-token") || "";
sessionStorage.setItem("control-token", token);
history.replaceState(null, "", "/");
let state;
let busy = false;
let renderedPolicy = "";
let renderedHistory = "";

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

function entry(list, label, detail, remove) {
  const row = document.createElement("li");
  const text = document.createElement("span");
  text.textContent = label;
  const small = document.createElement("small");
  small.textContent = detail;
  text.append(small);
  const button = document.createElement("button");
  button.className = "remove";
  button.textContent = "−";
  button.setAttribute("aria-label", `Revoke ${label}`);
  button.onclick = () => action(() => change(remove));
  row.append(text, button);
  list.append(row);
}

function renderPermissions() {
  for (const name of ["folders", "programs", "websites"]) byId(name).replaceChildren();
  state.policy.folders.forEach((folder, index) => entry(byId("folders"), folder.path,
    folder.write ? "Read + write + delete" : "Read only", policy => policy.folders.splice(index, 1)));
  state.policy.programs.forEach((program, index) => entry(byId("programs"), program,
    "Available in /allowed-programs", policy => policy.programs.splice(index, 1)));
  state.policy.websites.forEach((website, index) => entry(byId("websites"), website,
    state.policy.credential_origins.includes(website) ? "All subpaths · host keyring Authorization" : "All subpaths",
    policy => {
      policy.websites.splice(index, 1);
      policy.credential_origins = policy.credential_origins.filter(origin => origin !== website);
    }));
}

async function allowEvent(event) {
  if (!confirm(`Allow ${event.kind}: ${event.target}?\nFolders are granted read-only. Website subpaths are all allowed. Internet must also be enabled.`)) return;
  await change(policy => {
    if (event.kind === "website") policy.websites.push(event.target);
    else if (event.kind === "program") policy.programs.push(event.target);
    else policy.folders.push({path: event.target, write: false});
  });
}

function renderHistory() {
  const root = byId("history");
  const existing = new Map([...root.children].map(row => [row.dataset.key, row]));
  let before = root.firstChild;
  for (const event of state.history) {
    const key = JSON.stringify([event.kind, event.target]);
    let row = existing.get(key);
    if (!row) {
      row = document.createElement("div");
      row.className = "event";
      row.dataset.key = key;
      const detail = document.createElement("div");
      detail.textContent = event.target;
      detail.append(document.createElement("small"));
      const allow = document.createElement("button");
      allow.className = "allow";
      allow.textContent = "+";
      allow.setAttribute("aria-label", `Allow ${event.target}`);
      allow.onclick = () => action(() => allowEvent(event));
      row.append(detail, allow);
    }
    row.firstElementChild.lastElementChild.textContent =
      `${event.kind} · ${event.count} attempts · ${new Date(event.time * 1000).toLocaleString()} · ${event.reason}`;
    if (row !== before) {
      if (before) before.before(row);
      else root.append(row);
    }
    before = row.nextSibling;
    existing.delete(key);
  }
  for (const row of existing.values()) row.remove();
}

async function refresh() {
  state = await api("status");
  const running = state.runtime.running;
  byId("internet").checked = state.policy.internet;
  byId("history-error").textContent = state.history_error ? `Denied history is only in memory: ${state.history_error}` : "";
  const mode = state.runtime.mode === "container" ? "Podman" : "Unrestricted";
  byId("status").textContent = running ? `${mode} · running` : "Stopped";
  byId("start").disabled = running;
  byId("stop").disabled = !running;
  for (const id of ["mode", "gpu", "desktop", "executable"]) byId(id).disabled = running;
  if (running) {
    byId("mode").value = state.runtime.mode;
    byId("gpu").value = state.runtime.launch.gpu;
    byId("desktop").checked = state.runtime.launch.desktop;
    byId("executable").value = state.runtime.launch.executable;
    renderMode();
  }
  if (!byId("executable").value) byId("executable").value = state.executable;
  const policyKey = JSON.stringify(state.policy);
  const historyKey = JSON.stringify(state.history);
  if (renderedPolicy !== policyKey) { renderPermissions(); renderedPolicy = policyKey; }
  if (renderedHistory !== historyKey) { renderHistory(); renderedHistory = historyKey; }
  if (state.runtime.error) message(state.runtime.error);
}

byId("folder-form").onsubmit = event => {
  event.preventDefault();
  action(async () => {
    await change(policy => policy.folders.push({path: byId("folder").value.trim(), write: byId("write").checked}));
    byId("folder").value = "";
    byId("write").checked = false;
  });
};
byId("program-form").onsubmit = event => {
  event.preventDefault();
  action(async () => {
    await change(policy => policy.programs.push(byId("program").value.trim()));
    byId("program").value = "";
  });
};
byId("website-form").onsubmit = event => {
  event.preventDefault();
  action(async () => {
    await change(policy => {
      const website = byId("website").value.trim();
      policy.websites.push(website);
      if (byId("host-credential").checked) policy.credential_origins.push(website);
    });
    byId("website").value = "";
    byId("host-credential").checked = false;
  });
};
byId("internet").onchange = () => action(() => change(policy => { policy.internet = byId("internet").checked; }));
byId("downloads").onclick = () => { byId("folder").value = `${state.home}/Downloads`; byId("folder").focus(); };
byId("desktop-folder").onclick = () => { byId("folder").value = `${state.home}/Desktop`; byId("folder").focus(); };
function renderMode() {
  byId("boundary").textContent = byId("mode").value === "normal"
    ? "Unrestricted host launch: folder, program and website permissions below DO NOT APPLY. OMBP uses your normal host configuration and credentials."
    : "Container mode: OMBP and every OMP child run together. Only approved host resources are exposed. Network requests must pass the website gateway.";
}
byId("mode").onchange = renderMode;
byId("start").onclick = () => action(async () => {
  if (byId("mode").value === "normal" && !confirm("Start unrestricted on the host? None of these access controls will apply.")) return;
  await api("start", {executable: byId("executable").value.trim(), mode: byId("mode").value,
    desktop: byId("desktop").checked, gpu: byId("gpu").value});
});
byId("stop").onclick = () => action(() => api("stop", {}));
try { await refresh(); }
catch (error) { message(error); }
setInterval(() => { if (!busy) refresh().catch(message); }, 2000);
