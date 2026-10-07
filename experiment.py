#!/usr/bin/env python3
"""One-off orchestration around the pinned upstream footprint reporters."""

import gzip
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tarfile
import time

SOURCE = "cb58607b8ea29c725d31bd44d629d80ec0bce41e"
REPORTER = "40cb7be43e7db9b40cd5f720ee34d2ae964de415"
LOCK = "d64a6580ec5c7e247b2fce385f247186b08a82c0300b124120179634c7a78e1e"
TARGET = "x86_64-unknown-linux-gnu"
PROFILES = ("full-dist", "no-saas-coding", "no-native-adapters")
GIB = 1024**3
HERE = Path(__file__).resolve().parent
WORKSPACE = HERE.parent
POLICY = HERE / "measurement-policy.toml"


def output(command, cwd=None):
    return subprocess.check_output(command, cwd=cwd, text=True).strip()


def digest(path):
    with path.open("rb") as handle:
        return hashlib.file_digest(handle, "sha256").hexdigest()


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def cargo(arguments):
    temporary = Path(os.environ["RUNNER_TEMP"])
    require(shutil.disk_usage(temporary).free >= 4 * GIB, "Cargo admission: less than 4 GiB free")
    child = subprocess.Popen(["cargo", *arguments], start_new_session=True)
    try:
        while child.poll() is None:
            if shutil.disk_usage(temporary).free < 4 * GIB:
                raise RuntimeError("Cargo stopped: free space fell below 4 GiB reserve")
            time.sleep(1)
        return child.returncode
    finally:
        if child.poll() is None:
            os.killpg(child.pid, signal.SIGTERM)
            try:
                child.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()


def check_closures(report):
    profiles = {p["id"]: p for p in report["profiles"]}
    require(set(profiles) == set(PROFILES), "closure is missing an experiment arm")
    full = profiles["full-dist"]
    original = set(full["resolved_inputs"]["features"])
    adapters = {f for f in original if f.startswith("tool-")}
    require(len(original) == 29 and len(adapters) == 12, "canonical feature shape changed")
    enabled = lambda p: {x["name"]: set(x["features"]) for x in p["enabled_features"] if x["name"].startswith("zeroclaw")}
    base = enabled(full)
    for name in PROFILES[1:]:
        profile = profiles[name]
        expected = original - adapters
        if name == "no-native-adapters":
            expected.remove("tools-external")
        require(set(profile["resolved_inputs"]["features"]) == expected, "omission input mismatch")
        current = enabled(profile)
        for package in ("zeroclaw", "zeroclaw-config", "zeroclaw-runtime", "zeroclaw-tools", "zeroclaw-gateway"):
            require(not (adapters | {"tools-compat", "tools-saas", "tools-coding-cli"}) & current[package], "adapter re-enabled")
        for package in ("zeroclaw-channels", "zeroclaw-plugins"):
            require(current[package] == base[package], "channel/plugin features changed")
        for package in ("zeroclaw", "zeroclaw-config", "zeroclaw-runtime", "zeroclaw-tools"):
            require(("tools-external" in current[package]) == (name == "no-saas-coding"), "external-tool mismatch")
        require(current["zeroclaw-gateway"] == base["zeroclaw-gateway"] - {"tool-claude-code-runner"}, "gateway changed beyond adapter")
    require(all(c["status"] == "passed" for c in report["contracts"]), "closure contract failed")


def measure(profile):
    require(profile in PROFILES, "unknown experiment arm")
    destination = HERE / "results" / profile
    destination.mkdir(parents=True, exist_ok=False)
    temporary = Path(os.environ["RUNNER_TEMP"])
    source = WORKSPACE / "source"
    reporter = WORKSPACE / "reporter"
    require(output(["git", "rev-parse", "HEAD"], source) == SOURCE, "wrong source revision")
    require(output(["git", "rev-parse", "HEAD"], reporter) == REPORTER, "wrong reporter revision")
    require(not output(["git", "status", "--porcelain"], source), "source is dirty")
    require(digest(source / "Cargo.lock") == LOCK, "wrong source lockfile")
    require(output(["uname", "-m"]) == "x86_64" and sys.platform == "linux", "not native Linux x86_64")
    require(output(["rustc", "--version"]).startswith("rustc 1.98.0 "), "wrong compiler")
    require(shutil.disk_usage(temporary).free >= 20 * GIB, "need 20 GiB free before cold build")
    os.environ["CARGO_TARGET_DIR"] = str(temporary / "culling-targets" / "generator")
    os.environ["SOURCE_DATE_EPOCH"] = output(["git", "show", "-s", "--format=%ct", "HEAD"], source)
    context = {
        "source_tree": output(["git", "rev-parse", "HEAD^{tree}"], source),
        "reporter_revision": REPORTER,
        "image_os": os.environ["ImageOS"],
        "image_version": os.environ["ImageVersion"],
        "machine": output(["uname", "-m"]),
        "cc": output(["cc", "--version"]),
        "ld": output(["ld", "--version"]),
        "source_date_epoch": os.environ["SOURCE_DATE_EPOCH"],
        "cargo_jobs": os.environ["CARGO_BUILD_JOBS"],
        "incremental": os.environ["CARGO_INCREMENTAL"],
        "native_build_env": {name: os.environ[name] for name in ("CC", "CXX", "CFLAGS", "CXXFLAGS", "CPPFLAGS", "LDFLAGS", "AR", "RANLIB", "PKG_CONFIG_PATH", "PKG_CONFIG_LIBDIR", "PKG_CONFIG_SYSROOT_DIR") if name in os.environ},
        "free_bytes_at_admission": shutil.disk_usage(temporary).free,
    }
    (destination / "builder.json").write_text(json.dumps(context, indent=2) + "\n")
    common = ["--repo-root", str(source), "--policy", str(POLICY), "--target", TARGET, "--cargo", str(Path(__file__).resolve())]
    subprocess.run([sys.executable, str(reporter / "scripts/ci/dependency_footprint.py"), "capture", *common, "--output", str(destination / "closure.json")], cwd=source, check=True)
    closure = json.loads((destination / "closure.json").read_text())
    check_closures(closure)
    subprocess.run([sys.executable, str(reporter / "scripts/ci/binary_size_report.py"), "measure", *common, "--profile", profile, "--target-dir", str(temporary / "culling-targets" / "release"), "--output", str(destination / "binary.json")], cwd=source, check=True)
    report = json.loads((destination / "binary.json").read_text())
    binary = temporary / "culling-targets" / "release" / report["measurements"][0]["path"]
    with binary.open("rb") as contents:
        header = contents.read(20)
    require(header[:6] == b"\x7fELF\x02\x01" and int.from_bytes(header[18:20], "little") == 62, "not a Linux x86_64 ELF")
    archive = destination / "zeroclaw.tar.gz"
    with archive.open("wb") as handle, gzip.GzipFile(fileobj=handle, mode="wb", filename="", mtime=0, compresslevel=9) as compressed, tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        info = tar.gettarinfo(str(binary), arcname="zeroclaw")
        info.uid = info.gid = info.mtime = 0
        info.uname = info.gname = ""
        info.mode = 0o755
        with binary.open("rb") as contents:
            tar.addfile(info, contents)
    (destination / "archive.json").write_text(json.dumps({"bytes": archive.stat().st_size, "sha256": digest(archive)}, indent=2) + "\n")


def summarize():
    sys.path.insert(0, str(WORKSPACE / "reporter/scripts/ci"))
    import binary_size_report as reporter
    policy, policy_hash = reporter.read_policy(str(POLICY))
    records = {}
    controls = None
    for name in PROFILES:
        path = HERE / "results" / f"footprint-{name}"
        report = json.loads((path / "binary.json").read_text())
        reporter.validate_report(report, name, policy, policy_hash)
        require([m["id"] for m in report["measurements"]] == [name], "wrong arm artifact")
        builder = json.loads((path / "builder.json").read_text())
        archive = json.loads((path / "archive.json").read_text())
        require(digest(path / "zeroclaw.tar.gz") == archive["sha256"], "archive digest mismatch")
        require((path / "zeroclaw.tar.gz").stat().st_size == archive["bytes"], "archive size mismatch")
        with tarfile.open(path / "zeroclaw.tar.gz", "r:gz") as tar:
            members = tar.getmembers()
            require(len(members) == 1 and members[0].name == "zeroclaw" and members[0].isfile(), "wrong archive member")
            require(members[0].size == report["measurements"][0]["bytes"], "archived binary size mismatch")
            with tar.extractfile(members[0]) as contents:
                require(hashlib.file_digest(contents, "sha256").hexdigest() == report["measurements"][0]["sha256"], "archived binary digest mismatch")
        closure = json.loads((path / "closure.json").read_text())
        check_closures(closure)
        context = report["context"]
        require(context["git_revision"] == SOURCE and context["cargo_lock_sha256"] == LOCK and not context["git_dirty"], "source control mismatch")
        require(context["target"] == TARGET, "target mismatch")
        require({k: v for k, v in closure["context"].items() if k != "resolved_selections"} == {k: v for k, v in context.items() if k not in ("resolved_selections", "build_env")}, "closure/build context mismatch")
        selection = next(p["resolved_inputs"] for p in closure["profiles"] if p["id"] == name)
        require(selection == report["measurements"][0]["resolved_inputs"], "closure/build feature mismatch")
        current = {"context": {k: v for k, v in context.items() if k != "resolved_selections"}, "profile": report["cargo_profile"], "builder": {k: v for k, v in builder.items() if k != "free_bytes_at_admission"}}
        require(controls is None or current == controls, "toolchain/profile/environment/runner controls differ across arms")
        controls = current
        records[name] = {"binary": report["measurements"][0], "archive": archive}
    baseline = records["full-dist"]["binary"]["bytes"]
    summary = {"kind": "intentional-capability-cost-experiment", "same_capability_regression": False, "controls": controls, "arms": records, "omission_savings_bytes": {name: baseline - records[name]["binary"]["bytes"] for name in PROFILES[1:]}, "incremental_external_omission_bytes": records["no-saas-coding"]["binary"]["bytes"] - records["no-native-adapters"]["binary"]["bytes"]}
    (HERE / "results/summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as handle:
        handle.write("## Intentional capability-cost experiment\n\n| Arm | Binary bytes | tar.gz bytes |\n| --- | ---: | ---: |\n")
        for name, record in records.items():
            handle.write(f"| {name} | {record['binary']['bytes']} | {record['archive']['bytes']} |\n")
        handle.write("\nOmission savings are capability-cost controls, not equivalent-capability regression results.\n")


if __name__ == "__main__":
    try:
        command = sys.argv[1]
        if command == "measure":
            measure(sys.argv[2])
        elif command == "summarize":
            summarize()
        else:
            sys.exit(cargo(sys.argv[1:]))
    except Exception as error:
        print(f"experiment failed: {error}", file=sys.stderr)
        if sys.argv[1:2] == ["measure"] and sys.argv[2:3] and sys.argv[2] in PROFILES:
            destination = HERE / "results" / sys.argv[2]
            destination.mkdir(parents=True, exist_ok=True)
            (destination / "failure.json").write_text(json.dumps({"error": str(error), "complete": False}, indent=2) + "\n")
        sys.exit(1)
