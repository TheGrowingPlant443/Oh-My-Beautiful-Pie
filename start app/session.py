"""Container entrypoint. All descendants, including every OMP instance, stay here."""
import os
from pathlib import Path
import signal
import socket
import shutil
import subprocess
import sys
import threading
import time


def wait_relay(relay, port=18080):
    deadline = time.monotonic() + 10
    while True:
        if relay.poll() is not None:
            raise RuntimeError(f"Loopback relay on port {port} failed to start")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            if time.monotonic() >= deadline:
                raise RuntimeError(f"Loopback relay on port {port} did not become ready")
            time.sleep(0.05)


def prewarm(executable):
    """Read the browser package once so the first Browser click starts from page cache."""
    path = Path(executable).resolve()
    if path.parts[:3] != ("/", "nix", "store"):
        return
    os.nice(10)
    for item in Path(*path.parts[:4]).rglob("*"):
        try:
            if item.is_file() and not item.is_symlink():
                with item.open("rb") as stream, open(os.devnull, "wb") as sink:
                    shutil.copyfileobj(stream, sink, 1 << 20)
        except OSError:
            continue


def main():
    runtime = Path(os.environ["XDG_RUNTIME_DIR"])
    runtime.mkdir(mode=0o700, parents=True, exist_ok=True)
    password = sys.stdin.buffer.readline(256)
    if not password.endswith(b"\n") or not password.strip():
        raise RuntimeError("Missing private keyring unlock key")
    control = str(runtime / "keyring")
    os.environ["GNOME_KEYRING_CONTROL"] = control
    relay = subprocess.Popen([sys.executable, "/launcher/network.py", "relay", "/network/proxy.sock"])
    relays = [relay]
    app = None

    def terminate(_signal, _frame):
        if app is not None:
            app.terminate()
        for process in relays:
            process.terminate()

    signal.signal(signal.SIGTERM, terminate)
    signal.signal(signal.SIGINT, terminate)
    try:
        wait_relay(relay)
        sonar_port = os.environ.get("START_APP_SONAR_PORT")
        if sonar_port and Path("/engine/sonar.sock").exists():
            # OMBP polls its own loopback port; only the dedicated socket reaches host SonarQube.
            relays.append(subprocess.Popen([sys.executable, "/launcher/network.py", "relay",
                                            "/engine/sonar.sock", sonar_port]))
            wait_relay(relays[-1], int(sonar_port))
        keyring = os.environ["START_APP_KEYRING"]
        subprocess.run([keyring, "--login", "--control-directory", control],
                       input=password[:-1], check=True, capture_output=True, timeout=15)
        subprocess.run([keyring, "--start", "--components=secrets", "--control-directory", control],
                       check=True, capture_output=True, timeout=15)
        app = subprocess.Popen(sys.argv[1:])
        if os.environ.get("START_APP_BROWSER"):
            threading.Thread(target=prewarm, args=(os.environ["START_APP_BROWSER"],), daemon=True).start()
        return app.wait()
    finally:
        if app is not None and app.poll() is None:
            app.terminate()
        for process in relays:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


if __name__ == "__main__":
    sys.exit(main())
