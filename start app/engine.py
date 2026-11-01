"""A one-request Unix HTTP gateway exposing only the sandbox's quality containers."""

import asyncio
import contextlib
from dataclasses import dataclass
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import threading
import tempfile
import time
from urllib.parse import parse_qs, quote, unquote, urlencode, urlsplit

import h11

from network import _copy, _forward

SANDBOX_LABEL = "io.ombp-start.sandbox"
CONNECTION = "ombp-engine"
DEFAULT_SONAR_PORT = 9100
PG_READY = ["pg_isready", "-U", "sonar", "-d", "sonar"]
_ROLE = "dev.udvikler.omp-manager.role"
_JSON_LIMIT = 1024 * 1024
_VERSION = re.compile(r"^/v\d[\d.]*(?=/)", re.ASCII)
_NAME = re.compile(r"omp-q-[a-zA-Z0-9][a-zA-Z0-9_.-]*\Z")
_IDENT = re.compile(r"[a-zA-Z0-9][a-zA-Z0-9_.-]*\Z")
_DIGEST = re.compile(r"docker\.io/[a-z0-9_./-]+:[a-zA-Z0-9_.-]+@sha256:[0-9a-f]{64}\Z")
_PODMAN_SOCKET = "podman.sock"
_SONAR_SOCKET = "sonar.sock"
_ENDPOINT_DENIED = "Endpoint is not allowed"


def sandbox_id(state_dir):
    return hashlib.sha256(str(state_dir).encode()).hexdigest()[:16]


def pinned_images(source):
    try:
        data = json.loads((Path(source) / "bridge/code-quality/images.lock.json").read_text())
        values = {data[key] for key in ("sonarqubeCommunity", "postgres", "scannerCli")}
        if len(values) != 3 or not all(isinstance(v, str) and _DIGEST.fullmatch(v) for v in values):
            raise ValueError("Invalid pinned images")
        return values
    except (OSError, ValueError, KeyError, TypeError) as error:
        raise ValueError("OMBP checkout has no pinned SonarQube images; revoke 'SonarQube engine' to start without Code quality") from error


def _client_paths(home):
    config = Path(home) / ".config"
    containers = config / "containers"
    quality = config / "dev.udvikler.omp-manager/code-quality"
    files = (containers / "containers.conf", containers / "podman-connections.json", quality / "config.json")
    directories = (Path(home), config, containers, quality.parent, quality)
    for path in directories:
        if path.is_symlink() or (path.exists() and not path.is_dir()):
            raise RuntimeError(f"Unsafe sandbox configuration directory: {path}")
    for path in files:
        if path.is_symlink() or (path.exists() and not path.is_file()):
            raise RuntimeError(f"Unsafe sandbox configuration file: {path}")
    return containers, files


def _write_client_json(path, data):
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, prefix=".ombp-", delete=False) as stream:
            temporary = Path(stream.name)
            json.dump(data, stream, separators=(",", ":"))
        os.replace(temporary, path)
    finally:
        if temporary:
            temporary.unlink(missing_ok=True)


def _read_client_json(path):
    if path.stat().st_size > _JSON_LIMIT:
        raise ValueError("Sandbox configuration exceeds 1 MiB")
    return _object(json.loads(path.read_text()))


def _connection_maps(data):
    connection = _object(data.setdefault("Connection", {}))
    connections = _object(connection.setdefault("Connections", {}))
    farm = _object(data.get("Farm", {}))
    _object(farm.get("List", {}))
    for section in (connection, farm):
        if not isinstance(section.get("Default", ""), str):
            raise ValueError("Expected a connection/farm default name")
    for entry in connections.values():
        if not isinstance(_object(entry).get("URI", ""), str):
            raise ValueError("Expected a connection URI string")
    return connection, connections


def _seed_connection(path):
    try:
        data = _read_client_json(path) if path.exists() else {}
        connection, connections = _connection_maps(data)
        if CONNECTION in connections:
            return
        connections[CONNECTION] = {"URI": "unix:///engine/podman.sock", "Identity": "", "IsMachine": False}
        connection["Default"] = CONNECTION
        data.setdefault("Farm", {"Default": "", "List": {}})
        _write_client_json(path, data)
    except (OSError, ValueError, TypeError, RecursionError) as error:
        raise RuntimeError("Could not configure the sandbox Podman connection: " + str(error)) from error


def seed_client(home):
    # The stopped container's home is untrusted; never run host Podman with its config.
    containers, (configuration, connections, quality) = _client_paths(home)
    for directory in (containers.parent.parent, containers.parent, containers, quality.parent.parent, quality.parent):
        directory.mkdir(exist_ok=True, mode=0o700)
    if not configuration.exists():
        with configuration.open("x") as stream:
            os.chmod(configuration, 0o600)
            stream.write("[engine]\nremote = true\n")
    _seed_connection(connections)
    if not quality.exists():
        data = {"version": 1, "config": {"version": 1, "mode": "sonarqubeCommunity",
                "runtime": {"kind": "podman", "connection": CONNECTION},
                "communityPort": DEFAULT_SONAR_PORT, "enterpriseUrl": ""},
                "revision": 0, "activated": False, "startAtLaunch": False}
        _write_client_json(quality, data)
        return DEFAULT_SONAR_PORT
    try:
        port = _read_client_json(quality)["config"]["communityPort"]
        if type(port) is int and port == 18080:
            raise RuntimeError("Code quality port 18080 is reserved for the sandbox website proxy; choose another port")
        return port if type(port) is int and 1024 <= port <= 65535 else DEFAULT_SONAR_PORT
    except (OSError, ValueError, KeyError, TypeError, RecursionError):
        return DEFAULT_SONAR_PORT


@dataclass
class Decision:
    allowed: bool
    status: int = 200
    reason: str = ""
    body: dict | None = None
    query: dict | None = None
    track_exec: bool = False


def _denied(reason, status=403):
    return Decision(False, status, reason)


def _labels(data):
    config = data.get("Config") or {}
    return config.get("Labels") or data.get("Labels") or data.get("labels") or {}


def _object(value):
    if not isinstance(value, dict):
        raise ValueError("Expected a JSON object")
    return value


def _known(data, keys, defaults=None):
    _object(data)
    defaults = defaults or {}
    unknown = sorted(set(data) - set(keys) - set(defaults))
    if unknown:
        names = ", ".join(f"{key!r} ({type(data[key]).__name__})" for key in unknown)
        raise ValueError("Unsupported option: " + names)
    for key, expected in defaults.items():
        if key in data and (type(data[key]) is not type(expected) or data[key] != expected):
            raise ValueError(f"Non-default option is not allowed: {key!r} ({type(data[key]).__name__})")


def _namespace(value, allowed):
    value = _object(value or {})
    _known(value, {"nsmode"})
    if value.get("nsmode", "") not in allowed:
        raise ValueError("Host or external namespaces are not allowed")


def _name(value):
    return isinstance(value, str) and _NAME.fullmatch(value)


class EngineFilter:
    def __init__(self, sandbox_id, images, sonar_port, inspect):
        self.sandbox_id = sandbox_id
        self.images = set(images)
        self.sonar_port = sonar_port
        self.inspect = inspect
        self.exec_ids = set()

    async def _owned(self, kind, ident):
        if not isinstance(ident, str) or not _IDENT.fullmatch(ident):
            return None
        status, data = await self.inspect(kind, ident)
        if status != 200 or not isinstance(data, dict) or _labels(data).get(SANDBOX_LABEL) != self.sandbox_id:
            return None
        name = data.get("Name") or data.get("name")
        if name and not _name(name.lstrip("/")):
            return None
        return data

    async def authorize(self, method, path, query, body):
        try:
            return await self._authorize(method, _VERSION.sub("", path), query, body)
        except ValueError as error:
            return _denied(str(error))
        except (TypeError, KeyError, AttributeError):
            return _denied("Malformed or unsupported request options")

    async def _authorize(self, method, path, query, body):
        reads = {"/_ping": {"GET", "HEAD"}, "/libpod/_ping": {"GET", "HEAD"},
                 "/libpod/info": {"GET"}, "/libpod/version": {"GET"}, "/version": {"GET"}}
        if method in reads.get(path, set()):
            return self._query(query, set())
        if path.startswith("/libpod/images/"):
            return self._image_request(method, path, query)
        if path.startswith(("/libpod/networks/", "/libpod/volumes/")):
            return await self._storage_request(method, path, query, body)
        if path.startswith("/libpod/containers/"):
            return await self._container_request(method, path, query, body)
        return self._exec_request(method, path, query, body)

    def _image_request(self, method, path, query):
        if method == "GET" and re.fullmatch(r"/libpod/images/.+/(json|exists)", path):
            self._query_keys(query, {"size"})
            self._query_defaults(query, {"size": "false"})
            return Decision(True)
        if method != "POST" or path != "/libpod/images/pull":
            return _denied(_ENDPOINT_DENIED)
        self._query_keys(query, {"reference", "policy", "quiet", "retry", "retry-delay", "tlsVerify", "alltags", "arch", "authfile", "os", "variant"})
        self._query_defaults(query, {"alltags": "false", "arch": "", "authfile": "", "os": "", "variant": ""})
        if "reference" not in query:
            return _denied("Exactly one image reference is required")
        if query["reference"][0] not in self.images:
            return _denied("Only pinned quality images can be pulled")
        if query.get("tlsVerify", ["true"])[0].lower() not in ("true", "1"):
            return _denied("Registry TLS verification is required")
        return Decision(True)

    async def _storage_request(self, method, path, query, body):
        match = re.fullmatch(r"/libpod/(networks|volumes)/(create|([^/]+)/(json|exists))", path)
        if not match:
            return _denied(_ENDPOINT_DENIED)
        plural, action, ident, _ = match.groups()
        kind = {"networks": "network", "volumes": "volume"}[plural]
        if method == "POST" and action == "create":
            if kind == "network":
                self._query_keys(query, {"ignoreIfExists"})
                return self._network_create(body)
            self._query_keys(query, set())
            return self._volume_create(body)
        if method == "GET" and ident:
            self._query_keys(query, set())
            return Decision(True) if await self._owned(kind, ident) else _denied(f"no such {kind}", 404)
        return _denied(_ENDPOINT_DENIED)

    def _network_create(self, body):
        data = dict(_object(body))
        _known(data, {"name", "driver", "labels", "internal", "options", "network_interface", "created", "dns_enabled"},
               {"id": "", "ipv6_enabled": False})
        if not _name(data.get("name")) or data.get("driver") not in (None, "", "bridge") or data.get("options") or data.get("network_interface"):
            return _denied("Only private omp-q- bridge networks are allowed")
        data["internal"] = True
        data["labels"] = dict(_object(data.get("labels") or {}), **{SANDBOX_LABEL: self.sandbox_id})
        return Decision(True, body=data)

    def _volume_create(self, body):
        data = dict(_object(body))
        _known(data, {"Name", "name", "Driver", "driver", "Label", "Labels", "Options", "options", "IgnoreIfExists"},
               {"UID": None, "GID": None})
        if any(upper in data and lower in data and data[upper] != data[lower] for upper, lower in (("Name", "name"), ("Driver", "driver"))):
            return _denied("Conflicting volume options")
        if not _name(data.get("Name", data.get("name"))) or data.get("Driver", data.get("driver")) not in (None, "", "local") or data.get("Options") or data.get("options"):
            return _denied("Only named omp-q- local volumes without driver options are allowed")
        labels = dict(_object(data.get("Label") or {}))
        labels.update(_object(data.get("Labels") or {}))
        labels[SANDBOX_LABEL] = self.sandbox_id
        data["Label"] = labels.copy()
        data["Labels"] = labels
        return Decision(True, body=data)

    async def _container_request(self, method, path, query, body):
        if method == "POST" and path == "/libpod/containers/create":
            self._query_keys(query, set())
            return await self._container(_object(body))
        if method == "GET" and path == "/libpod/containers/json":
            return self._container_list(query)
        match = re.fullmatch(r"/libpod/containers/([^/]+)(?:/(json|exists|logs|start|stop|wait|archive|exec))?", path)
        if not match:
            return _denied(_ENDPOINT_DENIED)
        ident, action = match.groups()
        return await self._container_action(method, ident, action, query, body)

    def _container_list(self, query):
        self._query_keys(query, {"all", "limit", "size", "sync", "filters", "last", "latest", "external", "namespace"})
        self._query_defaults(query, {"external": "false", "namespace": "false"})
        filters = _object(json.loads(query.get("filters", ["{}"])[0]))
        labels = filters.get("label", [])
        if not isinstance(labels, list) or not all(isinstance(v, str) for v in labels):
            return _denied("Invalid label filters")
        filters["label"] = [v for v in labels if v.split("=", 1)[0] != SANDBOX_LABEL] + [f"{SANDBOX_LABEL}={self.sandbox_id}"]
        return Decision(True, query=dict(query, filters=[json.dumps(filters)]))

    def _container_query(self, method, action, query):
        allowed = {"json": {"GET"}, "exists": {"GET"}, "logs": {"GET"},
                   "start": {"POST"}, "stop": {"POST"}, "wait": {"POST"},
                   "archive": {"HEAD", "GET", "PUT"}, "exec": {"POST"}, None: {"DELETE"}}
        if method not in allowed[action]:
            raise ValueError(_ENDPOINT_DENIED)
        keys = {"json": {"size"}, "exists": {"external"}, "logs": {"follow", "stdout", "stderr", "since", "until", "timestamps", "tail"},
                "start": {"detachKeys", "detachkeys"}, "stop": {"t", "timeout", "ignore"}, "wait": {"condition", "interval"},
                "archive": {"path", "rename", "copyUIDGID", "noOverwriteDirNonDir", "nooverwritedirnondir"}, "exec": set(),
                None: {"force", "ignore", "timeout", "depend", "v", "volumes"}}
        self._query_keys(query, keys[action])
        if action == "exists":
            self._query_defaults(query, {"external": "false"})
        if action is None and any(query.get(k, ["false"])[0].lower() not in ("false", "0", "") for k in ("depend", "v", "volumes")):
            raise ValueError("Removing dependent containers or volumes is not allowed")

    async def _container_action(self, method, ident, action, query, body):
        self._container_query(method, action, query)
        data = await self._owned("container", ident)
        if data is None:
            return _denied("no such container", 404)
        if action == "archive" and _labels(data).get(_ROLE) != "quality-scan":
            return _denied("Archive access is limited to scanner containers")
        if action == "exec":
            return self._readiness_exec(data, body)
        return Decision(True)

    @staticmethod
    def _readiness_exec(data, body):
        _known(_object(body), {"Cmd", "AttachStdin", "AttachStdout", "AttachStderr", "Tty", "DetachKeys", "Detach"},
               {"User": "", "Privileged": False, "Env": [], "WorkingDir": ""})
        if _labels(data).get(_ROLE) != "quality-db" or body.get("Cmd") != PG_READY:
            return _denied("Only PostgreSQL readiness exec is allowed")
        return Decision(True, track_exec=True)

    def _exec_request(self, method, path, query, body):
        match = re.fullmatch(r"/libpod/exec/([^/]+)/(start|resize|json|remove)", path)
        if not match:
            return _denied(_ENDPOINT_DENIED)
        ident, action = match.groups()
        expected = "GET" if action == "json" else "POST"
        if method != expected:
            return _denied(_ENDPOINT_DENIED)
        self._query_keys(query, {"h", "w"} if action == "resize" else set())
        if ident not in self.exec_ids:
            return _denied("Unknown exec session")
        if body is not None:
            _known(body, {"Detach", "Tty", "h", "w", "Force"})
        return Decision(True)

    @staticmethod
    def _query_keys(query, keys):
        if any(k not in keys or not isinstance(v, list) or len(v) != 1 or not isinstance(v[0], str) for k, v in query.items()):
            raise ValueError("Unsupported query options")

    @staticmethod
    def _query_defaults(query, defaults):
        if any(key in query and query[key] != [value] for key, value in defaults.items()):
            raise ValueError("Non-default query option is not allowed")

    def _query(self, query, keys):
        self._query_keys(query, keys)
        return Decision(True)

    async def _container(self, data):
        self._container_identity(data)
        self._container_namespaces(data)
        for mount in data.get("mounts") or []:
            self._tmpfs_mount(mount)
        for volume in data.get("volumes") or []:
            await self._named_volume(volume)
        networks = await self._container_networks(data)
        for port in data.get("portmappings") or []:
            self._published_port(port)
        self._resource_limits(data)
        rewritten = dict(data)
        rewritten.pop("containerCreateCommand", None)
        rewritten["labels"] = dict(_object(data.get("labels") or {}), **{SANDBOX_LABEL: self.sandbox_id})
        rewritten["httpproxy"] = False
        rewritten.pop("networks", None)
        rewritten["Networks"] = networks
        rewritten["netns"] = {"nsmode": "bridge" if networks else "none"}
        return Decision(True, body=rewritten)

    def _container_identity(self, data):
        safe = set("image raw_image_name name entrypoint command env terminal stdin labels stop_timeout log_configuration restart_policy restart_tries systemd sdnotifyMode pidns utsns remove containerCreateCommand init_container_type unsetenvall manage_password image_volume_mode init mounts volumes ipcns work_dir volatile user seccomp_policy userns idmappings read_only_filesystem read_write_tmpfs umask cgroupns netns portmappings Networks networks resource_limits r_limits healthconfig healthLogDestination healthMaxLogCount healthMaxLogSize httpproxy".split())
        _known(data, safe, {"env_host": False, "privileged": False, "publish_image_ports": False,
                            "use_image_resolve_conf": False, "use_image_hostname": False, "use_image_hosts": False})
        create_command = data.get("containerCreateCommand", [])
        if not isinstance(create_command, list) or not all(isinstance(value, str) for value in create_command):
            raise ValueError("containerCreateCommand must contain only strings")
        if data.get("image") not in self.images or not _name(data.get("name")):
            raise ValueError("Only pinned images with omp-q- names are allowed")
        if data.get("raw_image_name") and data["raw_image_name"] != data["image"]:
            raise ValueError("Image reference mismatch")
        if data.get("log_configuration") or data.get("healthLogDestination", "local") not in ("local", "events"):
            raise ValueError("Host log destinations are not allowed")
        if data.get("healthconfig") or data.get("seccomp_policy", "default") not in ("", "default"):
            raise ValueError("Custom health checks or seccomp policies are not allowed")

    @staticmethod
    def _container_namespaces(data):
        for key in ("pidns", "ipcns", "utsns", "cgroupns", "userns"):
            _namespace(data.get(key), {"", "private", "default"})
        _namespace(data.get("netns"), {"", "bridge", "private", "none"})
        mappings = data.get("idmappings")
        if mappings:
            _known(mappings, {"HostUIDMapping", "HostGIDMapping", "AutoUserNsOpts"},
                   {"UIDMap": None, "GIDMap": None, "AutoUserNs": False})
            _known(mappings.get("AutoUserNsOpts") or {}, set(),
                   {"Size": 0, "InitialSize": 0, "PasswdFile": "", "GroupFile": "",
                    "AdditionalUIDMappings": None, "AdditionalGIDMappings": None})

    @staticmethod
    def _tmpfs_mount(mount):
        _known(mount, {"destination", "type", "source", "options"})
        if mount.get("type") != "tmpfs" or mount.get("source", "tmpfs") != "tmpfs":
            raise ValueError("Only tmpfs mounts are allowed")
        if not isinstance(mount.get("destination"), str) or not mount["destination"].startswith("/"):
            raise ValueError("Invalid tmpfs destination")
        for option in mount.get("options") or []:
            if option not in ("rw", "ro", "nosuid", "nodev", "noexec", "exec", "relatime") and not re.fullmatch(r"(size|mode|uid|gid)=\d+[kKmMgG]?", option, re.ASCII):
                raise ValueError("Unsupported tmpfs options")

    async def _named_volume(self, volume):
        _known(volume, {"Name", "Dest", "Options"}, {"IsAnonymous": False, "SubPath": ""})
        if not await self._owned("volume", volume.get("Name")):
            raise ValueError("Volume is not owned by this sandbox")
        if not isinstance(volume.get("Dest"), str) or not volume["Dest"].startswith("/"):
            raise ValueError("Invalid volume destination")
        if any(v not in ("ro", "rw", "z", "Z", "nocopy") for v in volume.get("Options") or []):
            raise ValueError("Unsupported named volume options")

    async def _container_networks(self, data):
        if "Networks" in data and "networks" in data and data["Networks"] != data["networks"]:
            raise ValueError("Conflicting container network maps")
        networks = _object(data.get("Networks", data.get("networks")) or {})
        mode = (data.get("netns") or {}).get("nsmode", "")
        if not networks:
            if mode != "none":
                raise ValueError("An owned network is required unless networking is explicitly disabled")
            return networks
        if mode == "none":
            raise ValueError("Disabled networking cannot include a network map")
        for name, options in networks.items():
            if not await self._owned("network", name):
                raise ValueError("Network is not owned by this sandbox")
            _known(options or {}, {"aliases"}, {"interface_name": ""})
        return networks

    def _published_port(self, port):
        _known(port, {"host_ip", "host_port", "container_port", "range", "protocol"})
        if (port.get("host_ip") != "127.0.0.1" or type(port.get("host_port")) is not int or port["host_port"] != self.sonar_port
                or port.get("container_port") != 9000 or port.get("range", 1) != 1 or port.get("protocol", "") not in ("", "tcp")):
            raise ValueError("Only the configured loopback SonarQube port can be published")

    @staticmethod
    def _resource_limits(data):
        resources = data.get("resource_limits") or {}
        _known(resources, {"memory", "cpu"})
        _known(resources.get("memory") or {}, {"limit", "swap", "reservation"})
        _known(resources.get("cpu") or {}, {"quota", "period", "shares", "cpus"})
        for limit in data.get("r_limits") or []:
            _known(limit, {"type", "hard", "soft"})


async def _event(connection, reader):
    while True:
        event = connection.next_event()
        if event is not h11.NEED_DATA:
            return event
        connection.receive_data(await reader.read(65536))


async def _close(writer):
    if writer:
        writer.close()
        with contextlib.suppress(OSError):
            await writer.wait_closed()


class _BodyTooLarge(ValueError):
    pass


async def _message_data(connection, reader):
    while True:
        event = await _event(connection, reader)
        if isinstance(event, h11.Data):
            yield event.data
        elif isinstance(event, h11.EndOfMessage):
            return
        else:
            raise ValueError("Incomplete HTTP message")


class _Request:
    def __init__(self, client, reader, writer):
        self.client = client
        self.reader = reader
        self.writer = writer
        self.event = None
        self.method = "?"
        self.path = "?"
        self.body = None
        self.responded = False

    def parse(self, event):
        if not isinstance(event, h11.Request):
            raise ValueError("Expected HTTP request")
        self.event = event
        self.method = event.method.decode("ascii")
        target = event.target.decode("ascii")
        parsed = urlsplit(target)
        if parsed.scheme or parsed.netloc or parsed.fragment or not target.startswith("/"):
            raise ValueError("Only origin-form HTTP targets are allowed")
        self.path = unquote(parsed.path, errors="strict")
        self._validate_path()
        self.query = parse_qs(parsed.query, keep_blank_values=True, max_num_fields=100)
        self.normal = _VERSION.sub("", self.path)
        self.headers = dict(event.headers)
        self._body_flags()

    def _validate_path(self):
        if any(ord(c) <= 32 or ord(c) == 127 for c in self.path):
            raise ValueError("Invalid API path")
        if any(c in self.path for c in ("\\", "%")) or any(p in (".", "..") for p in self.path.split("/")):
            raise ValueError("Invalid API path")

    def _body_flags(self):
        tokens = [token.strip() for token in self.headers.get(b"connection", b"").lower().split(b",")]
        self.upgrade = self.headers.get(b"upgrade", b"").lower() == b"tcp" and b"upgrade" in tokens
        if b"upgrade" in self.headers and (not self.upgrade or not re.fullmatch(r"/libpod/exec/[^/]+/start", self.normal)):
            raise ValueError("Only tracked exec TCP upgrades are allowed")
        self.json_body = self.method == "POST" and (self.normal.endswith("/create") or re.fullmatch(r"/libpod/(containers/[^/]+/exec|exec/[^/]+/(start|resize|remove))", self.normal))
        archive_upload = self.method == "PUT" and re.fullmatch(r"/libpod/containers/[^/]+/archive", self.normal)
        has_body = b"transfer-encoding" in self.headers or int(self.headers.get(b"content-length", b"0"))
        if not self.json_body and not archive_upload and has_body:
            raise ValueError("This endpoint does not accept a request body")

    async def read_body(self, reader, writer):
        if self.headers.get(b"expect", b"").lower() == b"100-continue":
            writer.write(self.client.send(h11.InformationalResponse(status_code=100, headers=[])))
            await writer.drain()
        if not self.json_body:
            return None
        if int(self.headers.get(b"content-length", b"0")) > _JSON_LIMIT:
            raise _BodyTooLarge("Request body exceeds 1 MiB")
        data = bytearray()
        async for chunk in _message_data(self.client, reader):
            data.extend(chunk)
            if len(data) > _JSON_LIMIT:
                raise _BodyTooLarge("Request body exceeds 1 MiB")
        return _object(json.loads(data)) if data else {}

    def forward_headers(self, payload):
        headers = [(k, v) for k, v in self.event.headers if k in (b"accept", b"content-type", b"user-agent")]
        headers.append((b"host", b"localhost"))
        headers.append((b"connection", b"Upgrade" if self.upgrade else b"close"))
        if self.upgrade:
            headers.append((b"upgrade", b"tcp"))
        if payload is not None:
            headers.append((b"content-length", str(len(payload)).encode()))
        elif b"transfer-encoding" in self.headers:
            headers.append((b"transfer-encoding", b"chunked"))
        elif b"content-length" in self.headers:
            headers.append((b"content-length", self.headers[b"content-length"]))
        return headers

    def target(self, query):
        target = self.path
        if query:
            target += "?" + urlencode(query, doseq=True)
        return quote(target, safe="/%?=&:+,[]@")



class EngineGateway:
    def __init__(self, policy, state_dir, images, sonar_port, log):
        self.policy = policy
        self.state_dir = Path(state_dir)
        self.api_dir = self.state_dir / "api"
        self.upstream_path = self.state_dir / "upstream/podman.sock"
        self.sonar_port = sonar_port
        self.log = log
        self.filter = EngineFilter(sandbox_id(state_dir), images, sonar_port, self._inspect)
        self._process = None
        self._thread = None
        self._loop = None
        self._halt = None
        self._ready = threading.Event()
        self._error = None
        self._handlers = set()

    def start(self):
        if self._thread is not None or self._process is not None:
            raise RuntimeError("Engine gateway is already started")
        for path in (self.state_dir, self.upstream_path.parent, self.api_dir):
            path.mkdir(parents=True, exist_ok=True, mode=0o700)
            path.chmod(0o700)
        if any(path.exists() for path in (self.upstream_path, self.api_dir / _PODMAN_SOCKET, self.api_dir / _SONAR_SOCKET)):
            raise RuntimeError("Engine socket already exists; another launcher may be running")
        self._started = True
        try:
            self._process = subprocess.Popen(["podman", "system", "service", "--time=0", "unix://" + str(self.upstream_path)],
                                             stdout=self.log, stderr=self.log)
            deadline = time.monotonic() + 15
            while not self.upstream_path.exists():
                if self._process.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("Podman API service did not start; see launch.log")
                time.sleep(0.05)
            self._ready.clear()
            self._error = None
            self._thread = threading.Thread(target=self._run, name="engine-gateway", daemon=True)
            self._thread.start()
            if not self._ready.wait(30):
                raise RuntimeError("Engine gateway startup timed out")
            if self._error:
                raise RuntimeError("Engine gateway could not start") from self._error
            return self
        except BaseException:
            self.stop()
            raise

    def _run(self):
        try:
            asyncio.run(self._serve())
        except Exception as error:
            self._error = error
            self._ready.set()
        finally:
            for name in (_PODMAN_SOCKET, _SONAR_SOCKET):
                (self.api_dir / name).unlink(missing_ok=True)

    async def _serve(self):
        self._loop = asyncio.get_running_loop()
        self._halt = asyncio.Event()
        servers = []
        try:
            for name, handler in ((_PODMAN_SOCKET, self._handle), (_SONAR_SOCKET, self._sonar)):
                path = self.api_dir / name
                servers.append(await asyncio.start_unix_server(handler, path=str(path)))
                path.chmod(0o600)
            self._ready.set()
            await self._halt.wait()
        finally:
            for server in servers:
                server.close()
            handlers = tuple(self._handlers)
            for task in handlers:
                task.cancel()
            await asyncio.gather(*handlers, return_exceptions=True)
            for server in servers:
                await server.wait_closed()

    async def _sonar(self, reader, writer):
        task = asyncio.current_task()
        self._handlers.add(task)
        try:
            await _forward(reader, writer, self._connect_sonar)
        finally:
            self._handlers.discard(task)

    async def _sonar_running(self):
        filters = json.dumps({"label": [f"{SANDBOX_LABEL}={self.filter.sandbox_id}", f"{_ROLE}=quality-sonar"]})
        status, containers = await self._upstream_json("/v5.0.0/libpod/containers/json?" + urlencode({"all": "true", "filters": filters}))
        if status != 200 or not isinstance(containers, list):
            return False
        for entry in containers:
            ident = _object(entry).get("Id")
            data = await self.filter._owned("container", ident)
            if not data or _labels(data).get(_ROLE) != "quality-sonar" or _object(data.get("State") or {}).get("Running") is not True:
                continue
            bindings = _object(_object(data.get("NetworkSettings") or {}).get("Ports") or {}).get("9000/tcp") or []
            if any(binding.get("HostIp") == "127.0.0.1" and str(binding.get("HostPort")) == str(self.sonar_port) for binding in bindings):
                return True
        return False

    async def _connect_sonar(self):
        try:
            if not await self._sonar_running():
                raise OSError("No owned SonarQube container publishes this port")
            reader, writer = await asyncio.open_connection("127.0.0.1", self.sonar_port)
            try:
                # Recheck after connecting so a stopped/replaced publisher fails closed.
                if not await self._sonar_running():
                    raise OSError("SonarQube publisher stopped while connecting")
                return reader, writer
            except BaseException:
                await _close(writer)
                raise
        except (ValueError, TypeError, AttributeError, h11.ProtocolError, TimeoutError, RecursionError) as error:
            raise OSError("Could not verify the SonarQube publisher") from error

    async def _inspect(self, kind, ident):
        return await self._upstream_json(f"/v5.0.0/libpod/{kind}s/{quote(ident, safe='')}/json")

    async def _upstream_json(self, path):
        reader, writer = await asyncio.open_unix_connection(str(self.upstream_path))
        connection = h11.Connection(h11.CLIENT, max_incomplete_event_size=65536)
        try:
            writer.write(connection.send(h11.Request(method="GET", target=path, headers=[("Host", "localhost"), ("Connection", "close")])))
            writer.write(connection.send(h11.EndOfMessage()))
            await writer.drain()
            status, body = 502, bytearray()
            async with asyncio.timeout(15):
                while True:
                    event = await _event(connection, reader)
                    if isinstance(event, h11.Response):
                        status = event.status_code
                    elif isinstance(event, h11.Data):
                        body.extend(event.data)
                        if len(body) > _JSON_LIMIT:
                            raise ValueError("Inspect response too large")
                    elif isinstance(event, (h11.EndOfMessage, h11.ConnectionClosed)):
                        break
            return status, json.loads(body) if status == 200 else None
        finally:
            await _close(writer)

    def _denial_log(self, method, path, reason):
        self.state_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        with (self.state_dir / "filter.log").open("a") as stream:
            stream.write(f"{datetime.now(timezone.utc).isoformat()} DENY {method} {path}: {reason}\n")

    async def _error_response(self, writer, method, path, status, reason):
        self._denial_log(method, path, reason)
        data = ({"cause": reason, "message": reason, "response": 404} if status == 404 else
                {"cause": "blocked by OMBP sandbox", "message": "OMBP sandbox engine filter: " + reason, "response": status})
        body = json.dumps(data).encode()
        writer.write(f"HTTP/1.1 {status} Error\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {len(body)}\r\n\r\n".encode() + body)
        await writer.drain()

    async def _handle(self, reader, writer):
        task = asyncio.current_task()
        self._handlers.add(task)
        upstream = None
        request = _Request(h11.Connection(h11.SERVER, max_incomplete_event_size=65536), reader, writer)
        try:
            async with asyncio.timeout(30):
                event = await _event(request.client, reader)
            request.parse(event)
            request.body = await request.read_body(reader, writer)
            decision = await self.filter.authorize(request.method, request.normal, request.query, request.body)
            if not decision.allowed:
                await self._error_response(writer, request.method, request.path, decision.status, decision.reason)
                return
            remote_reader, upstream = await asyncio.open_unix_connection(str(self.upstream_path))
            connection = h11.Connection(h11.CLIENT, max_incomplete_event_size=65536)
            await self._forward_request(request, decision, reader, upstream, connection)
            await self._relay_response(request, decision.track_exec, remote_reader, upstream, connection)
        except (OSError, ValueError, h11.ProtocolError, RecursionError) as error:
            if not request.responded:
                status = 413 if isinstance(error, _BodyTooLarge) else 403
                with contextlib.suppress(OSError, h11.ProtocolError):
                    await self._error_response(writer, request.method, request.path, status, str(error))
        finally:
            await _close(upstream)
            await _close(writer)
            self._handlers.discard(task)

    @staticmethod
    async def _forward_request(request, decision, reader, upstream, connection):
        payload = None
        if request.json_body:
            body = decision.body if decision.body is not None else request.body
            payload = json.dumps(body).encode()
        query = decision.query if decision.query is not None else request.query
        upstream.write(connection.send(h11.Request(method=request.method, target=request.target(query),
                                                   headers=request.forward_headers(payload))))
        if payload is not None:
            upstream.write(connection.send(h11.Data(data=payload)))
        else:
            async for chunk in _message_data(request.client, reader):
                upstream.write(connection.send(h11.Data(data=chunk)))
                await upstream.drain()
        upstream.write(connection.send(h11.EndOfMessage()))
        await upstream.drain()

    @staticmethod
    async def _response_head(connection, reader):
        while True:
            event = await _event(connection, reader)
            if isinstance(event, h11.Response):
                return event
            if not isinstance(event, h11.InformationalResponse):
                raise ValueError("Missing upstream response")
            if event.status_code == 101:
                return event

    async def _relay_response(self, request, track_exec, remote_reader, upstream, connection):
        writer = request.writer
        head = await self._response_head(connection, remote_reader)
        if head.status_code == 101:
            await self._upgrade(request, head, remote_reader, upstream, connection)
            return
        headers = [(k, v) for k, v in head.headers if k not in (b"connection", b"keep-alive")]
        headers.append((b"connection", b"close"))
        head = h11.Response(status_code=head.status_code, headers=headers, reason=head.reason)
        if track_exec:
            await self._exec_response(request, head, writer, remote_reader, connection)
            return
        writer.write(request.client.send(head))
        request.responded = True
        async for chunk in _message_data(connection, remote_reader):
            writer.write(request.client.send(h11.Data(data=chunk)))
            await writer.drain()
        writer.write(request.client.send(h11.EndOfMessage()))
        await writer.drain()

    async def _exec_response(self, request, head, writer, reader, connection):
        body = bytearray()
        async for chunk in _message_data(connection, reader):
            body.extend(chunk)
            if len(body) > _JSON_LIMIT:
                raise ValueError("Exec response too large")
        if 200 <= head.status_code < 300:
            eid = _object(json.loads(body)).get("Id")
            if not isinstance(eid, str) or not _IDENT.fullmatch(eid):
                raise ValueError("Invalid exec session ID")
            self.filter.exec_ids.add(eid)
        writer.write(request.client.send(head))
        request.responded = True
        writer.write(request.client.send(h11.Data(data=bytes(body))))
        writer.write(request.client.send(h11.EndOfMessage()))
        await writer.drain()

    async def _upgrade(self, request, head, remote_reader, upstream, connection):
        reader, writer = request.reader, request.writer
        if not request.upgrade:
            raise ValueError("Unexpected protocol upgrade")
        writer.write(request.client.send(head))
        request.responded = True
        writer.write(connection.trailing_data[0])
        upstream.write(request.client.trailing_data[0])
        await writer.drain()
        await upstream.drain()
        await self._tunnel(reader, writer, remote_reader, upstream)

    @staticmethod
    async def _tunnel(reader, writer, remote_reader, upstream):
        tasks = [asyncio.create_task(_copy(reader, upstream)), asyncio.create_task(_copy(remote_reader, writer))]
        try:
            await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        finally:
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)

    def stop(self):
        if not getattr(self, "_started", False):
            return
        error = None
        try:
            if self._thread:
                if self._loop and self._halt and not self._loop.is_closed():
                    self._loop.call_soon_threadsafe(self._halt.set)
                self._thread.join(timeout=10)
                if self._thread.is_alive():
                    error = RuntimeError("Engine gateway did not stop")
                else:
                    self._thread = None
        finally:
            self._stop_service()
            for name in (_PODMAN_SOCKET, _SONAR_SOCKET):
                (self.api_dir / name).unlink(missing_ok=True)
            self.upstream_path.unlink(missing_ok=True)
            self._started = False
        if error:
            raise error

    def _stop_service(self):
        if self._process is None:
            return
        if self._process.poll() is None:
            self._process.terminate()
            try:
                self._process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self._process.kill()
                self._process.wait()
        self._process = None
