import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from policy import Policy, origin


class OriginTests(unittest.TestCase):
    def test_subpaths_normalize_but_subdomains_remain_distinct(self):
        self.assertEqual(origin("https://EXAMPLE.com:443/v1/models?x=y"), "https://example.com")
        self.assertEqual(origin("http://example.com:8080/a"), "http://example.com:8080")
        self.assertNotEqual(origin("https://api.example.com"), origin("https://example.com"))

    def test_credentials_and_local_addresses_rejected(self):
        for value in ("ftp://example.com", "https://user:secret@example.com", "http://127.0.0.1",
                      "https://[::1]", "http://169.254.169.254", "http://localhost", "http://10.1.1.1"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                origin(value)


class PolicyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.policy = Policy(self.root / "state")
        self.folder = self.root / "vault"
        self.folder.mkdir()

    def test_rejected_update_keeps_last_permission_set(self):
        value = self.policy.snapshot()
        value["folders"] = [{"path": str(self.folder), "write": False}]
        self.policy.replace(value)
        value["folders"][0]["write"] = "yes"
        with self.assertRaises(ValueError):
            self.policy.replace(value)
        self.assertFalse(self.policy.snapshot()["folders"][0]["write"])
        self.assertEqual(Policy(self.root / "state").snapshot(), self.policy.snapshot())

    def test_state_and_symlink_folders_never_granted(self):
        link = self.root / "link"
        link.symlink_to(self.folder, target_is_directory=True)
        for target in (self.policy.directory, link, Path("/proc")):
            value = self.policy.snapshot()
            value["folders"] = [{"path": str(target), "write": True}]
            with self.subTest(target=target), self.assertRaises(ValueError):
                self.policy.replace(value)

    def test_program_name_lookup_cannot_admit_non_nix_host_code(self):
        binary = self.root / "host-program"
        binary.write_text("#!/bin/sh\nexit 0\n")
        binary.chmod(0o755)
        value = self.policy.snapshot()
        value["programs"] = ["host-program"]
        with patch.dict("os.environ", {"PATH": str(self.root)}), self.assertRaises(ValueError):
            self.policy.replace(value)
        self.assertEqual(self.policy.snapshot()["programs"], [])

    def test_denials_aggregate_and_persist(self):
        self.policy.deny("website", "https://example.com", "not allowed")
        self.policy.deny("website", "https://example.com", "internet disabled")
        row = Policy(self.root / "state").events()[0]
        self.assertEqual((row["count"], row["reason"]), (2, "internet disabled"))
        self.assertEqual(len(json.loads((self.policy.directory / "denied.json").read_text())), 1)

    def test_history_save_failure_keeps_events_and_recovers(self):
        blocked = self.policy.directory / ".denied.json.tmp"
        blocked.mkdir()
        self.policy.deny("website", "https://blocked.example", "not allowed")
        self.policy.deny("website", "https://blocked.example", "internet disabled")
        row = self.policy.events()[0]
        self.assertEqual((row["target"], row["count"]), ("https://blocked.example", 2))
        blocked.rmdir()
        self.policy.deny("website", "https://blocked.example", "still disabled")
        recovered = Policy(self.policy.directory).events()[0]
        self.assertEqual((recovered["target"], recovered["count"], recovered["reason"]),
                         ("https://blocked.example", 3, "still disabled"))
        self.assertEqual(self.policy.history_error, "")

    def test_permission_save_failure_does_not_change_effective_permissions(self):
        before = self.policy.snapshot()
        self.policy.replace(before)
        (self.policy.directory / ".policy.json.tmp").mkdir()
        value = self.policy.snapshot()
        value["internet"] = True
        with self.assertRaises(OSError):
            self.policy.replace(value)
        self.assertEqual(self.policy.snapshot(), before)
        self.assertEqual(Policy(self.policy.directory).snapshot(), before)

    def test_host_credentials_require_allowed_https_origin(self):
        value = self.policy.snapshot()
        value["websites"] = ["http://example.com"]
        value["credential_origins"] = ["http://example.com"]
        with self.assertRaises(ValueError):
            self.policy.replace(value)
        value["credential_origins"] = ["https://other.example.com"]
        with self.assertRaises(ValueError):
            self.policy.replace(value)



if __name__ == "__main__":
    unittest.main()
