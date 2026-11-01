"""Host-only HTTP/TLS policy gateway; the relay CLI needs only Python's stdlib.

Mount socket_path.parent and ca_path, never state_dir (which contains the CA key).
The container must have --network=none and no other host sockets. Certificate-
pinned applications fail closed; arbitrary TCP, WebSockets and HTTP upgrades are
not supported. Optional host keyring credentials are scoped to selected origins.
"""

import asyncio
import contextlib
import ipaddress
import os
from pathlib import Path
import socket
import sys
import threading
from urllib.parse import urlsplit

_INVALID_ORIGIN = "<invalid>"


def origin(value):
    if any(ord(character) <= 32 or ord(character) == 127 for character in value):
        raise ValueError("Whitespace/control characters are not allowed in origins")
    parsed = urlsplit(value)
    if (parsed.scheme not in ("http", "https") or not parsed.hostname
            or parsed.username is not None or parsed.password is not None
            or parsed.path not in ("", "/") or parsed.query or parsed.fragment):
        raise ValueError("Expected an HTTP(S) origin")
    host = parsed.hostname.encode("idna").decode("ascii").lower().rstrip(".")
    if not host or any(c in host for c in " /\\%@?#\t\r\n"):
        raise ValueError("Invalid origin hostname")
    port = parsed.port
    if port is None:
        port = 443 if parsed.scheme == "https" else 80
    if not 1 <= port <= 65535:
        raise ValueError("Invalid origin port")
    authority = f"[{host}]" if ":" in host else host
    if port != (443 if parsed.scheme == "https" else 80):
        authority += f":{port}"
    return f"{parsed.scheme}://{authority}"


def _origin(scheme, host, port):
    host = f"[{host}]" if ":" in host and not host.startswith("[") else host
    return origin(f"{scheme}://{host}:{port}")


def _public_address(value):
    address = ipaddress.ip_address(value)
    if not address.is_global or address.is_multicast or address.is_reserved:
        return False
    if isinstance(address, ipaddress.IPv6Address):
        # Translation/transition addresses can embed an otherwise denied IPv4 IP.
        if address.ipv4_mapped or address.sixtofour or address.teredo:
            return False
        if address in ipaddress.ip_network("64:ff9b::/96"):
            return False
    return True


async def _resolve_public(host, port):
    answers = await asyncio.get_running_loop().getaddrinfo(
        host, port, type=socket.SOCK_STREAM)
    addresses = [answer[4][0] for answer in answers]
    if not addresses or not all(_public_address(address) for address in addresses):
        raise ValueError("Destination resolves to a non-public address")
    return addresses[0]


async def _lookup_authorization(target):
    environment = {name: os.environ[name] for name in
                   ("PATH", "HOME", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS")
                   if name in os.environ}
    environment["LANG"] = "C.UTF-8"
    process = None
    try:
        async with asyncio.timeout(5):
            process = await asyncio.create_subprocess_exec(
                "secret-tool", "lookup", "application", "ombp-start", "origin", target,
                env=environment, stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL, limit=16386)
            try:
                await process.stdout.readexactly(16386)
            except asyncio.IncompleteReadError as result:
                output = result.partial
            else:
                raise ValueError("Invalid keyring credential")
            if await process.wait():
                raise ValueError("Keyring lookup failed")
            value = output.removesuffix(b"\n").decode("utf-8")
            if (not value or len(value) > 16384
                    or any(ord(character) < 32 or ord(character) == 127 for character in value)):
                raise ValueError("Invalid keyring credential")
            value.encode("latin-1")
            return value
    finally:
        if process and process.returncode is None:
            with contextlib.suppress(ProcessLookupError):
                process.kill()
            await process.wait()


class _Guard:
    def __init__(self, policy):
        self.policy = policy
        self.tunnels = {}
        self.authorized = {}
        self.bad_tls = set()
        self.active = {}
        self._watcher = None

    def running(self):
        self._watcher = asyncio.create_task(self._watch())

    async def _watch(self):
        while True:
            await asyncio.sleep(0.25)
            self._revoke()

    def _revoke(self):
        revoked = set()
        denials = []
        for key, (flow, target) in self.active.copy().items():
            reason = self._allowed(target)
            if reason:
                flow.kill()
                revoked.add(flow.client_conn.id)
                self.active.pop(key, None)
                denials.append((target, reason))
        if revoked:
            from mitmproxy import ctx
            connections = ctx.master.addons.get("proxyserver").connections
            for client_id in revoked:
                handler = connections.get(client_id)
                if handler:
                    # kill() marks the flow; handler closure also cancels pending connects.
                    for connection in tuple(handler.transports):
                        handler.close_connection(connection)
        for target, reason in denials:
            self.policy.deny("website", target, reason)

    def done(self):
        if self._watcher:
            self._watcher.cancel()

    def next_layer(self, nextlayer):
        from mitmproxy.proxy.layers import dns, http, tcp, udp
        if isinstance(nextlayer.layer, (dns.DNSLayer, tcp.TCPLayer, udp.UDPLayer)):
            nextlayer.layer = http.HttpLayer(nextlayer.context, http.HTTPMode.transparent)

    def _allowed(self, target):
        snapshot = self.policy.snapshot()
        if not snapshot.get("internet", False):
            return "Internet is disabled"
        for site in snapshot.get("websites", []):
            try:
                if origin(site) == target:
                    return None
            except ValueError:
                continue
        return "Origin is not allowed"

    def _deny(self, flow, target, reason):
        from mitmproxy import http
        flow.request.stream = False
        flow.response = http.Response.make(
            403, b"Website access denied by launcher policy.\n",
            {"Content-Type": "text/plain", "Connection": "close"})
        self.policy.deny("website", target, reason)

    def http_connect(self, flow):
        self.authorized.pop(flow.client_conn.id, None)
        try:
            target = _origin("https", flow.request.host, flow.request.port)
            reason = self._allowed(target)
            if flow.client_conn.id in self.tunnels:
                reason = "Nested CONNECT is not allowed"
        except ValueError:
            target, reason = _INVALID_ORIGIN, "Invalid CONNECT destination"
        if reason:
            self._deny(flow, target, reason)
        else:
            self.tunnels[flow.client_conn.id] = target

    def tls_clienthello(self, data):
        client = data.context.client
        expected = self.tunnels.get(client.id)
        try:
            host = urlsplit(expected).hostname if expected else None
            sni = data.client_hello.sni
            if sni:
                sni = sni.encode("idna").decode("ascii").lower().rstrip(".")
            elif host:
                ipaddress.ip_address(host)  # IP literals legitimately omit SNI.
                sni = host
            if not expected or sni != host:
                raise ValueError("TLS SNI must match CONNECT destination")
        except ValueError:
            self.bad_tls.add(client.id)
            self.policy.deny("website", expected or _INVALID_ORIGIN,
                             "TLS SNI does not match CONNECT destination")
        data.establish_server_tls_first = False

    def tls_start_client(self, data):
        if data.context.client.id in self.bad_tls:
            data.ssl_conn = None

    def _check_request(self, flow):
        request = flow.request
        try:
            target = _origin(request.scheme, request.host, request.port)
            reason = self._allowed(target)
            hosts = request.headers.get_all("host")
            if len(hosts) != 1 or origin(f"{request.scheme}://{hosts[0]}") != target:
                reason = "Host header does not match request origin"
            if request.authority and origin(f"{request.scheme}://{request.authority}") != target:
                reason = "Authority does not match request origin"
            tunnel = self.tunnels.get(flow.client_conn.id)
            if request.scheme == "https":
                if (not flow.client_conn.tls_established or tunnel != target
                        or flow.client_conn.id in self.bad_tls):
                    reason = "HTTPS origin does not match verified TLS tunnel"
            elif tunnel or flow.client_conn.tls_established:
                reason = "Plain HTTP inside TLS/CONNECT is not allowed"
            if request.method == "CONNECT" or "upgrade" in request.headers:
                reason = "Raw tunnels and protocol upgrades are not allowed"
        except ValueError:
            target, reason = _INVALID_ORIGIN, "Invalid request origin"
        if reason:
            self._deny(flow, target, reason)
            return
        self.authorized.setdefault(flow.client_conn.id, set()).add(target)
        self.active[id(flow)] = (flow, target)
        # Proxy credentials must never accidentally reach an upstream website.
        request.headers.pop("proxy-authorization", None)
        request.stream = True

    async def requestheaders(self, flow):
        from mitmproxy import http
        self._check_request(flow)
        if flow.response is not None:
            return
        target = _origin(flow.request.scheme, flow.request.host, flow.request.port)
        if target not in self.policy.snapshot().get("credential_origins", []):
            return
        try:
            await _resolve_public(flow.request.host, flow.request.port)
        except (ValueError, OSError):
            self._deny(flow, target, "Destination is not publicly reachable")
            return
        reason = self._allowed(target)
        if reason:
            self._deny(flow, target, reason)
            return
        if (id(flow) not in self.active
                or target not in self.policy.snapshot().get("credential_origins", [])):
            return
        try:
            value = await _lookup_authorization(target)
        except (ValueError, OSError):
            self.active.pop(id(flow), None)
            flow.request.stream = False
            flow.response = http.Response.make(
                502, b"Host credential unavailable.\n", {"Content-Type": "text/plain"})
            return
        reason = self._allowed(target)
        if reason:
            self._deny(flow, target, reason)
        elif (id(flow) in self.active
              and target in self.policy.snapshot().get("credential_origins", [])):
            flow.request.headers["Authorization"] = value

    async def server_connect(self, data):
        host, port = data.server.address
        target = None
        try:
            for scheme in ("http", "https"):
                candidate = _origin(scheme, host, port)
                if candidate in self.authorized.get(data.client.id, ()):
                    target = candidate
                    break
            if target is None:
                raise ValueError("No authorized HTTP request for this connection")
            reason = self._allowed(target)
            if reason:
                raise ValueError(reason)
            address = await _resolve_public(host, port)
            # Pin the resolved IP: mitmproxy must not perform a second DNS lookup.
            data.server.address = (address, port)
            data.server.sni = host
        except (ValueError, OSError) as error:
            reason = str(error) if isinstance(error, ValueError) else "DNS lookup failed"
            data.server.error = "Website destination denied"
            self.policy.deny("website", target or _INVALID_ORIGIN, reason)

    def responseheaders(self, flow):
        if flow.response.status_code == 101:
            self._deny(flow, _origin(flow.request.scheme, flow.request.host,
                                     flow.request.port), "Protocol upgrades are not allowed")
        else:
            flow.response.stream = True

    def request(self, flow):
        if flow.response is not None and flow.response.timestamp_end is not None:
            self.active.pop(id(flow), None)

    def response(self, flow):
        # An early final response does not finish a still-streaming upload.
        if flow.request.timestamp_end is not None:
            self.active.pop(id(flow), None)

    def error(self, flow):
        self.active.pop(id(flow), None)

    def client_disconnected(self, client):
        self.tunnels.pop(client.id, None)
        self.authorized.pop(client.id, None)
        self.bad_tls.discard(client.id)
        for key, (flow, _) in self.active.copy().items():
            if flow.client_conn.id == client.id:
                self.active.pop(key, None)


async def _copy(reader, writer):
    try:
        while data := await reader.read(65536):
            writer.write(data)
            await writer.drain()
    finally:
        if writer.can_write_eof():
            with contextlib.suppress(OSError):
                writer.write_eof()


async def _forward(reader, writer, connect):
    upstream = None
    try:
        remote_reader, upstream = await connect()
        await asyncio.gather(_copy(reader, upstream), _copy(remote_reader, writer))
    except (OSError, asyncio.CancelledError):
        pass
    finally:
        for stream in (writer, upstream):
            if stream:
                stream.close()
                with contextlib.suppress(OSError):
                    await stream.wait_closed()


async def relay(socket_path):
    """Container-only loopback TCP endpoint forwarding bytes to the Unix proxy."""
    server = await asyncio.start_server(
        lambda r, w: _forward(r, w, lambda: asyncio.open_unix_connection(socket_path)),
        "127.0.0.1", 18080)
    async with server:
        await server.serve_forever()


class NetworkGateway:
    """One gateway per launcher process (mitmproxy itself has process globals)."""

    def __init__(self, policy, state_dir):
        self.policy = policy
        self.state_dir = Path(state_dir)
        self.socket_path = self.state_dir / "socket" / "proxy.sock"
        self.ca_path = self.state_dir / "ca" / "mitmproxy-ca-cert.pem"
        self._thread = None
        self._master = None
        self._ready = threading.Event()
        self._error = None
        self._unix_server = None

    def start(self):
        if self._thread is not None:
            raise RuntimeError("Network gateway is already started")
        self.state_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.state_dir.chmod(0o700)
        self.socket_path.parent.mkdir(exist_ok=True, mode=0o700)
        self.socket_path.parent.chmod(0o700)
        self.ca_path.parent.mkdir(exist_ok=True, mode=0o700)
        self.ca_path.parent.chmod(0o700)
        if self.socket_path.exists():
            raise RuntimeError("Proxy socket already exists; another launcher may be running")
        self._ready.clear()
        self._error = None
        self._thread = threading.Thread(target=self._run, name="website-gateway", daemon=True)
        self._thread.start()
        if not self._ready.wait(30):
            self.stop()
            raise RuntimeError("Network gateway startup timed out")
        if self._error:
            self._thread.join()
            self._thread = None
            raise RuntimeError("Network gateway could not start") from self._error
        return self

    def _run(self):
        try:
            asyncio.run(self._serve())
        except Exception as error:
            self._error = error
            self._ready.set()
        finally:
            self.socket_path.unlink(missing_ok=True)

    async def _serve(self):
        from mitmproxy import master, options
        from mitmproxy.addons import core, next_layer, proxyserver, tlsconfig
        opts = options.Options(confdir=str(self.ca_path.parent),
                               listen_host="127.0.0.1", listen_port=0,
                               http2=False, rawtcp=False, upstream_cert=False)
        self._master = master.Master(opts, with_termlog=False)
        self._master.addons.add(core.Core(), proxyserver.Proxyserver(),
                                next_layer.NextLayer(), tlsconfig.TlsConfig(),
                                _Guard(self.policy), self)
        self._master.options.update(connection_strategy="lazy")
        try:
            await self._master.run()
        finally:
            if self._unix_server:
                self._unix_server.close()
                await self._unix_server.wait_closed()
            await self._master.addons.get("proxyserver").servers.update([])

    async def running(self):
        servers = self._master.addons.get("proxyserver").servers
        addresses = [address for server in servers for address in server.listen_addrs]
        if not addresses or not self.ca_path.is_file():
            self._error = RuntimeError("Proxy listener or CA did not initialize")
            self._ready.set()
            self._master.shutdown()
            return
        port = addresses[0][1]
        try:
            self._unix_server = await asyncio.start_unix_server(
                lambda r, w: _forward(r, w, lambda: asyncio.open_connection("127.0.0.1", port)),
                path=str(self.socket_path))
            self.socket_path.chmod(0o600)
        except OSError as error:
            self._error = error
            self._master.shutdown()
        finally:
            self._ready.set()

    def done(self):
        if self._unix_server:
            self._unix_server.close()

    def stop(self):
        if self._thread is None:
            return
        if self._master:
            self._master.shutdown()
        self._thread.join(timeout=10)
        if self._thread.is_alive():
            raise RuntimeError("Network gateway did not stop")
        self._thread = None
        self._master = None


if __name__ == "__main__":
    if len(sys.argv) != 3 or sys.argv[1] != "relay":
        raise SystemExit("Usage: python network.py relay /network/proxy.sock")
    try:
        asyncio.run(relay(sys.argv[2]))
    except KeyboardInterrupt:
        pass
