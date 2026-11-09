# OMBP Start app

An independent Linux launcher for the selected OMBP checkout. Its local browser UI remains outside the sandbox; OMBP, its private session bus/keyring, and every OMP/OMSP child run inside **one rootless Podman container**.

## Run

From the repository root:

```sh
cd 'start app'
nix run path:.
```

The launcher opens its four-column UI in your browser. If automatic browser opening is unavailable, open the loopback URL printed in the terminal. Keep that URL private: its fragment authenticates permission changes. A browser reload retains authentication in that tab's session storage.

The control UI intentionally uses authenticated HTTP on loopback, with Host/Origin checks and no cross-origin access. It installs no host certificates. Do not expose it through a port forward or reverse proxy.

Choose the **OMBP source folder** containing the parent `flake.nix`. It defaults to the launcher's working directory, or its parent when launched from `start app`; use `--source-dir /absolute/checkout` to choose a different Git worktree. Each **Start OMBP** click runs `nix build path:.#default --no-link --print-out-paths --no-write-lock-file` in that checkout and launches only its returned `bin/omp-manager`. Local saved changes are included, Nix reuses unchanged build results, and the existing lock file is not updated. This does not fetch or merge another branch, install a profile package, or fall back to an older executable when the build fails. Select the checkout containing the changes you want.

Choose **Podman · isolated** or **Normal · unrestricted host access**, then Start. The UI shows **Building current OMBP…** until the build and launch finish; build output and errors go to the state's `launch.log`. Closing the launcher during a build cancels its Nix client and prevents the pending OMBP launch. Builds happen on the host before the sandbox starts and may download Nix dependencies regardless of the container's website whitelist. Choose only trusted source checkouts. Normal mode runs the newly built executable with your normal host environment; **none of the whitelist controls apply in normal mode**.

**Automatic start** (next to Stop) starts OMBP as soon as the launcher starts, with the settings of your latest Start (source, mode, GPU, desktop); turning it on saves the current settings. A failed automatic start shows its error in the UI. Turning it on in Normal (unrestricted) mode asks for confirmation.

**Agent** chooses which coding agent OMBP drives: **omp** ([Oh My Pi](https://github.com/can1357/oh-my-pi), default; builds OMBP's `.#default`) or **omsp** ([Oh My Striped Pi](https://github.com/TheGrowingPlant443/oh-my-striped-pi), the stripped fork; builds `.#omsp`, which runs OMBP on the fork's source SDK and Bun). OMBP receives the choice as `OMBP_AGENT`. For now omsp shares your `~/.omp` (`OMSP_CONFIG_DIR=.omp`), so both use the same logins, models and sessions. An OMBP checkout without the `omsp` output fails to build with omsp selected.

Both container modes use the matching agent SDK and Bun shipped by the selected OMBP build, rather than installing a mismatched second agent version. Installed mode provides a directly usable `omp` or `omsp` command in `/installed-programs` and points `OMP_BIN` there. Inspection mode uses `/launcher/omp` with OMBP's `OMP_SDK_DIR`. The selected checkout must expose `agent-omp` and `agent-omsp` for installed mode.

Prerequisites: local rootless Podman, `/dev/fuse`, a host setuid `fusermount`/`fusermount3`, and Nix. The launcher supplies its Python, FUSE library, proxy, nested compositor and session tools through its own pinned flake. It does not change `/etc/fuse.conf`, install drivers, expose an unrestricted engine socket, or use another package manager. A graphical launch requires an existing Wayland session; raw host X11 is deliberately not supported.

For development or headless control:

```sh
nix develop path:.
python3 app.py --no-browser --state-dir /absolute/private/state
nix build path:. path:.#checks.x86_64-linux.syntax --no-link
PYTHONDONTWRITEBYTECODE=1 python3 -m coverage run -m unittest discover -v
python3 -m coverage xml
```

Python coverage is imported from `.quality-coverage/python.xml`; browser V8 smoke coverage uses `.quality-coverage/javascript.lcov`. Reports and bytecode are excluded from the Nix application source. Coverage tooling is supplied only by the development shell, not the shipped runtime.

The default state directory is `${XDG_STATE_HOME:-~/.local/state}/ombp-start`, owned by you with mode 0700. It contains permissions, denied history, the proxy CA private key, the private-keyring unlock key, the container's persistent home and `launch.log`. Never publish it. Network access receives only the public CA certificate and dedicated proxy socket directory. Closing the launcher stops its container; closing only the browser tab does not. Stop gives the container a 10-second graceful termination period before Podman forces retirement. Only one launcher may own a state directory. A subsequent launch retires a recorded orphaned container after verifying its ownership label.

State protection uses the canonical physical directory, including when a parent directory is reached through a symlink. A symlink used as the state directory itself is rejected.

## Installed or inspection mode

**Install into container** is on by default, including when opening existing launcher state. **Choose tools…** opens a modal with optional CLI tools, applications and local language servers. Save persists the selection; Cancel/Escape discards the draft. Changes apply on the next Start, including automatic start. Selecting no optional packages is valid: OMBP, the selected agent, Bash/sh, Python, D-Bus/keyring, coreutils, fonts, Podman and the file picker remain mandatory. Application runtime dependencies such as Bun, Node and Git are also always included.

Installed mode realizes the chosen pinned Nix packages on the host, then copies their complete runtime closures into a private installation under the launcher's state directory. It never mounts the entire host Nix store. The private store and `/installed-programs` are mounted read-only inside the container; program files there do not pass through the permission filesystem. Copies use reflinks where supported and are reused for identical selections. A changed selection gets a separate generation, so deselected optional packages do not remain in the new installation merely because they were installed before. Transitive dependencies may still provide additional executables: package selection is not a per-binary security policy.

The initial Nix build/package realization can download dependencies on the host before the sandbox starts; no host profile is modified. The running container keeps `--network=none`, the existing website proxy policy, and live host-folder permissions. Local tools/LSPs do not become hosted services, but project dependency downloads still require the existing network permissions. Required package files are immutable; private home/tmp remain writable.

Turn **Install into container** off to use the original **inspection mode**. Saved host-program grants and Revoke/Restore selections are retained for that mode, not applied to installed package files. Its filtered Nix-store and `/allowed-programs` views keep recording denied program lookups. In installed mode a program's **+** opens Choose tools; it neither installs immediately nor clears the unresolved denial.

For another machine, share this repository—not your state directory, `~/.omp`, caches, tokens or profiles. The friend still needs the Linux/Nix/rootless-Podman/FUSE/Wayland prerequisites above, chooses their own source folder and tool selection, and supplies their own provider login/model configuration. Installed mode uses pinned Chromium and optionally pinned Obsidian rather than discovering your personal host Chrome/Obsidian. Unfree-license choices are marked and off by default. The launcher may already cache tools for inspection mode, but only the selected package closures and mandatory runtime are copied into an installed generation.

### Local tool catalog

The chooser covers all 55 language-server definitions shipped by OMP 18.12.6 and OMSP 18.13.3, including their CLI-based linters. Some share one package, such as the VS Code HTML/CSS/JSON/ESLint servers. It also offers the agents' diagnostic/checker commands, toolchains, debugger adapters, FFmpeg/ffprobe, web-extraction helpers and local terminal tools. Hover a choice for its executable names. A fresh launcher state pre-selects only Basic CLI tools, Chromium, GitHub CLI and OpenSSH; every language server, compiler and other choice is opt-in. Existing saved selections are kept as they are. Select the tools your project needs rather than enabling several competing servers unnecessarily.

Server packages include their required runtime dependencies and essential analysis toolchains. TypeScript's native compiler is packaged so the SDK discovers its native LSP; the JavaScript-based server remains available for older project SDKs. Selected Python with debugpy takes precedence over the core Python command; selecting Flutter takes precedence over standalone Dart. JavaScript debugging receives the packaged adapter path. ElixirLS is compiled during the Nix build, not installed by Mix at startup; Ruby LSP inherits its packaged gems and leaves server-gem updates to Nix.

These installations do not supply every project's libraries, compiler version or platform SDK. Project lockfiles, ESLint plugins, gems, NuGet/Maven dependencies, schemas and similar inputs still belong to the project; downloads remain subject to network permissions. Ruby LSP still validates the project's Gemfile/lockfile and dependencies. Installing an adapter does not configure its debug target or install PHP Xdebug into a target project. Arbitrary shell/MCP commands, host clipboard/FUSE/desktop integration and public tunnel services are not added or granted by the catalog.

Known package constraints:

- Swift/SourceKit's matching toolchain is supported on x86_64 Linux; the pinned compiler is broken on ARM Linux, so those choices are disabled there and rejected by the API. Linux does not supply Xcode/macOS SDKs.
- JetBrains Kotlin LSP, Intelephense, Terraform and Obsidian have unfree licenses. Intelephense premium features still require a separate license.
- Erlang LS is the final archived upstream release, with its matching OTP runtime.
- TLA+ includes its actual language server, matching proof manager, pinned CommunityModules and Z3. Isabelle/TLA+, Zenon, LS4 and SANY remain additional proof-backend requirements.

The shipped CodeLLDB and Ruby debugger definitions need project transport/target settings. For example, a project `dap.json` can contain the following; replace `./main.rb` with that project's Ruby entry point:

```json
{
  "adapters": {
    "codelldb": {
      "args": ["--port", "${port}"],
      "connectMode": "tcp"
    },
    "rdbg": {
      "args": ["--open", "--port", "${port}", "--host", "127.0.0.1", "--command", "--", "ruby", "./main.rb"],
      "connectMode": "tcp"
    }
  }
}
```


## Four columns

Each column's top-right **i** opens keyboard-accessible help. Program and folder suggestions are local: typing does not perform a web search or grant access.

In the sandbox, file and folder Browse dialogs using Zenity (including SonarQube scanning) start at `/host`, where approved host folders retain their full paths: `/home/you/project` appears as `/host/home/you/project`. The sandbox's Zenity entrypoint supplies `--filename=/host/` only when the caller supplies no filename; normal host launches are unchanged. This changes navigation only, not folder permissions.

The nested OMBP display keeps the main application borderless/fullscreen. Only Zenity dialogs are maximized there, keeping their **OK** and **Cancel** controls visible without adding a titlebar to OMBP. The separate browser display also maximizes windows to retain the address bar.

Repository discovery for OMBP's Nix-packaged source walker checks the selected directory, then parents, stopping at the first `.git` marker or home. The container receives `OMBP_GIT_DISCOVERY_HOME=/host` followed by your host home path, so `/host/home/you`—not `/home/sandbox`—bounds scans of your shared projects. This neither grants parent-folder access nor hides denied attempts. Existing history entries remain until you clear them.

1. **Folder access:** choose Home, Downloads, Desktop, Documents, Pictures, Music, Videos, Public or Templates from the common-folder picker, which respects your localized XDG folder names, or type in **Host folder**: a name (for example `udvikler` → `/home/you/udvikler`) suggests matching folders under your home, names starting with it first and shorter paths first. The search skips hidden, symlinked, `node_modules`, `__pycache__` and `target` folders and launcher state, stops after 6 levels, 50,000 entries or 0.3 s, and never follows links or runs anything. You can also enter any absolute path. **Allow write and deletion** is the remembered default for new folders (off: they start read-only); each granted folder has its own **Write** toggle that applies live. Write always includes deletion. Host folders appear at `/host` followed by their path. `/tmp/omp-manager` is automatically granted read/write once and, when granted at launch, mounted at the same path inside the sandbox through the same permission gateway, so OMBP sees one canonical workspace path. Removing or re-adding it applies live; removal may break OMBP sessions. If it was not granted at launch, adding it later exposes it only under `/host` until the next start. Your real `~/.omp` is listed as a required folder and is bind-mounted read-write at the container's `~/.omp`, so OMP inside uses your own config, models, skills, plugins, logins and sessions. It deliberately bypasses the gateway (SQLite locking, mmap and hardlinks must behave exactly as for a host OMP), so the gateway protections below do not apply inside it. **Risk you accepted:** code in the container can change config, extensions, plugins or rules that your host OMP later loads outside the sandbox. Revoke it here to stop sharing on the next launch; symlinks inside `~/.omp` that point elsewhere on the host do not resolve in the container. Start is refused if `~/.omp` overlaps the launcher state, code or runtime directory. Private sandbox mounts are listed separately with Revoke/Restore; those changes apply on the next launch and can prevent startup. In gateway-shared folders, host symlinks, hardlinks, system trees and launcher state remain protected.
2. **Programs & desktop:** installed mode lists the required core and current package selection; use **Choose tools…** and restart to change it. A row whose name is underlined contains several programs (for example **Required installed core** or **Basic CLI tools**): click it to list them. After a Start the list shows the exact commands installed then; before that it shows the package's main commands. In inspection mode the required bundles open the same list. Search Nix applications in your profile, PATH and desktop entries, or enter a Nix executable path. Suggestions never execute commands. Extra programs appear in `/allowed-programs` with their dependencies and can be added/removed live; their heading lines up with **Host folder grants** in the Folder access column. Required-bundle Revoke/Restore selections persist across launches; revoking a bundle denies its primary store roots and may break OMBP. Already loaded code cannot be taken back. This is a **host-resource whitelist, not a ban on arbitrary code inside the container**. GPU selection detects the boot AMD/NVIDIA card (AMD wins a tie), otherwise software rendering; you can override it. GPU/display changes require a new launch; NVIDIA still requires configured CDI and is not hardware-tested.
3. **Websites & internet:** **Turn on internet for the container**, below the URL field, defaults on for a fresh state; existing on/off choices are preserved. It enforces the whitelist unless **Allow all network traffic** (Denied attempts) is on. Four removable defaults are seeded once: GitHub, Google, Stack Overflow and Atlassian. Removed defaults do not return on restart. Type a URL or domain in **Enter a URL** (no suggestions), then click Allow website. Bare domains gain `https://`. **Subdomains** is on by default for each site: `github.com` also allows `api.github.com`, over http or https on default ports, and a leading `www.` is ignored, so `www.google.com` allows `*.google.com` (including `http://clients2.google.com`); a site entered with an explicit port only covers that exact scheme and port; untick Subdomains for exactly the listed origin. Redirects to unrelated origins still need their own entry. Host keyring Authorization is only ever sent to the exact origin. Add/remove and the internet switch apply live; active proxy streams are revoked on the gateway's 250ms policy check. Whenever permissions change (or the launcher starts), denied website rows that are now allowed are removed from Denied attempts.
   **Auto-approve** (on by default) lets through matching hosts over http or https. The first time an origin is auto-approved it is added to the website list as an ordinary entry (Subdomains on), so you can see and manage it; removing it there does not stop auto-approve from letting it through again. An entry with a dot, such as `google.com`, matches any host that **contains** `google` and ends in `.com`, so `www.google.com`, `clients2.google.com` and `update.googleapis.com` pass, but `google.de` does not. An entry without a dot, such as `openai`, matches any host containing it. Defaults: `anthropic.com`, `chatgpt.com` and `google.com`. Containment is deliberately broad: `google.com` also matches a look-alike such as `evilgoogle.com`. Untick it to stop; **Settings** shows the entries with **−** each and an Add field. Each allowed site also has two opt-in dependency checkboxes:
   - **HTML/CSS** allows every origin that site's own served HTML and CSS load as resources (scripts, stylesheets, images, fonts, media, frames, CSS `url()`/`@import`); plain links are ignored. The proxy learns these from responses as you browse, so the sandbox cannot forge them. Dependencies that scripts request later are not covered. Sites that embed user-chosen resources (for example images from any host) widen the list.
   - **Header** allows any origin requested with that site in its `Origin`/`Referer` header, which also covers script-loaded dependencies. **Any program in the sandbox can fake that header, so while it is on, the sandbox can reach any website.**
   Approved dependency groups appear under their site with their own **−** to remove them together.
4. **Denied attempts:** **Allow all network traffic** at the top switches the website whitelist off (it asks first and turns red while on; click again to stop). Anything in the container can then reach any public website, so readable files can leave; private networks and host loopback stay blocked, and **Turn on internet** still overrides it. Gateway-observed denied folders, programs and websites have a **+** action; program actions depend on the selected container mode as described below. Approved folder/website rows disappear from the list and reappear if denied again. Denied websites are grouped under the site whose served page loaded them, or else the site named in the request's `Origin`/`Referer` (grouping only; you still approve). Expand a group to see each blocked website with a checkbox (all checked by default); the group's **+** allows the checked ones as that site's dependencies, removable later as a group under the site in the Websites column. Unapproved HTTPS destinations are denied after TLS interception so the requesting site is visible; nothing reaches them. **Clear history** clears the saved history without granting permissions; new attempts, including previously cleared resources, appear again. The latest 500 distinct resources are retained. Direct network syscalls and missing programs outside the gateways have no reliable target history here.

In installed mode the program **+** opens package selection for the next launch; only inspection mode grants host programs live. Folder quick-add uses the saved **Allow write and deletion** default, not a separate permission rule.

OMP and OMSP probe supported language-server commands, which can produce many denied program lookups in inspection mode. **Hide language-server denials** (on by default, saved with the permissions) filters program entries matching their 55 configured command names and shows how many entries are hidden. It does not change permissions or delete history. Unticking restores the entries. Matching uses the command name, not its caller or arguments, so other uses of shared commands such as `tsc` or `deno` are hidden too; folder and website denials remain visible.

The container's `/tmp` (**Private temporary files**) starts as a private in-memory filesystem, discarded on stop — not your host `/tmp`. The read-only root filesystem needs this writable scratch space for D-Bus, compilers and other tools. Separately, the default `/tmp/omp-manager` host grant can be mounted over that subdirectory through the permission gateway. Host `~/.omp` sharing is controlled by **Your OMP folder**; it is distinct from the container's own home and temporary files.

If denied-history storage fails, permission enforcement continues and current events remain in memory. The history column shows a warning until a later save succeeds. Failed permission saves remain errors and do not change the effective permissions.

The container has no external network interface. A loopback proxy relay connects to a host Unix socket. Clients that ignore proxy settings cannot bypass the whitelist; their requests fail. HTTPS is terminated and re-encrypted by mitmproxy with a launcher-local CA, and upstream certificates remain verified. CONNECT destination, SNI, request authority and Host must agree. DNS is checked for non-public addresses and pinned before connection, blocking private networks and rebinding. WebSockets to allowed origins work (for example ChatGPT's) and are cut when the origin or internet is revoked; other HTTP upgrades and raw tunnels are rejected. Certificate-pinned clients fail closed; trust/proxy environment variables are supplied for common Python, Node, Bun, curl and TLS clients, but an application may require its own proxy/CA configuration.

Unfinished uploads remain revocable after an early upstream response, including a rejected protocol-upgrade response. Registered exchanges are retired only after both messages finish, or after an error or disconnect.

**In-container loopback is allowed:** `localhost`, `*.localhost`, `127.0.0.1` and `[::1]` bypass the website proxy. With `--network=none`, these addresses reach only the sandbox's own processes, including OMBP's browser helper and the dedicated SonarQube relay. **Host loopback remains blocked.** Exposing the host's `127.0.0.1` would expose services that may trust local clients: this launcher's control UI, SonarQube on 9000, OMP's RPC listener, CUPS on 631, Podman services and local AI servers such as Ollama on 11434, LM Studio on 1234 and llama.cpp on 8080. Depending on each service's authentication and capabilities, that can expose credentials or permit host-side actions and sandbox escape. The SonarQube relay is the only explicit loopback-service exception.

## Default tools and Code quality

Chrome trusts the whitelist gateway's CA through a launcher-generated private NSS database, not a host trust-store change or a certificate-verification bypass. The database is recreated at each container start and mounted at the private home's `.pki`; it is writable because Chrome requires read-write NSS initialization. Custom NSS imports there do not persist. The sandbox's existing `.pki` files remain underneath the mount and are not overwritten. Host trust tools never reopen this database after exposing it to the sandbox.

The private container hostname stays stable across restarts, and each OMBP terminal's browser profile lives in the persistent private home, so logins (for example GitHub) are remembered across browser and launcher restarts. Engine archive staging uses the sandbox's private temporary filesystem, including clients that discard `TMPDIR`.

**Browser window:** OMBP's Browser button looks for google-chrome-stable, google-chrome, chromium, chromium-browser, chrome and brave-browser. In the sandbox all six names belong to the default **Sandbox browser window** requirement, so none show up as denied. Each name opens the real sandbox browser (your Google Chrome, or launcher Chromium when Chrome is absent; `brave-browser` is not real Brave) in a **second nested desktop window** that the launcher opens on first use and closes when the last browser exits, so the browser no longer covers OMBP. That display maximizes instead of fullscreening (a small pinned Weston kiosk-shell patch), so Chrome keeps its tabs and address bar. Revoking **Isolated desktop socket** also removes that window; browsers then share OMBP's display.

Startup speed: Nix-store files are immutable, so the permission filesystem lets the kernel keep their page cache between opens (each open is still authorized), and the sandbox reads the browser package once in the background after OMBP starts. Measured here: first Browser click about 6 s (was about 15 s), reopening about 5 s.

The following removable program-bundle descriptions apply to inspection mode; installed mode uses the modal selections and mandatory core described above.

Removable requirements include your installed Nix **Google Chrome** (launcher Chromium if Chrome is absent), **Obsidian** when installed, **gh**, **ssh**, **python**, **dbus-daemon**, **sh/bash** (the sandbox shell), the **Podman client**, and **Basic tools**: gdbus/gio/gsettings, notify-send, zenity (OMBP's file and folder pickers fall back to it because the sandbox has no desktop portal), ps/pgrep/pkill/kill/free/top (procps), killall/fuser (psmisc), util-linux (column, flock, setsid, …), find/xargs, grep, sed, awk, diff/cmp, patch, file, which, less, tar/gzip/bzip2/xz/zstd/zip/unzip, curl, wget, jq, rg, fd, rsync, tree, make, bc, strace, lsof and clear/tput. They only run inside the container: host files still pass the folder gateway and network still passes the website gateway, and with all capabilities dropped and no-new-privileges, tools such as mount or su cannot gain privileges. A shell adds no power the sandbox lacks: OMBP, the Terminal and the AI already run arbitrary code inside the container, which is what the container boundary is for. The sandbox has its own private session bus and a stable random `/etc/machine-id` (not the host's); it has no notification service, so `notify-send` reports that none is available instead of reaching your desktop. No host browser profile or login is imported. Revoke/Restore works like the other program bundles. `secret-tool` comes from OMBP's existing libsecret bundle and uses the private keyring. The Terminal uses the sandbox's Bash. A denied `docker` lookup is OMBP checking which container engines exist when Code quality opens; Code quality uses Podman through the filtered **SonarQube engine** connection, so Docker is not needed.

The Basic tools bundle includes **Marksman** (`marksman server`) for Markdown; **Emmet Language Server** (`emmet-language-server --stdio`) for HTML/CSS abbreviation completion; **TypeScript Language Server** and **TypeScript** (`typescript-language-server --stdio`, `tsc`); **VS Code's extracted language servers**, including `vscode-html-language-server --stdio`; the Nix language servers **nil** and **nixd**; and **Eclipse JDT Language Server** (`jdtls`) with its Java runtime. These provide language-specific assistance for OMP/OMSP rather than being required for basic file editing. OLS, Vim Language Server and Docker language servers are not bundled.

The **SonarQube engine** folder requirement exposes a filtered API, never the unrestricted host Podman socket. It permits OMBP's `omp-q-*` objects with the launcher's ownership label, the three pinned Community/PostgreSQL/CLI-scanner images, named owned volumes, internal networks and a single host-loopback SonarQube publication. Host bind mounts, host namespaces, privileged containers and other engine routes are blocked. Community + CLI scanning is supported; Maven/Gradle/.NET/custom-image modes are not allowed by this image filter.

SonarQube and PostgreSQL run in separate host rootless containers: a SonarQube crash does not stop OMBP. First launch seeds a Community configuration using connection `ombp-engine`, port **9100**, and manual Start; existing Code quality settings are left untouched. The user's host analyzer on port 9000 is not changed. The sandbox forwards only its configured SonarQube loopback port through a dedicated socket. Revoke the engine requirement and restart to disconnect it; stopping the launcher does not delete SonarQube's persistent data.

No additional website permissions are seeded for these tools. `gh` needs the appropriate GitHub origins and a login inside the sandbox. The SSH tools are available locally, but arbitrary outbound SSH is not supported by the HTTP(S)-only gateway.


## Credentials

For external credential injection, create an entry in your **host** Secret Service keyring:

```sh
secret-tool store --label='OMBP API' application ombp-start origin https://api.example.com
```

At the secure prompt enter the complete `Authorization` value, such as `Bearer …`; do not put the value in shell arguments or this repository. Add that base URL in the UI with **Attach Authorization from host keyring** checked. Injection requires **HTTPS** and the origin must match exactly, including any non-default port. To change the choice, remove and re-add the website.

The host gateway looks up and injects the header only for that approved origin, after validating its destination. The container gets neither the stored value nor access to the host keyring socket. Missing/locked/malformed credentials fail closed with a generic error. This does **not** stop the AI exercising that origin's approved privileges, or a trusted upstream deliberately echoing credentials in its response. Do not enable injection for an untrusted service.

If an Authorization-based client refuses to make a request without a local API key, configure a non-secret placeholder in its private container configuration; the gateway replaces the outgoing Authorization header. This does not support providers that require an `x-api-key` header or an interactive OAuth flow instead.

Alternatively, the container has its own persistent home and private Secret Service daemon. Its keyring is initialized and unlocked automatically using a random per-state key stored on the host in owner-only `keyring-unlock.json`. That key is delivered once over stdin, never mounted or put in arguments/environment; no host login password is imported. Back up the unlock key and private home together. An existing non-empty private home without its key is refused rather than silently rekeyed. By default OMP uses your shared host `~/.omp` (see Folder access); if you revoke that, configure OMBP/models in the private home instead. Credentials given to OMP, stored in a shared `~/.omp`, or retrieved from this private keyring can be seen by the AI. Injection does not provide arbitrary custom-header/OAuth flows or populate OMBP's model configuration.

## Desktop and GPUs

Desktop output uses a host-side **nested Weston compositor**, rendered with Pixman. Only its client socket is mounted; the host display socket, clipboard, screen/input protocols, session bus and portals are not exposed. OMBP appears in a separate desktop window. The helper watches the launcher process lifetime and exits if it dies. This intentionally does not provide host-desktop automation, which could escape the filesystem/network boundary by controlling a host terminal or browser.

The internal display resizes live to the space assigned to that desktop window when it is tiled, resized or maximized. The launcher uses Weston's mature kiosk shell with three small pinned Weston patches: the nested window has no Weston title bar or border, so OMBP gets the entire host allocation; a fullscreen kiosk window larger than its allocation is clipped instead of disconnecting the client; and each client's outgoing event queue may grow to 16 MiB instead of libwayland's 4 KiB default, so moving the mouse over OMBP while it is briefly busy (for example during startup) no longer disconnects it with `Data too big for buffer` and `Process exited 1`. Your host compositor's own border is unchanged. Move the window with the host compositor's normal shortcuts. Closing it (for example Super+Q) stops OMBP and its container like the launcher's Stop button, without reporting a crash. Allocations below OMBP's 720×480 minimum keep running but are clipped, not shrunk or downscaled. Native popups and focus handling remain Weston's kiosk implementation. No container restart or OMBP patch is needed.

**AMD:** exposes `/dev/dri/renderD*` and, when available, `/dev/kfd`, retaining supplementary device groups and supplying Nix Mesa drivers. Requires functioning host drivers and device permissions. This does not install ROCm or application-specific compute libraries. The AMD render/KFD devices and real OMBP startup are exercised on this machine; this is not a GPU workload/ROCm compatibility certification.

**NVIDIA:** requests Podman's `nvidia.com/gpu=all` CDI device. Requires a working host NVIDIA driver/toolkit and generated CDI configuration. The launcher does not generate or modify those settings. No NVIDIA hardware was available for testing.

The container runs as your non-root UID/GID, explicitly mapped to the launcher user's host UID/GID; container UID/GID 0 are unmapped. This lets crun set up FUSE mounts as their owner without globally enabling `allow_other`. All capabilities are dropped and no-new-privileges is set. The root filesystem is read-only, with a private writable home and temporary directories. No unrestricted host Podman/Docker socket, host PID/network/IPC namespace, session bus or keyring socket is shared.

Chrome retains its own user-namespace sandbox; no `--no-sandbox` wrapper is used. Podman's default seccomp profile blocked Chrome's zygote on this machine, so the outer container uses **`seccomp=unconfined`** while retaining dropped capabilities, no-new-privileges, private namespaces and the filesystem/network gateways. This removes the outer syscall filter and increases kernel attack surface. Nix Chrome/Electron wrappers are told to use the nested Wayland display, not a host X server.

## Security limits

- Rootless containers share the host kernel; they are not VMs. GPU devices and the compositor/proxy/filesystem gateways add attack surface. Do not treat this as protection against a kernel or driver exploit.
- Readable files may be sent to any approved website. Writable grants permit deletion and modification; keep backups. Broad folder grants can expose credentials outside the launcher's protected state.
- Host symlinks, hardlinked regular files, special files, ownership changes and nested cross-device mounts are blocked. A nested filesystem needs its own grant. Creating host symlinks/hardlinks is unsupported. Some build workflows therefore need a project in the container's own home instead of a writable host mount.
- Revocation blocks subsequent host-file IO and network flows in both container modes, and new program access in inspection mode. Installed package changes apply on the next Start. Neither mode can retract bytes already read, copied files, cached immutable content, loaded code, or data already sent. Stop the container when you need to retire already-running programs.
- Gateway-observed history cannot identify arbitrary denied OS calls. Exhaustive per-tool intent/history, native OMBP credential-provider integration or host-desktop control would require a separate OMBP/broker design; none is silently patched into OMBP here.
- Only local rootless Linux Podman and Nix-installed programs are supported. No remote Podman machine, Windows/macOS port, distro-binary dependency importer, automatic host configuration migration or host-browser login sharing is included.
