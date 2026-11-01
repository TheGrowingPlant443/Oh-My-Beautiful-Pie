# OMBP Start app

An independent Linux launcher. Nothing in the parent OMBP application, configuration or build is changed. Its local browser UI remains outside the sandbox; OMBP, its private session bus/keyring, and every OMP child run inside **one rootless Podman container**.

## Run

From the repository root:

```sh
cd 'start app'
nix run path:.
```

The launcher opens its four-column UI in your browser. If automatic browser opening is unavailable, open the loopback URL printed in the terminal. Keep that URL private: its fragment authenticates permission changes. A browser reload retains authentication in that tab's session storage.

The control UI intentionally uses authenticated HTTP on loopback, with Host/Origin checks and no cross-origin access. It installs no host certificates. Do not expose it through a port forward or reverse proxy.

Choose an installed Nix `omp-manager` executable, or use the detected one. This app does not build, install or patch OMBP. Choose **Podman · isolated** or **Normal · unrestricted host access**, then Start. Normal mode runs the selected executable directly with your normal host environment; **none of the whitelist controls apply in normal mode**.

Container mode sets OMBP's existing `OMP_BIN` hook to this launcher's CLI entrypoint, which runs `src/cli.ts` from OMBP's own `OMP_SDK_DIR` using its bundled Bun. This reuses the same launch pattern as the parent flake; it does not install a second OMP version or run any OMP process on the host. The selected OMBP package must expose its bundled SDK and Bun.

Prerequisites: local rootless Podman, `/dev/fuse`, a host setuid `fusermount`/`fusermount3`, and Nix. The launcher supplies its Python, FUSE library, proxy, nested compositor and session tools through its own pinned flake. It does not change `/etc/fuse.conf`, install drivers, expose an engine socket, or use another package manager. A graphical launch requires an existing Wayland session; raw host X11 is deliberately not supported.

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

## Four columns

1. **Folder access:** add a vault, Downloads, Desktop or another directory by absolute host path. It appears at `/host` followed by that path; for example `/home/me/Notes` becomes `/host/home/me/Notes`. Grant read-only or explicitly allow changes/deletion. Obsidian vault access shares its files; it does not control the host Obsidian process. Folder grants and revocations apply live.
2. **Programs & desktop:** add an installed Nix executable, including a profile symlink. Its executable appears in `/allowed-programs` on PATH and its Nix dependency closure becomes readable. Add/remove works live. Duplicate executable names are rejected. Programs already required by OMBP and its interpreters remain available: this is a **host-resource whitelist, not a ban on arbitrary code inside the container**. Display/GPU choices take effect on the next launch.
3. **Websites & internet:** internet starts disabled. When enabled, only exact allowed HTTP(S) origins work: scheme + hostname + port. Input subpaths are reduced to the base origin and all subpaths are allowed; subdomains require their own entries. Redirects are checked again. Add/remove and the internet switch apply live; active proxy streams are revoked on the gateway's 250ms policy check.
4. **Denied attempts:** gateway-observed denied folders, Nix programs and websites, with time, reason, count and a confirmation-protected **+** to allow them. Folder quick-add is read-only. Program basenames resolve on the launcher's PATH, still requiring a Nix-installed executable; unknown targets require selecting its installed path. The latest 500 distinct resources are retained. This is not a system-wide syscall audit: direct socket attempts and missing programs outside the gateways have no reliable target history here.

If denied-history storage fails, permission enforcement continues and current events remain in memory. The history column shows a warning until a later save succeeds. Failed permission saves remain errors and do not change the effective permissions.

The container has no external network interface. A loopback proxy relay connects to a host Unix socket. Clients that ignore proxy settings cannot bypass the whitelist; their requests fail. HTTPS is terminated and re-encrypted by mitmproxy with a launcher-local CA, and upstream certificates remain verified. CONNECT destination, SNI, request authority and Host must agree. DNS is checked for non-public addresses and pinned before connection, blocking private networks and rebinding. HTTP upgrades, WebSockets and raw tunnels are rejected. Certificate-pinned clients fail closed; trust/proxy environment variables are supplied for common Python, Node, Bun, curl and TLS clients, but an application may require its own proxy/CA configuration.

Unfinished uploads remain revocable after an early upstream response, including a rejected protocol-upgrade response. Registered exchanges are retired only after both messages finish, or after an error or disconnect.

## Credentials

For external credential injection, create an entry in your **host** Secret Service keyring:

```sh
secret-tool store --label='OMBP API' application ombp-start origin https://api.example.com
```

At the secure prompt enter the complete `Authorization` value, such as `Bearer …`; do not put the value in shell arguments or this repository. Add that base URL in the UI with **Attach Authorization from host keyring** checked. Injection requires **HTTPS** and the origin must match exactly, including any non-default port. To change the choice, remove and re-add the website.

The host gateway looks up and injects the header only for that approved origin, after validating its destination. The container gets neither the stored value nor access to the host keyring socket. Missing/locked/malformed credentials fail closed with a generic error. This does **not** stop the AI exercising that origin's approved privileges, or a trusted upstream deliberately echoing credentials in its response. Do not enable injection for an untrusted service.

If an Authorization-based client refuses to make a request without a local API key, configure a non-secret placeholder in its private container configuration; the gateway replaces the outgoing Authorization header. This does not support providers that require an `x-api-key` header or an interactive OAuth flow instead.

Alternatively, the container has its own persistent home and private Secret Service daemon. Its keyring is initialized and unlocked automatically using a random per-state key stored on the host in owner-only `keyring-unlock.json`. That key is delivered once over stdin, never mounted or put in arguments/environment; no host login password is imported. Back up the unlock key and private home together. An existing non-empty private home without its key is refused rather than silently rekeyed. Configure OMBP/models in that private home, not by mounting the host `.omp` directory. Credentials given to OMP or retrieved from this private keyring can be seen by the AI. Injection does not provide arbitrary custom-header/OAuth flows or populate OMBP's model configuration.

## Desktop and GPUs

Desktop output uses a host-side **nested Weston compositor**, rendered with Pixman. Only its client socket is mounted; the host display socket, clipboard, screen/input protocols, session bus and portals are not exposed. OMBP appears in a separate desktop window. The helper watches the launcher process lifetime and exits if it dies. This intentionally does not provide host-desktop automation, which could escape the filesystem/network boundary by controlling a host terminal or browser.

The internal display resizes live to the space assigned to that desktop window when it is tiled, resized or maximized. The launcher uses Weston's mature kiosk shell with two small pinned Weston patches: the nested window has no Weston title bar or border, so OMBP gets the entire host allocation; and a fullscreen kiosk window larger than its allocation is clipped instead of disconnecting the client. Your host compositor's own border is unchanged. Move or close the window with the host compositor's normal shortcuts, or stop it from the launcher. Allocations below OMBP's 720×480 minimum keep running but are clipped, not shrunk or downscaled. Native popups and focus handling remain Weston's kiosk implementation. No container restart or OMBP patch is needed.

**AMD:** exposes `/dev/dri/renderD*` and, when available, `/dev/kfd`, retaining supplementary device groups and supplying Nix Mesa drivers. Requires functioning host drivers and device permissions. This does not install ROCm or application-specific compute libraries. The AMD render/KFD devices and real OMBP startup are exercised on this machine; this is not a GPU workload/ROCm compatibility certification.

**NVIDIA:** requests Podman's `nvidia.com/gpu=all` CDI device. Requires a working host NVIDIA driver/toolkit and generated CDI configuration. The launcher does not generate or modify those settings. No NVIDIA hardware was available for testing.

The container's UID 0 maps to the launcher user's **unprivileged host UID**, with all capabilities dropped and no-new-privileges. This permits FUSE access without globally enabling `allow_other`; it is not host root. The root filesystem is read-only, with a private writable home and temporary directories. No host Podman/Docker socket, host PID/network/IPC namespace, session bus or keyring socket is shared.

## Security limits

- Rootless containers share the host kernel; they are not VMs. GPU devices and the compositor/proxy/filesystem gateways add attack surface. Do not treat this as protection against a kernel or driver exploit.
- Readable files may be sent to any approved website. Writable grants permit deletion and modification; keep backups. Broad folder grants can expose credentials outside the launcher's protected state.
- Host symlinks, hardlinked regular files, special files, ownership changes and nested cross-device mounts are blocked. A nested filesystem needs its own grant. Creating host symlinks/hardlinks is unsupported. Some build workflows therefore need a project in the container's own home instead of a writable host mount.
- Revocation blocks subsequent host-file IO, new program access and network flows. It cannot retract bytes already read, copied files, cached immutable Nix-store content, loaded code, or data already sent. Stop the container when you need to retire already-running programs.
- Gateway-observed history cannot identify arbitrary denied OS calls. Exhaustive per-tool intent/history, native OMBP credential-provider integration or host-desktop control would require a separate OMBP/broker design; none is silently patched into OMBP here.
- Only local rootless Linux Podman and Nix-installed programs are supported. No remote Podman machine, Windows/macOS port, distro-binary dependency importer, automatic host configuration migration or host-browser login sharing is included.
