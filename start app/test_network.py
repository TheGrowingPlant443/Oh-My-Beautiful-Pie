"""Local fixtures only: production never offers a private-address bypass."""

import asyncio
import base64
import contextlib
from datetime import datetime, timedelta, timezone
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import socket
import ssl
import sys
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import AsyncMock, patch

import network
from policy import Policy as PersistentPolicy


class Policy:
    def __init__(self, websites=(), internet=True):
        self.values = {"websites": list(websites), "internet": internet}
        self.denials = []
        self.remembered = []

    def snapshot(self):
        return dict(self.values)

    def deny(self, *entry):
        self.denials.append(entry)

    def remember_website(self, origin):
        self.remembered.append(origin)


def flow(url, host=None, tls=False):
    from mitmproxy import http
    parsed = network.urlsplit(url)
    request = http.Request.make("GET", url)
    request.headers["Host"] = host or parsed.netloc
    return SimpleNamespace(request=request, response=None,
                           client_conn=SimpleNamespace(id="client", tls_established=tls))


class OriginTests(unittest.TestCase):
    def test_canonical_origin_and_exact_match(self):
        self.assertEqual(network.origin("https://EXAMPLE.com:443/"), "https://example.com")
        self.assertEqual(network.origin("http://[2606:4700:4700::1111]:8080"),
                         "http://[2606:4700:4700::1111]:8080")
        policy = Policy(["https://www.example.com"])
        guard = network._Guard(policy)
        for target in ("https://www.example.com", "https://example.com", "https://api.example.com",
                       "http://example.com", "http://clients2.example.com"):
            self.assertIsNone(guard._allowed(target))
        for target in ("https://example.com:444", "http://api.example.com:8080",
                       "https://example.com.evil.test", "https://notexample.com"):
            self.assertIsNotNone(guard._allowed(target))
        policy.values["exact_hosts"] = ["https://www.example.com"]
        self.assertIsNone(guard._allowed("https://www.example.com"))
        self.assertIsNotNone(guard._allowed("https://api.example.com"))

    def test_auto_approve_matches_hosts_containing_text_only_while_enabled(self):
        policy = Policy([])
        policy.values["auto_approve"] = {"enabled": True, "sites": ["chatgpt", "anthropic.com"]}
        guard = network._Guard(policy)
        for target in ("https://chatgpt.com", "https://ab.chatgpt.com", "http://api.anthropic.com",
                       "https://anthropic.community.com"):
            self.assertIsNone(guard._allowed(target))
        policy.values["auto_approve"]["sites"].append("google.com")
        self.assertIsNone(guard._allowed("https://update.googleapis.com"))
        for target in ("https://openai.com", "https://anthropic.com.mirror.test", "https://www.google.de"):
            self.assertIsNotNone(guard._allowed(target))
        self.assertEqual(policy.remembered, ["https://chatgpt.com", "https://ab.chatgpt.com", "http://api.anthropic.com",
                                             "https://anthropic.community.com", "https://update.googleapis.com"])
        policy.values["auto_approve"]["enabled"] = False
        self.assertIsNotNone(guard._allowed("https://api.anthropic.com"))
        self.assertFalse(network.covers("https://10.0.0.1", "https://x.10.0.0.1"))

    def test_auto_approved_origin_is_listed_once_as_allowed_website(self):
        with tempfile.TemporaryDirectory() as directory:
            policy = PersistentPolicy(directory)
            guard = network._Guard(policy)
            before = policy.snapshot()["websites"]
            for _ in range(2):
                self.assertIsNone(guard._allowed("https://ab.chatgpt.com"))
            self.assertEqual(PersistentPolicy(directory).snapshot()["websites"],
                             sorted(before + ["https://ab.chatgpt.com"]))
            # Auto-approve switched off between the guard's check and remembering: nothing is granted.
            value = policy.snapshot()
            value["auto_approve"]["enabled"] = False
            policy.replace(value)
            policy.remember_website("https://cd.chatgpt.com")
            self.assertNotIn("https://cd.chatgpt.com", PersistentPolicy(directory).snapshot()["websites"])

    def test_allow_all_network_bypasses_whitelist_only_while_internet_is_on(self):
        policy = Policy([])
        guard = network._Guard(policy)
        self.assertIsNotNone(guard._allowed("https://anything.test"))
        policy.values["allow_all_network"] = True
        self.assertIsNone(guard._allowed("https://anything.test"))
        policy.values["internet"] = False
        self.assertIsNotNone(guard._allowed("https://anything.test"))

    def test_reject_non_origins(self):
        for value in ("file:///etc/passwd", "https://user:secret@example.com",
                      "https://example.com/path", "https://example.com?q=1",
                      "https://example.com#fragment", "http://example.com:0",
                      "http://example.com:65536", "http://exa%mple.com",
                      "https://exa\nmple.com"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                network.origin(value)

    def test_non_public_and_transition_addresses_denied(self):
        for value in ("127.0.0.1", "10.0.0.1", "192.168.1.1", "169.254.169.254",
                      "100.64.0.1", "0.0.0.0", "224.0.0.1", "255.255.255.255",
                      "::1", "fc00::1", "fe80::1", "ff02::1", "::ffff:127.0.0.1",
                      "64:ff9b::7f00:1", "2002:7f00:1::"):
            with self.subTest(value=value):
                self.assertFalse(network._public_address(value))
        self.assertTrue(network._public_address("93.184.216.34"))
        self.assertTrue(network._public_address("2606:4700:4700::1111"))

    def test_live_disable_and_host_credential_scope(self):
        policy = Policy(["http://example.test"])
        guard = network._Guard(policy)
        allowed = flow("http://example.test/path")
        allowed.request.headers["Authorization"] = "Bearer fixture-only"
        allowed.request.headers["Proxy-Authorization"] = "Basic fixture-only"
        guard._check_request(allowed)
        self.assertIsNone(allowed.response)
        self.assertNotIn("Proxy-Authorization", allowed.request.headers)
        self.assertEqual(allowed.request.headers["Authorization"], "Bearer fixture-only")
        mismatch = flow("http://example.test/path", "other.test")
        mismatch.request.headers["Authorization"] = "Bearer fixture-only"
        guard._check_request(mismatch)
        self.assertEqual(mismatch.response.status_code, 403)
        policy.values["internet"] = False
        denied = flow("http://example.test/other")
        guard._check_request(denied)
        self.assertEqual(denied.response.status_code, 403)
        self.assertEqual(policy.denials[-1][0:2], ("website", "http://example.test"))

    def test_completed_flow_cleanup_waits_for_both_messages(self):
        from mitmproxy import http
        for first in ("request", "response"):
            with self.subTest(first=first):
                guard = network._Guard(Policy(["http://example.test"]))
                selected = flow("http://example.test/upload")
                selected.request.timestamp_end = None
                guard._check_request(selected)
                selected.response = http.Response.make(200, b"")
                selected.response.timestamp_end = None
                getattr(selected, first).timestamp_end = 1
                getattr(guard, first)(selected)
                self.assertIn(id(selected), guard.active)
                second = "response" if first == "request" else "request"
                getattr(selected, second).timestamp_end = 2
                getattr(guard, second)(selected)
                self.assertNotIn(id(selected), guard.active)

    def test_tunnel_host_sni_and_upgrade_boundaries(self):
        policy = Policy(["https://example.test"])
        guard = network._Guard(policy)
        guard.tunnels["client"] = "https://example.test"
        wrong_host = flow("https://other.test/", tls=True)
        guard._check_request(wrong_host)
        self.assertEqual(wrong_host.response.status_code, 403)
        hello = SimpleNamespace(context=SimpleNamespace(client=SimpleNamespace(id="client")),
                                client_hello=SimpleNamespace(sni="other.test"))
        guard.tls_clienthello(hello)
        tls = SimpleNamespace(context=hello.context, ssl_conn=object())
        guard.tls_start_client(tls)
        self.assertIsNone(tls.ssl_conn)
        guard.bad_tls.clear()
        from mitmproxy import http
        websocket = flow("https://example.test/", tls=True)
        websocket.request.headers["Upgrade"] = "websocket"
        guard._check_request(websocket)
        self.assertIsNone(websocket.response)
        websocket.response = http.Response.make(101, b"", {"Upgrade": "websocket"})
        guard.responseheaders(websocket)
        self.assertEqual(websocket.response.status_code, 101)
        h2c = flow("https://example.test/", tls=True)
        h2c.request.headers["Upgrade"] = "h2c"
        guard._check_request(h2c)
        self.assertEqual(h2c.response.status_code, 403)
        unasked = flow("https://example.test/", tls=True)
        unasked.response = http.Response.make(101, b"", {"Upgrade": "websocket"})
        guard.responseheaders(unasked)
        self.assertEqual(unasked.response.status_code, 403)


class DependencyTests(unittest.TestCase):
    def test_served_resources_exclude_navigation_links(self):
        html = ('<link rel="stylesheet" href="https://assets.test/a.css"><script src="//cdn.test/s.js"></script>'
                '<img srcset="https://img.test/1x.png 1x, https://img2.test/2x.png 2x">'
                '<a href="https://elsewhere.test/">x</a><style>@import "https://fonts.test/f.css";</style>'
                '<img src="/local.png">')
        self.assertEqual(network.resource_origins(html, "text/html", "https://site.test/page"),
                         {"https://assets.test", "https://cdn.test", "https://img.test",
                          "https://img2.test", "https://fonts.test"})
        self.assertEqual(network.resource_origins("a{background:url('https://bg.test/x.png')}", "text/css",
                                                  "https://assets.test/a.css"), {"https://bg.test"})

    def test_base_href_resolves_relative_resources(self):
        html = '<base href="https://assets.test/static/"><script src="app.js"></script><link rel="stylesheet" href="s.css">'
        self.assertEqual(network.resource_origins(html, "text/html", "https://site.test/page"), {"https://assets.test"})

    def test_compressed_expansion_is_bounded_and_not_learned(self):
        import gzip
        import zlib
        policy = Policy(["https://site.test"])
        guard = network._Guard(policy)
        bomb = gzip.compress(b'<script src="https://cdn.test/s.js"></script>' + b" " * (network._LEARN_LIMIT + 1))
        self.assertLess(len(bomb), network._LEARN_LIMIT)
        guard._learn("https://site.test", "https://site.test/", "text/html", "gzip", bomb)
        self.assertEqual(guard.references.get("https://site.test", set()), set())
        raw = zlib.compressobj(wbits=-15)
        deflated = raw.compress(b'<img src="https://img.test/a.png">') + raw.flush()
        guard._learn("https://site.test", "https://site.test/", "text/html", "deflate", deflated)
        self.assertEqual(guard.references["https://site.test"], {"https://img.test"})
        guard._learn("https://site.test", "https://site.test/", "text/html", "br", b"\x00")
        self.assertEqual(guard.references["https://site.test"], {"https://img.test"})

    def test_served_mode_allows_only_learned_origins_of_opted_in_site(self):
        import gzip
        policy = Policy(["https://site.test"])
        policy.values.update(dependencies={}, all_dependencies=[])
        guard = network._Guard(policy)
        body = gzip.compress(b'<script src="https://cdn.test/s.js"></script><a href="https://evil.test/">x</a>')
        guard._learn("https://site.test", "https://site.test/", "text/html", "gzip", body)
        self.assertIsNotNone(guard._allowed("https://cdn.test"))
        policy.values["all_dependencies"] = ["https://site.test"]
        self.assertIsNone(guard._allowed("https://cdn.test"))
        self.assertIsNotNone(guard._allowed("https://evil.test"))

    def test_header_mode_and_denials_group_under_requesting_site(self):
        policy = Policy(["https://site.test"])
        guard = network._Guard(policy)
        request = flow("http://cdn.test/s.js")
        request.request.headers["Referer"] = "https://site.test/page"
        guard._check_request(request)
        self.assertEqual(request.response.status_code, 403)
        self.assertEqual(policy.denials[-1], ("website", "http://cdn.test", "Origin is not allowed", "https://site.test"))
        policy.values["header_dependencies"] = ["https://site.test"]
        allowed = flow("http://cdn.test/s.js")
        allowed.request.headers["Referer"] = "https://site.test/page"
        guard._check_request(allowed)
        self.assertIsNone(allowed.response)
        self.assertIsNotNone(guard._allowed("http://cdn.test"))


class CredentialTests(unittest.IsolatedAsyncioTestCase):
    async def test_injection_is_exact_origin_and_after_destination_checks(self):
        policy = Policy(["https://example.test", "http://other.test"])
        policy.values["credential_origins"] = ["https://example.test"]
        guard = network._Guard(policy)
        guard.tunnels["client"] = "https://example.test"
        selected = flow("https://example.test/private", tls=True)
        with (patch.object(network, "_resolve_public", return_value="93.184.216.34") as resolve,
              patch.object(network, "_lookup_authorization", return_value="Bearer host-fixture") as lookup):
            await guard.requestheaders(selected)
            resolve.assert_awaited_once_with("example.test", 443)
            lookup.assert_awaited_once_with("https://example.test")
            self.assertEqual(selected.request.headers["Authorization"], "Bearer host-fixture")
            lookup.reset_mock()
            resolve.reset_mock()
            denied = flow("https://example.test/private", host="other.test", tls=True)
            await guard.requestheaders(denied)
            self.assertEqual(denied.response.status_code, 403)
            lookup.assert_not_called()
            resolve.assert_not_called()
            guard.tunnels.clear()
            other = flow("http://other.test/")
            await guard.requestheaders(other)
            self.assertIsNone(other.response)
            self.assertNotIn("Authorization", other.request.headers)
            lookup.assert_not_called()

    async def test_failed_keyring_never_sends_request_or_reports_secret(self):
        policy = Policy(["https://example.test"])
        policy.values["credential_origins"] = ["https://example.test"]
        guard = network._Guard(policy)
        guard.tunnels["client"] = "https://example.test"
        for error in (ValueError("fixture-secret"), OSError("fixture-secret"), TimeoutError()):
            selected = flow("https://example.test/", tls=True)
            with (patch.object(network, "_resolve_public", return_value="93.184.216.34"),
                  patch.object(network, "_lookup_authorization", side_effect=error)):
                await guard.requestheaders(selected)
            self.assertEqual(selected.response.status_code, 502)
            self.assertNotIn("Authorization", selected.request.headers)
            self.assertNotIn(b"fixture-secret", selected.response.content)
            self.assertNotIn(id(selected), guard.active)
        self.assertFalse(policy.denials)

    async def test_private_address_never_reads_keyring(self):
        policy = Policy(["https://example.test"])
        policy.values["credential_origins"] = ["https://example.test"]
        guard = network._Guard(policy)
        guard.tunnels["client"] = "https://example.test"
        selected = flow("https://example.test/", tls=True)
        with (patch.object(network, "_resolve_public", side_effect=ValueError("private")),
              patch.object(network, "_lookup_authorization") as lookup):
            await guard.requestheaders(selected)
            lookup.assert_not_called()
        self.assertEqual(selected.response.status_code, 403)
        self.assertEqual(policy.denials[-1][1], "https://example.test")

    async def test_keyring_command_environment_and_output_validation(self):
        environment = {"PATH": "/fixture/bin", "HOME": "/fixture/home",
                       "DBUS_SESSION_BUS_ADDRESS": "unix:path=/fixture/bus",
                       "XDG_RUNTIME_DIR": "/fixture/runtime", "PROVIDER_TOKEN": "fixture-secret"}
        for output, valid in ((b"Bearer host-fixture\n", True), (b"\n", False),
                              (b"Bearer value\r\n", False), (b"a\nb\n", False),
                              ("Bearer \u2603\n".encode(), False), (b"x" * 16386, False)):
            reader = asyncio.StreamReader()
            reader.feed_data(output)
            reader.feed_eof()
            process = SimpleNamespace(stdout=reader, wait=AsyncMock(return_value=0), returncode=0)
            with (patch.dict(network.os.environ, environment, clear=True),
                  patch.object(network.asyncio, "create_subprocess_exec", return_value=process) as spawn):
                if valid:
                    self.assertEqual(await network._lookup_authorization("https://example.test"),
                                     "Bearer host-fixture")
                else:
                    with self.assertRaises(ValueError):
                        await network._lookup_authorization("https://example.test")
                self.assertEqual(spawn.call_args.args,
                                 ("secret-tool", "lookup", "application", "ombp-start",
                                  "origin", "https://example.test"))
                self.assertNotIn("PROVIDER_TOKEN", spawn.call_args.kwargs["env"])
                self.assertEqual(spawn.call_args.kwargs["stderr"], asyncio.subprocess.DEVNULL)


class DNSTests(unittest.IsolatedAsyncioTestCase):
    async def test_mixed_dns_answer_fails_closed(self):
        answers = [(socket.AF_INET, socket.SOCK_STREAM, 6, "", (address, 80))
                   for address in ("93.184.216.34", "127.0.0.1")]
        with patch.object(asyncio.get_running_loop(), "getaddrinfo", return_value=answers):
            with self.assertRaises(ValueError):
                await network._resolve_public("fixture.test", 80)

    async def test_dns_pin_and_live_revocation(self):
        policy = Policy(["http://fixture.test"])
        guard = network._Guard(policy)
        guard._check_request(flow("http://fixture.test/"))
        server = SimpleNamespace(address=("fixture.test", 80), error=None, sni=None)
        data = SimpleNamespace(server=server, client=SimpleNamespace(id="client"))
        with patch.object(network, "_resolve_public", return_value="93.184.216.34") as resolver:
            await guard.server_connect(data)
            self.assertEqual(server.address, ("93.184.216.34", 80))
            self.assertEqual(server.sni, "fixture.test")
            resolver.assert_awaited_once_with("fixture.test", 80)
        policy.values["websites"] = []
        server.address = ("fixture.test", 80)
        with patch.object(network, "_resolve_public") as resolver:
            await guard.server_connect(data)
            resolver.assert_not_called()
        self.assertEqual(server.error, "Website destination denied")
        self.assertEqual(policy.denials[-1][1], "http://fixture.test")

    async def test_localhost_denied_without_test_bypass(self):
        with self.assertRaises(ValueError):
            await network._resolve_public("127.0.0.1", 80)

    async def test_raw_connection_has_no_authorized_request(self):
        policy = Policy(["https://fixture.test"])
        guard = network._Guard(policy)
        data = SimpleNamespace(server=SimpleNamespace(address=("fixture.test", 443), error=None),
                               client=SimpleNamespace(id="unknown"))
        with patch.object(network, "_resolve_public") as resolver:
            await guard.server_connect(data)
            resolver.assert_not_called()
        self.assertIsNotNone(data.server.error)


class RelayTests(unittest.IsolatedAsyncioTestCase):
    async def test_relay_and_half_close(self):
        with tempfile.TemporaryDirectory(prefix="relay-") as directory:
            path = str(Path(directory) / "echo.sock")

            async def echo(reader, writer):
                writer.write((await reader.read()) + b"-reply")
                await writer.drain()
                writer.close()
                await writer.wait_closed()

            unix_server = await asyncio.start_unix_server(echo, path)
            ready = asyncio.Event()
            listener = None
            real_start = asyncio.start_server

            async def start(callback, host, port):
                nonlocal listener
                self.assertEqual((host, port), ("127.0.0.1", 18080))
                listener = await real_start(callback, host, 0)
                ready.set()
                return listener

            with patch.object(network.asyncio, "start_server", side_effect=start):
                task = asyncio.create_task(network.relay(path))
                try:
                    await asyncio.wait_for(ready.wait(), 5)
                    port = listener.sockets[0].getsockname()[1]
                    reader, writer = await asyncio.open_connection("127.0.0.1", port)
                    writer.write(b"fixture")
                    await writer.drain()
                    writer.write_eof()
                    self.assertEqual(await asyncio.wait_for(reader.read(), 5), b"fixture-reply")
                    writer.close()
                    await writer.wait_closed()
                finally:
                    task.cancel()
                    with contextlib.suppress(asyncio.CancelledError):
                        await task
                    unix_server.close()
                    await unix_server.wait_closed()


class GatewayIntegrationTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="gateway-")
        self.addCleanup(self.directory.cleanup)
        self.policy = Policy()
        self.seen = []
        seen = self.seen
        streams = self.streams = SimpleNamespace(
            upload_started=threading.Event(), upload_continued=threading.Event(),
            upload_done=threading.Event(), download_release=threading.Event(),
            download_done=threading.Event(), upload_after=None, download_closed=False)
        self.addCleanup(streams.download_release.set)

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_GET(self):
                if self.path == "/ws":
                    self.echo_websocket()
                    return
                if self.path == "/download":
                    self.connection.settimeout(5)
                    try:
                        self.send_response(200)
                        self.send_header("Content-Length", "1000000")
                        self.end_headers()
                        self.wfile.write(b"initial")
                        self.wfile.flush()
                        if streams.download_release.wait(5):
                            with contextlib.suppress(OSError):
                                self.wfile.write(b"after")
                                self.wfile.flush()
                            streams.download_closed = self.connection.recv(1) == b""
                    except ConnectionError:
                        streams.download_closed = True
                    except TimeoutError:
                        pass
                    finally:
                        streams.download_done.set()
                    return
                seen.append((self.path, self.headers.get("Authorization"), self.headers.get("Host")))
                if self.path == "/redirect":
                    self.send_response(302)
                    self.send_header("Location", "https://blocked.test/")
                    body = b""
                else:
                    self.send_response(200)
                    body = b"fixture response"
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def echo_websocket(self):
                accept = base64.b64encode(hashlib.sha1(
                    self.headers["Sec-WebSocket-Key"].encode() + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
                self.send_response(101)
                self.send_header("Upgrade", "websocket")
                self.send_header("Connection", "Upgrade")
                self.send_header("Sec-WebSocket-Accept", accept.decode())
                self.end_headers()
                self.wfile.flush()
                self.close_connection = True
                with contextlib.suppress(OSError):
                    while (head := self.rfile.read(2)) and head[0] & 0x0F != 8:
                        mask = self.rfile.read(4)
                        payload = bytes(b ^ mask[i % 4] for i, b in enumerate(self.rfile.read(head[1] & 0x7F)))
                        self.wfile.write(bytes([0x81, len(payload)]) + payload)
                        self.wfile.flush()

            def do_PUT(self):
                seen.append((self.path, self.headers.get("Authorization"), self.headers.get("Host")))
                if self.path not in ("/upload", "/early-upload"):
                    self.send_error(500, "Unexpected upstream request")
                    return
                self.connection.settimeout(5)
                try:
                    if self.rfile.read(7) == b"initial":
                        streams.upload_started.set()
                        if self.path == "/early-upload":
                            self.send_response(streams.early_status)
                            self.send_header("Content-Length", "0")
                            self.end_headers()
                            self.wfile.flush()
                            if self.rfile.read(6) == b"before":
                                streams.upload_continued.set()
                        streams.upload_after = self.rfile.read(5)
                except OSError:
                    pass
                finally:
                    streams.upload_done.set()

            do_POST = do_PUT

            def log_message(self, *args):
                pass

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.httpd.daemon_threads = True
        self.addCleanup(self.httpd.server_close)
        self.addCleanup(self.httpd.shutdown)
        self.server_thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.server_thread.start()

        async def fixture_resolution(host, port):
            if host != "fixture.test" or port != self.httpd.server_port:
                raise ValueError("Unexpected test destination")
            return "127.0.0.1"

        self.resolver = patch.object(network, "_resolve_public", side_effect=fixture_resolution)
        self.resolver.start()
        self.addCleanup(self.resolver.stop)
        self.gateway = network.NetworkGateway(self.policy, Path(self.directory.name) / "network")
        self.gateway.start()
        self.addCleanup(self.gateway.stop)

    def open_socket(self, scheme="http", authority=None):
        sock = socket.socket(socket.AF_UNIX)
        self.addCleanup(sock.close)
        sock.settimeout(5)
        sock.connect(str(self.gateway.socket_path))
        authority = authority or f"fixture.test:{self.httpd.server_port}"
        if scheme == "https":
            sock.sendall(f"CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n".encode())
            response = http.client.HTTPResponse(sock)
            response.begin()
            self.assertEqual(response.status, 200)
            response.close()
            context = ssl.create_default_context(cafile=str(self.gateway.ca_path))
            sock = context.wrap_socket(sock, server_hostname=network.urlsplit(f"https://{authority}").hostname)
            self.addCleanup(sock.close)
        return sock

    def request(self, scheme="http", path="/", host=None, authorization=None):
        sock = self.open_socket(scheme)
        authority = f"fixture.test:{self.httpd.server_port}"
        destination = path if scheme == "https" else f"http://{authority}{path}"
        headers = f"GET {destination} HTTP/1.1\r\nHost: {host or authority}\r\nConnection: close\r\n"
        if authorization:
            headers += f"Authorization: {authorization}\r\n"
        sock.sendall((headers + "\r\n").encode())
        response = http.client.HTTPResponse(sock)
        response.begin()
        result = response.status, dict(response.getheaders()), response.read()
        response.close()
        sock.close()
        return result

    def test_websocket_forwards_without_retaining_messages_and_stops_on_revocation(self):
        authority = f"fixture.test:{self.httpd.server_port}"
        self.policy.values["websites"] = [f"http://{authority}"]
        sock = self.open_socket()
        sock.sendall((f"GET http://{authority}/ws HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\n"
                      "Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
                      "Sec-WebSocket-Version: 13\r\n\r\n").encode())
        head = b""
        while b"\r\n\r\n" not in head:
            head += sock.recv(1)
        self.assertIn(b" 101 ", head.split(b"\r\n")[0])
        for message in (b"one", b"two", b"three"):
            sock.sendall(bytes([0x81, 0x80 | len(message)]) + b"\0\0\0\0" + message)
            self.assertEqual(sock.recv(2 + len(message)), bytes([0x81, len(message)]) + message)
        flows = [flow for flow, _ in self.gateway._master.addons.get("_guard").active.values()]
        self.assertEqual([len(flow.websocket.messages) for flow in flows], [0])
        self.revoke("websites")
        with contextlib.suppress(OSError):
            sock.sendall(bytes([0x81, 0x80 | 5]) + b"\0\0\0\0" + b"after")
        received = b""
        while chunk := sock.recv(64):
            received += chunk
        self.assertNotIn(b"after", received, "WebSocket kept forwarding after revocation")

    def connect_status(self, authority):
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(5)
            sock.connect(str(self.gateway.socket_path))
            sock.sendall(f"CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n".encode())
            response = http.client.HTTPResponse(sock)
            response.begin()
            status = response.status
            response.close()
            return status

    def enable_tls(self):
        from cryptography import x509
        from cryptography.hazmat.primitives import hashes, serialization
        from cryptography.hazmat.primitives.asymmetric import rsa
        from cryptography.x509.oid import NameOID
        ca_pem = (self.gateway.ca_path.parent / "mitmproxy-ca.pem").read_bytes()
        ca = x509.load_pem_x509_certificate(ca_pem)
        ca_key = serialization.load_pem_private_key(ca_pem, password=None)
        key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        now = datetime.now(timezone.utc)
        cert = (x509.CertificateBuilder()
                .subject_name(x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "fixture.test")]))
                .issuer_name(ca.subject).public_key(key.public_key())
                .serial_number(x509.random_serial_number())
                .not_valid_before(now - timedelta(minutes=1))
                .not_valid_after(now + timedelta(days=1))
                .add_extension(x509.SubjectAlternativeName([x509.DNSName("fixture.test")]), False)
                .sign(ca_key, hashes.SHA256()))
        cert_path = Path(self.directory.name) / "fixture.pem"
        key_path = Path(self.directory.name) / "fixture-key.pem"
        cert_path.write_bytes(cert.public_bytes(serialization.Encoding.PEM))
        key_path.write_bytes(key.private_bytes(serialization.Encoding.PEM,
                                             serialization.PrivateFormat.PKCS8,
                                             serialization.NoEncryption()))
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(cert_path, key_path)
        self.httpd.socket = context.wrap_socket(self.httpd.socket, server_side=True)
        ready = threading.Event()

        def trust_fixture():
            self.gateway._master.options.update(ssl_verify_upstream_trusted_ca=str(self.gateway.ca_path))
            ready.set()

        self.gateway._master.event_loop.call_soon_threadsafe(trust_fixture)
        self.assertTrue(ready.wait(5))

    def revoke(self, setting):
        self.policy.values[setting] = False if setting == "internet" else []
        revoked = threading.Event()

        def enforce():
            try:
                self.gateway._master.addons.get("_guard")._revoke()
            finally:
                revoked.set()

        self.gateway._master.event_loop.call_soon_threadsafe(enforce)
        self.assertTrue(revoked.wait(5), "Gateway did not process revocation")

    def streaming_upload_revocation(self, setting, early_status=None):
        authority = f"fixture.test:{self.httpd.server_port}"
        self.policy.values["websites"] = [f"http://{authority}"]
        sock = self.open_socket()
        self.streams.early_status = early_status
        path = "/early-upload" if early_status is not None else "/upload"
        sock.sendall((f"PUT http://{authority}{path} HTTP/1.1\r\nHost: {authority}\r\n"
                      "Content-Length: 1000000\r\n\r\n").encode() + b"initial")
        self.assertTrue(self.streams.upload_started.wait(5), "Upload never reached fixture")
        if early_status is not None:
            response = http.client.HTTPResponse(sock)
            self.addCleanup(response.close)
            response.begin()
            self.assertEqual(response.status, 403 if early_status == 101 else early_status)
            if early_status == 200:
                response.read()
            sock.sendall(b"before")
            self.assertTrue(self.streams.upload_continued.wait(5),
                            "Upload did not continue after the final response")
        self.revoke(setting)
        with contextlib.suppress(OSError):
            sock.sendall(b"after")
        self.assertTrue(self.streams.upload_done.wait(5), "Upstream upload transport remained open")
        self.assertEqual(self.streams.upload_after, b"", "Bytes reached upstream after revocation")
        self.assertEqual(sock.recv(1), b"", "Client upload transport remained open")

    def test_streaming_upload_stops_on_origin_revocation(self):
        self.streaming_upload_revocation("websites")

    def test_streaming_upload_stops_on_internet_revocation(self):
        self.streaming_upload_revocation("internet")

    def test_early_final_response_upload_stops_on_origin_revocation(self):
        self.streaming_upload_revocation("websites", early_status=200)

    def test_early_final_response_upload_stops_on_internet_revocation(self):
        self.streaming_upload_revocation("internet", early_status=200)

    def test_rejected_early_upgrade_upload_stops_on_origin_revocation(self):
        self.streaming_upload_revocation("websites", early_status=101)

    def test_rejected_early_upgrade_upload_stops_on_internet_revocation(self):
        self.streaming_upload_revocation("internet", early_status=101)

    def failing_history_policy(self, websites=(), internet=True):
        policy = PersistentPolicy(Path(self.directory.name) / "policy")
        # Fixture-only literals reach the network guard without weakening policy validation.
        policy.data.update(websites=list(websites), internet=internet)
        policy.save("policy.json", policy.data)
        (policy.directory / ".denied.json.tmp").mkdir()
        self.gateway._master.addons.get("_guard").policy = policy
        return policy

    def test_history_save_failure_still_closes_active_upload(self):
        authority = f"fixture.test:{self.httpd.server_port}"
        policy = self.failing_history_policy([f"http://{authority}"])
        sock = self.open_socket()
        sock.sendall((f"PUT http://{authority}/upload HTTP/1.1\r\nHost: {authority}\r\n"
                      "Content-Length: 1000000\r\n\r\n").encode() + b"initial")
        self.assertTrue(self.streams.upload_started.wait(5), "Upload never reached fixture")
        policy.replace({**policy.snapshot(), "internet": False})
        self.revoke("internet")
        with contextlib.suppress(OSError):
            sock.sendall(b"after")
        self.assertTrue(self.streams.upload_done.wait(5), "Upstream upload transport remained open")
        self.assertEqual(self.streams.upload_after, b"", "Bytes reached upstream after revocation")
        self.assertEqual(sock.recv(1), b"", "Client upload transport remained open")
        self.assertEqual(policy.events()[0]["target"], f"http://{authority}")

    def literal_request(self):
        authority = f"127.0.0.1:{self.httpd.server_port}"
        sock = self.open_socket()
        sock.sendall((f"GET http://{authority}/ HTTP/1.1\r\nHost: {authority}\r\n"
                      "Connection: close\r\n\r\n").encode())
        response = http.client.HTTPResponse(sock)
        self.addCleanup(response.close)
        response.begin()
        return response.status, response.read()

    def test_history_save_failure_still_returns_local_denial(self):
        self.failing_history_policy(internet=False)
        self.resolver.stop()
        status, body = self.literal_request()
        self.assertEqual(status, 403)
        self.assertEqual(body, b"Website access denied by launcher policy.\n")
        self.assertFalse(self.seen, "Denied literal request reached the local fixture")

    def test_history_save_failure_still_rejects_private_server_connection(self):
        authority = f"127.0.0.1:{self.httpd.server_port}"
        policy = self.failing_history_policy([f"http://{authority}"])
        self.resolver.stop()
        status, _ = self.literal_request()
        self.assertEqual(status, 502)
        self.assertFalse(self.seen, "Denied private connection reached the local fixture")
        self.assertIn("non-public", policy.events()[0]["reason"])

    def streaming_download_revocation(self, setting):
        authority = f"fixture.test:{self.httpd.server_port}"
        self.policy.values["websites"] = [f"http://{authority}"]
        sock = self.open_socket()
        sock.sendall((f"GET http://{authority}/download HTTP/1.1\r\n"
                      f"Host: {authority}\r\n\r\n").encode())
        response = http.client.HTTPResponse(sock)
        self.addCleanup(response.close)
        response.begin()
        self.assertEqual(response.status, 200)
        self.assertEqual(response.read(7), b"initial")
        self.revoke(setting)
        self.streams.download_release.set()
        self.assertEqual(response.read(1), b"", "Bytes reached client after revocation")
        self.assertTrue(self.streams.download_done.wait(5), "Upstream download transport remained open")
        self.assertTrue(self.streams.download_closed, "Upstream download transport remained open")

    def test_streaming_download_stops_on_origin_revocation(self):
        self.streaming_download_revocation("websites")

    def test_streaming_download_stops_on_internet_revocation(self):
        self.streaming_download_revocation("internet")

    def body_error_request(self, method, authority):
        sock = self.open_socket("https", authority)
        body = b'{"prompt":"body-secret"}'
        sock.sendall((f"{method} /private HTTP/1.1\r\nHost: {authority}\r\n"
                      f"Content-Length: {len(body)}\r\nExpect: 100-continue\r\n"
                      "Connection: close\r\n\r\n").encode())
        # Hold the body until the asynchronous request-header checks have finished.
        interim = sock.makefile("rb")
        self.addCleanup(interim.close)
        self.assertEqual(interim.readline(), b"HTTP/1.1 100 Continue\r\n")
        self.assertEqual(interim.readline(), b"\r\n")
        sock.sendall(body)
        response = http.client.HTTPResponse(sock)
        self.addCleanup(response.close)
        response.begin()
        result = response.status, response.read()
        self.assertFalse(self.seen, "Locally rejected request reached upstream")
        self.assertNotIn(b"body-secret", result[1])
        self.assertNotIn(b"fixture-secret", result[1])
        return result

    def test_body_bearing_late_destination_denial_returns_403(self):
        authority = f"127.0.0.1:{self.httpd.server_port}"
        site = f"https://{authority}"
        self.policy.values.update(websites=[site], credential_origins=[site])
        self.resolver.stop()
        for method in ("POST", "PUT"):
            with self.subTest(method=method):
                status, body = self.body_error_request(method, authority)
                self.assertEqual(status, 403)
                self.assertEqual(body, b"Website access denied by launcher policy.\n")

    def test_body_bearing_keyring_failure_returns_502(self):
        authority = f"fixture.test:{self.httpd.server_port}"
        site = f"https://{authority}"
        self.policy.values.update(websites=[site], credential_origins=[site])
        executable = Path(self.directory.name) / "secret-tool"
        executable.write_text(
            f"#!{sys.executable}\nimport sys\n"
            "sys.stderr.write('fixture-secret\\n')\nsys.exit(1)\n")
        executable.chmod(0o700)
        with patch.dict(network.os.environ, {"PATH": self.directory.name}):
            for method in ("POST", "PUT"):
                with self.subTest(method=method):
                    status, body = self.body_error_request(method, authority)
                    self.assertEqual(status, 502)
                    self.assertEqual(body, b"Host credential unavailable.\n")

    def test_stop_releases_listener(self):
        servers = self.gateway._master.addons.get("proxyserver").servers
        address = next(address for server in servers for address in server.listen_addrs)
        self.gateway.stop()
        with socket.socket() as listener:
            listener.bind(address)

    def test_http_live_allowlist_and_redirect(self):
        site = f"http://fixture.test:{self.httpd.server_port}"
        self.policy.values["websites"] = [site]
        self.assertEqual(self.request(path="/sub/path")[0], 200)
        self.assertEqual(self.request(path="/redirect")[0], 302)
        # Unapproved HTTPS is denied per request after TLS (to see its requesting site); CONNECT never reaches upstream.
        self.assertEqual(self.connect_status("blocked.test:443"), 200)
        self.assertEqual(self.request(host="blocked.test")[0], 403)
        self.policy.values["internet"] = False
        self.assertEqual(self.request()[0], 403)
        self.policy.values["internet"] = True
        self.policy.values["websites"] = []
        self.assertEqual(self.request()[0], 403)
        self.policy.values["websites"] = [site]
        self.assertEqual(self.request()[0], 200)
        self.assertEqual(len(self.seen), 3)
        self.assertTrue(all(entry[0] == "website" for entry in self.policy.denials))

    def test_https_interception_credentials_and_host_scope(self):
        self.enable_tls()
        self.policy.values["websites"] = [f"https://fixture.test:{self.httpd.server_port}"]
        status, _, body = self.request("https", "/nested", authorization="Bearer fixture-only")
        self.assertEqual((status, body), (200, b"fixture response"))
        self.assertEqual(self.seen[-1][0:2], ("/nested", "Bearer fixture-only"))
        self.assertEqual(self.request("https", host="blocked.test", authorization="Bearer fixture-only")[0], 403)
        self.assertEqual(len(self.seen), 1)
        self.policy.values["websites"] = []
        self.assertEqual(self.request()[0], 403)

    def test_host_keyring_injection_and_failure_before_upstream(self):
        self.enable_tls()
        site = f"https://fixture.test:{self.httpd.server_port}"
        self.policy.values["websites"] = [site]
        self.policy.values["credential_origins"] = [site]
        with patch.object(network, "_lookup_authorization", return_value="Bearer host-fixture") as lookup:
            self.assertEqual(self.request("https", "/private")[0], 200)
            self.assertEqual(self.seen[-1][1], "Bearer host-fixture")
            lookup.assert_awaited_once_with(site)
        with patch.object(network, "_lookup_authorization", side_effect=ValueError("fixture-secret")):
            status, _, body = self.request("https", "/failure")
            self.assertEqual(status, 502)
            self.assertNotIn(b"fixture-secret", body)
        self.assertEqual(len(self.seen), 1)

    def test_connect_does_not_tunnel_uninspected_plaintext(self):
        authority = f"fixture.test:{self.httpd.server_port}"
        self.policy.values["websites"] = [f"https://{authority}"]
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(5)
            sock.connect(str(self.gateway.socket_path))
            sock.sendall(f"CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n".encode())
            response = http.client.HTTPResponse(sock)
            response.begin()
            self.assertEqual(response.status, 200)
            response.close()
            sock.sendall(f"GET / HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n".encode())
            response = http.client.HTTPResponse(sock)
            response.begin()
            self.assertEqual(response.status, 403)
            response.close()
        self.assertFalse(self.seen)

    def test_connect_rejects_alternate_sni(self):
        authority = f"fixture.test:{self.httpd.server_port}"
        self.policy.values["websites"] = [f"https://{authority}"]
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(5)
            sock.connect(str(self.gateway.socket_path))
            sock.sendall(f"CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n".encode())
            response = http.client.HTTPResponse(sock)
            response.begin()
            self.assertEqual(response.status, 200)
            response.close()
            context = ssl.create_default_context(cafile=str(self.gateway.ca_path))
            with self.assertRaises((ssl.SSLError, ConnectionError)):
                context.wrap_socket(sock, server_hostname="other.test")
        self.assertFalse(self.seen)
        self.assertIn("SNI", self.policy.denials[-1][2])


if __name__ == "__main__":
    unittest.main()
