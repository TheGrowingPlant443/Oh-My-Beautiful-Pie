import errno
import json
import os
from pathlib import Path
import secrets
import stat
import tempfile
import unittest

from access_fs import AccessFS
from policy import Policy
from runtime import KEYRING_UNLOCK, Runtime


class PrivateKeyringTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.policy = Policy(self.root / "state")
        self.runtime = Runtime(self.policy)
        self.addCleanup(self.runtime.stop)
        self.key = self.policy.directory / KEYRING_UNLOCK

    def test_unlock_key_is_stable_private_and_separate_from_container_home(self):
        first = self.runtime._keyring_password()
        home = self.policy.directory / "home"
        home.mkdir()
        (home / "saved-data").write_text("persistent private data")
        # Boolean comparisons keep generated unlock keys out of assertion output.
        self.assertTrue(secrets.compare_digest(first, Runtime(self.policy)._keyring_password()), "Private keyring key rotated")
        other = Runtime(Policy(self.root / "other-state"))
        self.assertFalse(secrets.compare_digest(first, other._keyring_password()), "Different states share an unlock key")
        self.assertEqual(stat.S_IMODE(self.key.stat().st_mode), 0o600)
        self.assertFalse(self.key.is_relative_to(home))

    def test_broad_host_grant_cannot_read_unlock_key(self):
        self.runtime._keyring_password()
        value = self.policy.snapshot()
        value["folders"] = [{"path": str(self.root), "write": True}]
        self.policy.replace(value)
        filesystem = AccessFS(self.policy, [], lambda _: [])
        self.addCleanup(filesystem.destroy, "/")
        with self.assertRaises(OSError) as denied:
            filesystem.open("/host" + str(self.key), os.O_RDONLY)
        self.assertEqual(denied.exception.errno, errno.EACCES)

    def test_symlinked_state_parent_cannot_expose_unlock_key(self):
        real = self.root / "real"
        real.mkdir()
        alias = self.root / "alias"
        alias.symlink_to(real, target_is_directory=True)
        policy = Policy(alias / "state")
        runtime = Runtime(policy)
        runtime._keyring_password()
        (real / "shared").write_bytes(b"allowed data")
        value = policy.snapshot()
        value["folders"] = [{"path": str(real), "write": True}]
        policy.replace(value)
        filesystem = AccessFS(policy, [], lambda _: [])
        self.addCleanup(filesystem.destroy, "/")
        handle = filesystem.open("/host" + str(real / "shared"), os.O_RDONLY)
        try:
            self.assertEqual(filesystem.read("", 32, 0, handle), b"allowed data")
        finally:
            filesystem.release("", handle)
        with self.assertRaises(OSError) as denied:
            filesystem.open("/host" + str(real / "state" / KEYRING_UNLOCK), os.O_RDONLY)
        self.assertEqual(denied.exception.errno, errno.EACCES)

    def test_existing_private_home_is_never_rekeyed_without_its_key(self):
        home = self.policy.directory / "home"
        home.mkdir()
        saved = home / "encrypted-keyring"
        saved.write_bytes(b"existing encrypted data")
        with self.assertRaises(ValueError):
            self.runtime._keyring_password()
        self.assertEqual(saved.read_bytes(), b"existing encrypted data")
        self.assertFalse(self.key.exists())

    def test_symlink_and_public_unlock_keys_are_rejected(self):
        outside = self.root / "outside-key"
        outside.write_text(json.dumps("f" * 64))
        outside.chmod(0o600)
        self.key.symlink_to(outside)
        with self.assertRaises((OSError, ValueError)):
            self.runtime._keyring_password()
        self.key.unlink()
        self.key.write_text(json.dumps("f" * 64))
        self.key.chmod(0o644)
        with self.assertRaises(ValueError):
            self.runtime._keyring_password()
        self.key.chmod(0o600)
        self.assertEqual(self.runtime._keyring_password(), "f" * 64)

    def test_malformed_unlock_key_is_not_replaced(self):
        self.key.write_text(json.dumps("invalid"))
        self.key.chmod(0o600)
        with self.assertRaises(ValueError):
            self.runtime._keyring_password()
        self.assertEqual(json.loads(self.key.read_text()), "invalid")


if __name__ == "__main__":
    unittest.main()
