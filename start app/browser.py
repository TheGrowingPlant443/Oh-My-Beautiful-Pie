"""Open the sandbox browser in its own nested desktop window, kept open while the browser runs."""
import os
import socket
import sys

REQUEST = "/browser-display/request.sock"
DISPLAY = "/browser-display/wayland-browser"


def main(argv):
    real = os.environ.get("START_APP_BROWSER")
    if not real:
        sys.exit("START_APP_BROWSER is not set; start the browser inside the OMBP sandbox")
    client = None
    if os.path.exists(REQUEST):
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.settimeout(20)
        try:
            client.connect(REQUEST)
            reply = client.makefile("rb").readline(64)
        except OSError as error:
            client.close()
            sys.exit(f"Sandbox browser window did not open: {error}")
        if reply != b"ready\n":
            client.close()
            sys.exit("Sandbox browser window did not open; see the launcher's launch.log")
        client.settimeout(None)
        # The launcher keeps the window while any process holds this connection open.
        os.set_inheritable(client.fileno(), True)
        os.environ["WAYLAND_DISPLAY"] = DISPLAY
    try:
        os.execv(real, [real, *argv[1:]])
    except OSError as error:
        sys.exit(f"Cannot start sandbox browser {real}: {error}")
    return client


if __name__ == "__main__":
    main(sys.argv)
