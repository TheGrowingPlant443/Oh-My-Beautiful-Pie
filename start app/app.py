"""Local control UI; its bearer token and policy never enter the container."""
import argparse
import fcntl
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import secrets
import signal
import threading
from urllib.parse import parse_qs, urlsplit
import webbrowser

from catalog import catalog, find_folders
from policy import Policy


class Server(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, policy, runtime, port=0, source=None):
        self.policy = policy
        self.runtime = runtime
        self.source = str(source or Path.cwd())
        self.catalog = catalog()
        self.token = secrets.token_urlsafe(32)
        super().__init__(("127.0.0.1", port), Handler)
        self.origin = f"http://127.0.0.1:{self.server_port}"

    @property
    def url(self):
        return f"{self.origin}/#{self.token}"


class Handler(BaseHTTPRequestHandler):
    def log_message(self, _format, *args):
        # Local control requests are deliberately not retained in access logs.
        pass

    def respond(self, status, value, content_type="application/json"):
        data = value if isinstance(value, bytes) else json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "no-referrer")
        self.send_header("Content-Security-Policy", "default-src 'self'; script-src 'self'; style-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'")
        self.end_headers()
        self.wfile.write(data)

    def authorized(self):
        return (self.headers.get("Host") == self.server.origin.removeprefix("http://")
                and self.headers.get("Origin", self.server.origin) == self.server.origin
                and secrets.compare_digest(self.headers.get("Authorization", ""), "Bearer " + self.server.token))

    def do_GET(self):
        files = {"/": ("index.html", "text/html; charset=utf-8"),
                 "/ui.js": ("ui.js", "text/javascript; charset=utf-8"),
                 "/ui.css": ("ui.css", "text/css; charset=utf-8")}
        url = urlsplit(self.path)
        if self.path in files:
            name, content_type = files[self.path]
            self.respond(200, (Path(__file__).parent / name).read_bytes(), content_type)
        elif url.path in ("/api/status", "/api/catalog", "/api/folders"):
            if not self.authorized():
                self.respond(403, {"error": "Unauthorized control request"})
                return
            if url.path == "/api/catalog":
                self.respond(200, self.server.catalog)
                return
            if url.path == "/api/folders":
                query = parse_qs(url.query).get("q", [""])[0]
                self.respond(200, find_folders(query, private_paths=self.server.policy.private_paths))
                return
            self.respond(200, {"policy": self.server.policy.snapshot(), "history": self.server.policy.events(),
                               "runtime": self.server.runtime.status(), "source": self.server.source,
                               "history_error": self.server.policy.history_error,
                               "requirements": self.server.runtime.requirements_status(),
                               "home": str(Path.home())})
        else:
            self.respond(404, {"error": "Not found"})

    def do_POST(self):
        if not self.authorized():
            self.respond(403, {"error": "Unauthorized control request"})
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if not 0 < length <= 131072:
                raise ValueError("Invalid request size")
            self.connection.settimeout(10)
            value = json.loads(self.rfile.read(length))
            if self.path == "/api/policy":
                self.server.policy.replace(value)
            elif self.path == "/api/start":
                self.server.runtime.start(value["source"], value["mode"], value["desktop"], value["gpu"])
            elif self.path == "/api/stop":
                self.server.runtime.stop()
            elif self.path == "/api/history/clear":
                self.server.policy.clear_history()
            elif self.path == "/api/history/forget":
                self.server.policy.forget(value)
            else:
                self.respond(404, {"error": "Not found"})
                return
            self.respond(200, {"ok": True})
        except (ValueError, OSError, RuntimeError, KeyError, TypeError) as error:
            self.respond(400, {"error": str(error)})
        except Exception:
            self.respond(500, {"error": "Operation failed; inspect launcher terminal and launch.log"})
            raise


def auto_start(runtime, settings):
    """Start OMBP like a Start click; failures show in the UI like a failed run."""
    try:
        runtime.start(settings["source"], settings["mode"], settings["desktop"], settings["gpu"])
    except Exception as error:  # Any failure must reach the UI instead of dying in this thread.
        with runtime.lock:
            runtime.last_error = f"Automatic start failed: {error}"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-dir", type=Path, default=Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local/state")) / "ombp-start")
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--no-browser", action="store_true")
    parser.add_argument("--source-dir", type=Path,
                        default=Path.cwd().parent if Path.cwd().name == "start app" else Path.cwd(),
                        help="OMBP checkout to build on each Start click (default: working directory, or its parent from start app)")
    args = parser.parse_args()
    from runtime import Runtime
    policy = Policy(args.state_dir)
    lock_fd = os.open(policy.directory / "launcher.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        os.close(lock_fd)
        parser.error("A launcher already owns this state directory")
    runtime = Runtime(policy)
    server = Server(policy, runtime, args.port, args.source_dir)

    def shutdown(_signal, _frame):
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGTERM, shutdown)
    signal.signal(signal.SIGINT, shutdown)
    print(server.url, flush=True)
    if settings := policy.snapshot()["auto_start"]:
        threading.Thread(target=auto_start, args=(runtime, settings), daemon=True).start()
    if not args.no_browser:
        webbrowser.open(server.url)
    try:
        server.serve_forever()
    finally:
        runtime.close()
        server.server_close()
        os.close(lock_fd)


if __name__ == "__main__":
    main()
