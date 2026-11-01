import http.client
import json
from pathlib import Path
import tempfile
import threading
import unittest

from app import Server
from policy import Policy
from runtime import Runtime


class ControlTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.policy = Policy(Path(self.temp.name) / "state")
        self.runtime = Runtime(self.policy)
        self.addCleanup(self.runtime.stop)
        self.server = Server(self.policy, self.runtime)
        self.valid_policy = self.policy.snapshot()
        self.valid_policy.update(websites=["https://example.com/api"], internet=True)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.close)

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def request(self, method, path, body=None, headers=None):
        connection = http.client.HTTPConnection("127.0.0.1", self.server.server_port)
        self.addCleanup(connection.close)
        connection.request(method, path, body, headers or {})
        response = connection.getresponse()
        return response.status, response.read()

    def test_unauthorized_and_cross_origin_cannot_change_permissions(self):
        for headers in ({}, {"Authorization": f"Bearer {self.server.token}", "Origin": "https://evil.example"},
                        {"Authorization": f"Bearer {self.server.token}", "Host": "evil.example"}):
            self.assertEqual(self.request("POST", "/api/policy", json.dumps(self.valid_policy), headers)[0], 403)
            self.assertFalse(self.policy.snapshot()["internet"])

    def test_authenticated_control_applies_validated_policy(self):
        self.assertEqual(self.request("POST", "/api/policy", json.dumps(self.valid_policy),
                                     {"Authorization": f"Bearer {self.server.token}", "Origin": self.server.origin})[0], 200)
        self.assertEqual(self.policy.snapshot()["websites"], ["https://example.com"])
        self.assertTrue(self.policy.snapshot()["internet"])

    def test_private_history_requires_authenticated_same_origin_get(self):
        target = "https://private.example.com"
        self.policy.deny("website", target, "not allowed")
        for headers in ({}, {"Authorization": f"Bearer {self.server.token}", "Origin": "https://evil.example"}):
            status, body = self.request("GET", "/api/status", headers=headers)
            self.assertEqual(status, 403)
            self.assertNotIn(target.encode(), body)
        status, body = self.request("GET", "/api/status", headers={"Authorization": f"Bearer {self.server.token}"})
        self.assertEqual(status, 200)
        self.assertEqual(json.loads(body)["history"][0]["target"], target)

    def test_malformed_permission_requests_preserve_state(self):
        before = self.policy.snapshot()
        headers = {"Authorization": f"Bearer {self.server.token}"}
        for body in (None, "{", json.dumps({"internet": True})):
            self.assertEqual(self.request("POST", "/api/policy", body, headers)[0], 400)
            self.assertEqual(self.policy.snapshot(), before)

    def test_static_path_traversal_cannot_read_private_policy(self):
        self.policy.replace(self.valid_policy)
        for path in ("/../../policy.json", "/../state/policy.json"):
            status, body = self.request("GET", path)
            self.assertEqual(status, 404)
            self.assertNotIn(b"https://example.com", body)


if __name__ == "__main__":
    unittest.main()
