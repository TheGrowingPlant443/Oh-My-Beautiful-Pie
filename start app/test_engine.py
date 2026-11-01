"""No host engine needed: exercise policy decisions and HTTP framing on Unix sockets."""

import asyncio
import json
from pathlib import Path
import socket
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import AsyncMock, Mock, patch

import h11

import engine

IMAGE = "docker.io/library/postgres:17@sha256:" + "a" * 64
SANDBOX = "fixture"


def owned(role=None, name="omp-q-test"):
    labels = {engine.SANDBOX_LABEL: SANDBOX}
    if role:
        labels[engine._ROLE] = role
    return {"Name": name, "Config": {"Labels": labels}}


def container():
    return {"name": "omp-q-sonar", "image": IMAGE,
            "volumes": [{"Name": "omp-q-volume", "Dest": "/data", "Options": None}],
            "Networks": {"omp-q-net": {"aliases": ["omp-quality-db"], "interface_name": ""}},
            "netns": {"nsmode": "bridge"}, "portmappings": [
                {"host_ip": "127.0.0.1", "host_port": 9100, "container_port": 9000}],
            "mounts": [{"destination": "/tmp", "type": "tmpfs", "source": "tmpfs"}]}


class FilterTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.objects = {
            ("container", "db"): (200, owned("quality-db")),
            ("container", "scan"): (200, owned("quality-scan")),
            ("container", "other"): (200, {"Config": {"Labels": {}}}),
            ("volume", "omp-q-volume"): (200, {"Name": "omp-q-volume", "Labels": {engine.SANDBOX_LABEL: SANDBOX}}),
            ("network", "omp-q-net"): (200, {"name": "omp-q-net", "labels": {engine.SANDBOX_LABEL: SANDBOX}}),
        }

        async def inspect(kind, ident):
            return self.objects.get((kind, ident), (404, None))

        self.filter = engine.EngineFilter(SANDBOX, {IMAGE}, 9100, inspect)

    async def authorize(self, method, path, body=None, query=None):
        return await self.filter.authorize(method, "/v5.8.9" + path, query or {}, body)

    async def test_pull_is_digest_allowlisted_and_rejects_host_options(self):
        self.assertTrue((await self.authorize("POST", "/libpod/images/pull", query={"reference": [IMAGE]})).allowed)
        for query in ({"reference": ["docker.io/library/alpine"]},
                      {"reference": [IMAGE, IMAGE]}, {"reference": [IMAGE], "authfile": ["/home/token"]},
                      {"reference": [IMAGE], "certDir": ["/home"]},
                      {"reference": [IMAGE], "tlsVerify": ["false"]},
                      {"reference": [IMAGE], "destination": ["dir:/tmp/root"]}):
            with self.subTest(query=query):
                self.assertEqual((await self.authorize("POST", "/libpod/images/pull", query=query)).status, 403)

    async def test_valid_container_is_owned_and_proxy_disabled(self):
        body = container()
        decision = await self.authorize("POST", "/libpod/containers/create", body)
        self.assertTrue(decision.allowed)
        self.assertEqual(decision.body["labels"][engine.SANDBOX_LABEL], SANDBOX)
        self.assertIs(decision.body["httpproxy"], False)
        self.assertNotIn("labels", body)

    async def test_cli_create_metadata_is_validated_then_discarded(self):
        body = container()
        command = ["podman", "create", "--env", "TOKEN=do-not-log-this"]
        body["containerCreateCommand"] = command
        decision = await self.authorize("POST", "/libpod/containers/create", body)
        self.assertTrue(decision.allowed)
        self.assertNotIn("containerCreateCommand", decision.body)
        self.assertEqual(body["containerCreateCommand"], command)
        for value in ("not-a-list", None, [1], [{"TOKEN": "do-not-log-this"}]):
            with self.subTest(value=value):
                body["containerCreateCommand"] = value
                decision = await self.authorize("POST", "/libpod/containers/create", body)
                self.assertEqual(decision.status, 403)
                self.assertNotIn("do-not-log-this", decision.reason)
        body.pop("containerCreateCommand")
        body["containercreatecommand"] = []
        self.assertEqual((await self.authorize("POST", "/libpod/containers/create", body)).status, 403)

    async def test_option_diagnostics_identify_keys_and_types_not_values(self):
        body = container()
        body["env"] = {"TOKEN": "do-not-log-this"}
        body["unexpected"] = {"password": "do-not-log-this"}
        decision = await self.authorize("POST", "/libpod/containers/create", body)
        self.assertEqual(decision.status, 403)
        self.assertIn("'unexpected' (dict)", decision.reason)
        self.assertNotIn("do-not-log-this", decision.reason)
        self.assertNotIn("password", decision.reason)
        body.pop("unexpected")
        body["env_host"] = "do-not-log-this"
        decision = await self.authorize("POST", "/libpod/containers/create", body)
        self.assertEqual(decision.status, 403)
        self.assertIn("'env_host' (str)", decision.reason)
        self.assertNotIn("do-not-log-this", decision.reason)

    async def test_captured_podman_defaults_are_safe(self):
        body = container()
        body.update({"raw_image_name": IMAGE, "env_host": False, "httpproxy": True,
                     "entrypoint": ["sh"], "command": ["-c", "sleep 300"], "env": {"X": "1"},
                     "terminal": False, "stdin": False, "stop_timeout": 10, "log_configuration": {},
                     "restart_policy": "no", "restart_tries": 0, "systemd": "true", "sdnotifyMode": "container",
                     "pidns": {}, "utsns": {}, "remove": False, "init_container_type": "", "unsetenvall": False,
                     "manage_password": True, "image_volume_mode": "anonymous", "init": False,
                     "ipcns": {}, "work_dir": "/", "volatile": False, "privileged": False, "user": "999",
                     "seccomp_policy": "default", "userns": {}, "idmappings": {"HostUIDMapping": True,
                         "HostGIDMapping": True, "UIDMap": None, "GIDMap": None, "AutoUserNs": False,
                         "AutoUserNsOpts": {"Size": 0, "InitialSize": 0, "PasswdFile": "", "GroupFile": "",
                                            "AdditionalUIDMappings": None, "AdditionalGIDMappings": None}},
                     "read_only_filesystem": False, "read_write_tmpfs": False, "umask": "0022", "cgroupns": {},
                     "publish_image_ports": False, "use_image_resolve_conf": False, "use_image_hostname": False,
                     "use_image_hosts": False, "resource_limits": {"memory": {"limit": 1073741824, "swap": 2147483648},
                                                                  "cpu": {"quota": 100000, "period": 100000}},
                     "r_limits": [{"type": "nofile", "hard": 1024, "soft": 1024}], "healthconfig": {},
                     "healthLogDestination": "local", "healthMaxLogCount": 5, "healthMaxLogSize": 500})
        body["volumes"][0].update(IsAnonymous=False, SubPath="")
        self.assertTrue((await self.authorize("POST", "/libpod/containers/create", body)).allowed)

    async def test_rejects_container_escape_options(self):
        changes = [
            {"mounts": [{"type": "bind", "source": "/", "destination": "/host"}]},
            {"mounts": [{"type": "tmpfs", "source": "/home", "destination": "/tmp"}]},
            {"mounts": [{"type": "tmpfs", "destination": "/tmp", "options": ["bind"]}]},
            {"mounts": [{"type": "tmpfs", "destination": "/tmp", "options": ["size=١٠"]}]},
            {"privileged": True}, {"cap_add": ["SYS_ADMIN"]}, {"devices": [{"path": "/dev/sda"}]},
            {"pidns": {"nsmode": "host"}}, {"ipcns": {"nsmode": "host"}},
            {"netns": {"nsmode": "host"}}, {"netns": {"nsmode": "path", "value": "/proc/1/ns/net"}},
            {"userns": {"nsmode": "host"}}, {"userns": {"nsmode": "private", "value": "/proc/1/ns/user"}},
            {"Networks": {"host-net": {}}}, {"Networks": {"omp-q-net": {"static_ips": ["10.0.0.1"]}}},
            {"portmappings": [{"host_ip": "127.0.0.1", "host_port": 9000, "container_port": 9000}]},
            {"image": "alpine"}, {"raw_image_name": "alpine"}, {"name": "host-service"},
            {"log_configuration": {"path": "/home/victim", "driver": "k8s-file"}},
            {"healthLogDestination": "/home/victim"}, {"env_host": True}, {"pod": "host-pod"},
            {"annotations": {"run.oci.hooks": "/host/script"}}, {"security_opt": ["unconfined"]},
            {"idmappings": {"AutoUserNsOpts": {"PasswdFile": "/etc/passwd"}}},
            {"volumes": [{"Name": "omp-q-volume", "Dest": "/data", "Options": ["bind"]}]},
            {"volumes": [{"Name": "not-owned", "Dest": "/data"}]},
            {"resource_limits": {"devices": [{"allow": True}]}}, {"future_host_option": True},
        ]
        for change in changes:
            with self.subTest(change=change):
                body = container()
                body.update(change)
                self.assertEqual((await self.authorize("POST", "/libpod/containers/create", body)).status, 403)

    async def test_network_and_volume_creation(self):
        network = await self.authorize("POST", "/libpod/networks/create", {
            "name": "omp-q-new", "id": "", "driver": "bridge", "created": "0001-01-01T00:00:00Z",
            "ipv6_enabled": False, "internal": False, "dns_enabled": True, "labels": {"a": "b"}},
            query={"ignoreIfExists": ["false"]})
        self.assertTrue(network.allowed)
        self.assertIs(network.body["internal"], True)
        self.assertEqual(network.body["labels"][engine.SANDBOX_LABEL], SANDBOX)
        volume = await self.authorize("POST", "/libpod/volumes/create", {
            "Name": "omp-q-new", "Driver": "local", "Label": {"original": "yes"}, "Labels": None,
            "Options": {}, "IgnoreIfExists": False, "UID": None, "GID": None})
        self.assertTrue(volume.allowed)
        self.assertEqual(volume.body["Label"], volume.body["Labels"])
        self.assertEqual(volume.body["Label"]["original"], "yes")
        self.assertEqual(volume.body["Labels"][engine.SANDBOX_LABEL], SANDBOX)
        lower = await self.authorize("POST", "/libpod/volumes/create", {"name": "omp-q-lower"})
        self.assertEqual(lower.body["name"], "omp-q-lower")
        for path, body in (("volumes", {"Name": "omp-q-new", "Options": {"device": "/home"}}),
                           ("volumes", {"Name": "omp-q-new", "Driver": "nfs"}),
                           ("volumes", {"Name": "omp-q-new", "Driver": "local", "driver": "nfs"}),
                           ("volumes", {"Name": "omp-q-new", "name": "host-volume"}),
                           ("networks", {"name": "omp-q-new", "options": {"isolate": "false"}}),
                           ("networks", {"name": "omp-q-new", "network_interface": "eth0"}),
                           ("networks", {"name": "omp-q-new", "driver": "macvlan"})):
            with self.subTest(body=body):
                self.assertEqual((await self.authorize("POST", f"/libpod/{path}/create", body)).status, 403)

    async def test_exec_archive_and_missing_objects(self):
        decision = await self.authorize("POST", "/libpod/containers/db/exec", {"Cmd": engine.PG_READY})
        self.assertTrue(decision.allowed)
        self.assertTrue(decision.track_exec)
        for ident, body in (("scan", {"Cmd": engine.PG_READY}), ("db", {"Cmd": ["sh"]}),
                            ("db", {"Cmd": engine.PG_READY, "Privileged": True}),
                            ("db", {"Cmd": engine.PG_READY, "User": "0"})):
            self.assertFalse((await self.authorize("POST", f"/libpod/containers/{ident}/exec", body)).allowed)
        self.assertFalse((await self.authorize("PUT", "/libpod/containers/db/archive")).allowed)
        self.assertTrue((await self.authorize("PUT", "/libpod/containers/scan/archive")).allowed)
        missing = await self.authorize("GET", "/libpod/containers/other/json")
        self.assertEqual((missing.status, missing.reason), (404, "no such container"))
        self.objects[("container", "forged")] = (200, owned(name="host-container"))
        self.assertEqual((await self.authorize("GET", "/libpod/containers/forged/json")).status, 404)
        for action, method in (("start", "POST"), ("resize", "POST"), ("remove", "POST"), ("json", "GET")):
            self.assertFalse((await self.authorize(method, f"/libpod/exec/session/{action}")).allowed)
            self.filter.exec_ids.add("session")
            self.assertTrue((await self.authorize(method, f"/libpod/exec/session/{action}")).allowed)
            self.filter.exec_ids.clear()

    async def test_list_filter_replaces_conflicting_sandbox_label(self):
        labels = ["app=quality", engine.SANDBOX_LABEL + "=victim", engine.SANDBOX_LABEL]
        decision = await self.authorize("GET", "/libpod/containers/json", query={"filters": [json.dumps({"label": labels})]})
        self.assertEqual(json.loads(decision.query["filters"][0])["label"], ["app=quality", engine.SANDBOX_LABEL + "=" + SANDBOX])
        self.assertEqual(labels[1], engine.SANDBOX_LABEL + "=victim")
        self.assertEqual((await self.authorize("POST", "/libpod/containers/x/attach")).status, 403)
        self.assertFalse((await self.authorize("DELETE", "/libpod/containers/db", query={"depend": ["true"]})).allowed)
        self.assertTrue((await self.authorize("DELETE", "/libpod/containers/db", query={"depend": ["false"]})).allowed)

    async def test_captured_podman_query_rows(self):
        rows = [
            ("GET", f"/libpod/images/{IMAGE}/json", "size=false"),
            ("POST", "/libpod/images/pull", "alltags=false&arch=&authfile=&os=&policy=missing&quiet=false&reference=" + IMAGE + "&variant="),
            ("GET", "/libpod/containers/json", "all=true&external=false&filters=%7B%7D&namespace=false&size=false&sync=false"),
            ("POST", "/libpod/containers/db/start", "detachkeys=ctrl-p%2Cctrl-q"),
            ("GET", "/libpod/containers/db/exists", "external=false"),
            ("PUT", "/libpod/containers/scan/archive", "copyUIDGID=true&nooverwritedirnondir=true&path="),
            ("DELETE", "/libpod/containers/db", "depend=false&force=true&ignore=true&volumes=false"),
        ]
        for method, path, raw_query in rows:
            with self.subTest(method=method, path=path, query=raw_query):
                decision = await self.authorize(method, path, query=engine.parse_qs(raw_query, keep_blank_values=True))
                self.assertTrue(decision.allowed, decision.reason)
                if path == "/libpod/containers/json":
                    self.assertIn(engine.SANDBOX_LABEL + "=" + SANDBOX, json.loads(decision.query["filters"][0])["label"])

    async def test_capture_defaults_cannot_enable_host_options(self):
        rows = [
            ("GET", f"/libpod/images/{IMAGE}/json", {"size": ["true"]}),
            ("GET", "/libpod/containers/json", {"external": ["true"]}),
            ("GET", "/libpod/containers/json", {"namespace": ["true"]}),
            ("GET", "/libpod/containers/db/exists", {"external": ["true"]}),
            ("DELETE", "/libpod/containers/db", {"volumes": ["true"]}),
            ("DELETE", "/libpod/containers/db", {"depend": ["true"]}),
        ]
        for key, value in (("authfile", "/home/token"), ("alltags", "true"), ("arch", "amd64"), ("os", "linux"), ("variant", "v8")):
            rows.append(("POST", "/libpod/images/pull", {"reference": [IMAGE], key: [value]}))
        for method, path, query in rows:
            with self.subTest(path=path, query=query):
                self.assertEqual((await self.authorize(method, path, query=query)).status, 403)

    async def test_version_and_tmpfs_numbers_remain_ascii_only(self):
        decision = await self.filter.authorize("GET", "/v٥.٨.٩/libpod/info", {}, None)
        self.assertEqual(decision.status, 403)
        body = container()
        body["mounts"][0]["options"] = ["size=10M", "mode=1777"]
        self.assertTrue((await self.authorize("POST", "/libpod/containers/create", body)).allowed)

    async def test_unknown_falsy_fields_and_case_aliases_are_rejected(self):
        for value in (False, None, 0, "", [], {}):
            with self.subTest(value=value):
                body = container()
                body["unchecked"] = value
                self.assertEqual((await self.authorize("POST", "/libpod/containers/create", body)).status, 403)
        for key in ("Internal", "INTERNAL", "labelſ", "unchecked"):
            with self.subTest(network_key=key):
                body = {"name": "omp-q-new", "internal": False, key: False}
                self.assertEqual((await self.authorize("POST", "/libpod/networks/create", body)).status, 403)
        for key in ("HOST_IP", "Host_Ip", "hoſt_ip"):
            with self.subTest(port_key=key):
                body = container()
                body["portmappings"][0][key] = ""
                self.assertEqual((await self.authorize("POST", "/libpod/containers/create", body)).status, 403)
        for key in ("networKs", "NETWORKS", "USERNS", "Env_Host"):
            with self.subTest(container_key=key):
                body = container()
                body[key] = {}
                self.assertEqual((await self.authorize("POST", "/libpod/containers/create", body)).status, 403)
        for key in ("name", "Name", "naMe", "NaMe", "naME", "nAme", "nAmE", "nAMe", "nAME", "NAME"):
            if key == "name":
                continue
            with self.subTest(case_variant=key):
                body = container()
                body[key] = ""
                self.assertEqual((await self.authorize("POST", "/libpod/containers/create", body)).status, 403)

    async def test_readiness_exec_captured_defaults_remain_supported(self):
        body = {"User": "", "Privileged": False, "Tty": False, "AttachStdin": False,
                "AttachStderr": True, "AttachStdout": True, "DetachKeys": "ctrl-p,ctrl-q",
                "Env": [], "WorkingDir": "", "Cmd": engine.PG_READY, "Detach": False}
        decision = await self.authorize("POST", "/libpod/containers/db/exec", body)
        self.assertTrue(decision.allowed)
        self.assertTrue(decision.track_exec)
        body["Privileged"] = None
        self.assertEqual((await self.authorize("POST", "/libpod/containers/db/exec", body)).status, 403)

    async def test_default_network_fallback_is_blocked(self):
        for mode in (None, "", "bridge", "private"):
            for mapping in ("omitted", {}, None):
                with self.subTest(mode=mode, mapping=mapping):
                    body = container()
                    body.pop("Networks")
                    if mapping != "omitted":
                        body["Networks"] = mapping
                    if mode is None:
                        body.pop("netns")
                    else:
                        body["netns"] = {"nsmode": mode}
                    self.assertEqual((await self.authorize("POST", "/libpod/containers/create", body)).status, 403)
        body = container()
        body.pop("Networks")
        body["netns"] = {"nsmode": "none"}
        decision = await self.authorize("POST", "/libpod/containers/create", body)
        self.assertTrue(decision.allowed)
        self.assertEqual(decision.body["netns"], {"nsmode": "none"})
        self.assertEqual(decision.body["Networks"], {})

    async def test_network_aliases_are_canonical_and_conflicts_fail(self):
        for mode in ("bridge", "private", ""):
            body = container()
            body["networks"] = body.pop("Networks")
            body["netns"] = {"nsmode": mode}
            original = json.dumps(body, sort_keys=True)
            decision = await self.authorize("POST", "/libpod/containers/create", body)
            self.assertTrue(decision.allowed)
            self.assertNotIn("networks", decision.body)
            self.assertEqual(decision.body["Networks"], body["networks"])
            self.assertEqual(decision.body["netns"], {"nsmode": "bridge"})
            self.assertEqual(json.dumps(body, sort_keys=True), original)
        for alias in ({}, None, {"unowned": {}}):
            with self.subTest(alias=alias):
                body = container()
                body["networks"] = alias
                self.assertEqual((await self.authorize("POST", "/libpod/containers/create", body)).status, 403)
                body["Networks"], body["networks"] = body["networks"], body["Networks"]
                self.assertEqual((await self.authorize("POST", "/libpod/containers/create", body)).status, 403)
        body = container()
        body["networks"] = body["Networks"]
        decision = await self.authorize("POST", "/libpod/containers/create", body)
        self.assertTrue(decision.allowed)
        self.assertNotIn("networks", decision.body)
        body["netns"] = {"nsmode": "none"}
        self.assertEqual((await self.authorize("POST", "/libpod/containers/create", body)).status, 403)


class SeedTests(unittest.TestCase):
    def test_new_and_existing_configuration(self):
        with tempfile.TemporaryDirectory() as directory, patch("engine.subprocess.run") as run:
            home = Path(directory)
            self.assertEqual(engine.seed_client(home), 9100)
            path = home / ".config/dev.udvikler.omp-manager/code-quality/config.json"
            connections = home / ".config/containers/podman-connections.json"
            expected = {"version": 1, "config": {"version": 1, "mode": "sonarqubeCommunity", "runtime": {"kind": "podman", "connection": "ombp-engine"}, "communityPort": 9100, "enterpriseUrl": ""}, "revision": 0, "activated": False, "startAtLaunch": False}
            self.assertEqual(json.loads(path.read_text()), expected)
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            self.assertEqual(connections.stat().st_mode & 0o777, 0o600)
            self.assertEqual((home / ".config/containers/containers.conf").read_text(), "[engine]\nremote = true\n")
            self.assertEqual(json.loads(connections.read_text()), {
                "Connection": {"Default": "ombp-engine", "Connections": {"ombp-engine": {
                    "URI": "unix:///engine/podman.sock", "Identity": "", "IsMachine": False}}},
                "Farm": {"Default": "", "List": {}}})
            expected["config"]["communityPort"] = 9200
            untouched = json.dumps(expected, indent=2)
            path.write_text(untouched)
            previous_connections = connections.read_bytes()
            self.assertEqual(engine.seed_client(home), 9200)
            self.assertEqual(path.read_text(), untouched)
            self.assertEqual(connections.read_bytes(), previous_connections)
            for invalid in (True, 80, 65536, "9200"):
                expected["config"]["communityPort"] = invalid
                path.write_text(json.dumps(expected))
                self.assertEqual(engine.seed_client(home), 9100)
            run.assert_not_called()

    def test_saved_port_cannot_collide_with_website_proxy(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            engine.seed_client(home)
            path = home / ".config/dev.udvikler.omp-manager/code-quality/config.json"
            config = json.loads(path.read_text())
            config["config"]["communityPort"] = 18080
            unchanged = json.dumps(config)
            path.write_text(unchanged)
            with self.assertRaisesRegex(RuntimeError, "18080.*reserved.*website proxy"):
                engine.seed_client(home)
            self.assertEqual(path.read_text(), unchanged)

    def test_connection_merge_preserves_other_entries_and_ignores_helpers(self):
        with tempfile.TemporaryDirectory() as directory, patch("engine.subprocess.run") as run:
            home = Path(directory)
            containers = home / ".config/containers"
            containers.mkdir(parents=True)
            configuration = '[engine]\nhelper_binaries_dir = ["/outside/malicious"]\n'
            (containers / "containers.conf").write_text(configuration)
            path = containers / "podman-connections.json"
            other = {"URI": "ssh://example.test/run/podman.sock", "Identity": "/sandbox/key", "IsMachine": False}
            farm = {"Default": "custom", "List": {"custom": ["other"]}}
            data = {"Connection": {"Default": "other", "Connections": {"other": other}},
                    "Farm": farm, "Unrelated": {"value": 1}}
            path.write_text(json.dumps(data))
            engine.seed_client(home)
            actual = json.loads(path.read_text())
            self.assertEqual(actual["Connection"]["Connections"]["other"], other)
            self.assertEqual(actual["Connection"]["Default"], "ombp-engine")
            self.assertEqual(actual["Farm"], farm)
            self.assertEqual(actual["Unrelated"], {"value": 1})
            self.assertEqual((containers / "containers.conf").read_text(), configuration)
            run.assert_not_called()

    def test_connection_errors_preserve_malformed_configuration(self):
        malformed = ["{", "[]", '{"Connection":[]}', '{"Connection":{"Connections":[]}}',
                     '{"Connection":{"Connections":{"other":[]}}}', '{"Farm":{"List":[]}}',
                     '{"Connection":{"Default":false}}']
        for content in malformed:
            with self.subTest(content=content), tempfile.TemporaryDirectory() as directory:
                home = Path(directory)
                path = home / ".config/containers/podman-connections.json"
                path.parent.mkdir(parents=True)
                path.write_text(content)
                with self.assertRaisesRegex(RuntimeError, "Could not configure the sandbox Podman connection"):
                    engine.seed_client(home)
                self.assertEqual(path.read_text(), content)

    def test_symlink_components_fail_before_modifying_outside_files(self):
        directories = {".config", ".config/containers", ".config/dev.udvikler.omp-manager",
                       ".config/dev.udvikler.omp-manager/code-quality"}
        files = {".config/containers/containers.conf", ".config/containers/podman-connections.json",
                 ".config/dev.udvikler.omp-manager/code-quality/config.json"}
        for relative in sorted(directories | files):
            for dangling in (False, True):
                with self.subTest(path=relative, dangling=dangling), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    home, outside = root / "home", root / "outside"
                    home.mkdir()
                    outside.mkdir()
                    sentinel = outside / "sentinel"
                    sentinel.write_text("untouched")
                    link = home / relative
                    link.parent.mkdir(parents=True, exist_ok=True)
                    target = outside if relative in directories else outside / "target"
                    if dangling:
                        target = root / "missing"
                    elif relative in files:
                        target.write_text("untouched")
                    previous_names = sorted(p.name for p in outside.iterdir())
                    link.symlink_to(target, target_is_directory=relative in directories)
                    with self.assertRaisesRegex(RuntimeError, "Unsafe sandbox configuration"):
                        engine.seed_client(home)
                    self.assertEqual(sentinel.read_text(), "untouched")
                    self.assertEqual(sorted(p.name for p in outside.iterdir()), previous_names)
                    if relative in files and not dangling:
                        self.assertEqual(target.read_text(), "untouched")
                    if dangling:
                        self.assertFalse(target.exists())

    def test_pinned_images_require_all_verified_digest_references(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory)
            path = source / "bridge/code-quality/images.lock.json"
            path.parent.mkdir(parents=True)
            values = {key: IMAGE.replace("a" * 64, digit * 64) for key, digit in zip(("sonarqubeCommunity", "postgres", "scannerCli"), "abc")}
            path.write_text(json.dumps(values))
            self.assertEqual(engine.pinned_images(source), set(values.values()))
            values["postgres"] = "postgres:latest"
            path.write_text(json.dumps(values))
            with self.assertRaisesRegex(ValueError, "no pinned SonarQube images"):
                engine.pinned_images(source)
            values["postgres"] = "dir:/home/user@sha256:" + "a" * 64
            path.write_text(json.dumps(values))
            with self.assertRaisesRegex(ValueError, "no pinned SonarQube images"):
                engine.pinned_images(source)


class GatewayTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.directory = Path(self.temporary.name)
        self.gateway = engine.EngineGateway(None, self.directory, {IMAGE}, 9100, None)
        self.gateway.upstream_path.parent.mkdir()
        self.requests = []
        self.trailing = []
        self.completed = asyncio.Event()
        self.response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\n{}"
        self.upstream_tasks = set()
        self.upstream = await asyncio.start_unix_server(self.upstream_handler, path=str(self.gateway.upstream_path))
        self.path = self.directory / "client.sock"
        self.server = await asyncio.start_unix_server(self.gateway._handle, path=str(self.path))

        async def inspect(kind, ident):
            return 200, {"Name": "omp-q-fixture", "Config": {"Labels": {
                engine.SANDBOX_LABEL: self.gateway.filter.sandbox_id,
                engine._ROLE: "quality-db" if ident == "db" else "quality-scan"}}}

        self.gateway.filter.inspect = inspect

    async def asyncTearDown(self):
        for server in (self.server, self.upstream):
            server.close()
            await server.wait_closed()
        tasks = self.gateway._handlers | self.upstream_tasks
        for task in tuple(tasks):
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        self.temporary.cleanup()

    async def upstream_handler(self, reader, writer):
        task = asyncio.current_task()
        self.upstream_tasks.add(task)
        try:
            connection = h11.Connection(h11.SERVER)
            request = await engine._event(connection, reader)
            data = bytearray()
            while True:
                event = await engine._event(connection, reader)
                if isinstance(event, h11.Data):
                    data.extend(event.data)
                elif isinstance(event, h11.EndOfMessage):
                    break
                else:
                    raise AssertionError(event)
            self.requests.append((request, bytes(data)))
            writer.write(self.response)
            await writer.drain()
            if self.response.startswith(b"HTTP/1.1 101"):
                # Only bytes after a confirmed upgrade may be forwarded raw.
                self.upgrade_payload = await reader.readexactly(4)
                writer.write(b"pong")
                await writer.drain()
            else:
                self.trailing.append(await reader.read())
        finally:
            await engine._close(writer)
            self.upstream_tasks.discard(task)
            self.completed.set()

    async def exchange(self, data):
        reader, writer = await asyncio.open_unix_connection(str(self.path))
        try:
            writer.write(data)
            await writer.drain()
            async with asyncio.timeout(3):
                response = await reader.read()
                if self.requests:
                    await self.completed.wait()
                return response
        finally:
            await engine._close(writer)

    async def test_pipeline_is_dropped_and_close_is_client_visible(self):
        response = await self.exchange(b"GET /v5.8.9/libpod/info HTTP/1.1\r\nHost: localhost\r\nX-HTTP-Method-Override: DELETE\r\nX-Registry-Auth: secret\r\n\r\nPOST /v5.8.9/libpod/containers/host/stop HTTP/1.1\r\nHost: localhost\r\n\r\n")
        self.assertEqual(len(self.requests), 1)
        request, _ = self.requests[0]
        self.assertEqual(request.target, b"/v5.8.9/libpod/info")
        self.assertEqual(dict(request.headers)[b"connection"], b"close")
        self.assertNotIn(b"x-http-method-override", dict(request.headers))
        self.assertNotIn(b"x-registry-auth", dict(request.headers))
        self.assertIn(b"connection: close", response.lower())
        self.assertEqual(response.count(b"HTTP/1.1"), 1)
        self.assertEqual(self.trailing, [b""])

    async def test_chunked_archive_streams_without_pipeline_bytes(self):
        payload = b"PUT /v5.8.9/libpod/containers/scan/archive?copyUIDGID=true&nooverwritedirnondir=true&path=%2Ftmp HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n4\r\ntest\r\n3\r\ntar\r\n0\r\n\r\nDELETE /v5.8.9/libpod/containers/host HTTP/1.1\r\nHost: localhost\r\n\r\n"
        response = await self.exchange(payload)
        self.assertIn(b"200", response)
        self.assertEqual(len(self.requests), 1)
        self.assertEqual(self.requests[0][1], b"testtar")
        self.assertEqual(dict(self.requests[0][0].headers)[b"transfer-encoding"], b"chunked")
        self.assertEqual(self.trailing, [b""])

    async def test_exec_creation_tracks_chunked_json_response(self):
        self.response = b'HTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\n\r\n10\r\n{"Id":"session"}\r\n0\r\n\r\n'
        body = json.dumps({"Cmd": engine.PG_READY}).encode()
        response = await self.exchange(b"POST /v5.8.9/libpod/containers/db/exec HTTP/1.1\r\nHost: localhost\r\nContent-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
        self.assertIn(b"201", response)
        self.assertIn("session", self.gateway.filter.exec_ids)

    async def test_confirmed_upgrade_relays_trailing_data(self):
        self.gateway.filter.exec_ids.add("session")
        self.response = b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\nready"
        reader, writer = await asyncio.open_unix_connection(str(self.path))
        try:
            writer.write(b'POST /v5.8.9/libpod/exec/session/start HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: tcp\r\nContent-Length: 2\r\n\r\n{}ping')
            await writer.drain()
            async with asyncio.timeout(3):
                response = await reader.read()
            self.assertIn(b"101", response)
            self.assertTrue(response.endswith(b"readypong"), response)
            self.assertEqual(self.upgrade_payload, b"ping")
        finally:
            await engine._close(writer)

    async def test_non_upgrade_never_relays_raw_trailing_bytes(self):
        self.gateway.filter.exec_ids.add("session")
        response = await self.exchange(b'POST /v5.8.9/libpod/exec/session/start HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: tcp\r\nContent-Length: 2\r\n\r\n{}DANGEROUS')
        self.assertIn(b"200", response)
        self.assertEqual(self.requests[0][1], b"{}")
        self.assertEqual(self.trailing, [b""])

    async def test_malformed_and_forbidden_requests_fail_closed(self):
        requests = [
            b"GET http://localhost/v5.8.9/libpod/info HTTP/1.1\r\nHost: localhost\r\n\r\n",
            b"POST /v5.8.9/libpod/containers/create HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\n\r\n{",
            b"GET /v5.8.9/libpod/info HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n",
            b"GET /v5.8.9/libpod/../info HTTP/1.1\r\nHost: localhost\r\n\r\n",
            b"GET /v5.8.9/libpod/%252e%252e/info HTTP/1.1\r\nHost: localhost\r\n\r\n",
            b"GET /v5.8.9/libpod/info HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}",
            b"POST /v5.8.9/libpod/containers/create HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1048577\r\n\r\n",
        ]
        for request in requests:
            with self.subTest(request=request):
                response = await self.exchange(request)
                self.assertRegex(response.split(b"\r\n", 1)[0].decode(), r"HTTP/1.1 (403|413)")
        self.assertEqual(self.requests, [])
        self.assertIn(" DENY ", (self.directory / "filter.log").read_text())

    async def test_inspect_reads_framed_upstream_json(self):
        self.response = b'HTTP/1.1 200 OK\r\nContent-Length: 16\r\n\r\n{"Name":"owned"}'
        status, data = await self.gateway._inspect("container", "db")
        self.assertEqual(status, 200)
        self.assertEqual(data, {"Name": "owned"})
        await self.completed.wait()
        self.assertEqual(self.requests[0][0].target, b"/v5.0.0/libpod/containers/db/json")
        self.assertEqual(dict(self.requests[0][0].headers)[b"connection"], b"close")


class SonarRelayTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.gateway = engine.EngineGateway(None, Path("/unused"), {IMAGE}, 1234, None)
        self.gateway._upstream_json = AsyncMock(return_value=(200, [{"Id": "sonar"}]))
        self.data = owned("quality-sonar")
        self.data["Config"]["Labels"][engine.SANDBOX_LABEL] = self.gateway.filter.sandbox_id
        self.data["State"] = {"Running": True}
        self.data["NetworkSettings"] = {"Ports": {"9000/tcp": [{"HostIp": "127.0.0.1", "HostPort": "1234"}]}}
        self.gateway.filter.inspect = AsyncMock(return_value=(200, self.data))

    async def test_host_loopback_is_closed_without_owned_running_publisher(self):
        invalid = [
            {"State": {"Running": False}},
            {"Name": "host-sonar"},
            {"Config": {"Labels": {engine.SANDBOX_LABEL: self.gateway.filter.sandbox_id, engine._ROLE: "quality-scan"}}},
            {"Config": {"Labels": {engine.SANDBOX_LABEL: "other", engine._ROLE: "quality-sonar"}}},
            {"NetworkSettings": {"Ports": {"9000/tcp": [{"HostIp": "127.0.0.1", "HostPort": "9000"}]}}},
        ]
        for change in invalid:
            data = dict(self.data, **change)
            self.gateway.filter.inspect.return_value = (200, data)
            with self.subTest(change=change), patch("engine.asyncio.open_connection", new_callable=AsyncMock) as connect:
                with self.assertRaises(OSError):
                    await self.gateway._connect_sonar()
                connect.assert_not_called()
        self.gateway._upstream_json.return_value = (200, [])
        with patch("engine.asyncio.open_connection", new_callable=AsyncMock) as connect:
            with self.assertRaises(OSError):
                await self.gateway._connect_sonar()
            connect.assert_not_called()

    async def test_verified_publisher_is_checked_again_after_connect(self):
        remote_writer = SimpleNamespace(close=lambda: None, wait_closed=AsyncMock())
        with patch("engine.asyncio.open_connection", new_callable=AsyncMock, return_value=("reader", remote_writer)) as connect:
            result = await self.gateway._connect_sonar()
            self.assertEqual(result, ("reader", remote_writer))
            connect.assert_awaited_once_with("127.0.0.1", 1234)
            self.assertEqual(self.gateway.filter.inspect.await_count, 2)
        self.gateway._sonar_running = AsyncMock(side_effect=[True, False])
        with patch("engine.asyncio.open_connection", new_callable=AsyncMock, return_value=("reader", remote_writer)):
            with self.assertRaises(OSError):
                await self.gateway._connect_sonar()
            remote_writer.wait_closed.assert_awaited_once()


class LifecycleTests(unittest.TestCase):
    def test_stop_cancels_clients_and_only_stops_own_service(self):
        with tempfile.TemporaryDirectory() as directory:
            gateway = engine.EngineGateway(None, Path(directory), {IMAGE}, 9100, None)
            accepted = threading.Event()
            handler = gateway._handle

            async def registered(reader, writer):
                accepted.set()
                await handler(reader, writer)

            gateway._handle = registered
            process = Mock()
            process.poll.return_value = None

            def start_service(*args, **kwargs):
                gateway.upstream_path.touch()
                return process

            with patch("engine.subprocess.Popen", side_effect=start_service) as start:
                gateway.start()
                client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                client.settimeout(3)
                try:
                    client.connect(str(gateway.api_dir / "podman.sock"))
                    client.sendall(b"GET /v5.8.9/libpod/info HTTP/1.1\r\n")
                    self.assertTrue(accepted.wait(3))
                    self.assertEqual((gateway.api_dir / "podman.sock").stat().st_mode & 0o777, 0o600)
                    gateway.stop()
                    self.assertEqual(client.recv(1), b"")
                    self.assertFalse((gateway.api_dir / "podman.sock").exists())
                    self.assertFalse((gateway.api_dir / "sonar.sock").exists())
                    process.terminate.assert_called_once()
                    process.wait.assert_called_once_with(timeout=10)
                    process.kill.assert_not_called()
                    self.assertEqual(start.call_args.args[0][:4], ["podman", "system", "service", "--time=0"])
                    gateway.stop()
                    process.terminate.assert_called_once()
                finally:
                    client.close()
                    gateway.stop()


if __name__ == "__main__":
    unittest.main()
