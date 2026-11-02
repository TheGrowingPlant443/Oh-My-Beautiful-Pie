import os
from pathlib import Path
import stat
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from requirements import prepare_workspace


class WorkspaceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.path = self.root / 'workspace'

    def test_creates_owner_only_directory_without_replacing_existing_data(self):
        prepare_workspace(self.path)
        self.assertEqual(stat.S_IMODE(self.path.stat().st_mode), 0o700)
        marker = self.path / 'saved-session'
        marker.write_text('preserve')
        prepare_workspace(self.path)
        self.assertEqual(marker.read_text(), 'preserve')

    def test_rejects_symlinks_and_publicly_writable_directories(self):
        target = self.root / 'target'
        target.mkdir()
        self.path.symlink_to(target, target_is_directory=True)
        with self.assertRaises(OSError):
            prepare_workspace(self.path)
        self.path.unlink()
        self.path.mkdir(mode=0o777)
        self.path.chmod(0o777)
        with self.assertRaises(ValueError):
            prepare_workspace(self.path)
        self.assertEqual(stat.S_IMODE(self.path.stat().st_mode), 0o777)

    def test_rejects_foreign_owned_directory(self):
        self.path.mkdir()
        metadata = SimpleNamespace(st_uid=os.getuid() + 1, st_mode=self.path.stat().st_mode)
        with patch('requirements.os.fstat', return_value=metadata), self.assertRaises(ValueError):
            prepare_workspace(self.path)


if __name__ == '__main__':
    unittest.main()
