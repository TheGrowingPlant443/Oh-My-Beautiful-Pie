"""Host-only HTTP/TLS policy gateway; the relay CLI needs only Python's stdlib.

Mount socket_path.parent and ca_path, never state_dir (which contains the CA key).
The container must have --network=none and no other host sockets. Certificate-
pinned applications fail closed; arbitrary TCP and non-WebSocket HTTP upgrades are
not supported (WebSockets to allowed origins are). Optional host keyring credentials are scoped to selected origins.
"""

import asyncio
import contextlib
from html.parser import HTMLParser
import ipaddress
import os
from pathlib import Path
import re
import socket
import sys
import threading
from urllib.parse import urljoin, urlsplit
import zlib

_INVALID_ORIGIN = "<invalid>"
_LEARN_LIMIT = 4 * 1024 * 1024
_LEARN_PER_SITE = 256
_CSS_URL = re.compile(r"""url\(\s*['"]?([^'")\s]+)|@import\s+['"]([^'"]+)""", re.IGNORECASE)
_LINK_RELS = {"stylesheet", "preload", "modulepreload", "icon", "apple-touch-icon", "mask-icon", "manifest", "prefetch"}
_SRC_TAGS = {"script", "img", "iframe", "source", "video", "audio", "embed", "track", "input"}


def _css_urls(text):
    return [first or second for first, second in _CSS_URL.findall(text)]


class _Resources(HTMLParser):
    """Subresource URLs a page loads; plain <a href> navigation links are deliberately ignored."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.urls = []
        self.base = None
        self._style = False

    def handle_starttag(self, tag, attrs):
        attrs = {key: value or "" for key, value in attrs}
        if tag == "base" and self.base is None and attrs.get("href"):
            self.base = attrs["href"]
        self._style = tag == "style"
        if tag in _SRC_TAGS and attrs.get("src"):
            self.urls.append(attrs["src"])
        if tag in ("img", "source") and attrs.get("srcset"):
            self.urls += [part.split()[0] for part in attrs["srcset"].split(",") if part.split()]
        if tag == "video" and attrs.get("poster"):
            self.urls.append(attrs["poster"])
        if tag == "link" and _LINK_RELS & set(attrs.get("rel", "").lower().split()) and attrs.get("href"):
            self.urls.append(attrs["href"])
        self.urls += _css_urls(attrs.get("style", ""))

    def handle_endtag(self, tag):
        self._style = False

    def handle_data(self, data):
        if self._style:
            self.urls += _css_urls(data)


def url_origin(url):
    """Canonical public-form origin of an absolute HTTP(S) URL, or None."""
    try:
        parsed = urlsplit(url.strip())
        if parsed.scheme not in ("http", "https") or not parsed.hostname:
            return None
        return _origin(parsed.scheme, parsed.hostname, parsed.port or (443 if parsed.scheme == "https" else 80))
    except ValueError:
        return None


def covers(site, target):
    """Whether site grants target: its host or (ignoring a leading www.) a subdomain, over http or https.

    A site with an explicit port only covers that scheme and port; otherwise targets must use a default port.
    """
    allowed, wanted = urlsplit(site), urlsplit(target)
    if not allowed.hostname or not wanted.hostname:
        return False
    if (allowed.port is not None and (allowed.scheme, allowed.port) != (wanted.scheme, wanted.port)) or (
            allowed.port is None and wanted.port is not None):
        return False
    host = allowed.hostname
    if wanted.hostname == host:
        return True
    with contextlib.suppress(ValueError):
        ipaddress.ip_address(host)
        return False  # Addresses have no subdomains.
    base = host.removeprefix("www.")
    return wanted.hostname == base or wanted.hostname.endswith("." + base)


def covering_site(target, snapshot):
    """The allowed website granting target: exact, or a subdomain unless the site is exact-only."""
    exact = set(snapshot.get("exact_hosts", []))
    return next((site for site in snapshot.get("websites", [])
                 if url_origin(site) == target or (site not in exact and covers(site, target))), None)


def _auto_match(text, host):
    """'google.com': host contains 'google' and ends in '.com'; text without a dot: host contains it."""
    name, dot, tld = text.rpartition(".")
    return name in host and host.endswith("." + tld) if dot and name else text in host


def auto_approved(target, snapshot):
    auto = snapshot.get("auto_approve") or {}
    host = urlsplit(target).hostname or ""
    return auto.get("enabled", False) and any(_auto_match(text, host) for text in auto.get("sites", []))


def allowed_by_policy(snapshot, target, references=None):
    """Static grants (no request headers): whitelist switch, sites, subdomains, dependencies, auto-approve."""
    if snapshot.get("allow_all_network", False):
        return True
    granted = {url_origin(site) for site in snapshot.get("websites", [])}
    for origins in snapshot.get("dependencies", {}).values():
        granted.update(origins)
    for site in snapshot.get("all_dependencies", []):
        granted |= (references or {}).get(site, set())
    return (target in granted or covering_site(target, snapshot) is not None
            or auto_approved(target, snapshot))


def _requester(target, request):
    """Site named by Origin/Referer (forgeable by any client) when it differs from target."""
    for name in ("origin", "referer"):
        found = url_origin(request.headers.get(name, ""))
        if found and found != target:
            return found
    return None


def _websocket(flow):
    """A 101 that both sides negotiated as a WebSocket (the only upgrade allowed)."""
    return all(message.headers.get("upgrade", "").strip().lower() == "websocket"
               for message in (flow.request, flow.response))


def _learnable(request):
    """Keep HTML/CSS bodies learnable: no bodyless 304s, and only encodings we can decode with a bound."""
    if request.headers.get("sec-fetch-dest") in ("document", "iframe", "style"):
        for name in ("if-none-match", "if-modified-since"):
            request.headers.pop(name, None)
        request.headers["accept-encoding"] = "gzip, deflate"


def _inflate(body, name):
    """One gzip/deflate layer, never producing more than _LEARN_LIMIT bytes."""
    # gzip: 47 auto-detects gzip/zlib headers; deflate is zlib-wrapped or, from some servers, raw.
    for bits in (47,) if name != "deflate" else (15, -15):
        decoder = zlib.decompressobj(bits)
        try:
            decoded = decoder.decompress(body, _LEARN_LIMIT)
        except zlib.error:
            continue
        if decoder.unconsumed_tail:
            raise ValueError("Decoded body exceeds learning limit")
        return decoded
    raise ValueError("Invalid compressed body")


def _decode(body, coding):
    for name in reversed([part.strip().lower() for part in coding.split(",") if part.strip()]):
        if name in ("gzip", "x-gzip", "deflate"):
            body = _inflate(body, name)
        elif name != "identity":
            raise ValueError("Unsupported content encoding")
    return body


def resource_origins(text, content_type, url):
    """Other origins that served HTML/CSS (fetched from url) loads as resources."""
    base = url
    if content_type == "text/css":
        urls = _css_urls(text)
    else:
        parser = _Resources()
        parser.feed(text)
        parser.close()
        urls = parser.urls
        if parser.base:
            # The browser resolves relative HTML resources against <base href>, not the page URL.
            base = urljoin(url, parser.base.strip())
    found = {url_origin(urljoin(base, item.strip())) for item in urls}
    return found - {None, url_origin(url)}


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
        # Origins that each approved site's own HTML/CSS loaded; in memory, relearned on page loads.
        self.references = {}
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
            reason = self._allowed(target, _requester(target, flow.request))
            if reason:
                flow.kill()
                revoked.add(flow.client_conn.id)
                self.active.pop(key, None)
                denials.append((target, reason, self._site(target, flow.request)))
        if revoked:
            from mitmproxy import ctx
            connections = ctx.master.addons.get("proxyserver").connections
            for client_id in revoked:
                handler = connections.get(client_id)
                if handler:
                    # kill() marks the flow; handler closure also cancels pending connects.
                    for connection in tuple(handler.transports):
                        handler.close_connection(connection)
        for target, reason, site in denials:
            self.policy.deny("website", target, reason, site)

    def done(self):
        if self._watcher:
            self._watcher.cancel()

    def next_layer(self, nextlayer):
        from mitmproxy.proxy.layers import dns, http, tcp, udp
        if isinstance(nextlayer.layer, (dns.DNSLayer, tcp.TCPLayer, udp.UDPLayer)):
            nextlayer.layer = http.HttpLayer(nextlayer.context, http.HTTPMode.transparent)

    def _allowed(self, target, requester=None):
        snapshot = self.policy.snapshot()
        if not snapshot.get("internet", False):
            return "Internet is disabled"
        # allow_all_network still leaves private/loopback addresses blocked at connect.
        if allowed_by_policy(snapshot, target, self.references):
            if covering_site(target, snapshot) is None and auto_approved(target, snapshot):
                # Show auto-approved origins in the whitelist; failing to list one must not block it.
                with contextlib.suppress(OSError, ValueError):
                    self.policy.remember_website(target)
            return None
        # Header mode trusts a forgeable Referer/Origin: the user opted into that risk per site.
        if requester and requester in snapshot.get("header_dependencies", []):
            return None
        return "Origin is not allowed"

    def _owner(self, target, snapshot):
        """The approved site whose dependency set grants target, if any."""
        site = covering_site(target, snapshot)
        if site:
            return site
        for site, origins in snapshot.get("dependencies", {}).items():
            if target in origins:
                return site
        return next((site for site in snapshot.get("all_dependencies", [])
                     if target in self.references.get(site, ())), None)

    def _site(self, target, request=None):
        """Which site a denied origin belongs to, for grouping only."""
        site = next((site for site, origins in self.references.items() if target in origins), None)
        return site or (_requester(target, request) if request is not None else None)

    def _deny(self, flow, target, reason):
        from mitmproxy import http
        flow.request.stream = False
        flow.response = http.Response.make(
            403, b"Website access denied by launcher policy.\n",
            {"Content-Type": "text/plain", "Connection": "close"})
        self.policy.deny("website", target, reason, self._site(target, flow.request))

    def http_connect(self, flow):
        self.authorized.pop(flow.client_conn.id, None)
        try:
            target = _origin("https", flow.request.host, flow.request.port)
            # Unapproved origins are denied per request after TLS, so the denial shows its requesting site.
            # Upstream connects only for authorized requests (server_connect).
            reason = None if self.policy.snapshot().get("internet", False) else "Internet is disabled"
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

    def tls_failed_client(self, data):
        target = self.tunnels.get(data.context.client.id)
        if target and self._allowed(target):
            self.policy.deny("website", target, "Client rejected the inspected TLS connection", self._site(target))

    def _check_request(self, flow):
        request = flow.request
        try:
            target = _origin(request.scheme, request.host, request.port)
            reason = self._allowed(target, _requester(target, request))
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
            upgrade = request.headers.get("upgrade", "").strip().lower()
            if request.method == "CONNECT" or (upgrade and upgrade != "websocket"):
                reason = "Raw tunnels and non-WebSocket upgrades are not allowed"
        except ValueError:
            target, reason = _INVALID_ORIGIN, "Invalid request origin"
        if reason:
            self._deny(flow, target, reason)
            return
        self.authorized.setdefault(flow.client_conn.id, {})[target] = _requester(target, request)
        self.active[id(flow)] = (flow, target)
        # Proxy credentials must never accidentally reach an upstream website.
        request.headers.pop("proxy-authorization", None)
        _learnable(request)
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
            reason = self._allowed(target, self.authorized[data.client.id][target])
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
        if flow.response.status_code == 101 and not _websocket(flow):
            self._deny(flow, _origin(flow.request.scheme, flow.request.host,
                                     flow.request.port), "Only WebSocket upgrades are allowed")
        elif flow.response.status_code != 101:
            flow.response.stream = self._learner(flow) or True

    def websocket_start(self, flow):
        # Keep the open WebSocket revocable like any streaming request (response() retired it).
        self.active[id(flow)] = (flow, _origin(flow.request.scheme, flow.request.host, flow.request.port))

    def websocket_message(self, flow):
        # mitmproxy keeps every message for the flow's lifetime; the layer forwards its own reference after this hook.
        flow.websocket.messages.clear()

    def websocket_end(self, flow):
        self.active.pop(id(flow), None)

    def _learner(self, flow):
        content_type = flow.response.headers.get("content-type", "").split(";")[0].strip().lower()
        if flow.response.status_code != 200 or content_type not in ("text/html", "text/css"):
            return None
        owner = self._owner(url_origin(flow.request.url), self.policy.snapshot())
        if owner is None:
            return None
        url, coding = flow.request.url, flow.response.headers.get("content-encoding", "")
        chunks, size = [], 0

        def stream(data):
            # Bytes pass through unchanged; a bounded copy is parsed after the last chunk.
            nonlocal size
            size += len(data)
            if data and size <= _LEARN_LIMIT:
                chunks.append(data)
            elif not data and size <= _LEARN_LIMIT:
                self._learn(owner, url, content_type, coding, b"".join(chunks))
            return data
        return stream

    def _learn(self, owner, url, content_type, coding, body):
        try:
            body = _decode(body, coding)
            found = resource_origins(body.decode("utf-8", "replace"), content_type, url)
        except ValueError:
            return
        learned = self.references.setdefault(owner, set())
        learned.update(sorted(found - learned)[:max(0, _LEARN_PER_SITE - len(learned))])

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


async def relay(socket_path, port=18080):
    """Container-only loopback TCP endpoint forwarding bytes to a launcher Unix socket."""
    server = await asyncio.start_server(
        lambda r, w: _forward(r, w, lambda: asyncio.open_unix_connection(socket_path)),
        "127.0.0.1", port)
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
    if len(sys.argv) not in (3, 4) or sys.argv[1] != "relay":
        raise SystemExit("Usage: python network.py relay SOCKET [PORT]")
    try:
        asyncio.run(relay(sys.argv[2], *map(int, sys.argv[3:])))
    except KeyboardInterrupt:
        pass
