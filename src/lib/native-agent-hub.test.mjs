import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
const source = await readFile(new URL("./native-agent-hub.ts", import.meta.url), "utf8");
const { awaitNativeAgentHub, nativeHubDisabled, describeNativeHubError } = await import(`data:text/javascript,${encodeURIComponent(stripTypeScriptTypes(source))}`);

function fixture() {
  const state = {
    instance: { generation: 4, status: "running", activity: "streaming", bridgeReady: false, capabilities: { agentHub: true }, subagents: [] },
    current: true, terminalReady: false, opened: [], time: 0,
  };
  const options = {
    current: () => state.current,
    instance: () => state.instance,
    terminalReady: () => state.terminalReady,
    open: async generation => { state.opened.push(generation); },
    now: () => state.time,
    wait: async () => { state.time += 100; },
  };
  return { state, options };
}

test("empty native roster opens exactly once after both bridge and terminal mount, including streaming", async () => {
  const { state, options } = fixture();
  let waits = 0;
  options.wait = async () => {
    waits++;
    if (waits === 1) state.instance.bridgeReady = true;
    else state.terminalReady = true;
  };
  await awaitNativeAgentHub(options);
  assert.equal(waits, 2);
  assert.deepEqual(state.opened, [4]);
  assert.equal(nativeHubDisabled(state.instance), false);
});

test("navigation cancellation never opens a late hub", async () => {
  const { state, options } = fixture();
  options.wait = async () => { state.current = false; state.instance.bridgeReady = true; state.terminalReady = true; };
  await awaitNativeAgentHub(options);
  assert.deepEqual(state.opened, []);
});

test("generation replacement never opens a hub for the replacement session", async () => {
  const { state, options } = fixture();
  options.wait = async () => { state.instance = { ...state.instance, generation: 5, bridgeReady: true }; state.terminalReady = true; };
  await awaitNativeAgentHub(options);
  assert.deepEqual(state.opened, []);
});

test("missing capability fails conservatively and leaves terminal navigation available", async () => {
  const { state, options } = fixture();
  state.instance.bridgeReady = true;
  delete state.instance.capabilities;
  await assert.rejects(awaitNativeAgentHub(options), /native_agent_hub_unavailable: Reopen this session/);
  assert.deepEqual(state.opened, []);
  assert.equal(nativeHubDisabled(state.instance), true);
  assert.equal(nativeHubDisabled({ ...state.instance, status: "stopped" }), false);
});

test("readiness timeout and native busy errors are surfaced without retry", async () => {
  const { state, options } = fixture();
  await assert.rejects(awaitNativeAgentHub(options), /did not become ready/);
  state.instance.bridgeReady = true;
  state.terminalReady = true;
  let calls = 0;
  options.open = async () => { calls++; throw new Error("native_ui_busy"); };
  await assert.rejects(awaitNativeAgentHub(options), /native_ui_busy/);
  assert.equal(calls, 1);
});

test("stopped or failed startup never submits a native command", async () => {
  for (const status of ["stopped", "error"]) {
    const { state, options } = fixture();
    state.instance.status = status;
    await assert.rejects(awaitNativeAgentHub(options), /session did not become ready/);
    assert.deepEqual(state.opened, []);
  }
});

test("native hub error codes map to human wording and keep the raw code", () => {
  for (const code of ["native_ui_busy", "native_agent_hub_unavailable", "stale_generation", "bridge_not_ready", "not_controlled"]) {
    const mapped = describeNativeHubError(new Error(code));
    assert.equal(mapped.code, code);
    assert.ok(mapped.message.length > 0 && !mapped.message.includes(code));
  }
  assert.match(describeNativeHubError("native_ui_busy").message, /Another prompt is open/);
  const suffixed = describeNativeHubError(new Error("native_agent_hub_unavailable: Reopen this session with a compatible OMP version."));
  assert.equal(suffixed.code, "native_agent_hub_unavailable");
  assert.equal(describeNativeHubError("bridge error: stale_generation").code, "stale_generation");
});

test("unknown native hub errors pass through unchanged", () => {
  assert.deepEqual(describeNativeHubError(new Error("The native Agent Hub did not become ready.")), { code: null, message: "The native Agent Hub did not become ready." });
  assert.deepEqual(describeNativeHubError("my_native_ui_busy_x"), { code: null, message: "my_native_ui_busy_x" });
  assert.deepEqual(describeNativeHubError(42), { code: null, message: "42" });
});
