#!/usr/bin/env python3
"""Frozen downloaded-CLI acceptance. Offline mode never extracts or executes."""

import argparse
import ast
import hashlib
import http.server
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import secrets
import shutil
import signal
import struct
import subprocess
import tarfile
import threading
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request
import zipfile

HERE = Path(__file__).resolve().parent
INPUTS = json.loads((HERE / "inputs.json").read_text())
HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}))
CASES = {
    "inputs", "install-with-alias", "install-then-bind", "matching-instance-keys",
    "encrypted-persistence", "denied-egress", "granted-egress", "plugin-disabled",
    "initial-readiness", "fresh-delivery", "restart-readiness", "restart-delivery",
    "refusal-before", "encrypted-room-refusal", "refusal-after", "owned-teardown",
}
PHASES = {"admission", "acquisition", "inputs", "fixture", "setup", "runtime", "cleanup", "complete"}


def require(condition):
    if not condition:
        raise AssertionError("acceptance invariant")


def digest(data):
    return hashlib.sha256(data).hexdigest()


def file_record(path):
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(chunk)
    return {"bytes": path.stat().st_size, "sha256": h.hexdigest()}


def verify_fixture():
    source = (HERE / "prepare_synapse.py").read_bytes()
    require(digest(source) == INPUTS["fixture_sha256"])
    pins = [ast.literal_eval(node.value) for node in ast.parse(source).body
            if isinstance(node, ast.Assign) and any(isinstance(target, ast.Name) and target.id == "IMAGE" for target in node.targets)]
    require(pins == [INPUTS["synapse_image"]])


def verify_inputs(host_zip, guest_zip, destination=None):
    host, guest = INPUTS["host"], INPUTS["guest"]
    for path, spec in ((host_zip, host), (guest_zip, guest)):
        require(file_record(path) == {"bytes": spec["zip_bytes"], "sha256": spec["zip_sha256"]})
    verify_fixture()
    with zipfile.ZipFile(host_zip) as bundle:
        names = {host["archive"], "archive-evidence/input-manifest.json", "archive-evidence/qualification.json", "archive-evidence/validation.json"}
        names.update("archive-evidence/abi/" + name + ".txt" for name in ("zeroclaw", "zerocode", "zerorelay"))
        require(set(bundle.namelist()) == names and len(bundle.infolist()) == len(names))
        manifest_data = bundle.read("archive-evidence/input-manifest.json")
        validation_data = bundle.read("archive-evidence/validation.json")
        manifest, validation = json.loads(manifest_data), json.loads(validation_data)
        receipt = json.loads(bundle.read("archive-evidence/qualification.json"))
        require(receipt["binding"] == {"source": host["source"], "tree": host["tree"], "run": host["run"], "attempt": host["attempt"], "profile": "stable-gnu-ubuntu22"})
        require(receipt["role"] == "build" and receipt["target"] == host["target"])
        for document in (manifest, validation):
            require(all(document[key] == host[key] for key in ("source", "run", "attempt", "target")))
            require(document["main"] == "zeroclaw" and document["features"] == receipt["features"])
        require("plugins-wasm-cranelift" in receipt["features"].split(","))
        require(validation["verdict"] == "PASS" and validation["files"] == manifest["files"])
        for data, record in ((manifest_data, receipt["manifest"]), (validation_data, receipt["validation"])):
            require(record == {"bytes": len(data), "sha256": digest(data)})
        require(validation["manifest_sha256"] == digest(manifest_data))
        compressed = bundle.read(host["archive"])
        require({"bytes": len(compressed), "sha256": digest(compressed)} == receipt["archive"] == {"bytes": host["archive_bytes"], "sha256": host["archive_sha256"]})
        require(validation["archive_sha256"] == host["archive_sha256"])
        for name, observation in receipt["observations"].items():
            require(observation["input"] == manifest["files"][name])
            data = bundle.read("archive-evidence/abi/" + name + ".txt")
            require(observation["output"] == {"bytes": len(data), "sha256": digest(data)})
        if destination is not None:
            (destination / "host").mkdir()
        found = {}
        count = 0
        with bundle.open(host["archive"]) as stream, tarfile.open(fileobj=stream, mode="r|gz") as archive:
            for member in archive:
                count += 1
                parts = PurePosixPath(member.name).parts
                require(count <= 200 and parts and not member.name.startswith("/") and ".." not in parts)
                if member.isdir():
                    require(member.name.rstrip("/") in {"web/dist", "web/dist/assets", "web/dist/blog"})
                    continue
                require(member.isfile() and member.name in manifest["files"] and member.name not in found)
                require(member.size == manifest["files"][member.name]["bytes"] and member.size <= 100 * 1024**2)
                h, size = hashlib.sha256(), 0
                output = None
                if destination is not None:
                    target = destination / "host" / member.name
                    target.parent.mkdir(parents=True, exist_ok=True)
                    output = target.open("xb")
                try:
                    payload = archive.extractfile(member)
                    header = payload.read(64)
                    if member.name in {"zeroclaw", "zerocode", "zerorelay"}:
                        require(header[:6] == b"\x7fELF\x02\x01" and struct.unpack_from("<H", header, 18)[0] == 62)
                        require(member.mode & 0o111 and not member.mode & 0o022)
                    chunk = header
                    while chunk:
                        h.update(chunk)
                        size += len(chunk)
                        if output:
                            output.write(chunk)
                        chunk = payload.read(1024 * 1024)
                finally:
                    if output:
                        output.close()
                found[member.name] = {"bytes": size, "sha256": h.hexdigest()}
                require(found[member.name] == manifest["files"][member.name])
                if destination is not None:
                    target.chmod(0o500 if member.name in {"zeroclaw", "zerocode", "zerorelay"} else 0o400)
        require(found == manifest["files"] and len(found) == host["payloads"])
        require(found["zeroclaw"] == {"bytes": host["binary_bytes"], "sha256": host["binary_sha256"]})
    with zipfile.ZipFile(guest_zip) as bundle:
        require(len(bundle.infolist()) == 18 and len(set(bundle.namelist())) == 18)
        for member in bundle.infolist():
            require(not member.filename.startswith("/") and ".." not in PurePosixPath(member.filename).parts and member.file_size < 4 * 1024**2)
            require((member.external_attr >> 16) & 0o170000 != 0o120000)
        wasm, manifest_data = bundle.read("matrix/matrix.wasm"), bundle.read("matrix/manifest.toml")
        require(len(wasm) == guest["component_bytes"] and digest(wasm) == guest["component_sha256"] and wasm[:8] == b"\x00asm\x0d\x00\x01\x00")
        require(len(manifest_data) == guest["manifest_bytes"] and digest(manifest_data) == guest["manifest_sha256"])
        manifest = tomllib.loads(manifest_data.decode())
        require(manifest["name"] == "matrix" and manifest["version"] == "0.3.0" and manifest["registry"] is False)
        if destination is not None:
            (destination / "package").mkdir()
            for name, data in (("matrix.wasm", wasm), ("manifest.toml", manifest_data)):
                with (destination / "package" / name).open("xb") as output:
                    output.write(data)


def matrix(fixture, path, token, body=None, method=None):
    request = urllib.request.Request(fixture["homeserver"] + "/_matrix/client/v3/" + path, data=None if body is None else json.dumps(body).encode(), headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"}, method=method)
    with HTTP.open(request, timeout=8) as response:
        return json.load(response)


def request_body(headers, stream):
    lengths = headers.get_all("Content-Length", [])
    encodings = headers.get_all("Transfer-Encoding", [])
    require(len(lengths) <= 1 and len(encodings) <= 1 and not (lengths and encodings))
    maximum = 2 * 1024**2
    if not encodings:
        value = lengths[0] if lengths else "0"
        require(re.fullmatch(r"[0-9]+", value) is not None)
        size = int(value)
        require(size <= maximum)
        body = stream.read(size)
        require(len(body) == size)
        return body or None
    require(encodings[0].lower() == "chunked")
    body = bytearray()
    while True:
        line = stream.readline(1025)
        require(len(line) <= 1024 and re.fullmatch(rb"[0-9a-fA-F]+(?:;[^\r\n]*)?\r\n", line) is not None)
        size = int(line.split(b";", 1)[0].strip(), 16)
        require(size <= maximum - len(body))
        if size == 0:
            require(stream.readline(3) == b"\r\n")
            return bytes(body) or None
        chunk = stream.read(size)
        require(len(chunk) == size and stream.read(2) == b"\r\n")
        body.extend(chunk)


def trace_counts(path):
    counts = {"agent_complete": 0, "agent_fail": 0, "provider_complete": 0, "provider_fail": 0}
    kinds = {"context_window", "credentials_missing", "auth", "rate_limited", "provider_server", "connect_timeout", "timeout", "connect", "dns", "model_not_found", "unknown", "http_client", "http_server"}
    if path.exists():
        require(path.stat().st_size <= 8 * 1024**2)
        for line in path.read_text().splitlines():
            event = json.loads(line)
            descriptor = event.get("event", {})
            label = descriptor.get("category", "") + "_" + descriptor.get("action", "")
            if label in counts:
                counts[label] = min(counts[label] + 1, 1000000)
            if descriptor.get("category") == "provider":
                kind = event.get("attributes", {}).get("error_kind")
                if kind in kinds:
                    label = "provider_error_" + kind
                    counts[label] = min(counts.get(label, 0) + 1, 1000000)
    return counts


class Observer:
    def __init__(self, fixture):
        self.fixture = fixture
        self.bot_requests = 0
        self.syncs = 0
        self.synced_events = set()
        self.nonzero_error = False
        self.known = set()
        self.forbidden = set()
        self.calls = {}
        self.send_requests = 0
        self.encryption_checks = 0
        self.pending = 0
        self.drained = threading.Condition()
        owner = self

        class Server(http.server.ThreadingHTTPServer):
            def get_request(self):
                connection, address = super().get_request()
                connection.settimeout(40)
                return connection, address

            def process_request(self, request, address):
                with owner.drained:
                    owner.pending += 1
                try:
                    super().process_request(request, address)
                except Exception:
                    with owner.drained:
                        owner.pending -= 1
                        owner.drained.notify_all()
                    raise

            def process_request_thread(self, request, address):
                try:
                    super().process_request_thread(request, address)
                finally:
                    with owner.drained:
                        owner.pending -= 1
                        owner.drained.notify_all()

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def handle_request(self):
                try:
                    body = request_body(self.headers, self.rfile)
                    if self.path == "/v1/models" and self.command == "GET":
                        require(body is None and self.headers.get("Authorization") == "Bearer synthetic-provider-acceptance-key")
                        data = json.dumps({"object": "list", "data": [{"id": "matrix-fixture", "object": "model", "owned_by": "fixture"}]}).encode()
                        status, content_type = 200, "application/json"
                    elif self.path == "/v1/chat/completions" and self.command == "POST":
                        require(self.headers.get("Authorization") == "Bearer synthetic-provider-acceptance-key")
                        payload = json.loads(body)
                        require(not any(nonce in json.dumps(payload) for nonce in owner.forbidden))
                        users = [str(message.get("content", "")) for message in payload["messages"] if message.get("role") == "user"]
                        matches = re.findall(r"MATRIX_ACCEPT_[0-9a-f]{32}", users[-1] if users else "")
                        require(matches and matches[-1] in owner.known)
                        nonce = matches[-1]
                        owner.calls[nonce] = owner.calls.get(nonce, 0) + 1
                        marker = "MATRIX_REPLY_" + nonce
                        common = {"id": "fixture", "model": "matrix-fixture", "created": 1}
                        if payload.get("stream"):
                            records = [dict(common, object="chat.completion.chunk", choices=[{"index": 0, "delta": {"role": "assistant", "content": marker}, "finish_reason": None}]), dict(common, object="chat.completion.chunk", choices=[{"index": 0, "delta": {}, "finish_reason": "stop"}])]
                            data = ("".join("data: " + json.dumps(record) + "\n\n" for record in records) + "data: [DONE]\n\n").encode()
                            content_type = "text/event-stream"
                        else:
                            data = json.dumps(dict(common, object="chat.completion", choices=[{"index": 0, "message": {"role": "assistant", "content": marker}, "finish_reason": "stop"}], usage={"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2})).encode()
                            content_type = "application/json"
                        status = 200
                    else:
                        require(self.path.startswith("/_matrix/client/v3/") and self.headers.get("Authorization") == "Bearer " + fixture["bot"]["token"])
                        owner.bot_requests += 1
                        if self.command == "PUT" and "/send/m.room.message/" in self.path:
                            owner.send_requests += 1
                        if self.command == "GET" and "/state/m.room.encryption/" in self.path:
                            owner.encryption_checks += 1
                        request = urllib.request.Request(fixture["homeserver"] + self.path, data=body, headers={"Authorization": self.headers["Authorization"], "Content-Type": "application/json"}, method=self.command)
                        try:
                            response = HTTP.open(request, timeout=35)
                        except urllib.error.HTTPError as error:
                            response = error
                        with response:
                            status, data = response.status, response.read(2 * 1024**2 + 1)
                        require(len(data) <= 2 * 1024**2)
                        content_type = "application/json"
                        if status == 200 and self.path.startswith("/_matrix/client/v3/sync"):
                            sync = json.loads(data)
                            for room in sync.get("rooms", {}).get("join", {}).values():
                                owner.synced_events.update(event["event_id"] for event in room.get("timeline", {}).get("events", []) if "event_id" in event)
                            owner.syncs += 1
                    self.send_response(status)
                    self.send_header("Content-Type", content_type)
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except Exception:
                    owner.nonzero_error = True
                    self.send_error(502)

            do_GET = do_POST = do_PUT = handle_request

        self.server = Server(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.base = "http://127.0.0.1:" + str(self.server.server_port)

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
        require(not self.thread.is_alive())
        with self.drained:
            require(self.drained.wait_for(lambda: self.pending == 0, timeout=45))


class Runtime:
    def __init__(self, root):
        self.root = root
        self.private = root / "proof-private"
        self.public = root / "proof-public"
        self.private.mkdir(mode=0o700)
        self.public.mkdir(mode=0o700)
        self.started = time.monotonic()
        self.deadline = self.started + INPUTS["limits"]["proof_seconds"]
        self.phase_deadline = self.deadline
        self.children = []
        self.logs = []
        self.listener = None
        self.observer = None
        self.stop_watch = threading.Event()
        self.stop_reason = False
        self.result = {"status": "running", "phase": "admission", "failure_kind": None, "cleanup": False, "cases": [], "host_sha256": INPUTS["host"]["binary_sha256"], "guest_sha256": INPUTS["guest"]["component_sha256"], "inputs_sha256": digest((HERE / "inputs.json").read_bytes())}
        self.watcher = threading.Thread(target=self.watch, daemon=True)
        self.watcher.start()
        print("matrix acceptance phase: admission", flush=True)

    def watch(self):
        last_report = 0
        while not self.stop_watch.wait(1):
            elapsed = int(time.monotonic() - self.started)
            if elapsed // 30 > last_report:
                last_report = elapsed // 30
                print("matrix acceptance progress: " + self.result["phase"] + " / " + str(elapsed) + "s elapsed", flush=True)
            if time.monotonic() > self.deadline or shutil.disk_usage(self.root).free < INPUTS["limits"]["reserve_bytes"]:
                self.stop_reason = True
                for process in list(self.children):
                    if process.poll() is None:
                        try:
                            os.killpg(process.pid, signal.SIGTERM)
                        except ProcessLookupError:
                            pass
                return

    def guard(self):
        require(not self.stop_reason and time.monotonic() < min(self.deadline, self.phase_deadline))
        require(shutil.disk_usage(self.root).free >= INPUTS["limits"]["reserve_bytes"])
        if self.observer:
            require(not self.observer.nonzero_error)

    def command(self, args, timeout=60, env=None, cwd=None, output=None):
        self.guard()
        process = subprocess.Popen(args, cwd=cwd or self.root, env=env, stdout=output or subprocess.PIPE, stderr=subprocess.PIPE, stdin=subprocess.DEVNULL, start_new_session=True)
        self.children.append(process)
        try:
            stdout, stderr = process.communicate(timeout=min(timeout, max(1, min(self.deadline, self.phase_deadline) - time.monotonic())))
            if process.returncode:
                print("matrix command failed: exit " + str(process.returncode), flush=True)
                if args[:2] == ["docker", "update"] and b"memoryswap" in stderr.lower().replace(b" ", b""):
                    print("matrix fixture limit failure: memory/swap conflict", flush=True)
            require(process.returncode == 0)
            return (stdout or b"") + stderr
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
            self.children.remove(process)

    def case(self, name):
        require(name in CASES and name not in self.result["cases"])
        self.result["cases"].append(name)
        print("matrix acceptance case passed: " + name, flush=True)

    def cli(self, profile, *args):
        home = profile / "home"
        env = {"PATH": "/usr/bin:/bin", "HOME": str(home), "ZEROCLAW_CONFIG_DIR": str(profile), "ZEROCLAW_DATA_DIR": str(profile / "data"), "XDG_CONFIG_HOME": str(home / "config"), "XDG_DATA_HOME": str(home / "data"), "XDG_CACHE_HOME": str(home / "cache"), "LANG": "C.UTF-8", "NO_COLOR": "1"}
        print("matrix CLI operation: " + " ".join(args[:2]), flush=True)
        output = self.command([str(self.private / "host/zeroclaw"), "--config-dir", str(profile), *args], env=env, cwd=home).decode()
        require(all(secret not in output for secret in self.secrets))
        return output

    def setting(self, profile, path, value):
        return self.cli(profile, "config", "set", path, value, "--no-interactive", "--json")

    def config(self, profile):
        return tomllib.loads((profile / "config.toml").read_text())

    def setup(self, name, combined):
        profile = self.private / name
        profile.mkdir()
        (profile / "home").mkdir()
        self.cli(profile, "config", "get", "plugins.enabled", "--json")
        self.setting(profile, "plugins.plugins_dir", str(profile / "plugins"))
        package = str(self.private / "package")
        if combined:
            output = self.cli(profile, "plugin", "install", package, "--channel-alias", "operations", "--egress", "none")
        else:
            bare = self.cli(profile, "plugin", "install", package)
            require(not self.config(profile).get("plugins", {}).get("entries") and "plugin bind" in bare)
            output = self.cli(profile, "plugin", "bind", "matrix", "--channel-alias", "operations", "--egress", "none")
        config = self.config(profile)
        rows = config["plugins"]["entries"]
        require(len(rows) == 1)
        key = rows[0]["name"]
        require(key == "zpi1_WyJtYXRyaXgiLCJjaGFubmVsIiwib3BlcmF0aW9ucyJd" and set(re.findall(r"zpi1_[A-Za-z0-9_-]+", output)) == {key})
        require("config.homeserver" in output and "config.access_token" in output and config["channels"]["plugin"]["operations"]["package"] == "matrix")
        require(not rows[0].get("egress_hosts"))
        installed = list((profile / "plugins").rglob("matrix.wasm"))
        require(len(installed) == 1 and file_record(installed[0])["sha256"] == INPUTS["guest"]["component_sha256"])
        require(digest((installed[0].parent / "manifest.toml").read_bytes()) == INPUTS["guest"]["manifest_sha256"])
        before = file_record(profile / "config.toml")
        self.cli(profile, "plugin", "bind", "matrix", "--channel-alias", "operations", "--egress", "none")
        require(file_record(profile / "config.toml") == before)
        require("[loads]" in self.cli(profile, "plugin", "list", "--verify") and "Loads: yes" in self.cli(profile, "plugin", "info", "matrix"))
        self.case(name)
        return profile, key

    def start(self, profile):
        home = profile / "home"
        env = {"PATH": "/usr/bin:/bin", "HOME": str(home), "ZEROCLAW_CONFIG_DIR": str(profile), "ZEROCLAW_DATA_DIR": str(profile / "data"), "XDG_CONFIG_HOME": str(home / "config"), "XDG_DATA_HOME": str(home / "data"), "XDG_CACHE_HOME": str(home / "cache"), "LANG": "C.UTF-8", "NO_COLOR": "1"}
        log = (self.private / ("runtime-" + str(len(self.logs)) + ".log")).open("xb")
        self.logs.append(log)
        self.listener = subprocess.Popen([str(self.private / "host/zeroclaw"), "--config-dir", str(profile), "channel", "start"], env=env, cwd=home, stdout=log, stderr=log, stdin=subprocess.DEVNULL, start_new_session=True)
        self.children.append(self.listener)
        return self.listener.pid

    def stop(self):
        if self.listener is not None:
            process = self.listener
            try:
                if process.poll() is None:
                    try:
                        os.killpg(process.pid, signal.SIGINT)
                    except ProcessLookupError:
                        pass
                    try:
                        process.wait(timeout=25)
                    except subprocess.TimeoutExpired:
                        try:
                            os.killpg(process.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        process.wait(timeout=5)
                        raise RuntimeError("shutdown deadline") from None
            finally:
                if process.poll() is not None:
                    self.children.remove(process)
                    self.listener = None

    def post(self, room, forbidden=False):
        nonce = "MATRIX_ACCEPT_" + secrets.token_hex(16)
        self.observer.known.add(nonce)
        if forbidden:
            self.observer.forbidden.add(nonce)
        event = matrix(self.fixture, "rooms/" + urllib.parse.quote(room, safe="") + "/send/m.room.message/" + secrets.token_hex(16), self.fixture["sender"]["token"], {"msgtype": "m.text", "body": nonce}, "PUT")["event_id"]
        return nonce, event

    def bot_messages(self, room):
        events = matrix(self.fixture, "rooms/" + urllib.parse.quote(room, safe="") + "/messages?dir=b&limit=100", self.fixture["sender"]["token"])["chunk"]
        return [event for event in events if event.get("sender") == self.fixture["bot"]["id"] and event.get("type") == "m.room.message"]

    def reply(self, room, nonce):
        marker = "MATRIX_REPLY_" + nonce
        return next((event for event in self.bot_messages(room) if marker in event.get("content", {}).get("body", "")), None)

    def await_roundtrip(self, room, nonce, event, seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            self.guard()
            require(self.listener is not None and self.listener.poll() is None)
            reply = self.reply(room, nonce)
            if self.observer.calls.get(nonce, 0) and reply:
                require(reply["event_id"] != event and event in self.observer.synced_events)
                return True
            time.sleep(0.5)
        print("matrix roundtrip timeout: " + json.dumps({
            "syncs": min(self.observer.syncs, 1000000),
            "probe_synced": event in self.observer.synced_events,
            "provider_calls": min(self.observer.calls.get(nonce, 0), 1000000),
            "reply_present": bool(reply),
            "bot_messages": min(len(self.bot_messages(room)), 1000000),
            "send_requests": min(self.observer.send_requests, 1000000),
            "encryption_checks": min(self.observer.encryption_checks, 1000000),
        }, sort_keys=True), flush=True)
        return False

    def ready(self, room, name):
        start_syncs = self.observer.syncs
        end = time.monotonic() + INPUTS["limits"]["readiness_seconds"]
        while self.observer.syncs == start_syncs:
            self.guard()
            require(self.listener.poll() is None and time.monotonic() < end)
            time.sleep(0.2)
        while time.monotonic() < end:
            nonce, event = self.post(room)
            if self.await_roundtrip(room, nonce, event, min(15, max(1, end - time.monotonic()))):
                self.case(name)
                return
        raise RuntimeError("readiness deadline")

    def scored(self, room, name):
        nonce, event = self.post(room)
        require(self.await_roundtrip(room, nonce, event, INPUTS["limits"]["scored_delivery_seconds"]))
        self.case(name)


def acquire(runtime):
    runtime.result["phase"] = "acquisition"
    print("matrix acceptance phase: acquisition", flush=True)
    runtime.phase_deadline = time.monotonic() + 300
    verify_fixture()
    paths = []
    for name in ("host", "guest"):
        spec = INPUTS[name]
        api = "repos/" + spec["repository"] + "/actions/artifacts/" + str(spec["artifact"])
        metadata = json.loads(runtime.command(["gh", "api", api], timeout=20))
        require(metadata["id"] == spec["artifact"] and metadata["name"] == spec["artifact_name"] and metadata["expired"] is False)
        require(metadata["size_in_bytes"] == spec["zip_bytes"] and metadata["digest"] == "sha256:" + spec["zip_sha256"])
        require(metadata["workflow_run"]["id"] == spec["run"] and metadata["workflow_run"]["head_sha"] == spec["source"])
        path = runtime.private / (name + ".zip")
        with path.open("xb") as output:
            runtime.command(["gh", "api", api + "/zip"], timeout=180, output=output)
        paths.append(path)
    runtime.command(["docker", "pull", "--platform", "linux/amd64", INPUTS["synapse_image"]], timeout=240)
    runtime.phase_deadline = runtime.deadline
    return paths


def prove(runtime, host_zip, guest_zip):
    runtime.result["phase"] = "inputs"
    print("matrix acceptance phase: inputs", flush=True)
    verify_inputs(host_zip, guest_zip, runtime.private)
    runtime.case("inputs")
    runtime.result["phase"] = "fixture"
    print("matrix acceptance phase: fixture", flush=True)
    runtime.phase_deadline = time.monotonic() + 180
    fixture_root = runtime.private / "fixture"
    fixture_root.mkdir()
    (runtime.private / "proof-public").mkdir()
    (runtime.private / "proof-public/identity.json").write_text("{}")
    print("matrix fixture stage: provisioning", flush=True)
    try:
        runtime.command(["python3", str(HERE / "prepare_synapse.py"), "prepare", str(fixture_root)], timeout=180)
    except Exception:
        try:
            detail = json.loads((runtime.private / "proof-public/identity.json").read_text())["synapse_setup"]
            stages = {"admission", "private-config", "image-inspection", "network-create", "container-start", "isolation-inspection", "server-readiness", "user-registration", "user-login", "room-create", "room-join", "fixture-write", "complete"}
            kinds = {"CalledProcessError", "TimeoutExpired", "HTTPError", "URLError", "TimeoutError", "TypeError", "KeyError", "ValueError", "OSError", "RuntimeError", "other"}
            require(detail["stage"] in stages and detail["success"] is False and detail["error_kind"] in kinds)
            print("matrix fixture failure: " + detail["stage"] + " / " + detail["error_kind"], flush=True)
            for key, lower, upper in (("http_status", 100, 599), ("command_exit_code", -128, 255)):
                if key in detail:
                    require(type(detail[key]) is int and lower <= detail[key] <= upper)
                    print("matrix fixture " + key + ": " + str(detail[key]), flush=True)
        except Exception:
            print("matrix fixture diagnostics unavailable", flush=True)
        raise
    runtime.fixture = json.loads((fixture_root / "fixture.json").read_text())
    owned = json.loads((fixture_root / "owned.json").read_text())
    print("matrix fixture stage: resource limits", flush=True)
    runtime.command(["docker", "update", "--memory", "2g", "--memory-swap", "2g", "--cpus", "2", "--pids-limit", "256", owned["container"]])
    print("matrix fixture stage: resource limits applied", flush=True)
    runtime.phase_deadline = runtime.deadline
    runtime.secrets = [runtime.fixture[role]["token"] for role in ("bot", "sender", "deny")] + ["synthetic-provider-acceptance-key"]
    runtime.observer = Observer(runtime.fixture)
    runtime.result["phase"] = "setup"
    print("matrix acceptance phase: setup", flush=True)
    first, key = runtime.setup("install-with-alias", True)
    _, other_key = runtime.setup("install-then-bind", False)
    require(key == other_key)
    runtime.case("matching-instance-keys")
    rooms = [runtime.fixture["room"]]
    for encrypted in (False, True):
        body = {"visibility": "private", "preset": "private_chat", "invite": [runtime.fixture["bot"]["id"]]}
        if encrypted:
            body["initial_state"] = [{"type": "m.room.encryption", "state_key": "", "content": {"algorithm": "m.megolm.v1.aes-sha2"}}]
        room = matrix(runtime.fixture, "createRoom", runtime.fixture["sender"]["token"], body)["room_id"]
        matrix(runtime.fixture, "join/" + urllib.parse.quote(room, safe=""), runtime.fixture["bot"]["token"], {})
        rooms.append(room)
    for section in ("agents.operator", "risk_profiles.default", "providers.models.custom.fixture", "peer_groups.proof"):
        runtime.cli(first, "config", "init", section, "--json")
    settings = {
        "plugins.enabled": "true", "plugins.auto_discover": "false",
        "observability.log_persistence_path": str(runtime.private / "runtime-events.jsonl"),
        "observability.log_persistence_max_entries": "1000",
        "providers.models.custom.fixture.uri": runtime.observer.base + "/v1",
        "providers.models.custom.fixture.api_key": "synthetic-provider-acceptance-key",
        "providers.models.custom.fixture.model": "matrix-fixture", "providers.models.custom.fixture.wire_api": "chat_completions",
        "providers.models.custom.fixture.live_pricing": "false", "providers.models.custom.fixture.timeout_secs": "20",
        "agents.operator.enabled": "true", "agents.operator.model_provider": "custom.fixture",
        "agents.operator.risk_profile": "default", "agents.operator.channels": "plugin.operations",
        "peer_groups.proof.channel": "plugin.operations", "peer_groups.proof.agents": "operator",
        "peer_groups.proof.external_peers": runtime.fixture["sender"]["id"],
        "plugins.entries." + key + ".config.homeserver": runtime.observer.base,
        "plugins.entries." + key + ".config.access_token": runtime.fixture["bot"]["token"],
        "plugins.entries." + key + ".config.user_id": runtime.fixture["bot"]["id"],
        "plugins.entries." + key + ".config.allowed_rooms": json.dumps(rooms),
        "plugins.entries." + key + ".config.mention_only": "false",
        "plugins.entries." + key + ".config.reply_in_thread": "false",
    }
    for path, value in settings.items():
        runtime.setting(first, path, value)
    config = runtime.config(first)
    require(not config["channels"].get("matrix") and not config["plugins"].get("auto_discover", False))
    require(config["agents"]["operator"]["channels"] == ["plugin.operations"])
    ciphertext = config["plugins"]["entries"][0]["config"]["access_token"]
    require(ciphertext.startswith("enc2:") and len(bytes.fromhex(ciphertext[5:])) >= 29)
    require(all(secret not in (first / "config.toml").read_text() for secret in runtime.secrets))
    require(key in runtime.cli(first, "plugin", "info", "matrix"))
    runtime.case("encrypted-persistence")
    runtime.cli(first, "channel", "doctor")
    require(runtime.observer.bot_requests == 0)
    runtime.case("denied-egress")
    runtime.setting(first, "plugins.entries." + key + ".egress_hosts", "127.0.0.1")
    runtime.setting(first, "plugins.entries." + key + ".egress_allow_private", "127.0.0.1")
    runtime.cli(first, "channel", "doctor")
    require(runtime.observer.bot_requests > 0)
    runtime.case("granted-egress")
    runtime.result["phase"] = "runtime"
    print("matrix acceptance phase: runtime", flush=True)
    runtime.phase_deadline = min(runtime.deadline, time.monotonic() + 900)
    runtime.setting(first, "plugins.enabled", "false")
    runtime.start(first)
    nonce, _ = runtime.post(rooms[1])
    time.sleep(INPUTS["limits"]["refusal_observation_seconds"])
    require(not runtime.observer.calls.get(nonce) and runtime.reply(rooms[1], nonce) is None)
    runtime.stop()
    runtime.case("plugin-disabled")
    runtime.setting(first, "plugins.enabled", "true")
    old_pid = runtime.start(first)
    runtime.ready(rooms[0], "initial-readiness")
    runtime.scored(rooms[0], "fresh-delivery")
    runtime.stop()
    require(runtime.start(first) != old_pid)
    runtime.ready(rooms[1], "restart-readiness")
    runtime.scored(rooms[1], "restart-delivery")
    runtime.scored(rooms[1], "refusal-before")
    baseline = {message["event_id"] for message in runtime.bot_messages(rooms[2])}
    nonce, event = runtime.post(rooms[2], forbidden=True)
    end = time.monotonic() + 60
    while event not in runtime.observer.synced_events:
        runtime.guard()
        require(runtime.listener.poll() is None and time.monotonic() < end)
        time.sleep(0.2)
    end = time.monotonic() + INPUTS["limits"]["refusal_observation_seconds"]
    while time.monotonic() < end:
        runtime.guard()
        require(runtime.listener.poll() is None and not runtime.observer.calls.get(nonce))
        require({message["event_id"] for message in runtime.bot_messages(rooms[2])} <= baseline)
        time.sleep(0.5)
    runtime.case("encrypted-room-refusal")
    runtime.scored(rooms[1], "refusal-after")
    require({message["event_id"] for message in runtime.bot_messages(rooms[2])} <= baseline)
    runtime.stop()
    runtime.observer.close()
    runtime.guard()
    require({message["event_id"] for message in runtime.bot_messages(rooms[2])} <= baseline)
    require(runtime.config(first)["plugins"]["entries"][0]["config"]["access_token"].startswith("enc2:"))
    for log in runtime.logs:
        log.close()
    for log in runtime.private.glob("runtime-*.log"):
        require(all(secret.encode() not in log.read_bytes() for secret in runtime.secrets))


def check_evidence(root):
    public = root / "proof-public"
    require(public.is_dir() and not public.is_symlink() and sorted(path.name for path in public.iterdir()) == ["result.json"])
    path = public / "result.json"
    require(path.is_file() and not path.is_symlink() and path.stat().st_size < 8192)
    result = json.loads(path.read_text())
    require(set(result) == {"status", "phase", "failure_kind", "cleanup", "cases", "host_sha256", "guest_sha256", "inputs_sha256"})
    require(result["status"] in {"passed", "failed"} and result["phase"] in PHASES and type(result["cleanup"]) is bool)
    require(result["failure_kind"] in {None, "AssertionError", "OSError", "TimeoutExpired", "HTTPError", "URLError", "RuntimeError", "ValueError", "KeyError", "TypeError", "other"})
    require(type(result["cases"]) is list and len(set(result["cases"])) == len(result["cases"]) and set(result["cases"]) <= CASES)
    require(result["host_sha256"] == INPUTS["host"]["binary_sha256"] and result["guest_sha256"] == INPUTS["guest"]["component_sha256"] and result["inputs_sha256"] == digest((HERE / "inputs.json").read_bytes()))
    if result["status"] == "passed":
        require(result["cleanup"] and set(result["cases"]) == CASES and result["failure_kind"] is None and result["phase"] == "complete")
    if os.environ.get("GITHUB_ENV"):
        with open(os.environ["GITHUB_ENV"], "a") as output:
            output.write("EVIDENCE_CHECKED=true\n")


def run(root):
    require(platform.system() == "Linux" and platform.machine() == "x86_64" and not sys_optimized())
    require(os.environ.get("GITHUB_REPOSITORY") == INPUTS["repository"] and os.environ.get("GITHUB_ACTOR") == "Audacity88" and os.environ.get("GITHUB_EVENT_NAME") == "push" and os.environ.get("GITHUB_REF") == "refs/heads/" + INPUTS["branch"])
    require(root == Path(os.environ["GITHUB_WORKSPACE"]).resolve() and shutil.disk_usage(root).free >= INPUTS["limits"]["initial_free_bytes"])
    os.umask(0o077)
    runtime = Runtime(root)
    try:
        host_zip, guest_zip = acquire(runtime)
        prove(runtime, host_zip, guest_zip)
        runtime.result["status"] = "passed"
    except Exception as error:
        runtime.result["status"] = "failed"
        kinds = {"AssertionError", "OSError", "TimeoutExpired", "HTTPError", "URLError", "RuntimeError", "ValueError", "KeyError", "TypeError"}
        runtime.result["failure_kind"] = type(error).__name__ if type(error).__name__ in kinds else "other"
        if runtime.result["phase"] == "runtime":
            print("matrix trace counts: " + json.dumps(trace_counts(runtime.private / "runtime-events.jsonl"), sort_keys=True), flush=True)
            patterns = {
                "sender_denied": b"plugin_channel_sender_denied",
                "owner_missing": b"no agent owns this channel",
                "inbound_received": b"channel inbound message",
                "listener_error": b"channel listener error",
                "poll_error": b"matrix: HTTP",
                "reply_error": b"failed to reply",
                "stream_fallback": b"llm_stream_fallback",
                "llm_completed": b"LLM call completed",
                "http_deadline": b"matrix: HTTP deadline exceeded",
                "send_refused": b"matrix: encryption state present or uncertain",
                "fuel_exhausted": b"fuel consumed",
                "send_trapped": b"channel.send trapped",
                "call_deadline": b"deadline exceeded",
            }
            paths = list(runtime.private.glob("runtime-*.log")) + list(runtime.private.glob("runtime-events.jsonl"))
            for label, pattern in patterns.items():
                found = any(pattern in path.read_bytes() for path in paths)
                print("matrix runtime flag: " + label + "=" + str(found).lower(), flush=True)
    finally:
        failed_phase = runtime.result["phase"]
        print("matrix acceptance phase: cleanup", flush=True)
        runtime.stop_watch.set()
        runtime.watcher.join(timeout=3)
        fixture_root = runtime.private / "fixture"
        cleanup = [runtime.stop]
        if runtime.observer:
            cleanup.append(runtime.observer.close)
        if fixture_root.exists():
            cleanup.append(lambda: subprocess.run(["python3", str(HERE / "prepare_synapse.py"), "cleanup", str(fixture_root)], check=True, capture_output=True, timeout=120))
        cleanup.extend(log.close for log in runtime.logs)
        cleanup_failed = runtime.watcher.is_alive()
        for action in cleanup:
            try:
                action()
            except Exception:
                cleanup_failed = True
        if not cleanup_failed:
            runtime.result["cleanup"] = True
            runtime.case("owned-teardown")
        else:
            runtime.result.update(status="failed", phase="cleanup", failure_kind="RuntimeError")
        if runtime.result["status"] == "passed":
            runtime.result["phase"] = "complete"
        elif runtime.result["phase"] != "cleanup":
            runtime.result["phase"] = failed_phase
        (runtime.public / "result.json").write_text(json.dumps(runtime.result, sort_keys=True, indent=2) + "\n")
        check_evidence(root)
    print("matrix acceptance: " + runtime.result["status"] + " / " + runtime.result["phase"])
    return 0 if runtime.result["status"] == "passed" else 1


def sys_optimized():
    import sys
    return bool(sys.flags.optimize)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--verify-inputs-only", action="store_true")
    mode.add_argument("--check-evidence", action="store_true")
    mode.add_argument("--run", action="store_true")
    parser.add_argument("--host-zip", type=Path)
    parser.add_argument("--guest-zip", type=Path)
    args = parser.parse_args()
    try:
        require(not sys_optimized())
        if args.verify_inputs_only:
            require(args.host_zip is not None and args.guest_zip is not None)
            verify_inputs(args.host_zip, args.guest_zip)
            print("inputs-only verification: PASS; no extraction, network or execution")
        elif args.check_evidence:
            check_evidence(HERE.parent)
            print("evidence allowlist: PASS")
        else:
            raise SystemExit(run(HERE.parent))
    except Exception:
        print("acceptance admission or evidence check failed")
        raise SystemExit(1) from None
