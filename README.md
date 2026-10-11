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

## Data ownership

The manager stores launch definitions and their native session paths in Tauri's app-config directory (`instances.json`, identifier `dev.udvikler.omp-manager`). OMP owns sessions, chats, plans, configs, models, approvals, tools, and thinking visibility.

The Rust reader consumes native OMP JSONL journals and their sibling artifact directories. It does not write OMP journals or copy them into a separate chat database. Subagent chats use `omp render` with the instance's working directory, profile, and config overlays; no replacement agent or transcript renderer is implemented. No bridge/plugin is installed.

Tile **working/idle** is an output-activity indicator, not an authoritative agent-lifecycle state. Tasks, plans, and child metadata reflect OMP's last persisted state; in-flight subagent status may lag until OMP persists an update. Streaming chat remains visible in the real terminal. PTY replay is bounded to 256 KiB; older history remains in OMP.

## Safety and dependencies

- No telemetry, updater, tray/autostart service, external dashboard server, cross-instance messaging, or model-management logic.
- The manager itself makes no external network requests. OMP can use the providers, MCP servers, and extensions configured by the user.
- The sole non-core Tauri permission is `dialog:allow-open`. The frontend loads only bundled assets in release mode. Windows DLL search excludes the current directory and `PATH` to avoid ConPTY DLL search-path hijacking.
- Rust PTYs use pinned `portable-pty 0.9.0` from WezTerm. UUID and Windows API crates were inspected from their published source manifests. The Tauri CLI is compiled from source rather than installed as a prebuilt npm CLI package.
- `package-lock.json` and `src-tauri/Cargo.lock` are checked in. Use `npm ci --ignore-scripts` and Cargo's `--locked` option.
- None of the direct npm dependencies has an install hook. The lockfile includes optional macOS `fsevents`, which does declare one; `--ignore-scripts` disables it. Vite's development toolchain includes upstream prebuilt Rolldown/Lightning CSS platform binaries. These are build-time dependencies, not silently downloaded application executables.
- OMP extensions are executable user configuration, not a sandbox. The manager does not weaken OMP's approval or thinking configuration.

## Verification

```sh
cargo test --manifest-path src-tauri/Cargo.toml --locked
npm run check
npm run build
```

Exercised on Windows: independent real OMP children; terminal handshake, input and response; preview-only startup; stop/restart and saved conversation continuity; application close/reopen with no autostart; native folder picker; native reconnect listing; plan viewing; subagent previews/full transcript; compact tree with no dashboard terminals; and responsive preview sizing.

Focus regression checked in the native desktop: with an OMP terminal running, open **New instance**, type in the Name field, resize the window, and let background updates arrive. The modal must retain keyboard focus. Creating the second instance must leave the first running. Terminal resizing never requests keyboard focus; the creation form uses a native modal dialog.

Rendering regression checked in the native desktop: type an unsent draft and leave the focused session open across background updates. The xterm screen, input focus, and draft must remain stable without same-size PTY resize requests. Real window resizing still updates rows/columns once per geometry change, including the 500-column limit; restarting the OMP process intentionally creates a new terminal.

Regression tests cover persisted launch definitions, native todo/plan/subagent decoding, append updates, truncation/replacement, partial UTF-8 records, and local-plan traversal rejection.
