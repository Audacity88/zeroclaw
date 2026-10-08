#!/usr/bin/env python3
"""Bounded same-runner pairs around unchanged upstream footprint reporters."""

import gzip
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tomllib

import experiment as common

FULL = (
    "acp-bridge", "agent-runtime", "channel-acp-server", "channel-discord",
    "channel-email", "channel-filesystem", "channel-git", "channel-lark",
    "channel-matrix", "channel-telegram", "channel-webhook", "gateway",
    "observability-prometheus", "plugins-wasm-cranelift", "schema-export",
    "tool-claude-code", "tool-claude-code-runner", "tool-codex-cli",
    "tool-composio", "tool-gemini-cli", "tool-google-workspace", "tool-jira",
    "tool-linkedin", "tool-microsoft365", "tool-notion", "tool-opencode-cli",
    "tool-project-intel", "tools-external", "whatsapp-web",
)
LANES = {
    "tuning": {"full-dist": (), "opt-s": ()},
    "plugin": {"full-dist": (), "no-plugin-host": ("plugins-wasm-cranelift",)},
    "channels": {
        "full-dist": (), "no-matrix": ("channel-matrix",),
        "no-whatsapp": ("whatsapp-web",), "no-git": ("channel-git",),
    },
    "gateway": {"full-dist": (), "no-gateway": ("gateway",)},
    "tls": {"full-dist": (), "matrix-ring": ()},
}
HERE = Path(__file__).resolve().parent
WORKSPACE = HERE.parent
SOURCE = WORKSPACE / "source"
REPORTER = WORKSPACE / "reporter"
PATCH = HERE / "profile-s.patch"
TLS_PATCH = HERE / "matrix-ring.patch"
MANIFESTS = {
    "Cargo.toml": "4225ae1b982da32a5794dc699c6e4cea9d7dcd880b7ad3cfc9db14d01cab7444",
    "crates/zeroclaw-channels/Cargo.toml": "826b1179d07ce19c5899a4727fe23f3ee1a1265c25c993db5ed14132c3b89b34",
}
PATCHES = {"opt-s": PATCH, "matrix-ring": TLS_PATCH}
require = common.require
output = common.output
digest = common.digest


def policy_text(lane):
    rows = ['schema_version = 1', 'edge_kinds = ["normal", "build"]']
    for name, omitted in LANES[lane].items():
        rows += ["", "[[profiles]]", f'id = "{name}"', 'package = "zeroclaw"']
        if name == "full-dist":
            rows += ['mode = "selection"', 'selection = "dist"']
        else:
            rows += ['mode = "features"', "features = " + json.dumps([f for f in FULL if f not in omitted])]
    return "\n".join(rows) + "\n"


def check_closure(report, lane):
    profiles = {p["id"]: p for p in report["profiles"]}
    require(set(profiles) == set(LANES[lane]), "wrong closure profiles")
    require(report["context"]["resolved_selections"]["dist"] == list(FULL), "canonical distribution changed")
    for name, omitted in LANES[lane].items():
        expected = set(FULL) - set(omitted)
        require(set(profiles[name]["resolved_inputs"]["features"]) == expected, "wrong omission inputs")
        enabled = {p["name"]: set(p["features"]) for p in profiles[name]["enabled_features"]}
        for feature in FULL:
            require((feature in enabled["zeroclaw"]) == (feature in expected), f"root feature mismatch: {feature}")
        if name == "no-plugin-host":
            require(not any(f.startswith("plugins-wasm") for f in enabled["zeroclaw"]), "plugin host re-enabled")
            require("wasmtime" not in enabled, "Wasmtime retained without host")
        if name == "no-gateway":
            require("zeroclaw-gateway" not in enabled, "gateway dependency re-enabled")
        if name in ("no-matrix", "no-whatsapp", "no-git"):
            require(not set(omitted) & enabled["zeroclaw-channels"], "channel re-enabled")
        if lane == "tls" and report["context"]["git_dirty"]:
            reqwest13 = [p for p in profiles[name]["enabled_features"] if p["name"] == "reqwest" and p["version"].startswith("0.13.")]
            require(len(reqwest13) == 1, "wrong Matrix HTTP client version")
            require(not {"rustls", "__rustls-aws-lc-rs"} & set(reqwest13[0]["features"]), "Matrix TLS fallback re-enabled")
            require("rustls-aws-lc-rs" not in enabled["matrix-sdk"], "Matrix provider feature re-enabled")


def expected_manifests(name):
    result = dict(MANIFESTS)
    if name == "opt-s":
        result["Cargo.toml"] = "7b322cd0ec23a522af76d72dd2dc50706859e4a6d7b40a1cad25c8b9780839b7"
    if name == "matrix-ring":
        result["crates/zeroclaw-channels/Cargo.toml"] = "696e79f14217dff81294abee4525d56e6cb79aa6c5f0fbf29040dbd1d1469768"
    return result


def source_guard(name):
    require(output(["git", "rev-parse", "HEAD"], SOURCE) == common.SOURCE, "wrong source revision")
    require(digest(SOURCE / "Cargo.lock") == common.LOCK, "wrong source lockfile")
    require({p: digest(SOURCE / p) for p in MANIFESTS} == expected_manifests(name), "unexpected manifest change")
    status = output(["git", "status", "--porcelain"], SOURCE)
    expected_status = {"opt-s": "M Cargo.toml", "matrix-ring": "M crates/zeroclaw-channels/Cargo.toml"}.get(name, "")
    require(status == expected_status, "unexpected source dirt")


def archive_binary(binary, destination):
    with binary.open("rb") as handle:
        header = handle.read(20)
    require(header[:6] == b"\x7fELF\x02\x01" and int.from_bytes(header[18:20], "little") == 62, "wrong ELF target")
    path = destination / "zeroclaw.tar.gz"
    with path.open("wb") as handle, gzip.GzipFile(fileobj=handle, mode="wb", filename="", mtime=0, compresslevel=9) as compressed, tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        info = tar.gettarinfo(str(binary), arcname="zeroclaw")
        info.uid = info.gid = info.mtime = 0
        info.uname = info.gname = ""
        info.mode = 0o755
        with binary.open("rb") as contents:
            tar.addfile(info, contents)
    (destination / "archive.json").write_text(json.dumps({"bytes": path.stat().st_size, "sha256": digest(path)}, indent=2) + "\n")


def run_lane(lane):
    require(lane in LANES, "unknown lane")
    require(output(["uname", "-m"]) == "x86_64" and sys.platform == "linux", "not native Linux x86_64")
    require(output(["rustc", "--version"]).startswith("rustc 1.98.0 "), "wrong compiler")
    require(output(["git", "rev-parse", "HEAD"], REPORTER) == common.REPORTER, "wrong reporter revision")
    require(not output(["git", "status", "--porcelain"], REPORTER), "reporter is dirty")
    source_guard("full-dist")
    root = HERE / "results" / lane
    root.mkdir(parents=True, exist_ok=False)
    policy = root / "policy.toml"
    policy.write_text(policy_text(lane))
    os.environ["CARGO_TARGET_DIR"] = str(Path(os.environ["RUNNER_TEMP"]) / "culling-targets" / "generator")
    os.environ["SOURCE_DATE_EPOCH"] = output(["git", "show", "-s", "--format=%ct", "HEAD"], SOURCE)
    current = "full-dist"
    applied = None
    try:
        for current in LANES[lane]:
            destination = root / current
            destination.mkdir()
            if current in PATCHES:
                patch = PATCHES[current]
                subprocess.run(["git", "apply", "--check", str(patch)], cwd=SOURCE, check=True)
                subprocess.run(["git", "apply", str(patch)], cwd=SOURCE, check=True)
                applied = current
            source_guard(current)
            temporary = Path(os.environ["RUNNER_TEMP"])
            require(common.shutil.disk_usage(temporary).free >= 20 * common.GIB, "need 20 GiB free before build")
            builder = {
                "source_tree": output(["git", "rev-parse", "HEAD^{tree}"], SOURCE),
                "reporter_revision": common.REPORTER,
                "image_os": os.environ["ImageOS"], "image_version": os.environ["ImageVersion"],
                "machine": output(["uname", "-m"]),
                "cc": output(["cc", "--version"]), "ld": output(["ld", "--version"]),
                "source_date_epoch": os.environ["SOURCE_DATE_EPOCH"],
                "cargo_jobs": os.environ["CARGO_BUILD_JOBS"], "incremental": os.environ["CARGO_INCREMENTAL"],
                "native_build_env": {n: os.environ[n] for n in ("CC", "CXX", "CFLAGS", "CXXFLAGS", "CPPFLAGS", "LDFLAGS", "AR", "RANLIB", "PKG_CONFIG_PATH", "PKG_CONFIG_LIBDIR", "PKG_CONFIG_SYSROOT_DIR") if n in os.environ},
                "manifest_sha256": {p: digest(SOURCE / p) for p in MANIFESTS},
                "patch_sha256": digest(PATCHES[current]) if current in PATCHES else None,
                "free_bytes_at_admission": common.shutil.disk_usage(temporary).free,
            }
            (destination / "builder.json").write_text(json.dumps(builder, indent=2) + "\n")
            options = ["--repo-root", str(SOURCE), "--policy", str(policy), "--target", common.TARGET, "--cargo", str(Path(__file__).resolve())]
            subprocess.run([sys.executable, str(REPORTER / "scripts/ci/dependency_footprint.py"), "capture", *options, "--output", str(destination / "closure.json")], cwd=SOURCE, check=True)
            check_closure(json.loads((destination / "closure.json").read_text()), lane)
            targets = temporary / "culling-targets" / "release"
            subprocess.run([sys.executable, str(REPORTER / "scripts/ci/binary_size_report.py"), "measure", *options, "--profile", current, "--target-dir", str(targets), "--output", str(destination / "binary.json")], cwd=SOURCE, check=True)
            source_guard(current)
            report = json.loads((destination / "binary.json").read_text())
            archive_binary(targets / report["measurements"][0]["path"], destination)
    except Exception as error:
        (root / "failure.json").write_text(json.dumps({"arm": current, "error": str(error), "complete": False}, indent=2) + "\n")
        raise
    finally:
        if applied:
            source_guard(applied)
            subprocess.run(["git", "apply", "--reverse", str(PATCHES[applied])], cwd=SOURCE, check=True)
        source_guard("full-dist")
    summarize(lane, root)


def summarize(lane, root):
    sys.path.insert(0, str(REPORTER / "scripts/ci"))
    import binary_size_report as reporter
    policy, policy_hash = reporter.read_policy(str(root / "policy.toml"))
    require((root / "policy.toml").read_text() == policy_text(lane), "unexpected policy")
    records = {}
    baseline = None
    for name in LANES[lane]:
        destination = root / name
        report = json.loads((destination / "binary.json").read_text())
        reporter.validate_report(report, name, policy, policy_hash)
        require([m["id"] for m in report["measurements"]] == [name], "wrong binary arm")
        closure = json.loads((destination / "closure.json").read_text())
        check_closure(closure, lane)
        context = report["context"]
        require(context["git_revision"] == common.SOURCE and context["cargo_lock_sha256"] == common.LOCK and context["target"] == common.TARGET, "wrong source/lock/target")
        require(context["git_dirty"] == (name in PATCHES), "unexpected source dirt in report")
        require({k: v for k, v in closure["context"].items() if k != "resolved_selections"} == {k: v for k, v in context.items() if k not in ("resolved_selections", "build_env")}, "closure/build context mismatch")
        require(next(p["resolved_inputs"] for p in closure["profiles"] if p["id"] == name) == report["measurements"][0]["resolved_inputs"], "closure/build selection mismatch")
        builder = json.loads((destination / "builder.json").read_text())
        require(builder["reporter_revision"] == common.REPORTER and builder["patch_sha256"] == (digest(PATCHES[name]) if name in PATCHES else None), "wrong reporter/patch")
        require(builder["manifest_sha256"] == expected_manifests(name), "wrong manifest identities")
        archive = json.loads((destination / "archive.json").read_text())
        path = destination / "zeroclaw.tar.gz"
        require(path.stat().st_size == archive["bytes"] and digest(path) == archive["sha256"], "wrong archive identity")
        with tarfile.open(path, "r:gz") as tar:
            members = tar.getmembers()
            require(len(members) == 1 and members[0].name == "zeroclaw" and members[0].isfile(), "wrong archive member")
            binary = report["measurements"][0]
            require(members[0].size == binary["bytes"], "wrong archived binary size")
            with tar.extractfile(members[0]) as handle:
                require(hashlib.file_digest(handle, "sha256").hexdigest() == binary["sha256"], "wrong archived binary digest")
        record = {"report": report, "builder": builder, "archive": archive}
        if baseline is None:
            baseline = record
        else:
            permitted = {"git_dirty", "git_worktree_digest_sha256"} if name in PATCHES else set()
            normalize = lambda c: {k: v for k, v in c.items() if k not in permitted | {"resolved_selections"}}
            require(normalize(context) == normalize(baseline["report"]["context"]), "nonexperimental source/toolchain/environment drift")
            settings = baseline["report"]["cargo_profile"]
            if name == "opt-s":
                settings = json.loads(json.dumps(settings))
                require(settings["settings"]["opt-level"] == "z", "wrong original profile")
                settings["settings"]["opt-level"] = "s"
                require(closure["profiles"][0]["enabled_features"] == json.loads((root / "full-dist/closure.json").read_text())["profiles"][0]["enabled_features"], "tuning changed closure")
            require(report["cargo_profile"] == settings, "unexpected profile difference")
            ignored = {"free_bytes_at_admission"} | ({"manifest_sha256", "patch_sha256"} if name in PATCHES else set())
            normalize_builder = lambda b: {k: v for k, v in b.items() if k not in ignored}
            require(normalize_builder(builder) == normalize_builder(baseline["builder"]), "runner/native controls drift")
        records[name] = record
    base_bytes = baseline["report"]["measurements"][0]["bytes"]
    kind = {"tuning": "intentional-profile-cost", "tls": "diagnostic-TLS-fallback-cost"}.get(lane, "intentional-capability-cost")
    result = {"lane": lane, "source": common.SOURCE, "reporter": common.REPORTER, "target": common.TARGET, "arms": records, "savings_bytes": {n: base_bytes - r["report"]["measurements"][0]["bytes"] for n, r in records.items() if n != "full-dist"}, "comparison": kind}
    (root / "summary.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def aggregate(root):
    results = {lane: summarize(lane, root / lane) for lane in LANES}
    (root / "summary.json").write_text(json.dumps({"lanes": results, "additive": False}, indent=2) + "\n")
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as handle:
        handle.write("## Same-runner paired footprint experiments\n\n| Lane | Arm | Binary bytes | Savings vs own baseline |\n| --- | --- | ---: | ---: |\n")
        for lane, result in results.items():
            for name, record in result["arms"].items():
                handle.write(f"| {lane} | {name} | {record['report']['measurements'][0]['bytes']} | {result['savings_bytes'].get(name, 0)} |\n")
        handle.write("\nEach lane has its own same-runner baseline. Savings are not additive. Omission builds are attribution controls, not retirement or replacement proof.\n")


if __name__ == "__main__":
    try:
        if sys.argv[1] == "lane":
            run_lane(sys.argv[2])
        elif sys.argv[1] == "aggregate":
            aggregate(Path(sys.argv[2]))
        else:
            sys.exit(common.cargo(sys.argv[1:]))
    except Exception as error:
        print(f"parallel experiment failed: {error}", file=sys.stderr)
        sys.exit(1)
