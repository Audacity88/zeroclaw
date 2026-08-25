#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
selector="${script_dir}/windows_test_scope.py"
workflow="${script_dir}/../../.github/workflows/ci.yml"
fixture_dir="$(mktemp -d)"
trap 'rm -rf "$fixture_dir"' EXIT

repo_root="${fixture_dir}/repo"
mkdir -p "$repo_root/crates/zeroclaw-channels" "$repo_root/crates/zeroclaw-providers" "$repo_root/apps/tauri"
metadata_file="${fixture_dir}/metadata.json"
cat > "$metadata_file" <<EOF
{
  "packages": [
    {"id": "path+file://${repo_root}#zeroclaw 0.8.4", "name": "zeroclaw", "manifest_path": "Cargo.toml"},
    {"id": "path+file://${repo_root}/crates/zeroclaw-channels#zeroclaw-channels 0.8.4", "name": "zeroclaw-channels", "manifest_path": "crates/zeroclaw-channels/Cargo.toml"},
    {"id": "path+file://${repo_root}/crates/zeroclaw-providers#zeroclaw-providers 0.8.4", "name": "zeroclaw-providers", "manifest_path": "crates/zeroclaw-providers/Cargo.toml"},
    {"id": "path+file://${repo_root}/apps/tauri#zeroclaw-desktop 0.8.4", "name": "zeroclaw-desktop", "manifest_path": "apps/tauri/Cargo.toml"}
  ],
  "workspace_members": [
    "path+file://${repo_root}#zeroclaw 0.8.4",
    "path+file://${repo_root}/crates/zeroclaw-channels#zeroclaw-channels 0.8.4",
    "path+file://${repo_root}/crates/zeroclaw-providers#zeroclaw-providers 0.8.4",
    "path+file://${repo_root}/apps/tauri#zeroclaw-desktop 0.8.4"
  ],
  "resolve": {
    "nodes": [
      {"id": "path+file://${repo_root}#zeroclaw 0.8.4", "deps": [{"pkg": "path+file://${repo_root}/crates/zeroclaw-channels#zeroclaw-channels 0.8.4"}, {"pkg": "path+file://${repo_root}/crates/zeroclaw-providers#zeroclaw-providers 0.8.4"}]},
      {"id": "path+file://${repo_root}/crates/zeroclaw-channels#zeroclaw-channels 0.8.4", "deps": []},
      {"id": "path+file://${repo_root}/crates/zeroclaw-providers#zeroclaw-providers 0.8.4", "deps": []},
      {"id": "path+file://${repo_root}/apps/tauri#zeroclaw-desktop 0.8.4", "deps": [{"pkg": "path+file://${repo_root}/crates/zeroclaw-channels#zeroclaw-channels 0.8.4"}]}
    ]
  }
}
EOF

run_selector() {
    local event="$1"
    local paths_file="$2"
    local metadata="$3"
    python3 "$selector" --event "$event" --changed-paths-file "$paths_file" --metadata-file "$metadata" --repo-root "$repo_root"
}

assert_selection() {
    local name="$1"
    local expected_mode="$2"
    local expected_packages="$3"
    local expected_reason="$4"
    local paths_file="$5"
    local output
    output="$(run_selector pull_request "$paths_file" "$metadata_file")"
    SELECTION_OUTPUT="$output" EXPECTED_MODE="$expected_mode" EXPECTED_PACKAGES="$expected_packages" EXPECTED_REASON="$expected_reason" python3 - <<'PY'
import json
import os

values = {}
for line in os.environ["SELECTION_OUTPUT"].splitlines():
    key, separator, value = line.partition("=")
    assert separator and key in {"mode", "packages", "reason"} and "\n" not in value
    values[key] = value
assert values["mode"] == os.environ["EXPECTED_MODE"], (values, os.environ["EXPECTED_MODE"])
assert json.loads(values["packages"]) == json.loads(os.environ["EXPECTED_PACKAGES"]), values
if os.environ["EXPECTED_REASON"]:
    assert values["reason"] == os.environ["EXPECTED_REASON"], values
assert set(values) == {"mode", "packages", "reason"}, values
PY
}

paths_file="$fixture_dir/paths"
printf '' > "$paths_file"
assert_selection "empty change set" skip '[]' 'No covered Rust compilation or test paths changed.' "$paths_file"

printf '%s\n' 'docs/book/src/testing.md' > "$paths_file"
assert_selection "skip" skip '[]' 'No covered Rust compilation or test paths changed.' "$paths_file"

printf '%s\n' 'crates/zeroclaw-channels/src/lib.rs' > "$paths_file"
assert_selection "one package and reverse dependent" scoped '["zeroclaw","zeroclaw-channels"]' '' "$paths_file"

printf '%s\n' 'crates/zeroclaw-providers/src/lib.rs' 'crates/zeroclaw-channels/src/lib.rs' > "$paths_file"
assert_selection "multiple packages" scoped '["zeroclaw","zeroclaw-channels","zeroclaw-providers"]' '' "$paths_file"

printf '%s\n' 'src/lib.rs' 'tests/integration.rs' > "$paths_file"
assert_selection "root package" scoped '["zeroclaw"]' '' "$paths_file"

printf '%s\n' 'crates/zeroclaw-channels/src/lib.rs' 'crates/zeroclaw-channels/tests/one.rs' > "$paths_file"
assert_selection "deduplication" scoped '["zeroclaw","zeroclaw-channels"]' '' "$paths_file"

printf '%s\n' 'crates/zeroclaw-channels/tests/fixture.md' > "$paths_file"
assert_selection "test fixture" scoped '["zeroclaw","zeroclaw-channels"]' '' "$paths_file"

printf '%s\n' 'Cargo.toml' > "$paths_file"
assert_selection "full workspace manifest" full '[]' '' "$paths_file"

printf '%s\n' 'crates/unknown/src/lib.rs' > "$paths_file"
assert_selection "unknown path" full '[]' '' "$paths_file"

printf '%s\n' 'crates/zeroclaw-channels/config/ambiguous.yaml' > "$paths_file"
assert_selection "ambiguous package path" full '[]' '' "$paths_file"

printf '%s\n' 'Cargo.lock' > "$paths_file"
assert_selection "lockfile only" full '[]' 'Cargo.lock cannot be attributed to package-local manifest changes.' "$paths_file"

printf '%s\n' 'Cargo.lock' 'crates/zeroclaw-channels/Cargo.toml' > "$paths_file"
assert_selection "attributed lockfile" scoped '["zeroclaw","zeroclaw-channels"]' 'Cargo.lock is attributable to package-local manifest changes.' "$paths_file"

printf '%s\n' 'Cargo.lock' 'crates/zeroclaw-channels/Cargo.toml' 'crates/zeroclaw-providers/src/lib.rs' > "$paths_file"
assert_selection "partially attributed multi-package lockfile" full '[]' 'Cargo.lock cannot be attributed to package-local manifest changes.' "$paths_file"

printf '%s\n' 'Cargo.lock' 'crates/zeroclaw-channels/src/lib.rs' > "$paths_file"
assert_selection "unattributed lockfile" full '[]' 'Cargo.lock cannot be attributed to package-local manifest changes.' "$paths_file"

assert_selection "desktop exclusion" skip '[]' 'No covered Rust compilation or test paths changed.' <(printf '%s\n' 'apps/tauri/src/main.rs')

printf '%s\n' '.cargo/config.toml' > "$paths_file"
assert_selection "cargo configuration" full '[]' '' "$paths_file"

printf '%s\n' '.github/workflows/ci.yml' > "$paths_file"
assert_selection "workflow itself" full '[]' '' "$paths_file"

printf '%s\n' 'crates/zeroclaw-channels/src/$(touch should-not-exist).rs' > "$paths_file"
output="$(run_selector pull_request "$paths_file" "$metadata_file")"
if [ -e "$repo_root/should-not-exist" ] || printf '%s\n' "$output" | grep -q 'should-not-exist'; then
    echo "FAIL: changed path was executed or echoed" >&2
    exit 1
fi
printf '%s\n' "$output" | while IFS= read -r line; do
    case "$line" in
        mode=*|packages=*|reason=*) ;;
        *) echo "FAIL: unsafe selector output: $line" >&2; exit 1 ;;
    esac
done

for event in push merge_group workflow_dispatch unknown; do
    output="$(python3 "$selector" --event "$event" --repo-root "$repo_root")"
    printf '%s\n' "$output" | grep -Fx 'mode=full' >/dev/null
done

malformed_metadata="$fixture_dir/malformed.json"
printf '%s\n' '{"packages": []}' > "$malformed_metadata"
printf '%s\n' 'crates/zeroclaw-channels/src/lib.rs' > "$paths_file"
output="$(run_selector pull_request "$paths_file" "$malformed_metadata")"
printf '%s\n' "$output" | grep -Fx 'mode=full' >/dev/null
printf '%s\n' "$output" | grep -F 'reason=Cargo metadata is malformed or unavailable' >/dev/null

package_args="$(python3 "$selector" --package-args-json '["zeroclaw","zeroclaw-channels"]')"
test "$package_args" = $'-p\nzeroclaw\n-p\nzeroclaw-channels'

for invalid_packages in '[]' '{}' '["zeroclaw",""]' '["zeroclaw","zeroclaw"]' '["$(touch unsafe)"]'; do
    if python3 "$selector" --package-args-json "$invalid_packages" >/dev/null 2>&1; then
        echo "FAIL: invalid package JSON was accepted: $invalid_packages" >&2
        exit 1
    fi
done

if [ -e "$repo_root/unsafe" ]; then
    echo "FAIL: package JSON was executed" >&2
    exit 1
fi

WORKFLOW="$workflow" python3 - <<'PY'
import os
from pathlib import Path

workflow = Path(os.environ["WORKFLOW"]).read_text()
windows_job = workflow.split("\n  windows-test:\n", 1)[1].split(
    "\n  parallel-runtime-test-changes:\n", 1
)[0]
normalization = 'archive="$(cygpath -u "$archive")"'
extraction = 'tar zxf "$archive" -C "$HOME/.cargo/bin"'
assert normalization in windows_job
assert extraction in windows_job
assert windows_job.index(normalization) < windows_job.index(extraction)
PY

echo "windows test scope contract tests: pass"
