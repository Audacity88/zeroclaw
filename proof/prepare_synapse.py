#!/usr/bin/env python3
"""Hosted-run-only, synthetic Synapse provisioning. Never reads real config.

The immutable image must already have been acquired. No request or helper
output is copied to public evidence. Container listener is bridge-local;
Docker publishes only 127.0.0.1 on the host, on an internal-only network.
"""

import argparse
import json
import os
from pathlib import Path
import secrets
import subprocess
import sys
import time
import urllib.error
import urllib.request

IMAGE = "ghcr.io/element-hq/synapse@sha256:43fd704aedef503a6fba2e5696439ef7bac24479a3472e364561973f550e4aad"
LABEL = "zeroclaw.matrix-proof.owner"
HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def private_write(path, value):
    with open(path, "x", encoding="utf-8", opener=lambda p, f: os.open(p, f, 0o600)) as out:
        out.write(value)


def docker(*args):
    return subprocess.run(
        ["docker", *args], check=True, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, timeout=60, text=True,
    ).stdout.strip()


def request(base, path, body=None, token=None):
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = "Bearer " + token
    req = urllib.request.Request(
        base + "/_matrix/client/v3/" + path,
        data=None if body is None else json.dumps(body).encode(), headers=headers,
    )
    with HTTP.open(req, timeout=5) as response:
        return json.load(response)


def prepare(root):
    os.umask(0o077)
    root = root.resolve(strict=True)
    data = root / "synapse"
    data.mkdir(mode=0o700)
    owner = secrets.token_hex(12)
    network, container = "matrix-proof-" + owner, "matrix-proof-" + owner
    # Persist ownership before any resource creation, so failure is cleanable.
    private_write(root / "owned.json", json.dumps({
        "owner": owner, "network": network, "container": container,
    }))
    shared_secret = secrets.token_hex(32)
    private_write(data / "homeserver.yaml", f"""server_name: proof.test
pid_file: /data/synapse.pid
public_baseurl: http://127.0.0.1:8008/
report_stats: false
enable_registration: false
registration_shared_secret: {shared_secret}
allow_public_rooms_without_auth: false
allow_public_rooms_over_federation: false
federation_domain_whitelist: []
send_federation: false
trusted_key_servers: []
listeners:
  - port: 8008
    type: http
    tls: false
    bind_addresses: ['0.0.0.0']
    x_forwarded: false
    resources:
      - names: [client]
        compress: false
database:
  name: sqlite3
  args:
    database: /data/homeserver.db
media_store_path: /data/media
signing_key_path: /data/proof.test.signing.key
log_config: /data/log.yaml
macaroon_secret_key: {secrets.token_hex(32)}
form_secret: {secrets.token_hex(32)}
""")
    private_write(data / "log.yaml", """version: 1
disable_existing_loggers: true
handlers:
  silent:
    class: logging.NullHandler
root:
  level: CRITICAL
  handlers: [silent]
""")
    docker("image", "inspect", IMAGE)
    docker("network", "create", "--internal", "--label", LABEL + "=" + owner, network)
    docker(
        "run", "--detach", "--pull=never", "--name", container,
        "--label", LABEL + "=" + owner, "--network", network,
        "--publish", "127.0.0.1::8008", "--user", f"{os.getuid()}:{os.getgid()}",
        "--cap-drop=ALL", "--security-opt=no-new-privileges", "--log-driver=none",
        "--mount", f"type=bind,src={data},dst=/data", "--entrypoint", "/bin/sh",
        IMAGE, "-ec",
        "python -m synapse.app.homeserver --generate-keys -c /data/homeserver.yaml; "
        "exec python -m synapse.app.homeserver -c /data/homeserver.yaml",
    )
    details = json.loads(docker("inspect", container))[0]
    mapping = details["NetworkSettings"]["Ports"]["8008/tcp"]
    if len(mapping) != 1 or mapping[0]["HostIp"] != "127.0.0.1":
        raise RuntimeError("isolation")
    if not json.loads(docker("network", "inspect", network))[0]["Internal"]:
        raise RuntimeError("isolation")
    base = "http://127.0.0.1:" + mapping[0]["HostPort"]
    deadline = time.monotonic() + 60
    while True:
        try:
            request(base, "login")
            break
        except (urllib.error.URLError, TimeoutError, OSError):
            if time.monotonic() >= deadline:
                raise RuntimeError("startup") from None
            time.sleep(0.2)
    users = {}
    for name in ("bot", "sender", "deny"):
        password = secrets.token_urlsafe(32)
        docker(
            "exec", container, "python", "-m", "synapse._scripts.register_new_matrix_user",
            "-c", "/data/homeserver.yaml", "-u", name, "-p", password,
            "--no-admin", "http://127.0.0.1:8008",
        )
        login = request(base, "login", {
            "type": "m.login.password", "identifier": {"type": "m.id.user", "user": name},
            "password": password,
        })
        users[name] = {"id": login["user_id"], "token": login["access_token"]}
    room = request(base, "createRoom", {
        "visibility": "private", "preset": "private_chat",
        "invite": [users["sender"]["id"], users["deny"]["id"]],
        "power_level_content_override": {"users": {users["bot"]["id"]: 100}},
    }, users["bot"]["token"])["room_id"]
    from urllib.parse import quote
    for name in ("sender", "deny"):
        request(base, "join/" + quote(room, safe=""), {}, users[name]["token"])
    private_write(root / "fixture.json", json.dumps({"homeserver": base, "room": room, **users}))


def cleanup(root):
    ownership = root / "owned.json"
    if not ownership.exists():
        return
    owned = json.loads(ownership.read_text())
    for kind, name in (("container", owned["container"]), ("network", owned["network"])):
        probe = subprocess.run(["docker", kind, "inspect", name], capture_output=True, text=True, timeout=15)
        if probe.returncode:
            if "No such" in probe.stderr:
                continue
            raise RuntimeError("cleanup inspection")
        resource = json.loads(probe.stdout)[0]
        labels = resource["Config"]["Labels"] if kind == "container" else resource["Labels"]
        if labels.get(LABEL) != owned["owner"]:
            raise RuntimeError("ownership")
        if kind == "container":
            docker("stop", "--time=10", name)
            docker("container", "rm", name)
        else:
            docker("network", "rm", name)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=["prepare", "cleanup"])
    parser.add_argument("private_root", type=Path)
    args = parser.parse_args()
    try:
        (prepare if args.mode == "prepare" else cleanup)(args.private_root)
    except Exception:
        # Exception bodies and subprocess output may contain synthetic tokens.
        print("synapse_" + args.mode + "_failed", file=sys.stderr)
        sys.exit(1)
