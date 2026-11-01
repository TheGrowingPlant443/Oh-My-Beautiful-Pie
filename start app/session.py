"""Container entrypoint. All descendants, including every OMP instance, stay here."""
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time


def main():
    runtime = Path(os.environ["XDG_RUNTIME_DIR"])
    runtime.mkdir(mode=0o700, parents=True, exist_ok=True)
    password = sys.stdin.buffer.readline(256)
    if not password.endswith(b"\n") or not password.strip():
        raise RuntimeError("Missing private keyring unlock key")
    control = str(runtime / "keyring")
    os.environ["GNOME_KEYRING_CONTROL"] = control
    relay = subprocess.Popen([sys.executable, "/launcher/network.py", "relay", "/network/proxy.sock"])
    app = None

    def terminate(_signal, _frame):
        if app is not None:
            app.terminate()
        relay.terminate()

    signal.signal(signal.SIGTERM, terminate)
    signal.signal(signal.SIGINT, terminate)
    try:
        deadline = time.monotonic() + 10
        while True:
            if relay.poll() is not None:
                raise RuntimeError("Network relay failed to start")
            try:
                with socket.create_connection(("127.0.0.1", 18080), timeout=0.2):
                    break
            except OSError:
                if time.monotonic() >= deadline:
                    raise RuntimeError("Network relay did not become ready")
                time.sleep(0.05)
        keyring = os.environ["START_APP_KEYRING"]
        subprocess.run([keyring, "--login", "--control-directory", control],
                       input=password[:-1], check=True, capture_output=True, timeout=15)
        subprocess.run([keyring, "--start", "--components=secrets", "--control-directory", control],
                       check=True, capture_output=True, timeout=15)
        app = subprocess.Popen(sys.argv[1:])
        return app.wait()
    finally:
        if app is not None and app.poll() is None:
            app.terminate()
        relay.terminate()
        try:
            relay.wait(timeout=5)
        except subprocess.TimeoutExpired:
            relay.kill()
            relay.wait()


if __name__ == "__main__":
    sys.exit(main())
