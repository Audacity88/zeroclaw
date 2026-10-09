#!/usr/bin/env bash
# Proposed hosted execution only. Do not run this preparation on the local host.
# Later publication supplies proof/matrix-port.patch and frozen-inputs.json.
# Lock schema: harness_sha256 maps the three code filenames to SHA-256;
# base_archive_sha256 maps host/plugins to git archive --format=tar HEAD hashes;
# source_patch_sha256 is the reviewed seven-file patch hash below. This lock's
# SHA-256 remains an unlaunchable placeholder in proof.yml until review/freeze.
set -euo pipefail
umask 077
[[ "${GITHUB_REPOSITORY:-}" == Audacity88/zeroclaw && "${GITHUB_ACTOR:-}" == Audacity88 &&
   "${GITHUB_EVENT_NAME:-}" == push &&
   "${GITHUB_REF:-}" == refs/heads/codex/culling-matrix-host-proof-20261009 ]] || exit 64
[[ "${FROZEN_INPUTS_SHA256:-}" =~ ^[0-9a-f]{64}$ ]] || exit 64
root=$(realpath "${1:?workspace required}")
[[ "$root" == "${GITHUB_WORKSPACE:-}" && "$(uname -m)" == x86_64 && "$(uname -s)" == Linux ]] || exit 64
proof="$root/harness/proof"
echo "$FROZEN_INPUTS_SHA256  $proof/frozen-inputs.json" | sha256sum --check --status
export CARGO_BUILD_JOBS=2 CARGO_TERM_COLOR=never CARGO_INCREMENTAL=0 RUSTUP_TOOLCHAIN=1.98.0
export CARGO_NET_RETRY=0 PYTHONOPTIMIZE=0
export PROOF_ROOT="$root"
export PROOF_DEADLINE=$(python3 -c 'import time; print(time.monotonic() + 70 * 60)')
export MATRIX_PROOF_PRIVATE="$root/proof-private" MATRIX_PROOF_PUBLIC="$root/proof-public"
mkdir "$MATRIX_PROOF_PRIVATE" "$MATRIX_PROOF_PUBLIC"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export HOME="$MATRIX_PROOF_PRIVATE/home" CARGO_HOME="$MATRIX_PROOF_PRIVATE/cargo-home"
export CARGO_TARGET_DIR="$MATRIX_PROOF_PRIVATE/target"
export MATRIX_PROOF_PACKAGE="$MATRIX_PROOF_PRIVATE/packages"
export MATRIX_PROOF_CONFIG="$MATRIX_PROOF_PRIVATE/fixture.json"
export ZEROCLAW_CONFIG_DIR="$MATRIX_PROOF_PRIVATE/config" ZEROCLAW_DATA_DIR="$MATRIX_PROOF_PRIVATE/data"
# RUSTUP_HOME must remain the action-installed compiler store; isolate only the
# Cargo registry/targets and user configuration. No save/restore cache action.
mkdir -p "$HOME" "$CARGO_HOME" "$MATRIX_PROOF_PACKAGE/matrix"
classification=admission
cleanup() {
  status=$?
  trap - EXIT INT TERM
  set +e
  python3 "$proof/prepare_synapse.py" cleanup "$MATRIX_PROOF_PRIVATE" >"$MATRIX_PROOF_PRIVATE/cleanup.log" 2>&1
  cleanup_status=$?
  if (( cleanup_status != 0 )); then
    status=1; classification=cleanup
  elif [[ -f "$MATRIX_PROOF_PRIVATE/cargo-failure-class" ]]; then
    classification=$(<"$MATRIX_PROOF_PRIVATE/cargo-failure-class")
  fi
  PROOF_STATUS="$status" PROOF_CLASS="$classification" python3 - <<'PY'
import json, os
from pathlib import Path
p = Path(os.environ['MATRIX_PROOF_PUBLIC']) / 'result.json'
p.write_text(json.dumps({'exit_code': int(os.environ['PROOF_STATUS']),
                        'failure_class': os.environ['PROOF_CLASS'], 'automatic_retry': False}))
PY
  if (( $? != 0 )); then status=1; fi
  exit "$status"
}
trap cleanup EXIT
trap 'classification=cancelled; exit 130' INT TERM
python3 - <<'PY'
import hashlib, json, os, shutil, subprocess
from pathlib import Path
root = Path(os.environ['PROOF_ROOT'])
proof = root / 'harness/proof'
lock = json.loads((proof / 'frozen-inputs.json').read_text())
expected = {'plugins/matrix/' + p for p in ('Cargo.toml', 'Cargo.lock', 'manifest.toml',
            'README.md', 'src/lib.rs', 'src/matrix.rs', 'tests/matrix.rs')}
def digest(data): return hashlib.sha256(data).hexdigest()
def check(data, wanted):
    assert isinstance(wanted, str) and len(wanted) == 64 and all(c in '0123456789abcdef' for c in wanted)
    assert digest(data) == wanted
assert shutil.disk_usage(root).free >= 20 * 1024**3, 'initial disk floor'
assert subprocess.check_output(['lsb_release', '-rs'], text=True).strip() == '24.04'
identity = {'synapse_amd64': '43fd704aedef503a6fba2e5696439ef7bac24479a3472e364561973f550e4aad',
            'platform': 'ubuntu-24.04/amd64', 'input_sha256': os.environ['FROZEN_INPUTS_SHA256']}
experiment_head = os.environ['GITHUB_SHA']
assert len(experiment_head) == 40 and all(c in '0123456789abcdef' for c in experiment_head)
assert subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root/'harness', text=True).strip() == experiment_head
identity['experiment_head'] = experiment_head
for name in ('run-proof.sh', 'prepare_synapse.py', 'matrix_plugin_smoke.rs'):
    check((proof / name).read_bytes(), lock['harness_sha256'][name])
identity['harness_sha256'] = lock['harness_sha256']
identity['workflow_sha256'] = digest((root / 'harness/.github/workflows/proof.yml').read_bytes())
for directory, sha in (('host', '35dad4a6f398de83ffa7b6632d2ad2637c971cde'),
                       ('plugins', '5efe782cec284b30d9cfda143e11fa9b53dbfd17')):
    cwd = root / directory
    assert subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=cwd, text=True).strip() == sha
    assert not subprocess.check_output(['git', 'status', '--porcelain'], cwd=cwd)
    archive = subprocess.check_output(['git', 'archive', '--format=tar', sha], cwd=cwd)
    check(archive, lock['base_archive_sha256'][directory])
    (Path(os.environ['MATRIX_PROOF_PRIVATE']) / (directory + '-base.tar')).write_bytes(archive)
    identity[directory + '_head'] = sha
    identity[directory + '_archive_sha256'] = digest(archive)
patch = proof / 'matrix-port.patch'
patch_hash = 'bc93efd4aeb9bbd386822e74e7b65796add8bf4aed9ba5997b5ce0fa92c04b82'
assert lock['source_patch_sha256'] == patch_hash
check(patch.read_bytes(), patch_hash)
cwd = root/'plugins'
stats = subprocess.check_output(['git', 'apply', '--numstat', '-z', str(patch)], cwd=cwd)
rows = [row.split(b'\t', 2) for row in stats.split(b'\0') if row]
assert len(rows) == 7 and all(len(row) == 3 and row[0].isdigit() and row[1].isdigit() for row in rows)
assert {row[2].decode() for row in rows} == expected
for path in expected:
    dest = cwd/path
    assert not dest.is_symlink() and dest.resolve().is_relative_to(cwd.resolve())
subprocess.run(['git', 'apply', '--check', str(patch)], cwd=cwd, check=True, capture_output=True)
subprocess.run(['git', 'apply', str(patch)], cwd=cwd, check=True, capture_output=True)
changed = subprocess.check_output(['git', 'diff', '--name-only'], cwd=cwd, text=True).splitlines()
new = subprocess.check_output(['git', 'ls-files', '--others', '--exclude-standard'], cwd=cwd, text=True).splitlines()
assert set(changed + new) == expected
identity['source_patch_sha256'] = patch_hash
identity['overlay_files_sha256'] = {path: digest((cwd/path).read_bytes()) for path in sorted(expected)}
check((cwd/'plugins/matrix/manifest.toml').read_bytes(), '81e00d8917642f0a00f2b87eb48463919cc0899f2cdd6e2ba2a9968a4afb08a4')
check((cwd/'plugins/matrix/Cargo.lock').read_bytes(), 'e96c902848ef5101c644fbbab922a8a438676618b3903b9384299b9f42801966')
identity['local_reference'] = {'platform': 'Darwin', 'rust': '1.97.0', 'bytes': 289647,
    'component_sha256': '2b6af334048291f159ac37ebda3c892f24bb055899e0d530acef50924cdc3148',
    'provenance': 'source-owner-report', 'linux_byte_equality_required': False}
def tree(path):
    assert not any(p.is_symlink() for p in path.rglob('*'))
    return {str(p.relative_to(path)): p.read_bytes() for p in path.rglob('*') if p.is_file()}
assert tree(root/'plugins/wit/next') == tree(root/'host/wit/v0'), 'WIT byte mismatch'
identity['wit_byte_equal'] = True
identity['host_lock_sha256'] = digest((root/'host/Cargo.lock').read_bytes())
identity['plugin_lock_sha256'] = digest((root/'plugins/plugins/matrix/Cargo.lock').read_bytes())
test = root/'host/tests/matrix_plugin_smoke.rs'
assert not test.exists()
shutil.copyfile(proof/'matrix_plugin_smoke.rs', test)
(Path(os.environ['MATRIX_PROOF_PUBLIC'])/'identity.json').write_text(json.dumps(identity, sort_keys=True))
PY
# Supervisor owns only its new Cargo session/process group. Disk pressure never
# invokes pkill, deletes caches, or sends signals to other jobs/process groups.
owned_cargo() {
  python3 - "$@" <<'PY'
import json, os, re, shutil, signal, subprocess, sys, time
from pathlib import Path
private = Path(os.environ['MATRIX_PROOF_PRIVATE'])
log = private / ('cargo-' + str(time.monotonic_ns()) + '.log')
def retain_build_diagnostics(rc):
    # Export no compiler prose, source snippets, absolute paths or runtime logs.
    # Error codes and source locations are sufficient to inspect the frozen code.
    errors = []
    total = 0
    success = None
    with log.open('rb') as source:
        for line in source:
            if len(line) > 1024 * 1024:
                continue
            try: item = json.loads(line)
            except (ValueError, UnicodeDecodeError): continue
            if not isinstance(item, dict): continue
            if item.get('reason') == 'build-finished':
                value = item.get('success')
                if isinstance(value, bool): success = value
            if item.get('reason') != 'compiler-message': continue
            message = item.get('message', {})
            if not isinstance(message, dict) or message.get('level') != 'error': continue
            total += 1
            if len(errors) >= 20: continue
            code = message.get('code')
            code = code.get('code') if isinstance(code, dict) else None
            if not isinstance(code, str) or not re.fullmatch(r'E[0-9]{4}', code): code = None
            locations = []
            for span in message.get('spans', []):
                if not isinstance(span, dict): continue
                name = span.get('file_name')
                if not isinstance(name, str) or not re.fullmatch(r'[A-Za-z0-9_./-]{1,240}', name): continue
                path = Path(name)
                if path.is_absolute() or '..' in path.parts: continue
                if not name.endswith('.rs') or path.parts[0] not in ('src', 'tests', 'crates', 'apps', 'tools', 'xtask', 'build.rs'): continue
                values = [span.get(key) for key in ('line_start', 'column_start', 'line_end', 'column_end')]
                if not all(type(value) is int and 0 < value <= 10000000 for value in values): continue
                locations.append({'file': name, 'line_start': values[0], 'column_start': values[1],
                                  'line_end': values[2], 'column_end': values[3],
                                  'primary': span.get('is_primary') is True})
                if len(locations) == 8: break
            target = item.get('target', {})
            target = target.get('name') if isinstance(target, dict) else None
            if not isinstance(target, str) or not re.fullmatch(r'[A-Za-z0-9_-]{1,120}', target): target = None
            errors.append({'code': code, 'target': target, 'locations': locations})
    report = {'phase': 'host-build', 'exit_code': rc, 'cargo_build_success': success,
              'compiler_error_count': total, 'errors': errors, 'truncated': total > len(errors)}
    (Path(os.environ['MATRIX_PROOF_PUBLIC'])/'host-build.json').write_text(json.dumps(report, sort_keys=True))
def retain_runtime_diagnostics(rc):
    # Match only frozen labels and interface identifiers. Never export raw text.
    stages = ('proof-stage-relay-start', 'proof-stage-primary', 'proof-stage-startup-backlog',
              'proof-stage-operator-config', 'proof-stage-production-activation',
              'proof-stage-relay-stop', 'proof-stage-scripted-start', 'proof-stage-scripted',
              'proof-stage-scripted-stop')
    contexts = ('synthetic operator config', 'package admission', 'manifest identity',
                'default host call limit', 'one actual configured plugin required',
                'host endpoint type', 'authenticated self identity', 'synthetic API transport',
                'synthetic API status', 'synthetic API JSON', 'synthetic users only',
                'native Matrix build forbidden', 'private fixture path', 'fixture mode 0600',
                'test-owned internal bridge fixture only',
                'inbound deadline', 'listener closed', 'listener task', 'listener shutdown deadline',
                'HTTP observation deadline', 'server task', 'server shutdown deadline',
                'restart resync inbound', 'synthetic event ID', 'reply observation transport',
                'bootstrap health readiness', 'queued first event',
                'queued event escaped live policy', 'unexpected queue event',
                'reply observation status', 'reply observation JSON', 'reply events',
                'outbound thread relation', 'outbound root relation', 'reply event deadline',
                'real encryption refusal', 'real encryption preflight reached, zero PUTs',
                'encryption events', 'encrypted plaintext absent',
                'proof-restart-received-prior-fixture-event',
                'proof-restart-received-unclassified-event',
                'proof-restart-initial-cursor-reused', 'proof-restart-initial-cursor-different',
                'proof-restart-initial-cursor-unobserved')
    events = ('Failed to discover WASM channel plugins', 'Failed to admit logical plugin instances',
              'Failed to bind WASM channel plugin endpoint', 'Failed to construct WASM channel plugin')
    # Closed error literals from the frozen Matrix lib.rs and matrix.rs.
    matrix_errors = (
        'matrix: HTTP body unavailable', 'matrix: HTTP byte budget exceeded',
        'matrix: HTTP completion already taken', 'matrix: HTTP deadline exceeded',
        'matrix: HTTP finish failed', 'matrix: HTTP flush failed',
        'matrix: HTTP input unavailable', 'matrix: HTTP output unavailable',
        'matrix: HTTP response already taken', 'matrix: HTTP response incomplete',
        'matrix: HTTP response too large', 'matrix: HTTP response unavailable',
        'matrix: HTTP target refused', 'matrix: HTTP timeout refused',
        'matrix: HTTP transport failed', 'matrix: HTTP transport refused',
        'matrix: HTTP write failed', 'matrix: access_token unavailable',
        'matrix: attachments unsupported', 'matrix: binding changed; fresh configure required',
        'matrix: config unavailable', 'matrix: conflicting sync membership',
        'matrix: encryption state present or uncertain; send refused', 'matrix: fresh configure required',
        'matrix: incremental timeline gap; sync refused', 'matrix: invalid HTTP headers',
        'matrix: invalid HTTP target', 'matrix: invalid access_token',
        'matrix: invalid alias resolution', 'matrix: invalid authenticated identity',
        'matrix: invalid direct-room metadata', 'matrix: invalid public config',
        'matrix: invalid recipient', 'matrix: invalid response JSON',
        'matrix: invalid room member count', 'matrix: invalid room summary',
        'matrix: invalid sync cursor', 'matrix: invalid sync departure',
        'matrix: invalid sync room', 'matrix: invalid sync room id',
        'matrix: invalid sync structure', 'matrix: invalid sync timeline',
        'matrix: invalid timeline limited flag', 'matrix: message too large',
        'matrix: public config too large', 'matrix: room metadata limit exceeded',
        'matrix: send body too large', 'matrix: sync queue limit exceeded',
    )
    host_contexts = ('invalid plugin config', 'permission denied', 'failed to load WASM component',
                     'failed to add channel plugin imports to linker', 'failed to instantiate channel plugin',
                     'channel.configure trapped', 'channel.get-channel-capabilities failed',
                     'channel.self-handle failed', 'channel.self-addressed-mention failed',
                     'plugin call exceeded wall-clock deadline', 'IO error')
    classes = {'component-instantiation': ('failed to instantiate', 'unknown import', 'component imports'),
               'component-type': ('type mismatch', 'incompatible import', 'failed to parse'),
               'guest-execution': ('error while executing', 'wasm trap', 'fuel consumed'),
               'config-schema': ('schema validation', 'missing required', 'RequiredFieldEmpty', 'DanglingReference'),
               'egress-policy': ('egress denied', 'egress blocked', 'destination denied'),
               'authentication': ('M_UNKNOWN_TOKEN', 'M_FORBIDDEN')}
    found_stages, found_contexts, found_events, found_classes, interfaces = set(), set(), set(), set(), set()
    found_matrix_errors, found_host_contexts, http_statuses = set(), set(), set()
    termination = set()
    child_codes = set()
    entered = capture_installed = False
    oversized_lines = max_line_bytes = line_bytes = 0
    overlap = b''
    def record_line(length):
        nonlocal oversized_lines, max_line_bytes
        oversized_lines += length > 1024 * 1024
        max_line_bytes = max(max_line_bytes, length)
    with log.open('rb') as source:
        while chunk := source.read(64 * 1024):
            parts = chunk.split(b'\n')
            if len(parts) == 1:
                line_bytes += len(chunk)
            else:
                record_line(line_bytes + len(parts[0]))
                for part in parts[1:-1]:
                    record_line(len(part))
                line_bytes = len(parts[-1])
            window = overlap + chunk
            text = window.decode('utf-8', errors='replace')
            for needle, label in (
                ('signal: 9, SIGKILL', 'sigkill'), ('signal: 11, SIGSEGV', 'sigsegv'),
                ('signal: 6, SIGABRT', 'sigabrt'), ('panicked at', 'panic'),
                ('test result: FAILED', 'failed-tests'), ('test result: ok', 'passed-tests'),
            ):
                if needle in text: termination.add(label)
            for value in re.findall(r'exit status: ([0-9]{1,3})(?![0-9])', text):
                if int(value) <= 255: child_codes.add(int(value))
            overlap = window[-512:]
            entered |= 'proof-runtime-entered' in text
            capture_installed |= 'proof-runtime-entered capture-installed=true' in text
            for values, found in ((stages, found_stages), (contexts, found_contexts), (events, found_events),
                                  (matrix_errors, found_matrix_errors), (host_contexts, found_host_contexts)):
                for value in values:
                    if re.search(r'(?<![a-z0-9-])' + re.escape(value) + r'(?![a-z0-9-])', text): found.add(value)
            for name, patterns in classes.items():
                if any(pattern in text for pattern in patterns): found_classes.add(name)
            for method, status in re.findall(r'\bmatrix: HTTP (GET|PUT) status ([1-5][0-9]{2})(?![0-9])', text):
                http_statuses.add((method, int(status)))
            # Only wasi:cli/clocks/filesystem/http/io/random/sockets interface names.
            for interface in re.findall(r'\bwasi:(?:cli|clocks|filesystem|http|io|random|sockets)/[a-z][a-z-]{0,63}@[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\b', text):
                if len(interfaces) < 20: interfaces.add(interface)
    record_line(line_bytes)
    report = {'phase': 'host-cases', 'exit_code': rc, 'test_entered': entered,
              'capture_installed': capture_installed, 'matched_stages': sorted(found_stages),
              'matched_contexts': sorted(found_contexts), 'matched_host_events': sorted(found_events),
              'matched_error_classes': sorted(found_classes), 'wasi_interfaces': sorted(interfaces),
              'matched_matrix_errors': sorted(found_matrix_errors),
              'matched_host_contexts': sorted(found_host_contexts),
              'oversized_lines': oversized_lines, 'max_line_bytes': max_line_bytes,
              'matrix_http_statuses': [{'method': method, 'status': status} for method, status in sorted(http_statuses)],
              'child_termination': sorted(termination), 'child_exit_codes': sorted(child_codes),
              'process_group_peak_rss_bytes': peak_rss, 'process_group_peak_count': peak_count,
              'host_total_memory_bytes': total_memory, 'host_min_available_memory_bytes': min_available,
              'cgroup_oom_delta': oom_delta()}
    (Path(os.environ['MATRIX_PROOF_PUBLIC'])/'host-runtime.json').write_text(json.dumps(report, sort_keys=True))
def terminate(p):
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    try: os.killpg(p.pid, signal.SIGTERM)
    except ProcessLookupError: pass
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        p.poll()
        try: os.killpg(p.pid, 0)
        except ProcessLookupError: break
        time.sleep(0.1)
    try: os.killpg(p.pid, signal.SIGKILL)
    except ProcessLookupError: pass
    p.wait()
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        try: os.killpg(p.pid, 0)
        except ProcessLookupError: return
        time.sleep(0.1)
def memory_values():
    values = {}
    try:
        for line in Path('/proc/meminfo').read_text().splitlines():
            key, value = line.split(':', 1)
            if key in ('MemTotal', 'MemAvailable'): values[key] = int(value.split()[0]) * 1024
    except (OSError, ValueError): pass
    return values
def oom_values():
    values = {}
    try:
        for line in Path('/sys/fs/cgroup/memory.events').read_text().splitlines():
            key, value = line.split()
            if key in ('oom', 'oom_kill', 'oom_group_kill'): values[key] = int(value)
    except (OSError, ValueError): pass
    return values
oom_before = oom_values()
def oom_delta():
    after = oom_values()
    return {key: max(0, after[key] - value) for key, value in oom_before.items() if key in after}
memory = memory_values()
total_memory = memory.get('MemTotal')
min_available = memory.get('MemAvailable')
peak_rss = peak_count = 0
def sample_owned_memory(pgid):
    global peak_rss, peak_count, min_available
    rss = count = 0
    for stat in Path('/proc').glob('[0-9]*/stat'):
        try:
            fields = stat.read_text().rsplit(')', 1)[1].split()
            if int(fields[2]) == pgid:
                count += 1
                rss += max(0, int(fields[21])) * os.sysconf('SC_PAGE_SIZE')
        except (OSError, ValueError, IndexError): pass
    peak_rss = max(peak_rss, rss)
    peak_count = max(peak_count, count)
    available = memory_values().get('MemAvailable')
    if available is not None:
        min_available = available if min_available is None else min(min_available, available)
with log.open('wb') as out:
    if shutil.disk_usage(private).free < 4 * 1024**3:
        (private/'cargo-failure-class').write_text('disk-reserve')
        raise SystemExit(75)
    p = subprocess.Popen(sys.argv[1:], stdout=out, stderr=subprocess.STDOUT, start_new_session=True)
    def cancel(sig, frame):
        terminate(p)
        raise SystemExit(130)
    signal.signal(signal.SIGTERM, cancel)
    signal.signal(signal.SIGINT, cancel)
    deadline = float(os.environ['PROOF_DEADLINE'])
    while p.poll() is None:
        if os.environ.get('PROOF_RUNTIME_DIAGNOSTICS') == '1': sample_owned_memory(p.pid)
        if shutil.disk_usage(private).free < 4 * 1024**3:
            (private/'cargo-failure-class').write_text('disk-reserve')
            terminate(p)
            raise SystemExit(75)
        if time.monotonic() > deadline:
            (private/'cargo-failure-class').write_text('owned-command-deadline')
            terminate(p)
            raise SystemExit(124)
        time.sleep(0.5)
    rc = p.wait()
    # A successful root must not leave a compiler or owned test child running.
    try: os.killpg(p.pid, 0)
    except ProcessLookupError: pass
    else:
        terminate(p)
        rc = 1
    if os.environ.get('PROOF_BUILD_DIAGNOSTICS') == '1':
        retain_build_diagnostics(rc)
    if os.environ.get('PROOF_RUNTIME_DIAGNOSTICS') == '1':
        retain_runtime_diagnostics(rc)
    raise SystemExit(rc)
PY
}
classification=plugin-build
cd "$root/plugins/plugins/matrix"
owned_cargo cargo build --locked --release --target wasm32-wasip2
cp "$CARGO_TARGET_DIR/wasm32-wasip2/release/matrix.wasm" "$MATRIX_PROOF_PACKAGE/matrix/matrix.wasm"
cp manifest.toml "$MATRIX_PROOF_PACKAGE/matrix/manifest.toml"
python3 - <<'PY'
import hashlib, json, os, subprocess
from pathlib import Path
p = Path(os.environ['MATRIX_PROOF_PUBLIC'])/'identity.json'
identity = json.loads(p.read_text())
for name, expected in identity['overlay_files_sha256'].items():
    source = Path(os.environ['PROOF_ROOT'])/'plugins'/name
    assert hashlib.sha256(source.read_bytes()).hexdigest() == expected, 'source drift during build'
for name in ('matrix.wasm', 'manifest.toml'):
    identity[name + '_sha256'] = hashlib.sha256((Path(os.environ['MATRIX_PROOF_PACKAGE'])/'matrix'/name).read_bytes()).hexdigest()
assert identity['manifest.toml_sha256'] == '81e00d8917642f0a00f2b87eb48463919cc0899f2cdd6e2ba2a9968a4afb08a4'
identity['component_bytes'] = (Path(os.environ['MATRIX_PROOF_PACKAGE'])/'matrix/matrix.wasm').stat().st_size
identity['wasm_target'] = 'wasm32-wasip2'
identity['rustc'] = subprocess.check_output(['rustc', '-Vv'], text=True)
identity['cargo'] = subprocess.check_output(['cargo', '-V'], text=True).strip()
assert identity['rustc'].startswith('rustc 1.98.0 ')
p.write_text(json.dumps(identity, sort_keys=True))
PY
classification=host-build
cd "$root/host"
PROOF_BUILD_DIAGNOSTICS=1 owned_cargo cargo test --locked --no-default-features --features plugins-wasm-cranelift --test matrix_plugin_smoke --no-run --message-format=json
classification=image-acquisition
docker pull --platform linux/amd64 ghcr.io/element-hq/synapse@sha256:43fd704aedef503a6fba2e5696439ef7bac24479a3472e364561973f550e4aad >"$MATRIX_PROOF_PRIVATE/image.log" 2>&1
classification=synapse-setup
python3 "$proof/prepare_synapse.py" prepare "$MATRIX_PROOF_PRIVATE" >"$MATRIX_PROOF_PRIVATE/setup.log" 2>&1
classification=host-cases
cd "$root/host"
PROOF_RUNTIME_DIAGNOSTICS=1 owned_cargo cargo test --locked --no-default-features --features plugins-wasm-cranelift --test matrix_plugin_smoke -- --test-threads=1 --show-output
python3 - <<'PY'
import hashlib, json, os
from pathlib import Path
root = Path(os.environ['PROOF_ROOT'])
identity = json.loads((Path(os.environ['MATRIX_PROOF_PUBLIC'])/'identity.json').read_text())
assert hashlib.sha256((root/'host/Cargo.lock').read_bytes()).hexdigest() == identity['host_lock_sha256']
assert hashlib.sha256((root/'plugins/plugins/matrix/Cargo.lock').read_bytes()).hexdigest() == identity['plugin_lock_sha256']
PY
classification=passed
