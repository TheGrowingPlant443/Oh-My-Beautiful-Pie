import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import installed


class InstalledProfileTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.store = self.root / "source-store"
        self.store.mkdir()
        self.state = self.root / "state"
        self.state.mkdir(mode=0o700)
        self.core = self.package("core", "agent", b"agent-runtime")
        self.optional = self.package("optional", "language-server", b"optional-tool")
        self.dependency = self.package("dependency", "helper", b"runtime-library")
        self.copies = 0
        for change in (
            patch.object(installed, "_STORE_ROOT", re.compile(re.escape(str(self.store)) + r"/[^/]+\Z")),
            patch.object(installed, "store_root", side_effect=lambda value:
                         str(self.store / Path(value).relative_to(self.store).parts[0])),
            patch.dict(os.environ, {"START_APP_COREUTILS": os.environ.get("START_APP_COREUTILS", "/usr/bin")}),
        ):
            change.start()
            self.addCleanup(change.stop)

    def package(self, name, command, content):
        root = self.store / name
        (root / "bin").mkdir(parents=True)
        program = root / "bin" / command
        program.write_bytes(content)
        program.chmod(0o755)
        return root

    def bundle(self, root):
        return {"roots": [str(root)], "primary_roots": [str(root)],
                "programs": [str(path) for path in (root / "bin").iterdir()]}

    def execute(self, argv, cwd=None):
        if argv[0] == "nix-store":
            return "\n".join(sorted(set(argv[3:]) | {str(self.dependency)})) + "\n"
        self.copies += 1
        return subprocess.run(argv, cwd=cwd, check=True, text=True, capture_output=True).stdout

    def install(self, bundles, run=None, cancel=lambda: None):
        return installed.install_profile(self.state, bundles, run or self.execute, cancel)

    def test_private_copy_keeps_dependencies_but_not_unrelated_store_entries(self):
        unrelated = self.package("unrelated", "secret-tool", b"not-selected")
        profile = self.install({"core": self.bundle(self.core)})
        self.assertEqual((profile / "store/core/bin/agent").read_bytes(), b"agent-runtime")
        self.assertEqual((profile / "store/dependency/bin/helper").read_bytes(), b"runtime-library")
        self.assertFalse((profile / "store" / unrelated.name).exists())
        self.assertEqual(os.readlink(profile / "programs/agent"), str(self.core / "bin/agent"))
        self.assertNotEqual((profile / "store/core/bin/agent").stat().st_ino,
                            (self.core / "bin/agent").stat().st_ino)
        (profile / "store/core/bin/agent").write_bytes(b"private-copy")
        self.assertEqual((self.core / "bin/agent").read_bytes(), b"agent-runtime")

    def test_deselection_uses_a_new_generation_and_same_selection_reuses_cache(self):
        selected = {"core": self.bundle(self.core), "optional": self.bundle(self.optional)}
        first = self.install(selected)
        self.assertEqual(self.install(selected), first)
        self.assertEqual(self.copies, 1)
        second = self.install({"core": self.bundle(self.core)})
        self.assertNotEqual(first, second)
        self.assertTrue((first / "store/optional/bin/language-server").exists())
        self.assertFalse((second / "store/optional").exists())
        self.assertFalse((second / "programs/language-server").exists())
        self.assertTrue((second / "store/dependency/bin/helper").exists())

    def test_lexical_wrapper_root_is_not_lost_when_commands_are_symlinks(self):
        profile_root = self.store / "profile"
        (profile_root / "bin").mkdir(parents=True)
        (profile_root / "bin/agent").symlink_to(self.core / "bin/agent")
        bundle = {"roots": [str(self.core)], "primary_roots": [str(profile_root)],
                  "programs": [str(profile_root / "bin/agent")]}
        profile = self.install({"agent": bundle})
        self.assertTrue((profile / "store/profile/bin/agent").is_symlink())
        self.assertTrue((profile / "store/core/bin/agent").is_file())

    def test_failed_copy_never_publishes_a_partial_installation(self):
        def fail(argv, cwd=None):
            if argv[0] == "nix-store":
                return self.execute(argv, cwd)
            destination = Path(argv[argv.index("--target-directory") + 1])
            (destination / "incomplete").write_bytes(b"partial")
            raise RuntimeError("copy failed")
        bundles = {"core": self.bundle(self.core)}
        with self.assertRaisesRegex(RuntimeError, "copy failed"):
            self.install(bundles, run=fail)
        self.assertEqual(list((self.state / "installed").iterdir()), [])
        self.assertEqual((self.core / "bin/agent").read_bytes(), b"agent-runtime")

    def test_cancelled_copy_never_publishes_and_preserves_prior_generation(self):
        previous = self.install({"core": self.bundle(self.core)})
        def cancel():
            raise RuntimeError("cancelled")
        bundles = {"core": self.bundle(self.core), "optional": self.bundle(self.optional)}
        with self.assertRaisesRegex(RuntimeError, "cancelled"):
            self.install(bundles, cancel=cancel)
        self.assertEqual(list((self.state / "installed").iterdir()), [previous])
        self.assertEqual((previous / "store/core/bin/agent").read_bytes(), b"agent-runtime")

    def test_untrusted_or_incomplete_closure_is_rejected_before_copying(self):
        bundles = {"core": self.bundle(self.core)}
        for output in ("/home/user/private\n", str(self.dependency) + "\n"):
            with self.subTest(output=output), self.assertRaisesRegex(ValueError, "closure"):
                self.install(bundles, run=lambda *args, **kwargs: output)
        self.assertEqual(self.copies, 0)
        self.assertFalse((self.state / "installed").exists())

    def test_symlinked_cache_and_changed_manifest_are_refused(self):
        outside = self.root / "outside"
        outside.mkdir()
        (self.state / "installed").symlink_to(outside)
        bundles = {"core": self.bundle(self.core)}
        with self.assertRaisesRegex(ValueError, "owner-only"):
            self.install(bundles)
        self.assertEqual(list(outside.iterdir()), [])
        (self.state / "installed").unlink()
        profile = self.install({"core": self.bundle(self.core)})
        (profile / "manifest.json").write_text(json.dumps({"version": 999}))
        with self.assertRaisesRegex(ValueError, "incomplete or changed"):
            self.install(bundles)


if __name__ == "__main__":
    unittest.main()
