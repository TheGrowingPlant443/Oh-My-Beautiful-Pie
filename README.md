# OMP Manager

A local-first Windows desktop dashboard for independent Oh My Pi processes. Rust + Tauri, plain Svelte/TypeScript, and xterm.js. Herdr is not a dependency.

## Run

Prerequisites: Rust/MSVC with the Windows SDK, Node.js compatible with Vite 8, the system WebView2 runtime, and an installed `omp` on `PATH`. Windows 11 x64 is the exercised platform; other platforms are not verified.

```sh
npm ci --ignore-scripts
cargo install tauri-cli --version 2.12.0 --locked
cargo tauri dev
```

If OMP is not on `PATH`, set `OMP_BIN` to its executable path before launching the manager.

Build a standalone executable without downloading installer tooling:

```sh
cargo tauri build --no-bundle -- --locked
```

Output: `src-tauri/target/release/omp-manager.exe`. It embeds the UI; no Vite server is needed for the release executable.

## Use

- **New instance**: select/type a working directory, optional OMP profile, ordered YAML config overlays, and optional model/role override. Empty profile selects OMP's default profile; empty model leaves model resolution to OMP.
- **Reconnect** lists native saved sessions for the chosen project/profile. It does not attach to an externally running terminal process; the session must not already be open elsewhere.
- A tile opens the actual interactive OMP terminal. Switching views does not stop other processes.
- **Stop** sends OMP's double-Ctrl+C exit sequence, waits for graceful shutdown, then force-stops the direct child if necessary. **Restart/Reopen** use the saved native session path.
- **Subagents** opens read-only previews; clicking a child opens its native rendered transcript. **View Plan** opens the session's plan file.
- **Previews Off** replaces the dashboard grid with Project → OMP → Subagent rows. Focused interactive terminals remain available. Preview-size controls and native window resizing make previews larger.
- Closing the desktop stops its managed processes. Definitions persist, but reopening the app never auto-starts agents. Remove deletes only the dashboard definition, not OMP history.

### The Manager

- Click **The Manager**, select its working directory/profile/config, and create it. Subsequent clicks open/reopen the same native OMP session.
- Ask it to use **manage_sessions** to create dashboard workers, review their code/execution, inspect native plans, and handle native questions/approvals. Workers appear as ordinary dashboard instances, not OMP subagents.
- Existing user instances are independent until you click **Hand over to The Manager**. **Take back control** immediately revokes controller access without restarting the worker.
- **Needs you** shows an intervention reason. Open the worker and handle its native prompt, or press **Return to The Manager** after supplying the missing context. The Manager cannot clear its own human-intervention gate.
- **Show Manager tile** hides only the overview/tree row. The toolbar button and session switcher remain available; notifications continue with the tile hidden or previews off.
- Stopping The Manager leaves workers running. Completion/decision notifications queue while it is offline and replay when it reconnects. Queues last only for the desktop app's lifetime; native OMP history remains durable.

Example: “Use manage_sessions to create a worker named API in C:/projects/api, ask it to review the failing build, and report when it finishes. Inspect its work before approving further execution.”

### Global config

- **Save as global config** previews non-secret native OMP settings from the selected instance (its `config.yml` plus ordered overlays) or from native default-profile config, then publishes an immutable snapshot under the app-config folder. **Import config** does the same from a `.yml`/`.yaml` file. Neither action writes to or modifies `.omp`.
- The active snapshot is the default for **new** instances, including Manager-created workers (`useGlobalConfig` defaults on; uncheck **Use global config** or pass `false` to opt out). It is placed before any explicit overlays and the `--model` override, so those win. Existing definitions, running sessions and older snapshots never change.
- Use a snapshot in another terminal with `omp --config '<saved path>'`; the dialog shows the exact command.
- Sanitized export/import keeps model roles and other registered non-secret settings. It omits credentials, headers/env maps, unknown settings, and `extensions`, `hooks`, `customTools`, `mcp`, `telemetry`, `auth`, and `shellPath`; excluded paths (never values) are listed. Model catalogs and authentication must exist locally. Requires Bun and the installed OMP SDK (`BUN_BIN`, `OMP_SDK_DIR`, `OMP_BIN` override discovery; the SDK and executable versions must match).

### Agent setup

- **Agent setup** is a full page: choose **Global — default OMP profile** or **Just The Manager**, then describe `AGENTS.md`, `RULES.md`, skills (`SKILL.md` plus a readable `SKILLS.md` catalog), reusable Python/Bash/TypeScript tools (native TS wrapper with `exec` approval), and workflows (always-apply native rules for delegation/review conditions; there is no scheduler). Review the exact target paths, then start the setup agent.
- Setup runs in a real, unowned OMP session in native plan mode. You review its native plan and choose a native execution option; target files are not written before approval, and The Manager cannot approve it. Completion is whatever the native session reports.
- Manager-only assets live in the app-owned `manager-setup` folder, loaded only into The Manager through a directory extension. `AGENTS.md`, `RULES.md` and `SKILLS.md` are re-read each Manager turn; new skills/tools/rules need a native reload or reopen. Nothing restarts The Manager automatically.
- **Open global .omp in VS Code** and **Open Manager folder in VS Code** open the displayed folders with `Code.exe` directly (no shell). Neither creates a missing native folder.

## Data ownership

The manager stores launch definitions and their native session paths in Tauri's app-config directory (`instances.json`, identifier `dev.udvikler.omp-manager`). OMP owns sessions, chats, plans, configs, models, approvals, tools, and thinking visibility.

The Rust reader consumes native OMP JSONL journals and their sibling artifact directories. It does not write OMP journals or copy them into a separate chat database. Main/subagent transcripts use `omp render` with the instance's working directory, profile, and config overlays. A bundled extension is loaded only into app-managed OMP processes; it reports exact native session identity, completion, and decisions, and exposes the Manager's controller tool. It is not installed into global OMP configuration.

Tile **working/idle** is an output-activity indicator, not an authoritative agent-lifecycle state. Tasks, plans, and child metadata reflect OMP's last persisted state; in-flight subagent status may lag until OMP persists an update. Streaming chat remains visible in the real terminal. PTY replay is bounded to 256 KiB; older history remains in OMP.

## Safety and dependencies

- No telemetry, updater, tray/autostart service, external dashboard server, worker-to-worker messaging, or model-management logic.
- The desktop broker listens only on `127.0.0.1` at an ephemeral port while the app runs. Each child authenticates with a private per-generation capability. No external network requests are made by the manager itself. OMP can use the providers, MCP servers, and extensions configured by the user.
- The sole non-core Tauri permission is `dialog:allow-open`. The frontend loads only bundled assets in release mode. Windows DLL search excludes the current directory and `PATH` to avoid ConPTY DLL search-path hijacking.
- Rust PTYs use pinned `portable-pty 0.9.0` from WezTerm. UUID and Windows API crates were inspected from their published source manifests. The Tauri CLI is compiled from source rather than installed as a prebuilt npm CLI package.
- The headless terminal mirror uses pinned MIT `vt100 0.16.2`, inspected with its ordinary `itoa`, `unicode-width`, and `vte` dependencies; it has no build script or binary-download hook. Rust handles terminal cursor reports even when no preview exists.
- `package-lock.json` and `src-tauri/Cargo.lock` are checked in. Use `npm ci --ignore-scripts` and Cargo's `--locked` option.
- None of the direct npm dependencies has an install hook. The lockfile includes optional macOS `fsevents`, which does declare one; `--ignore-scripts` disables it. Vite's development toolchain includes upstream prebuilt Rolldown/Lightning CSS platform binaries. These are build-time dependencies, not silently downloaded application executables.
- OMP extensions are executable user configuration, not a sandbox. The manager does not weaken OMP's approval or thinking configuration.

## Verification

```sh
cargo test --manifest-path src-tauri/Cargo.toml --locked
node --test bridge/omp-manager.test.mjs bridge/omp-config.test.mjs
npm run check
npm run build
```

Exercised on Windows: independent real OMP children; terminal handshake, input and response; preview-only startup; stop/restart and saved conversation continuity; application close/reopen with no autostart; native folder picker; native reconnect listing; plan viewing; subagent previews/full transcript; compact tree with no dashboard terminals; and responsive preview sizing.

Focus regression checked in the native desktop: with an OMP terminal running, open **New instance**, type in the Name field, resize the window, and let background updates arrive. The modal must retain keyboard focus. Creating the second instance must leave the first running. Terminal resizing never requests keyboard focus; the creation form uses a native modal dialog.

Rendering regression checked in the native desktop: type an unsent draft and leave the focused session open across background updates. The xterm screen, input focus, and draft must remain stable without same-size PTY resize requests. Real window resizing still updates rows/columns once per geometry change, including the 500-column limit; restarting the OMP process intentionally creates a new terminal.

Regression tests cover persisted launch definitions, native todo/plan/subagent decoding, append updates, truncation/replacement, partial UTF-8 records, and local-plan traversal rejection.

Controller verification used isolated native desktop projects and real OMP 18.8.7 sessions with the configured GPT-6.1-Sol: background worker creation without a mounted preview; completion notifications; native always-ask write approval; code/file review; pending-dialog handover without restart; human escalation/return; native plan review and execution using terminal keys; hidden-manager/tree notifications; and stopping/reopening The Manager without stopping workers. Unit checks cover capability rejection, stale/revoked decisions, subscriptions, ownership persistence, native answer validation, and headless cursor reports.

Config and setup verification used an isolated desktop: sanitized preview/save/import; defaults inherited by new instances and a Manager-created worker while running sessions kept their snapshot; explicit overlay precedence and opt-out; complete Global and Manager-only setups reviewed and approved in native plan mode with every file kind generated; native discovery and real execution of the generated Python/Bash/TypeScript tools; a running Manager applying a changed private instruction without restart; both VS Code windows; and narrow-window layout. Unit checks cover the sanitizer, immutable snapshots, creation-time freezing, setup request validation, replay protection, and plan-path correlation.

Controller authorization is not a filesystem sandbox: OMP's ordinary tools retain the user's configured access. Native explicit deny rules, disabled choices, and thinking visibility remain authoritative. Custom/plan overlays are controlled through their real terminal screen and keys, not fabricated approval APIs. Compatibility is grounded in OMP 18.8.7; incompatible native UI contracts surface errors rather than silently bypassing policy.
