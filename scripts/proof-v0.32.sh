#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

if [[ -x "$project_root/.tools/cargo/bin/cargo" ]]; then
  export RUSTUP_HOME="$project_root/.tools/rustup"
  export CARGO_HOME="$project_root/.tools/cargo"
  export PATH="$CARGO_HOME/bin:$PATH"
fi

umask 077
export HF_HUB_OFFLINE=1
export HF_DATASETS_OFFLINE=1
export TRANSFORMERS_OFFLINE=1
export CARGO_NET_OFFLINE=true
export CARGO_TERM_COLOR=never

proof_python="${INFERLAB_V32_REFERENCE_PYTHON:-python3}"
"$proof_python" - <<'PY'
import tokenizers
if tokenizers.__version__ != "0.23.1":
    raise SystemExit(
        f"v0.32 proof requires tokenizers==0.23.1, found {tokenizers.__version__}"
    )
PY

offline_mode="${INFERLAB_V32_OFFLINE:-0}"
if [[ "$offline_mode" != 0 && "$offline_mode" != 1 ]]; then
  echo 'INFERLAB_V32_OFFLINE must be exactly 0 or 1' >&2
  exit 1
fi
run_id="${INFERLAB_V32_RUN_ID:-v0.32-canonical-20260814}"
if ! [[ "$run_id" =~ ^[a-z0-9][a-z0-9._-]{0,95}$ ]]; then
  echo 'INFERLAB_V32_RUN_ID is outside the retained-evidence contract' >&2
  exit 1
fi

proof_tmp_root="${TMPDIR:-/tmp}"
proof_tmp="$(mktemp -d "$proof_tmp_root/inferlab-v032.XXXXXX")"
results_dir="$proof_tmp/results"
case_dir="$proof_tmp/cases"
mkdir -p "$results_dir" "$case_dir"
proof_succeeded=0

cleanup() {
  if [[ "${INFERLAB_V32_KEEP_TMP:-0}" == 1 && "$proof_succeeded" != 1 ]]; then
    echo "retaining v0.32 proof temporary directory: $proof_tmp" >&2
    return
  fi
  case "$(basename "$proof_tmp")" in
    inferlab-v032.*) rm -rf -- "$proof_tmp" ;;
    *) echo "refusing unexpected v0.32 cleanup path: $proof_tmp" >&2 ;;
  esac
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

prepare_output_dir() {
  [[ -z "${INFERLAB_V32_OUTPUT_DIR:-}" ]] && return
  if [[ -L "$INFERLAB_V32_OUTPUT_DIR" ]]; then
    echo 'INFERLAB_V32_OUTPUT_DIR must not be a symlink' >&2
    exit 1
  fi
  mkdir -p "$INFERLAB_V32_OUTPUT_DIR"
  if [[ ! -d "$INFERLAB_V32_OUTPUT_DIR" ]]; then
    echo 'INFERLAB_V32_OUTPUT_DIR must be a real directory' >&2
    exit 1
  fi
  if find "$INFERLAB_V32_OUTPUT_DIR" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
    echo 'INFERLAB_V32_OUTPUT_DIR must be empty' >&2
    exit 1
  fi
}
prepare_output_dir

"$proof_python" - "$run_id" "$results_dir/source-integrity.json" <<'PY'
import hashlib
import os
import re
import stat
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, "benchmarks")
from check_public_checkpoint_tokenizer import (
    MAX_EVIDENCE_FILE_BYTES,
    PROOF_TOOL_FILES,
    RUNTIME_BASE_PATHS,
    SOURCE_INTEGRITY_SCHEMA,
)
from public_tokenizer_reference_v032 import canonical_json_bytes

run_id = sys.argv[1]
output = Path(sys.argv[2])


def git_bytes(*arguments: str) -> bytes:
    process = subprocess.run(
        ["git", *arguments],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if process.returncode != 0:
        raise SystemExit("v0.32 source attribution could not read the runtime base")
    return process.stdout


runtime_base_commit = git_bytes("rev-parse", "HEAD").decode("ascii").strip()
if re.fullmatch(r"[0-9a-f]{40}", runtime_base_commit) is None:
    raise SystemExit("v0.32 runtime base commit is invalid")
runtime_diff = subprocess.run(
    ["git", "diff", "--quiet", "HEAD", "--", *RUNTIME_BASE_PATHS],
    check=False,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)
if runtime_diff.returncode != 0:
    raise SystemExit("v0.32 runtime paths differ from the attributed base commit")

base_manifest = git_bytes("show", "HEAD:Cargo.toml")
base_lock = git_bytes("show", "HEAD:Cargo.lock")
current_manifest = Path("Cargo.toml").read_bytes()
current_lock = Path("Cargo.lock").read_bytes()
version_pattern = re.compile(rb'(?m)^version = "([0-9]+\.[0-9]+\.[0-9]+)"$')
base_version_match = version_pattern.search(base_manifest)
current_version_match = version_pattern.search(current_manifest)
if base_version_match is None or current_version_match is None:
    raise SystemExit("workspace release version could not be attributed")
base_version = base_version_match.group(1).decode("ascii")
current_version = current_version_match.group(1).decode("ascii")

if base_manifest == current_manifest and base_lock == current_lock:
    if base_version != "0.32.0" or current_version != "0.32.0":
        raise SystemExit("clean workspace is not at the v0.32 release version")
    overlay = {
        "status": "none",
        "base_workspace_version": base_version,
        "observed_workspace_version": current_version,
        "cargo_manifest_version_replacements": 0,
        "cargo_lock_version_replacements": 0,
    }
else:
    old_line = b'version = "0.31.0"'
    new_line = b'version = "0.32.0"'
    marker = b'version = "__INFERLAB_RELEASE_VERSION__"'
    if (
        base_version != "0.31.0"
        or current_version != "0.32.0"
        or base_manifest.count(old_line) != 1
        or current_manifest.count(new_line) != 1
        or base_manifest.replace(old_line, marker, 1)
        != current_manifest.replace(new_line, marker, 1)
        or base_lock.count(old_line) != 12
        or current_lock.count(new_line) != 12
        or base_lock.replace(old_line, marker)
        != current_lock.replace(new_line, marker)
    ):
        raise SystemExit("workspace release overlay exceeds the bounded v0.32 version change")
    overlay = {
        "status": "workspace_version_only",
        "base_workspace_version": base_version,
        "observed_workspace_version": current_version,
        "cargo_manifest_version_replacements": 1,
        "cargo_lock_version_replacements": 12,
    }

proof_tool_files = []
for name in PROOF_TOOL_FILES:
    path = Path(name)
    metadata = os.lstat(path)
    if not stat.S_ISREG(metadata.st_mode):
        raise SystemExit(f"proof-tool source is not a regular file: {name}")
    if not 0 < metadata.st_size <= MAX_EVIDENCE_FILE_BYTES:
        raise SystemExit(f"proof-tool source is outside its byte bound: {name}")
    data = path.read_bytes()
    proof_tool_files.append(
        {"name": name, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
    )

document = {
    "schema": SOURCE_INTEGRITY_SCHEMA,
    "run_id": run_id,
    "runtime_base_commit": runtime_base_commit,
    "runtime_paths": list(RUNTIME_BASE_PATHS),
    "runtime_paths_clean_against_base": True,
    "workspace_release_overlay": overlay,
    "proof_tool_files": proof_tool_files,
}
output.write_bytes(canonical_json_bytes(document))
PY

now_ms() {
  "$proof_python" - <<'PY'
import time
print(time.monotonic_ns() // 1_000_000)
PY
}

elapsed_ms() {
  local started="$1" finished duration
  finished="$(now_ms)"
  duration=$((finished - started))
  ((duration > 0)) || duration=1
  printf '%s\n' "$duration"
}

lock_path="models/public/pythia-14m-v0.32.lock.json"
revision='cf967c0a9a04383db6f7b1108d86b2962634b4ac'
cache_key="pythia-14m/$revision"

asset_started="$(now_ms)"
if [[ "$offline_mode" == 1 ]]; then
  if [[ -z "${INFERLAB_V32_CACHE_ROOT:-}" ]]; then
    echo 'offline v0.32 proof requires INFERLAB_V32_CACHE_ROOT' >&2
    exit 1
  fi
  cache_mode='offline-warm-cache'
  cache_root="$INFERLAB_V32_CACHE_ROOT"
else
  cache_mode='clean-online'
  cache_root="$proof_tmp/cache"
fi

fetch_assets() {
  if [[ "$offline_mode" == 1 ]]; then
    ./scripts/fetch-v0.32-assets.sh --lock "$lock_path" --cache-root "$cache_root" --offline
  else
    ./scripts/fetch-v0.32-assets.sh --lock "$lock_path" --cache-root "$cache_root"
  fi
}

fetch_assets >"$proof_tmp/acquisition-initial.json"
fetch_assets >"$proof_tmp/acquisition-replay.json"
./scripts/fetch-v0.32-assets.sh \
  --lock "$lock_path" \
  --cache-root "$cache_root" \
  --offline \
  >"$proof_tmp/acquisition-offline.json"
asset_duration="$(elapsed_ms "$asset_started")"
asset_dir="$cache_root/$cache_key"
asset_dir="$("$proof_python" -c 'import os,sys; print(os.path.abspath(sys.argv[1]))' \
  "$asset_dir")"

cp "$lock_path" "$results_dir/source-lock.json"
"$proof_python" - \
  "$run_id" "$cache_mode" "$lock_path" \
  "$proof_tmp/acquisition-initial.json" \
  "$proof_tmp/acquisition-replay.json" \
  "$proof_tmp/acquisition-offline.json" \
  "$results_dir/artifact-acquisition.json" <<'PY'
import hashlib
import json
import sys
from pathlib import Path

sys.path.insert(0, "benchmarks")
from public_tokenizer_reference_v032 import canonical_json_bytes, validate_lock

run_id, mode = sys.argv[1:3]
lock_path = Path(sys.argv[3])
input_paths = [Path(value) for value in sys.argv[4:7]]
output_path = Path(sys.argv[7])
lock_bytes = lock_path.read_bytes()
lock = json.loads(lock_bytes)
validate_lock(lock)
expected_lock_sha = "76da77f329e3135e15febf6f017b09e54eb008c45f9a1f4fb05683d4834aaa49"
if hashlib.sha256(lock_bytes).hexdigest() != expected_lock_sha:
    raise SystemExit("checked-in source lock bytes drifted")

passes = []
names = ("initial", "replay", "offline")
expected_statuses = (
    ("downloaded", "warm-cache", "offline-verified")
    if mode == "clean-online"
    else ("offline-verified", "offline-verified", "offline-verified")
)
for name, expected_status, path in zip(names, expected_statuses, input_paths):
    value = json.loads(path.read_text(encoding="utf-8"))
    expected = {
        "schema": "inferlab.public-model-cache-result.v0.32",
        "status": expected_status,
        "repository": lock["source"]["repository"],
        "revision": lock["source"]["revision"],
        "cache_key": f"pythia-14m/{lock['source']['revision']}",
        "file_count": 6,
        "total_bytes": sum(item["bytes"] for item in lock["files"]),
    }
    if value != expected:
        raise SystemExit(f"asset preparation result drifted for {name}")
    passes.append({"name": name, "status": value["status"]})

repository = lock["source"]["repository"]
revision = lock["source"]["revision"]
document = {
    "schema": "inferlab.public-model-acquisition-proof.v0.32",
    "run_id": run_id,
    "mode": mode,
    "source": {
        **lock["source"],
        "lock_schema": lock["schema"],
        "lock_sha256": expected_lock_sha,
    },
    "cache_key": f"pythia-14m/{revision}",
    "immutable_urls": [
        f"https://huggingface.co/{repository}/resolve/{revision}/{item['name']}"
        for item in lock["files"]
    ],
    "file_count": len(lock["files"]),
    "total_bytes": sum(item["bytes"] for item in lock["files"]),
    "files": lock["files"],
    "passes": passes,
    "atomic_publication_commit_point": "directory_rename",
    "retained_weight_bytes": 0,
}
output_path.write_bytes(canonical_json_bytes(document))
PY

production_started="$(now_ms)"
export INFERLAB_TOKENIZER_TEST_ASSETS="$asset_dir"
cargo_log="$proof_tmp/cargo-tests.log"
fetch_test_log="$proof_tmp/fetch-tests.log"
tokenizer_test_log="$proof_tmp/tokenizer-reference-tests.log"
checkpoint_test_log="$proof_tmp/checkpoint-reference-tests.log"

cargo_test_started="$(now_ms)"
cargo test --locked --offline --workspace --all-targets >"$cargo_log" 2>&1 || {
  tail -n 100 "$cargo_log" >&2
  exit 1
}
cargo_test_duration="$(elapsed_ms "$cargo_test_started")"

fetch_test_started="$(now_ms)"
"$proof_python" benchmarks/test_fetch_public_model_assets.py \
  -v \
  >"$fetch_test_log" 2>&1 || {
    tail -n 100 "$fetch_test_log" >&2
    exit 1
  }
fetch_test_duration="$(elapsed_ms "$fetch_test_started")"

fetch_failure_test_log="$proof_tmp/fetch-failure-matrix-tests.log"
fetch_failure_test_started="$(now_ms)"
"$proof_python" benchmarks/test_public_model_fetch_failure_matrix.py \
  -v \
  >"$fetch_failure_test_log" 2>&1 || {
    tail -n 100 "$fetch_failure_test_log" >&2
    exit 1
  }
fetch_failure_test_duration="$(elapsed_ms "$fetch_failure_test_started")"

tokenizer_test_started="$(now_ms)"
"$proof_python" benchmarks/test_public_tokenizer_reference.py \
  -v \
  >"$tokenizer_test_log" 2>&1 || {
    tail -n 100 "$tokenizer_test_log" >&2
    exit 1
  }
tokenizer_test_duration="$(elapsed_ms "$tokenizer_test_started")"

checkpoint_test_started="$(now_ms)"
"$proof_python" benchmarks/test_public_checkpoint_reference.py \
  -v \
  >"$checkpoint_test_log" 2>&1 || {
    tail -n 100 "$checkpoint_test_log" >&2
    exit 1
  }
checkpoint_test_duration="$(elapsed_ms "$checkpoint_test_started")"

cargo build --locked --offline --package model-artifacts --bin inferlab-model-inspect \
  >"$proof_tmp/cargo-build.log" 2>&1 || {
    tail -n 100 "$proof_tmp/cargo-build.log" >&2
    exit 1
  }
production_duration="$(elapsed_ms "$production_started")"

"$proof_python" - \
  "$run_id" \
  "$cargo_log" "$cargo_test_duration" \
  "$fetch_test_log" "$fetch_test_duration" \
  "$fetch_failure_test_log" "$fetch_failure_test_duration" \
  "$tokenizer_test_log" "$tokenizer_test_duration" \
  "$checkpoint_test_log" "$checkpoint_test_duration" \
  "$results_dir/production-tests.json" <<'PY'
import re
import sys
from pathlib import Path

sys.path.insert(0, "benchmarks")
from check_public_checkpoint_tokenizer import REGRESSION_FAILURES
from public_tokenizer_reference_v032 import canonical_json_bytes

run_id = sys.argv[1]
fixtures = (
    (
        "rust-workspace",
        [
            "cargo",
            "test",
            "--locked",
            "--offline",
            "--workspace",
            "--all-targets",
        ],
        Path(sys.argv[2]),
        int(sys.argv[3]),
        "cargo",
    ),
    (
        "python-fetch-assets",
        ["python3", "benchmarks/test_fetch_public_model_assets.py", "-v"],
        Path(sys.argv[4]),
        int(sys.argv[5]),
        "python",
    ),
    (
        "python-fetch-failure-matrix",
        ["python3", "benchmarks/test_public_model_fetch_failure_matrix.py", "-v"],
        Path(sys.argv[6]),
        int(sys.argv[7]),
        "python",
    ),
    (
        "python-tokenizer-reference",
        ["python3", "benchmarks/test_public_tokenizer_reference.py", "-v"],
        Path(sys.argv[8]),
        int(sys.argv[9]),
        "python",
    ),
    (
        "python-checkpoint-reference",
        ["python3", "benchmarks/test_public_checkpoint_reference.py", "-v"],
        Path(sys.argv[10]),
        int(sys.argv[11]),
        "python",
    ),
)
commands = []
logs = {}
for name, argv, path, duration_ms, kind in fixtures:
    text = path.read_text(encoding="utf-8", errors="strict")
    logs[name] = text
    if kind == "cargo":
        counts = [
            int(value)
            for value in re.findall(r"test result: ok\. ([0-9]+) passed;", text)
        ]
        if not counts:
            raise SystemExit("cargo test summaries were not observed")
        test_count = sum(counts)
    else:
        matches = re.findall(r"Ran ([0-9]+) tests? in", text)
        if len(matches) != 1:
            raise SystemExit(f"Python test summary drifted for {name}")
        test_count = int(matches[0])
    if test_count <= 0 or duration_ms <= 0:
        raise SystemExit(f"non-positive production result for {name}")
    commands.append(
        {
            "name": name,
            "argv": argv,
            "status": "passed",
            "test_count": test_count,
            "duration_ms": duration_ms,
        }
    )
coverage = []
for failure_case, binding in REGRESSION_FAILURES.items():
    command = binding["command"]
    test_name = binding["test_name"]
    text = logs[command]
    if command == "rust-workspace":
        pattern = rf"^test {re.escape(test_name)} \.\.\. ok$"
    else:
        class_name, method_name = test_name.split(".", 1)
        pattern = (
            rf"^{re.escape(method_name)} "
            rf"\(__main__\.{re.escape(class_name)}"
            rf"(?:\.{re.escape(method_name)})?\) "
            rf"\.\.\. ok$"
        )
    observed_count = len(re.findall(pattern, text, flags=re.MULTILINE))
    if observed_count != 1:
        raise SystemExit(
            "required failure regression was not observed exactly once: "
            f"{failure_case} -> {test_name} (count={observed_count})"
        )
    coverage.append(
        {
            "failure_case": failure_case,
            "command": command,
            "test_name": test_name,
            "observed_count": observed_count,
        }
    )
document = {
    "schema": "inferlab.public-checkpoint-tokenizer-tests.v0.32",
    "run_id": run_id,
    "all_passed": True,
    "total_tests": sum(item["test_count"] for item in commands),
    "commands": commands,
    "regression_coverage": coverage,
}
Path(sys.argv[12]).write_bytes(canonical_json_bytes(document))
PY

target_root="${CARGO_TARGET_DIR:-$project_root/target}"
if [[ "$target_root" != /* ]]; then
  target_root="$project_root/$target_root"
fi
inspect_binary="$target_root/debug/inferlab-model-inspect"
if [[ ! -x "$inspect_binary" ]]; then
  echo 'inferlab-model-inspect binary was not produced by the locked build' >&2
  exit 1
fi

checkpoint_verify_started="$(now_ms)"
"$inspect_binary" inspect --lock "$lock_path" --assets "$asset_dir" \
  >"$results_dir/checkpoint-verification.json" 2>"$proof_tmp/inspect-a.stderr"
"$inspect_binary" inspect --lock "$lock_path" --assets "$asset_dir" \
  >"$proof_tmp/checkpoint-verification-replay.json" 2>"$proof_tmp/inspect-b.stderr"
if [[ -s "$proof_tmp/inspect-a.stderr" || -s "$proof_tmp/inspect-b.stderr" ]]; then
  echo 'offline checkpoint inspection unexpectedly wrote stderr' >&2
  exit 1
fi
cmp "$results_dir/checkpoint-verification.json" \
  "$proof_tmp/checkpoint-verification-replay.json"
checkpoint_verify_duration="$(elapsed_ms "$checkpoint_verify_started")"

checkpoint_reference_started="$(now_ms)"
"$proof_python" benchmarks/generate_public_checkpoint_reference.py \
  --lock "$lock_path" \
  --assets "$asset_dir" \
  --output "$results_dir/checkpoint-reference.json"
"$proof_python" benchmarks/generate_public_checkpoint_reference.py \
  --lock "$lock_path" \
  --assets "$asset_dir" \
  --output "$proof_tmp/checkpoint-reference-replay.json"
cmp "$results_dir/checkpoint-reference.json" \
  "$proof_tmp/checkpoint-reference-replay.json"
"$proof_python" benchmarks/check_public_checkpoint_reference.py \
  --lock "$lock_path" \
  --reference "$results_dir/checkpoint-reference.json" \
  --output "$results_dir/checkpoint-reference-check.json"
checkpoint_reference_duration="$(elapsed_ms "$checkpoint_reference_started")"

tokenizer_reference_started="$(now_ms)"
"$proof_python" benchmarks/generate_public_tokenizer_reference.py \
  --lock "$lock_path" \
  --assets "$asset_dir" \
  --output "$results_dir/tokenizer-reference.json"
"$proof_python" benchmarks/generate_public_tokenizer_reference.py \
  --lock "$lock_path" \
  --assets "$asset_dir" \
  --output "$proof_tmp/tokenizer-reference-replay.json"
cmp "$results_dir/tokenizer-reference.json" \
  "$proof_tmp/tokenizer-reference-replay.json"
"$proof_python" benchmarks/check_public_tokenizer_reference.py \
  --lock "$lock_path" \
  --reference "$results_dir/tokenizer-reference.json" \
  --output "$results_dir/tokenizer-reference-check.json"
tokenizer_reference_duration="$(elapsed_ms "$tokenizer_reference_started")"

tokenizer_production_started="$(now_ms)"
"$proof_python" benchmarks/public_checkpoint_tokenizer_probe.py \
  --binary "$inspect_binary" \
  --lock "$lock_path" \
  --assets "$asset_dir" \
  --reference "$results_dir/tokenizer-reference.json" \
  --output "$results_dir/tokenizer-production.json" \
  --timeout-seconds 60
"$proof_python" benchmarks/public_checkpoint_tokenizer_probe.py \
  --binary "$inspect_binary" \
  --lock "$lock_path" \
  --assets "$asset_dir" \
  --reference "$results_dir/tokenizer-reference.json" \
  --output "$proof_tmp/tokenizer-production-replay.json" \
  --timeout-seconds 60
cmp "$results_dir/tokenizer-production.json" \
  "$proof_tmp/tokenizer-production-replay.json"
tokenizer_production_duration="$(elapsed_ms "$tokenizer_production_started")"

failure_started="$(now_ms)"
expect_inspect_failure() {
  local expected="$1" case_lock="$2" case_assets="$3" stdout_file stderr_file code
  stdout_file="$proof_tmp/failure.stdout"
  stderr_file="$proof_tmp/failure.stderr"
  set +e
  "$inspect_binary" inspect --lock "$case_lock" --assets "$case_assets" \
    >"$stdout_file" 2>"$stderr_file"
  code=$?
  set -e
  if [[ "$code" != 1 || -s "$stdout_file" || "$(cat "$stderr_file")" != "$expected" ]]; then
    echo "unexpected artifact failure: expected '$expected', exit 1" >&2
    sed -n '1,10p' "$stderr_file" >&2
    exit 1
  fi
}

make_case_assets() {
  local destination="$1"
  mkdir -p "$(dirname "$destination")"
  cp -R "$asset_dir" "$destination"
  chmod 0700 "$destination"
  find "$destination" -type f -exec chmod 0600 {} +
}

"$proof_python" - "$lock_path" "$case_dir/mutable-lock.json" \
  "$case_dir/unknown-lock.json" <<'PY'
import json
import sys
from pathlib import Path

source = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
mutable = json.loads(json.dumps(source))
mutable["source"]["revision"] = "b" * 40
Path(sys.argv[2]).write_text(json.dumps(mutable, indent=2) + "\n", encoding="utf-8")
unknown = json.loads(json.dumps(source))
unknown["unexpected"] = True
Path(sys.argv[3]).write_text(json.dumps(unknown, indent=2) + "\n", encoding="utf-8")
PY
expect_inspect_failure \
  'model artifact verification failed: lock_mismatch' \
  "$case_dir/mutable-lock.json" "$asset_dir"
expect_inspect_failure \
  'model artifact verification failed: lock_invalid' \
  "$case_dir/unknown-lock.json" "$asset_dir"

case_assets="$case_dir/missing-assets"
make_case_assets "$case_assets"
rm -- "$case_assets/README.md"
expect_inspect_failure \
  'model artifact verification failed: inventory_mismatch' \
  "$lock_path" "$case_assets"
rm -rf -- "$case_assets"

case_assets="$case_dir/extra-assets"
make_case_assets "$case_assets"
printf 'not retained\n' >"$case_assets/unexpected.json"
expect_inspect_failure \
  'model artifact verification failed: inventory_mismatch' \
  "$lock_path" "$case_assets"
rm -rf -- "$case_assets"

case_assets="$case_dir/symlink-assets"
make_case_assets "$case_assets"
rm -- "$case_assets/README.md"
ln -s config.json "$case_assets/README.md"
expect_inspect_failure \
  'model artifact verification failed: file_unsafe (README.md)' \
  "$lock_path" "$case_assets"
rm -rf -- "$case_assets"

case_assets="$case_dir/fifo-assets"
make_case_assets "$case_assets"
rm -- "$case_assets/README.md"
mkfifo "$case_assets/README.md"
expect_inspect_failure \
  'model artifact verification failed: file_unsafe (README.md)' \
  "$lock_path" "$case_assets"
rm -rf -- "$case_assets"

case_assets="$case_dir/corrupt-config-assets"
make_case_assets "$case_assets"
"$proof_python" - "$case_assets/config.json" <<'PY'
import sys
from pathlib import Path
path = Path(sys.argv[1])
data = bytearray(path.read_bytes())
data[-2] ^= 1
path.write_bytes(data)
PY
expect_inspect_failure \
  'model artifact verification failed: hash_mismatch (config.json)' \
  "$lock_path" "$case_assets"
rm -rf -- "$case_assets"

case_assets="$case_dir/corrupt-checkpoint-assets"
make_case_assets "$case_assets"
"$proof_python" - "$case_assets/model.safetensors" <<'PY'
import sys
from pathlib import Path
path = Path(sys.argv[1])
with path.open("r+b") as target:
    target.seek(-1, 2)
    value = target.read(1)
    target.seek(-1, 2)
    target.write(bytes([value[0] ^ 1]))
PY
expect_inspect_failure \
  'model artifact verification failed: hash_mismatch (model.safetensors)' \
  "$lock_path" "$case_assets"
rm -rf -- "$case_assets"

set +e
./scripts/fetch-v0.32-assets.sh \
  --lock "$lock_path" \
  --cache-root "$case_dir/offline-missing-cache" \
  --offline \
  >"$proof_tmp/fetch-missing.stdout" 2>"$proof_tmp/fetch-missing.stderr"
fetch_missing_code=$?
set -e
expected_missing="v0.32 asset preparation failed: offline verification requires cache key $cache_key"
if [[ "$fetch_missing_code" != 1 || -s "$proof_tmp/fetch-missing.stdout" || \
      "$(cat "$proof_tmp/fetch-missing.stderr")" != "$expected_missing" ]]; then
  echo 'offline missing-cache failure drifted' >&2
  exit 1
fi

invalid_root="$case_dir/invalid-existing-cache"
invalid_assets="$invalid_root/$cache_key"
make_case_assets "$invalid_assets"
"$proof_python" - "$invalid_assets/README.md" <<'PY'
import sys
from pathlib import Path
path = Path(sys.argv[1])
data = bytearray(path.read_bytes())
data[0] ^= 1
path.write_bytes(data)
PY
invalid_before="$(shasum -a 256 "$invalid_assets/README.md" | awk '{print $1}')"
set +e
./scripts/fetch-v0.32-assets.sh \
  --lock "$lock_path" \
  --cache-root "$invalid_root" \
  >"$proof_tmp/fetch-invalid.stdout" 2>"$proof_tmp/fetch-invalid.stderr"
fetch_invalid_code=$?
set -e
invalid_after="$(shasum -a 256 "$invalid_assets/README.md" | awk '{print $1}')"
expected_invalid='v0.32 asset preparation failed: existing cache is invalid and will not be repaired automatically: cache file README.md identity mismatch'
if [[ "$fetch_invalid_code" != 1 || -s "$proof_tmp/fetch-invalid.stdout" || \
      "$(cat "$proof_tmp/fetch-invalid.stderr")" != "$expected_invalid" || \
      "$invalid_before" != "$invalid_after" ]]; then
  echo 'invalid existing-cache refusal drifted' >&2
  exit 1
fi
rm -rf -- "$invalid_root"

"$proof_python" - "$run_id" "$results_dir/failure-matrix.json" <<'PY'
import sys
from pathlib import Path

sys.path.insert(0, "benchmarks")
from check_public_checkpoint_tokenizer import DIRECT_FAILURES, REGRESSION_FAILURES
from public_tokenizer_reference_v032 import canonical_json_bytes

run_id = sys.argv[1]
document = {
    "schema": "inferlab.public-checkpoint-tokenizer-failures.v0.32",
    "run_id": run_id,
    "all_passed": True,
    "direct_cases": [
        {
            "name": name,
            "method": "proof-owned-real-copy",
            "expected_error_kind": error_kind,
            "observed_error_kind": error_kind,
            "exit_code": 1,
            "passed": True,
            "retained_payload_bytes": 0,
        }
        for name, error_kind in DIRECT_FAILURES.items()
    ],
    "regression_cases": [
        {
            "name": name,
            "method": "focused-regression-suite",
            "command": binding["command"],
            "test_name": binding["test_name"],
            "observed_count": 1,
            "passed": True,
            "retained_payload_bytes": 0,
        }
        for name, binding in REGRESSION_FAILURES.items()
    ],
}
Path(sys.argv[2]).write_bytes(canonical_json_bytes(document))
PY
failure_duration="$(elapsed_ms "$failure_started")"

cargo metadata --locked --offline --format-version 1 --no-deps \
  >"$proof_tmp/cargo-metadata.json"
"$proof_python" - \
  "$run_id" "$proof_tmp/cargo-metadata.json" \
  "$results_dir/runtime-boundary.json" <<'PY'
import json
import sys
from pathlib import Path

sys.path.insert(0, "benchmarks")
from public_tokenizer_reference_v032 import canonical_json_bytes

run_id = sys.argv[1]
metadata = json.loads(Path(sys.argv[2]).read_text(encoding="utf-8"))
package = next(item for item in metadata["packages"] if item["name"] == "model-artifacts")
normal = [item for item in package["dependencies"] if item["kind"] is None]
names = sorted(item["name"] for item in normal)
expected_names = ["libc", "safetensors", "serde", "serde_json", "sha2", "tokenizers"]
if names != expected_names:
    raise SystemExit("model-artifacts direct dependencies drifted")
tokenizers = next(item for item in normal if item["name"] == "tokenizers")
if (
    tokenizers["req"] != "=0.23.1"
    or tokenizers["uses_default_features"] is not False
    or tokenizers["features"] != ["fancy-regex"]
):
    raise SystemExit("pinned Rust tokenizer dependency drifted")
document = {
    "schema": "inferlab.public-checkpoint-tokenizer-runtime.v0.32",
    "run_id": run_id,
    "rust_binary_commands": ["inspect", "tokenize"],
    "rust_binary_fetch_command": False,
    "rust_binary_network_client": False,
    "model_artifacts_direct_normal_dependencies": names,
    "tokenizer_dependency": {
        "package": "tokenizers",
        "version": tokenizers["req"],
        "default_features": tokenizers["uses_default_features"],
        "features": tokenizers["features"],
    },
    "offline_environment": {
        "hf_hub_offline": True,
        "hf_datasets_offline": True,
        "transformers_offline": True,
    },
    "cargo_net_offline": True,
    "cargo_term_color": "never",
    "checkpoint_inspection_reports": 2,
    "checkpoint_inspection_byte_identical": True,
    "production_tokenizer_probes": 2,
    "production_tokenizer_byte_identical": True,
    "ambient_hub_cache_discovery": False,
}
Path(sys.argv[3]).write_bytes(canonical_json_bytes(document))
PY

runtime_base_commit="$(git rev-parse HEAD)"
operating_system="$(uname -s)"
architecture="$(uname -m)"
python_version="$("$proof_python" -c 'import platform; print(platform.python_version())')"
rustc_version="$(rustc --version)"
cargo_version="$(cargo --version)"
"$proof_python" - \
  "$run_id" "$runtime_base_commit" "$operating_system" "$architecture" \
  "$python_version" "$rustc_version" "$cargo_version" \
  "$results_dir/environment.json" <<'PY'
import sys
from pathlib import Path

sys.path.insert(0, "benchmarks")
from public_tokenizer_reference_v032 import canonical_json_bytes

run_id, runtime_base_commit, operating_system, architecture = sys.argv[1:5]
python_version, rustc_version, cargo_version = sys.argv[5:8]
document = {
    "schema": "inferlab.public-checkpoint-tokenizer-environment.v0.32",
    "run_id": run_id,
    "runtime_base_commit": runtime_base_commit,
    "operating_system": operating_system,
    "architecture": architecture,
    "python": python_version,
    "rustc": rustc_version,
    "cargo": cargo_version,
    "reference_package": {"package": "tokenizers", "version": "0.23.1"},
    "offline_environment": {
        "hf_hub_offline": True,
        "hf_datasets_offline": True,
        "transformers_offline": True,
    },
    "cargo_net_offline": True,
    "cargo_term_color": "never",
}
Path(sys.argv[8]).write_bytes(canonical_json_bytes(document))
PY

"$proof_python" - "$run_id" "$cache_mode" \
  "$results_dir/proof-contract.json" "$results_dir/process-continuity.json" <<'PY'
import sys
from pathlib import Path

sys.path.insert(0, "benchmarks")
from public_tokenizer_reference_v032 import canonical_json_bytes

run_id, mode = sys.argv[1:3]
source = {
    "repository": "EleutherAI/pythia-14m",
    "revision": "cf967c0a9a04383db6f7b1108d86b2962634b4ac",
    "license": "Apache-2.0",
    "lock_schema": "inferlab.public-model-lock.v1",
    "lock_sha256": "76da77f329e3135e15febf6f017b09e54eb008c45f9a1f4fb05683d4834aaa49",
}
network = "pinned_revision_https_only" if mode == "clean-online" else "disabled"
contract = {
    "schema": "inferlab.public-checkpoint-tokenizer-proof-contract.v0.32",
    "milestone": "v0.32",
    "run_id": run_id,
    "scope": "pinned-checkpoint-artifact-verification-and-production-tokenizer-parity",
    "source": source,
    "cache_mode": mode,
    "runtime_commands": ["inspect", "tokenize"],
    "external_network_authority": {
        "asset_preparation": network,
        "post_acquisition": "disabled",
        "hub_post_acquisition": "disabled",
        "cargo": "disabled",
    },
    "public_model_network_authority": {
        "asset_preparation": network,
        "checkpoint_inspection": "disabled",
        "tokenizer_reference": "disabled",
        "production_tokenizer": "disabled",
    },
    "regression_fixture_network": {
        "loopback": "allowed_ephemeral_regression_fixtures",
        "external": "disabled",
        "public_model": "disabled",
    },
    "proof_owned_long_lived_topology": [],
    "public_model_forward_passes": 0,
    "public_model_generations": 0,
    "public_model_services_started": 0,
    "retained_weight_bytes": 0,
}
processes = {
    "schema": "inferlab.public-checkpoint-tokenizer-process-accounting.v0.32",
    "run_id": run_id,
    "accounting_method": "static_synchronous_role_contract",
    "process_identity_sampling": False,
    "public_model_services_started": 0,
    "proof_owned_background_jobs_started": 0,
    "proof_owned_long_lived_topology": [],
    "regression_fixtures": {
        "ephemeral_child_processes": "allowed",
        "ephemeral_loopback_listeners": "allowed",
        "classification": "outside_public_model_runtime_continuity",
    },
    "synchronous_roles": [
        {
            "name": "asset-preparation",
            "external_network_authority": network,
            "public_model_network_authority": network,
            "loopback_fixture_authority": "disabled",
        },
        {
            "name": "checkpoint-reference",
            "external_network_authority": "disabled",
            "public_model_network_authority": "disabled",
            "loopback_fixture_authority": "disabled",
        },
        {
            "name": "inferlab-model-inspect",
            "external_network_authority": "disabled",
            "public_model_network_authority": "disabled",
            "loopback_fixture_authority": "disabled",
        },
        {
            "name": "maintained-tokenizer-reference",
            "external_network_authority": "disabled",
            "public_model_network_authority": "disabled",
            "loopback_fixture_authority": "disabled",
        },
        {
            "name": "proof-checkers",
            "external_network_authority": "disabled",
            "public_model_network_authority": "disabled",
            "loopback_fixture_authority": "disabled",
        },
        {
            "name": "regression-tests",
            "external_network_authority": "disabled",
            "public_model_network_authority": "disabled",
            "loopback_fixture_authority": "allowed_ephemeral",
        },
    ],
}
Path(sys.argv[3]).write_bytes(canonical_json_bytes(contract))
Path(sys.argv[4]).write_bytes(canonical_json_bytes(processes))
PY

"$proof_python" - \
  "$run_id" \
  "$asset_duration" \
  "$checkpoint_verify_duration" \
  "$checkpoint_reference_duration" \
  "$tokenizer_reference_duration" \
  "$tokenizer_production_duration" \
  "$failure_duration" \
  "$production_duration" \
  "$results_dir/timings.json" <<'PY'
import sys
from pathlib import Path

sys.path.insert(0, "benchmarks")
from public_tokenizer_reference_v032 import canonical_json_bytes

run_id = sys.argv[1]
names = (
    "asset_preparation",
    "checkpoint_offline_verification",
    "checkpoint_reference",
    "tokenizer_reference",
    "tokenizer_production",
    "failure_matrix",
    "production_tests",
)
durations = [int(value) for value in sys.argv[2:9]]
phases = [
    {"name": name, "duration_ms": duration}
    for name, duration in sorted(zip(names, durations))
]
if any(item["duration_ms"] <= 0 for item in phases):
    raise SystemExit("proof phase duration must be positive")
document = {
    "schema": "inferlab.public-checkpoint-tokenizer-timings.v0.32",
    "run_id": run_id,
    "clock": "monotonic",
    "phases": phases,
    "total_capture_ms": sum(item["duration_ms"] for item in phases),
}
Path(sys.argv[9]).write_bytes(canonical_json_bytes(document))
PY

printf '{\n}\n' >"$results_dir/assertions.json"
printf '<svg xmlns="http://www.w3.org/2000/svg"/>\n' \
  >"$results_dir/public-checkpoint-tokenizer-proof.svg"
printf '{\n}\n' >"$results_dir/sanitizer.json"

scan_retained_evidence() {
  local output="$1"
  "$proof_python" - \
    "$run_id" "$results_dir" "$output" \
    "$project_root" "$proof_tmp" "$cache_root" "$asset_dir" <<'PY'
import os
import re
import stat
import sys
from pathlib import Path

sys.path.insert(0, "benchmarks")
from check_public_checkpoint_tokenizer import NON_MANIFEST_FILES
from public_tokenizer_reference_v032 import canonical_json_bytes

run_id = sys.argv[1]
directory = Path(sys.argv[2])
output = Path(sys.argv[3])
runtime_paths = sys.argv[4:]
names = sorted(NON_MANIFEST_FILES - {"sanitizer.json"})
absolute_patterns = (
    re.compile(r"/Users/"),
    re.compile(r"/home/"),
    re.compile(r"/tmp/"),
    re.compile(r"/private/tmp/"),
    re.compile(r"/var/folders/"),
    re.compile(r"/workspace/"),
    re.compile(r"[A-Za-z]:\\Users\\"),
)
private_patterns = (
    re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
    re.compile(r"(?i)authorization:\s*bearer"),
    re.compile(r"(?i)hf_token\s*="),
)
absolute_matches = 0
exact_runtime_path_matches = 0
private_matches = 0
weight_files = 0
weight_bytes = 0
exact_runtime_paths = set()
for value in runtime_paths:
    if value:
        exact_runtime_paths.add(value)
        exact_runtime_paths.add(os.path.abspath(value))
for name in names:
    path = directory / name
    metadata = os.lstat(path)
    if not stat.S_ISREG(metadata.st_mode):
        raise SystemExit(f"retained evidence is not a regular file: {name}")
    raw = path.read_bytes()
    text = raw.decode("utf-8", errors="strict")
    absolute_matches += sum(len(pattern.findall(text)) for pattern in absolute_patterns)
    exact_runtime_path_matches += sum(text.count(value) for value in exact_runtime_paths)
    private_matches += sum(len(pattern.findall(text)) for pattern in private_patterns)
    if path.suffix == ".safetensors" or len(raw) == 28_143_920:
        weight_files += 1
        weight_bytes += len(raw)
if (
    absolute_matches
    or exact_runtime_path_matches
    or private_matches
    or weight_files
    or weight_bytes
):
    raise SystemExit("retained evidence sanitizer found prohibited material")
document = {
    "schema": "inferlab.public-checkpoint-tokenizer-sanitizer.v0.32",
    "run_id": run_id,
    "status": "passed",
    "scanned_files": names,
    "scanned_file_count": len(names),
    "utf8_text_files": len(names),
    "absolute_host_path_matches": absolute_matches,
    "exact_runtime_path_matches": exact_runtime_path_matches,
    "private_material_matches": private_matches,
    "retained_weight_files": weight_files,
    "retained_weight_bytes": weight_bytes,
}
output.write_bytes(canonical_json_bytes(document))
PY
}

scan_retained_evidence "$results_dir/sanitizer.json"
"$proof_python" benchmarks/check_public_checkpoint_tokenizer.py \
  --evidence-dir "$results_dir" \
  --output "$results_dir/assertions.json"
"$proof_python" benchmarks/render_public_checkpoint_tokenizer_svg.py \
  --evidence-dir "$results_dir" \
  --output "$results_dir/public-checkpoint-tokenizer-proof.svg"
scan_retained_evidence "$proof_tmp/sanitizer-replay.json"
cmp "$results_dir/sanitizer.json" "$proof_tmp/sanitizer-replay.json"

"$proof_python" benchmarks/check_public_checkpoint_tokenizer.py \
  --evidence-dir "$results_dir" \
  --output "$proof_tmp/assertions-replay.json"
cmp "$results_dir/assertions.json" "$proof_tmp/assertions-replay.json"
"$proof_python" benchmarks/render_public_checkpoint_tokenizer_svg.py \
  --evidence-dir "$results_dir" \
  --output "$proof_tmp/proof-replay.svg"
cmp "$results_dir/public-checkpoint-tokenizer-proof.svg" \
  "$proof_tmp/proof-replay.svg"

"$proof_python" - "$results_dir" <<'PY'
import hashlib
import os
import stat
import sys
from pathlib import Path

sys.path.insert(0, "benchmarks")
from check_public_checkpoint_tokenizer import (
    MANIFEST_SCHEMA,
    NON_MANIFEST_FILES,
)
from public_tokenizer_reference_v032 import canonical_json_bytes

directory = Path(sys.argv[1])
observed = {entry.name for entry in os.scandir(directory)}
if observed != NON_MANIFEST_FILES:
    raise SystemExit("pre-manifest retained inventory drifted")
entries = []
for name in sorted(NON_MANIFEST_FILES):
    path = directory / name
    metadata = os.lstat(path)
    if not stat.S_ISREG(metadata.st_mode):
        raise SystemExit(f"manifest source is not a regular file: {name}")
    raw = path.read_bytes()
    entries.append(
        {"name": name, "bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}
    )
manifest = {
    "schema": MANIFEST_SCHEMA,
    "file_count": len(entries),
    "files": entries,
}
(directory / "manifest.json").write_bytes(canonical_json_bytes(manifest))
PY

"$proof_python" benchmarks/check_public_checkpoint_tokenizer.py \
  --evidence-dir "$results_dir" \
  --require-manifest \
  --output "$proof_tmp/post-manifest-assertions.json"
cmp "$results_dir/assertions.json" "$proof_tmp/post-manifest-assertions.json"
"$proof_python" benchmarks/render_public_checkpoint_tokenizer_svg.py \
  --evidence-dir "$results_dir" \
  --output "$proof_tmp/post-manifest-proof.svg"
cmp "$results_dir/public-checkpoint-tokenizer-proof.svg" \
  "$proof_tmp/post-manifest-proof.svg"

if [[ -n "${INFERLAB_V32_OUTPUT_DIR:-}" ]]; then
  while IFS= read -r name; do
    cp "$results_dir/$name" "$INFERLAB_V32_OUTPUT_DIR/$name"
  done < <(find "$results_dir" -mindepth 1 -maxdepth 1 -type f \
    ! -name manifest.json -exec basename {} \; | sort)
  cp "$results_dir/manifest.json" "$INFERLAB_V32_OUTPUT_DIR/manifest.json"
  "$proof_python" benchmarks/check_public_checkpoint_tokenizer.py \
    --evidence-dir "$INFERLAB_V32_OUTPUT_DIR" \
    --require-manifest \
    --output "$proof_tmp/retained-assertions.json"
  cmp "$results_dir/assertions.json" "$proof_tmp/retained-assertions.json"
  "$proof_python" benchmarks/render_public_checkpoint_tokenizer_svg.py \
    --evidence-dir "$INFERLAB_V32_OUTPUT_DIR" \
    --output "$proof_tmp/retained-proof.svg"
  cmp "$results_dir/public-checkpoint-tokenizer-proof.svg" \
    "$proof_tmp/retained-proof.svg"
fi

"$proof_python" - "$results_dir/assertions.json" "$results_dir/manifest.json" <<'PY'
import hashlib
import json
import sys
from pathlib import Path
assertions = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
manifest = Path(sys.argv[2]).read_bytes()
print(
    "v0.32 public-checkpoint/tokenizer proof complete: "
    f"{assertions['passed']}/{assertions['total']} assertions passed; "
    f"manifest_sha256={hashlib.sha256(manifest).hexdigest()}"
)
PY
proof_succeeded=1
