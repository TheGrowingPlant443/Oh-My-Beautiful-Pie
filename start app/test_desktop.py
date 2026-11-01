import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import unittest


class DesktopLifetimeTests(unittest.TestCase):
    def test_creator_thread_exit_is_safe_but_launcher_exit_retires_children(self):
        helper = str(Path(__file__).with_name("desktop.py"))
        parent_code = '''
import json, os, subprocess, sys, threading
processes = []
def start():
    child_code = "import os, signal; signal.signal(signal.SIGTERM, lambda *_: exit(0)); print(os.getpid(), flush=True); signal.pause()"
    process = subprocess.Popen([sys.executable, sys.argv[1], str(os.getpid()), sys.executable, '-c', child_code], stdout=subprocess.PIPE, text=True)
    processes.append(process)
thread = threading.Thread(target=start)
thread.start(); thread.join()
process = processes[0]
print(json.dumps([process.pid, int(process.stdout.readline())]), flush=True)
sys.stdin.read()
'''
        parent = subprocess.Popen([sys.executable, "-c", parent_code, helper], stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, text=True)
        descriptors = []
        try:
            ready = select.select([parent.stdout], [], [], 5)[0]
            self.assertTrue(ready, "Nested desktop helper failed to start")
            helper_pid, compositor_pid = json.loads(parent.stdout.readline())
            descriptors = [os.pidfd_open(helper_pid), os.pidfd_open(compositor_pid)]
            self.assertFalse(select.select(descriptors, [], [], 0.3)[0],
                             "Helper died with its creator thread instead of the launcher process")
            parent.terminate()
            parent.wait(timeout=5)
            for descriptor in descriptors:
                self.assertTrue(select.select([descriptor], [], [], 5)[0],
                                "Nested desktop process survived launcher exit")
        finally:
            if parent.poll() is None:
                parent.terminate()
                parent.wait(timeout=5)
            for descriptor in descriptors:
                if not select.select([descriptor], [], [], 0)[0]:
                    signal.pidfd_send_signal(descriptor, signal.SIGKILL)
                os.close(descriptor)
            parent.stdin.close()
            parent.stdout.close()


if __name__ == "__main__":
    unittest.main()
