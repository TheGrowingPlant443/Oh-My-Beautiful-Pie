from pathlib import Path
import tempfile
import unittest
import sys

import catalog
from catalog import detect_gpu, find_folders, host_program, installed_programs, resolve_program, user_folders


class CatalogTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.home = self.root / "home"
        self.home.mkdir()

    def gpu(self, name, vendor, boot=None):
        device = self.root / "drm" / name / "device"
        device.mkdir(parents=True)
        (device / "vendor").write_text(vendor + "\n")
        if boot is not None:
            (device / "boot_vga").write_text(str(boot))

    def executable(self, relative):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#!/bin/sh\nexit 99\n")
        path.chmod(0o755)
        return path

    def test_gpu_prefers_boot_card_and_falls_back_deterministically(self):
        self.gpu("card0", "0x10de", 1)
        self.gpu("card1", "0x1002", 0)
        self.assertEqual(detect_gpu(self.root / "drm"), "nvidia")
        (self.root / "drm/card0/device/boot_vga").unlink()
        self.assertEqual(detect_gpu(self.root / "drm"), "amd")

    def test_gpu_supports_render_nodes_without_device_access(self):
        self.gpu("card0", "0x8086", 1)
        self.gpu("renderD128", "0x10DE")
        self.gpu("card1-HDMI-A-1", "0x1002", 1)
        self.assertEqual(detect_gpu(self.root / "drm"), "nvidia")
        (self.root / "drm/renderD128/device/vendor").unlink()
        self.assertEqual(detect_gpu(self.root / "drm"), "none")
        self.assertEqual(detect_gpu(self.root / "missing"), "none")

    def test_folders_honor_localized_xdg_paths_without_creating_them(self):
        config = self.root / "settings"
        config.mkdir()
        (config / "user-dirs.dirs").write_text(
            'XDG_DOWNLOAD_DIR="$HOME/Hentede filer"\n'
            'XDG_DOCUMENTS_DIR="${HOME}/Dokumenter" # local name\n'
            f"XDG_PICTURES_DIR='{self.home}/Mine billeder'\n"
            'XDG_DESKTOP_DIR="$HOME"\n')
        rows = user_folders(self.home, {"XDG_CONFIG_HOME": str(config)})
        folders = {row["name"]: row["path"] for row in rows}
        self.assertEqual(folders["Downloads"], str(self.home / "Hentede filer"))
        self.assertEqual(folders["Documents"], str(self.home / "Dokumenter"))
        self.assertEqual(folders["Pictures"], str(self.home / "Mine billeder"))
        self.assertEqual(folders["Home"], str(self.home))
        self.assertNotIn("Desktop", folders)
        self.assertFalse((self.home / "Hentede filer").exists())
        self.assertEqual(len({row["path"] for row in rows}), len(rows))

    def test_folders_reject_expansion_relative_and_malformed_values(self):
        config = self.home / ".config"
        config.mkdir()
        marker = self.root / "must-not-exist"
        values = ['"$OTHER/folder"', '"relative/path"', '"$HOME/../outside"',
                  f'"$(touch {marker})"', f'"`touch {marker}`"',
                  f'"{self.root}/safe";touch {marker}', '"unterminated',
                  '"$HOME/path" extra', '"$HOMEISH/path"']
        for value in values:
            with self.subTest(value=value):
                (config / "user-dirs.dirs").write_text(f"XDG_DOWNLOAD_DIR={value}\n")
                rows = user_folders(self.home, {"XDG_CONFIG_HOME": "relative-config"})
                self.assertEqual(next(row["path"] for row in rows if row["name"] == "Downloads"),
                                 str(self.home / "Downloads"))
        self.assertFalse(marker.exists())

    def test_program_resolver_rejects_non_store_missing_and_nonexecutable_files(self):
        store = self.root / "store"
        executable = self.executable("store/pkg/bin/tool")
        host = self.executable("host/bin/tool")
        self.assertEqual(resolve_program(str(executable), "", store), str(executable))
        self.assertIsNone(resolve_program(str(host), "", store))
        self.assertIsNone(resolve_program("missing", str(executable.parent), store))
        self.assertIsNone(resolve_program("./tool", str(executable.parent), store))
        executable.chmod(0o644)
        self.assertIsNone(resolve_program(str(executable), "", store))

    def test_profile_programs_win_over_launcher_runtime_path(self):
        user = self.executable("store/user/bin/tool")
        runtime = self.executable("store/runtime/bin/tool")
        profile = self.home / ".nix-profile/bin"
        profile.mkdir(parents=True)
        (profile / "tool").symlink_to(user)
        rows = installed_programs(self.home, {"PATH": str(runtime.parent)}, self.root / "store",
                                  profile_bins=[profile], application_dirs=[])
        self.assertEqual(rows, [{"name": "tool", "path": str(user)}])

    def test_host_program_resolves_profile_links_and_missing_names(self):
        executable = Path(sys.executable).resolve()
        if not executable.is_relative_to("/nix/store"):
            self.skipTest("Run through the project's Nix development shell")
        profile = self.home / ".nix-profile/bin"
        profile.mkdir(parents=True)
        (profile / "tool").symlink_to(executable)
        self.assertEqual(host_program(("missing", "tool"), self.home, {"PATH": ""}), str(executable))
        self.assertIsNone(host_program(("missing",), self.home, {"PATH": ""}))

    def test_folder_search_ranks_name_prefix_before_substring_then_shorter_path(self):
        for relative in ("work/old-udvikler", "udvikler", "a/udvikler-deep", "notes/udvikler", "udvikling"):
            (self.home / relative).mkdir(parents=True)
        rows = find_folders("UDVIK", self.home)
        self.assertEqual([row["path"] for row in rows], [str(self.home / path) for path in (
            "udvikler", "udvikling", "notes/udvikler", "a/udvikler-deep", "work/old-udvikler")])
        self.assertEqual(rows[0]["name"], "udvikler")
        self.assertEqual(find_folders(str(self.home / "udvikler"), self.home), [])

    def test_folder_search_skips_hidden_symlinked_heavy_and_private_trees(self):
        for relative in (".hidden/match", "node_modules/match", "__pycache__/match", "target/match",
                         "state/match", "real/match"):
            (self.home / relative).mkdir(parents=True)
        (self.home / "linked-match").symlink_to(self.home / "real")
        rows = find_folders("match", self.home, (str(self.home / "state"),))
        self.assertEqual(rows, [{"name": "match", "path": str(self.home / "real/match")}])

    def test_folder_search_caps_results(self):
        for index in range(25):
            (self.home / f"match{index:02}").mkdir()
        self.assertEqual(len(find_folders("match", self.home)), 20)

    def test_folder_walk_stops_at_its_entry_budget(self):
        for index in range(10):
            (self.home / f"match{index}").mkdir()
            (self.home / f"match{index}" / "child").mkdir()
        self.assertEqual(len(list(catalog._walk_folders(self.home, [], entries=5))), 5)
        self.assertEqual(len(list(catalog._walk_folders(self.home, []))), 20)

    def test_desktop_labels_honor_quoted_exec(self):
        executable = self.executable("store/pkg/bin/space tool")
        bins = self.root / "profile/bin"
        bins.mkdir(parents=True)
        (bins / "space-tool").symlink_to(executable)
        applications = self.root / "applications"
        applications.mkdir()
        text = ('[Desktop Entry]\nType=Application\nName=Friendly App\n'
                f'Exec="{executable}" --title "Two words" %U\n')
        (applications / "app.desktop").write_text(text)
        rows = installed_programs(self.home, {"PATH": str(bins)}, self.root / "store",
                                  profile_bins=[], application_dirs=[applications])
        self.assertEqual(rows, [{"name": "Friendly App", "path": str(executable)}])

    def test_desktop_name_resolution_uses_xdg_application_directories(self):
        executable = self.executable("store/pkg/bin/editor")
        data = self.root / "data"
        applications = data / "applications"
        applications.mkdir(parents=True)
        (applications / "editor.desktop").write_text(
            '[Desktop Entry]\nType=Application\nName=My Editor\nExec=editor %F\n')
        rows = installed_programs(self.home,
                                  {"PATH": str(executable.parent), "XDG_DATA_HOME": str(data),
                                   "XDG_DATA_DIRS": "relative-is-ignored"},
                                  self.root / "store", profile_bins=[])
        self.assertEqual(rows, [{"name": "My Editor", "path": str(executable)}])

    def test_hidden_malformed_wrapped_and_non_nix_desktop_entries_are_skipped(self):
        executable = self.executable("store/pkg/bin/editor")
        host = self.executable("host/bin/host-editor")
        marker = self.root / "must-not-exist"
        applications = self.root / "applications"
        applications.mkdir()
        entries = {
            "hidden": f"Type=Application\nHidden=true\nExec={executable}",
            "nodisplay": f"Type=Application\nNoDisplay=true\nExec={executable}",
            "link": f"Type=Link\nExec={executable}",
            "host": f"Type=Application\nExec={host}",
            "env": f"Type=Application\nExec=env TOOL=1 {executable}",
            "shell": f"Type=Application\nExec=$(touch {marker})",
            "field": "Type=Application\nExec=%f",
            "relative": "Type=Application\nExec=./editor",
            "badquote": 'Type=Application\nExec="unterminated',
            "badbool": f"Type=Application\nHidden=maybe\nExec={executable}",
        }
        for name, entry in entries.items():
            (applications / f"{name}.desktop").write_text(f"[Desktop Entry]\nName={name}\n{entry}\n")
        (applications / "invalid.desktop").write_text("not an ini file")
        rows = installed_programs(self.home, {"PATH": ""}, self.root / "store",
                                  profile_bins=[], application_dirs=[applications])
        self.assertEqual(rows, [])
        self.assertFalse(marker.exists())



if __name__ == "__main__":
    unittest.main()
