#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

if [[ -x "$project_root/.tools/cargo/bin/cargo" ]]; then
  export RUSTUP_HOME="$project_root/.tools/rustup"
  export CARGO_HOME="$project_root/.tools/cargo"
  export PATH="$CARGO_HOME/bin:$PATH"
fi

export NO_PROXY='127.0.0.1,localhost'
export no_proxy="$NO_PROXY"
unset HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy || true
unset INFERLAB_PUBLIC_EDGE_MODE INFERLAB_PUBLIC_API_KEYS INFERLAB_OPERATOR_BIND \
  INFERLAB_OPERATOR_API_KEY INFERLAB_PUBLIC_MAX_MESSAGES \
  INFERLAB_PUBLIC_MAX_PROMPT_BYTES INFERLAB_PUBLIC_MAX_OUTPUT_TOKENS \
  INFERLAB_PUBLIC_RATE_REQUESTS_PER_MINUTE INFERLAB_PUBLIC_RATE_BURST || true
proof_release_version="${INFERLAB_V28_EXPECTED_RELEASE_VERSION-0.28.0}"
if [[ ! "$proof_release_version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  echo 'INFERLAB_V28_EXPECTED_RELEASE_VERSION must be an exact semantic version' >&2
  exit 1
fi
export INFERLAB_V28_EXPECTED_RELEASE_VERSION="$proof_release_version"
umask 077

proof_python="$(command -v python3)"
proof_perl="$(command -v perl)"
if [[ -z "$proof_python" || -z "$proof_perl" ]]; then
  echo 'v0.28 proof requires python3 and perl with core Time::HiRes' >&2
  exit 1
fi
python3() {
  "$proof_python" "$@"
}

proof_tmp_root="${TMPDIR:-/tmp}"
proof_tmp="$(mktemp -d "$proof_tmp_root/inferlab-v028.XXXXXX")"
results_dir="$proof_tmp/results"
mkdir -p "$results_dir"

public_url='http://127.0.0.1:11080'
operator_url='http://127.0.0.1:11081'
worker_url='http://127.0.0.1:11082'
gateway_metrics_url='http://127.0.0.1:11083'
worker_metrics_url='http://127.0.0.1:11084'

public_key_a='edge-public-alpha-00000001'
public_key_b='edge-public-bravo-00000002'
operator_key='edge-operator-admin-00000003'
wrong_key='edge-wrong-credential-00000004'
proof_prompt='teach me streaming'
request_id_marker='edge-proof-request-id-00000005'
export V28_PUBLIC_A="$public_key_a"
export V28_PUBLIC_B="$public_key_b"
export V28_OPERATOR="$operator_key"
export V28_WRONG="$wrong_key"
export V28_WRONG_SCHEME="Basic $public_key_a"
export V28_REQUEST_ID="$request_id_marker"

live_pids=(sentinel)
worker_pid=''
gateway_pid=''
disconnect_probe_pid=''

forget_pid() {
  local forgotten="$1" pid
  local retained=(sentinel)
  for pid in "${live_pids[@]}"; do
    if [[ "$pid" != "$forgotten" ]]; then
      retained+=("$pid")
    fi
  done
  live_pids=("${retained[@]:1}")
}

is_owned_child() {
  local pid="$1" parent
  if [[ ! "$pid" =~ ^[1-9][0-9]*$ ]]; then
    return 1
  fi
  parent="$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d '[:space:]')"
  [[ "$parent" == "$$" ]]
}

shutdown_child() {
  local pid="$1" attempt state
  if ! is_owned_child "$pid"; then
    return
  fi
  kill -TERM "$pid" 2>/dev/null || true
  for ((attempt = 0; attempt < 100; attempt++)); do
    if ! is_owned_child "$pid"; then
      break
    fi
    state="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
    if [[ "$state" == *Z* ]]; then
      break
    fi
    sleep 0.02
  done
  if is_owned_child "$pid"; then
    state="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
    if [[ "$state" != *Z* ]]; then
      kill -KILL "$pid" 2>/dev/null || true
    fi
  fi
  wait "$pid" 2>/dev/null || true
}

cleanup() {
  local owned_pids=("${live_pids[@]}") index pid
  for ((index = ${#owned_pids[@]} - 1; index >= 0; index--)); do
    pid="${owned_pids[$index]}"
    shutdown_child "$pid"
    forget_pid "$pid"
  done
  if [[ -n "$proof_tmp" && -d "$proof_tmp" ]]; then
    case "$(basename "$proof_tmp")" in
      inferlab-v028.*) rm -rf -- "$proof_tmp" ;;
      *) echo "refusing to remove unexpected proof path: $proof_tmp" >&2 ;;
    esac
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

prepare_output_dir() {
  if [[ -z "${INFERLAB_V28_OUTPUT_DIR:-}" ]]; then
    return
  fi
  mkdir -p "$INFERLAB_V28_OUTPUT_DIR"
  if find "$INFERLAB_V28_OUTPUT_DIR" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
    echo 'INFERLAB_V28_OUTPUT_DIR must be empty so stale evidence cannot survive' >&2
    exit 1
  fi
}

check_ports_are_free() {
  python3 - "$@" <<'PY'
import socket
import sys

for raw_port in sys.argv[1:]:
    port = int(raw_port)
    with socket.socket() as probe:
        probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            probe.bind(("127.0.0.1", port))
        except OSError as error:
            raise SystemExit(f"refusing v0.28 proof: 127.0.0.1:{port} is busy: {error}")
PY
}

monotonic_ns() {
  "$proof_perl" -MTime::HiRes=clock_gettime,CLOCK_MONOTONIC \
    -e 'printf "%.0f\n", clock_gettime(CLOCK_MONOTONIC)*1000000000'
}

listener_is_open() {
  local port="$1"
  (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null
}

wait_endpoint() {
  local url="$1" pid="$2" label="$3" status="${4:-200}"
  local deadline=$((SECONDS + 60)) observed
  while true; do
    observed="$(curl --noproxy '*' --connect-timeout 0.1 --max-time 0.4 \
      --silent --output /dev/null --write-out '%{http_code}' "$url" 2>/dev/null || true)"
    if [[ "$observed" == "$status" ]]; then
      return
    fi
    if ! is_owned_child "$pid"; then
      echo "$label exited before becoming healthy" >&2
      tail -n 40 "$proof_tmp/$label.log" 2>/dev/null || true
      return 1
    fi
    if ((SECONDS >= deadline)); then
      echo "timed out waiting for $label at $url; last status=$observed" >&2
      tail -n 40 "$proof_tmp/$label.log" 2>/dev/null || true
      return 1
    fi
    sleep 0.05
  done
}

assert_owned_processes_alive() {
  local name pid state
  while (($#)); do
    name="$1"
    pid="$2"
    shift 2
    state="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
    if ! is_owned_child "$pid" || [[ -z "$state" || "$state" == *Z* ]]; then
      echo "owned proof process is not alive: $name" >&2
      return 1
    fi
  done
}

capture_process_snapshot() {
  local output="$1"
  shift
  python3 - "$$" "$@" >"$output" <<'PY'
import json
import subprocess
import sys

proof_shell_pid = int(sys.argv[1])
values = sys.argv[2:]
if len(values) % 2:
    raise SystemExit("process snapshot requires name/pid pairs")

def observe(raw_pid: str) -> dict:
    result = subprocess.run(
        ["ps", "-o", "ppid=", "-o", "stat=", "-o", "lstart=", "-o", "command=", "-p", raw_pid],
        check=False, capture_output=True, text=True,
    )
    fields = result.stdout.strip().split(None, 7)
    ppid = int(fields[0]) if len(fields) >= 1 and fields[0].isdigit() else None
    state = fields[1] if len(fields) >= 2 else None
    started = " ".join(fields[2:7]) if len(fields) >= 7 else None
    command = fields[7] if len(fields) >= 8 else None
    return {
        "pid": int(raw_pid), "parent_pid": ppid, "state": state,
        "start_token": started, "command": command,
        "alive": result.returncode == 0 and ppid is not None,
        "owned_child": ppid == proof_shell_pid,
        "non_zombie": state is not None and "Z" not in state,
    }

processes = {values[index]: observe(values[index + 1]) for index in range(0, len(values), 2)}
if not all(item["alive"] and item["owned_child"] and item["non_zombie"] for item in processes.values()):
    raise SystemExit("could not capture exact owned proof children")
print(json.dumps({"proof_shell_pid": proof_shell_pid, "processes": processes}, indent=2, sort_keys=True))
PY
}

record_process_continuity() {
  assert_owned_processes_alive cpu-worker "$worker_pid" gateway "$gateway_pid"
  python3 - "$proof_tmp/initial-processes.json" "$$" "$worker_pid" "$gateway_pid" \
    >"$results_dir/process-continuity.json" <<'PY'
import json
import subprocess
import sys
from pathlib import Path

initial = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
proof_shell_pid = int(sys.argv[2])
current_pids = {"cpu-worker": int(sys.argv[3]), "gateway": int(sys.argv[4])}

def observe(pid: int) -> dict:
    result = subprocess.run(
        ["ps", "-o", "ppid=", "-o", "stat=", "-o", "lstart=", "-o", "command=", "-p", str(pid)],
        check=False, capture_output=True, text=True,
    )
    fields = result.stdout.strip().split(None, 7)
    ppid = int(fields[0]) if fields and fields[0].isdigit() else None
    state = fields[1] if len(fields) >= 2 else None
    started = " ".join(fields[2:7]) if len(fields) >= 7 else None
    command = fields[7] if len(fields) >= 8 else None
    return {"parent_pid": ppid, "state": state, "start_token": started, "command": command,
            "alive": result.returncode == 0 and ppid is not None,
            "owned_child": ppid == proof_shell_pid,
            "non_zombie": state is not None and "Z" not in state}

processes = {}
for name, pid in current_pids.items():
    before = initial["processes"][name]
    current = observe(pid)
    processes[name] = {
        "initial_pid": before["pid"], "current_pid": pid,
        "same_pid": before["pid"] == pid,
        "initial_start_token": before["start_token"], "current_start_token": current["start_token"],
        "same_start_token": before["start_token"] == current["start_token"],
        "initial_command": before["command"], "current_command": current["command"],
        "same_command": before["command"] == current["command"],
        "initial_parent_pid": before["parent_pid"], "current_parent_pid": current["parent_pid"],
        "alive": current["alive"], "owned_child": current["owned_child"],
        "non_zombie": current["non_zombie"],
    }
print(json.dumps({
    "schema": "inferlab.public-edge-process-continuity.v0.28",
    "proof_shell_pid": proof_shell_pid,
    "processes": processes,
}, indent=2, sort_keys=True))
PY
}

scan_private_material() {
  local output="$1"
  python3 - "$results_dir" "$public_key_a" "$public_key_b" "$operator_key" \
    >"$output" <<'PY'
import json
import hashlib
import re
import sys
import urllib.parse
from pathlib import Path

evidence = Path(sys.argv[1])
credentials = sys.argv[2:]
files = sorted(
    path for path in evidence.iterdir()
    if path.is_file() and path.name not in {"manifest.json", "private-material-scan.json"}
)
host_path = re.compile(
    r"(?:/Users|/home|/private/var|/var/folders|/tmp|/workspace|/workspaces|"
    r"/github/workspace)/[^\s\"'<>]+"
)
markers = ("-----BEGIN", "PRIVATE KEY", "CERTIFICATE-----")
matches = []
for path in files:
    raw = path.read_text(encoding="utf-8", errors="replace")
    for index, credential in enumerate(credentials):
        if (
            credential in raw
            or urllib.parse.quote(credential, safe="") in raw
            or hashlib.sha256(credential.encode()).hexdigest() in raw.lower()
        ):
            matches.append(f"{path.name}:credential-{index + 1}")
    if any(marker in raw.upper() for marker in markers):
        matches.append(f"{path.name}:private-marker")
    if host_path.search(raw):
        matches.append(f"{path.name}:host-path")
if matches:
    raise SystemExit("private material or path leaked: " + ", ".join(matches))
print(json.dumps({
    "schema": "inferlab.private-material-scan.v0.28",
    "files_scanned": [path.name for path in files],
    "credential_count": len(credentials),
    "encodings_checked": ["literal", "percent-encoded", "sha256"],
    "private_marker_checks": len(markers),
    "path_patterns_checked": True,
    "matches": 0,
}, indent=2, sort_keys=True))
PY
}

final_leak_scan() {
  python3 benchmarks/public_edge_probe.py sanitize-evidence \
    --evidence-dir "$results_dir" --forbidden-values-file "$proof_tmp/forbidden-values.json" \
    --proof-root "$proof_tmp" --project-root "$project_root" \
    >"$proof_tmp/final-sanitizer.json"
  cmp "$results_dir/sanitizer.json" "$proof_tmp/final-sanitizer.json"
  scan_private_material "$proof_tmp/final-private-scan.json"
  cmp "$results_dir/private-material-scan.json" "$proof_tmp/final-private-scan.json"
}

write_manifest() {
  python3 - "$results_dir" "$@" <<'PY'
import hashlib
import json
import sys
from pathlib import Path

directory = Path(sys.argv[1])
expected = sorted(sys.argv[2:])
if "manifest.json" not in expected or len(expected) != len(set(expected)):
    raise SystemExit("manifest expected set must contain one unique manifest.json")
without_manifest = [name for name in expected if name != "manifest.json"]
observed = sorted(path.name for path in directory.iterdir())
if observed != without_manifest:
    raise SystemExit(f"unexpected pre-manifest evidence set: {observed}")
files = []
for name in without_manifest:
    content = (directory / name).read_bytes()
    files.append({"path": name, "sha256": hashlib.sha256(content).hexdigest(), "bytes": len(content)})
(directory / "manifest.json").write_text(json.dumps({
    "schema": "inferlab.evidence-manifest.v0.28",
    "expected_files": expected,
    "file_count": len(expected),
    "hashed_file_count": len(files),
    "files": files,
}, indent=2, sort_keys=True) + "\n", encoding="utf-8")
if sorted(path.name for path in directory.iterdir()) != expected:
    raise SystemExit("final evidence set differs from its exact manifest")
PY
}

retain_results() {
  if [[ -z "${INFERLAB_V28_OUTPUT_DIR:-}" ]]; then
    return
  fi
  local name
  for name in "$@"; do
    if [[ "$name" != 'manifest.json' ]]; then
      cp "$results_dir/$name" "$INFERLAB_V28_OUTPUT_DIR/$name"
    fi
  done
  python3 - "$results_dir/manifest.json" "$INFERLAB_V28_OUTPUT_DIR" <<'PY'
import hashlib
import json
import sys
from pathlib import Path

manifest = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
destination = Path(sys.argv[2])
expected = sorted(name for name in manifest["expected_files"] if name != "manifest.json")
if sorted(path.name for path in destination.iterdir()) != expected:
    raise SystemExit("partial retained evidence set differs before manifest publication")
for item in manifest["files"]:
    content = (destination / item["path"]).read_bytes()
    if len(content) != item["bytes"] or hashlib.sha256(content).hexdigest() != item["sha256"]:
        raise SystemExit(f"retained evidence hash mismatch: {item['path']}")
PY
  cp "$results_dir/manifest.json" "$INFERLAB_V28_OUTPUT_DIR/manifest.json"
}

run_startup_failure() {
  local name="$1" public_port="$2" operator_port="$3" expected="$4"
  local log="$proof_tmp/startup-$name.log" ready_file="$proof_tmp/startup-$name.ready"
  local release_file="$proof_tmp/startup-$name.release"
  local pid state ever_public='false' ever_operator='false' released='false'
  local samples=0 status diagnostic ports deadline
  shift 4
  (
    printf 'ready\n' >"$ready_file"
    while [[ ! -s "$release_file" ]]; do
      sleep 0.005
    done
    exec env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$no_proxy" \
      "$@" target/debug/gateway
  ) >"$log" 2>&1 &
  pid="$!"
  live_pids+=("$pid")
  deadline=$((SECONDS + 5))
  while true; do
    state="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
    if [[ -z "$state" || "$state" == *Z* ]]; then
      break
    fi
    samples="$((samples + 1))"
    if listener_is_open "$public_port"; then
      ever_public='true'
    fi
    if [[ "$operator_port" != "$public_port" ]] && listener_is_open "$operator_port"; then
      ever_operator='true'
    fi
    if [[ "$released" == 'false' && -s "$ready_file" ]]; then
      printf 'release\n' >"$release_file"
      released='true'
      continue
    fi
    if ((SECONDS >= deadline)); then
      shutdown_child "$pid"
      forget_pid "$pid"
      echo "startup failure candidate did not exit within five seconds: $name" >&2
      return 1
    fi
    sleep 0.005
  done
  set +e
  wait "$pid"
  status="$?"
  set -e
  forget_pid "$pid"
  samples="$((samples + 1))"
  if listener_is_open "$public_port"; then
    ever_public='true'
  fi
  if [[ "$operator_port" != "$public_port" ]] && listener_is_open "$operator_port"; then
    ever_operator='true'
  fi
  rm -f -- "$ready_file" "$release_file"
  diagnostic="$(tail -n 1 "$log" | tr -d '\r')"
  if [[ "$operator_port" == "$public_port" ]]; then
    ever_operator="$ever_public"
    ports="$public_port"
  else
    ports="$public_port,$operator_port"
  fi
  if [[ "$released" != 'true' || "$status" == '0' || "$ever_public" != 'false' || "$ever_operator" != 'false' || "$samples" -lt 2 || "$diagnostic" != "$expected" ]]; then
    echo "startup failure contract mismatch for $name" >&2
    tail -n 20 "$log" >&2 || true
    return 1
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$name" "$status" "$ports" "$ever_public" "$ever_operator" "$samples" "$diagnostic"
}

start_worker() {
  env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$no_proxy" \
  INFERLAB_CPU_WORKER_ID='cpu-worker-edge' \
  INFERLAB_CPU_BIND='127.0.0.1:11082' \
  INFERLAB_METRICS_BIND='127.0.0.1:11084' \
  INFERLAB_MODEL_PATH='models/tiny-inferlab-v2.bin' \
  INFERLAB_CPU_DECODER_MODE='paged-kv-cache' \
  INFERLAB_CPU_QUANTIZATION='fp32' \
  INFERLAB_CPU_SPECULATIVE_DRAFT_QUANTIZATION='int8' \
  INFERLAB_CPU_ATTENTION_KERNEL='online-tiled' \
  INFERLAB_CPU_ATTENTION_PRECISION='fp32' \
  INFERLAB_CPU_ATTENTION_TILE_TOKENS=32 \
  INFERLAB_CPU_MAX_BATCH_SIZE=1 \
  INFERLAB_CPU_SCHEDULER_QUEUE_CAPACITY=4 \
  INFERLAB_CPU_KV_PAGE_TOKENS=4 \
  INFERLAB_CPU_KV_PAGE_COUNT=32 \
  INFERLAB_CPU_PREFIX_CACHE_CAPACITY=8 \
  INFERLAB_CPU_BATCH_TICK_MS=100 \
    target/debug/cpu-worker >"$proof_tmp/cpu-worker.log" 2>&1 &
  worker_pid="$!"
  live_pids+=("$worker_pid")
  wait_endpoint "$worker_url/health" "$worker_pid" cpu-worker
  wait_endpoint "$worker_metrics_url/healthz" "$worker_pid" cpu-worker-metrics
}

start_gateway() {
  env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$no_proxy" \
  INFERLAB_PUBLIC_EDGE_MODE='hosted' \
  INFERLAB_BIND='127.0.0.1:11080' \
  INFERLAB_OPERATOR_BIND='127.0.0.1:11081' \
  INFERLAB_PUBLIC_API_KEYS="$public_key_a,$public_key_b" \
  INFERLAB_OPERATOR_API_KEY="$operator_key" \
  INFERLAB_PUBLIC_MAX_MESSAGES=3 \
  INFERLAB_PUBLIC_MAX_PROMPT_BYTES=64 \
  INFERLAB_PUBLIC_MAX_OUTPUT_TOKENS=8 \
  INFERLAB_PUBLIC_RATE_REQUESTS_PER_MINUTE=60 \
  INFERLAB_PUBLIC_RATE_BURST=2 \
  INFERLAB_WORKERS='cpu-worker-edge=http://127.0.0.1:11082' \
  INFERLAB_ROUTING_POLICY='round-robin' \
  INFERLAB_WORKER_CONCURRENCY=1 \
  INFERLAB_ADMISSION_QUEUE_CAPACITY=0 \
  INFERLAB_REQUEST_DEADLINE_MS=15000 \
  INFERLAB_ATTEMPT_TIMEOUT_MS=12000 \
  INFERLAB_MAX_RETRIES=0 \
  INFERLAB_METRICS_BIND='127.0.0.1:11083' \
    target/debug/gateway >"$proof_tmp/gateway.log" 2>&1 &
  gateway_pid="$!"
  live_pids+=("$gateway_pid")
  wait_endpoint "$public_url/health" "$gateway_pid" gateway
  wait_endpoint "$gateway_metrics_url/healthz" "$gateway_pid" gateway-metrics
  V28_OPERATOR="$operator_key" python3 benchmarks/public_edge_probe.py request \
    --url "$operator_url/internal/workers" --bearer-env V28_OPERATOR \
    --projection operator-status >"$proof_tmp/operator-ready.json"
  python3 - "$proof_tmp/operator-ready.json" <<'PY'
import json
import sys
value = json.load(open(sys.argv[1], encoding="utf-8"))
if value.get("status") != 200:
    raise SystemExit("operator listener did not become ready")
PY
}

prepare_output_dir
check_ports_are_free 11080 11081 11082 11083 11084 11180 11181 11182 11183
python3 -m py_compile \
  benchmarks/public_edge_probe.py benchmarks/check_public_edge.py \
  benchmarks/render_public_edge_svg.py
cargo build --locked --workspace --bins --quiet
cargo test --locked -p gateway --all-targets --no-run --quiet

missing_error='Error: Custom { kind: InvalidInput, error: "hosted public edge requires explicit nonempty INFERLAB_PUBLIC_API_KEYS" }'
collision_error='Error: Custom { kind: InvalidInput, error: "INFERLAB_BIND and INFERLAB_OPERATOR_BIND must not overlap" }'
overlap_error='Error: Custom { kind: InvalidInput, error: "INFERLAB_OPERATOR_API_KEY must not match any INFERLAB_PUBLIC_API_KEYS entry" }'

run_startup_failure missing_public_keys 11180 11181 "$missing_error" \
  INFERLAB_PUBLIC_EDGE_MODE=hosted \
  INFERLAB_BIND=127.0.0.1:11180 \
  INFERLAB_OPERATOR_BIND=127.0.0.1:11181 \
  INFERLAB_OPERATOR_API_KEY="$operator_key" \
  INFERLAB_WORKERS=cpu-worker-edge=http://127.0.0.1:11082 \
  >"$proof_tmp/startup-missing.tsv"
run_startup_failure bind_collision 11181 11181 "$collision_error" \
  INFERLAB_PUBLIC_EDGE_MODE=hosted \
  INFERLAB_BIND=127.0.0.1:11181 \
  INFERLAB_OPERATOR_BIND=127.0.0.1:11181 \
  INFERLAB_PUBLIC_API_KEYS="$public_key_a,$public_key_b" \
  INFERLAB_OPERATOR_API_KEY="$operator_key" \
  INFERLAB_WORKERS=cpu-worker-edge=http://127.0.0.1:11082 \
  >"$proof_tmp/startup-collision.tsv"
run_startup_failure credential_overlap 11182 11183 "$overlap_error" \
  INFERLAB_PUBLIC_EDGE_MODE=hosted \
  INFERLAB_BIND=127.0.0.1:11182 \
  INFERLAB_OPERATOR_BIND=127.0.0.1:11183 \
  INFERLAB_PUBLIC_API_KEYS="$public_key_a,$operator_key" \
  INFERLAB_OPERATOR_API_KEY="$operator_key" \
  INFERLAB_WORKERS=cpu-worker-edge=http://127.0.0.1:11082 \
  >"$proof_tmp/startup-overlap.tsv"
python3 - "$proof_tmp/startup-missing.tsv" "$proof_tmp/startup-collision.tsv" \
  "$proof_tmp/startup-overlap.tsv" >"$results_dir/startup-contract.json" <<'PY'
import json
import sys
from pathlib import Path

cases = []
for path in sys.argv[1:]:
    line = Path(path).read_text(encoding="utf-8").rstrip("\n")
    name, status, raw_ports, ever_public, ever_operator, samples, diagnostic = line.split("\t", 6)
    ports = [int(value) for value in raw_ports.split(",")]
    observations = {str(ports[0]): ever_public == "true"}
    if len(ports) == 2:
        observations[str(ports[1])] = ever_operator == "true"
    cases.append({
        "name": name,
        "exit_code": int(status),
        "listener_ports": ports,
        "listener_ever_open_by_port": observations,
        "listener_ever_open": any(observations.values()),
        "listener_poll_samples": int(samples),
        "process_exited": True,
        "diagnostic": diagnostic,
    })
print(json.dumps({
    "schema": "inferlab.public-edge-startup-contract.v0.28",
    "cases": cases,
}, indent=2, sort_keys=True))
PY

start_worker
start_gateway
capture_process_snapshot "$proof_tmp/initial-processes.json" \
  cpu-worker "$worker_pid" gateway "$gateway_pid"

python3 - "$proof_tmp" "$proof_prompt" <<'PY'
import json
import sys
from pathlib import Path

directory = Path(sys.argv[1])
prompt = sys.argv[2]

def write(name, value):
    (directory / name).write_text(json.dumps(value, separators=(",", ":")) + "\n", encoding="utf-8")

base = {"model": "inferlab-tiny", "temperature": 0, "max_tokens": 1,
        "messages": [{"role": "user", "content": prompt}]}
write("valid-small.json", base)
write("valid-json.json", {**base, "max_tokens": 8, "stream": False})
write("valid-sse.json", {**base, "max_tokens": 8, "stream": True})
write("missing-messages.json", {"max_tokens": 1})
write("invalid-content.json", {"messages": [{"role": "user", "content": {}}], "max_tokens": 1})
write("too-many.json", {"messages": [{"role": "user", "content": "x"}] * 4, "max_tokens": 1})
write("prompt-large.json", {"messages": [{"role": "user", "content": "x" * 65}], "max_tokens": 1})
write("max-zero.json", {"messages": [{"role": "user", "content": "x"}], "max_tokens": 0})
write("max-string.json", {"messages": [{"role": "user", "content": "x"}], "max_tokens": "1"})
write("max-large.json", {"messages": [{"role": "user", "content": "x"}], "max_tokens": 9})
(directory / "malformed.json").write_text("{\n", encoding="utf-8")
(directory / "oversize.bin").write_bytes(b"x" * 65537)
encoded = json.dumps(base, separators=(",", ":")).encode()
if len(encoded) >= 65536:
    raise SystemExit("boundary fixture base unexpectedly exceeds the wire limit")
(directory / "request-boundary.json").write_bytes(encoded + b" " * (65536 - len(encoded)))
PY

python3 - >"$results_dir/proof-contract.json" <<'PY'
import json
print(json.dumps({
    "schema": "inferlab.public-edge-proof-contract.v0.28",
    "version": "0.28.0",
    "processes": ["cpu-worker", "gateway"],
    "config": {
        "credential_count": 2,
        "max_messages": 3,
        "max_output_tokens": 8,
        "max_prompt_bytes": 64,
        "max_request_bytes": 65536,
        "rate_burst": 2,
        "rate_requests_per_minute": 60,
    },
}, indent=2, sort_keys=True))
PY

python3 benchmarks/public_edge_probe.py request \
  --url "$public_url/internal/workers" --kind public-internal-missing \
  >"$proof_tmp/public-internal-missing.json"
python3 benchmarks/public_edge_probe.py request \
  --url "$public_url/internal/workers" --kind public-internal-public \
  --bearer-env V28_PUBLIC_A >"$proof_tmp/public-internal-public.json"
python3 benchmarks/public_edge_probe.py request \
  --url "$public_url/internal/workers" --kind public-internal-operator \
  --bearer-env V28_OPERATOR >"$proof_tmp/public-internal-operator.json"

python3 benchmarks/public_edge_probe.py request \
  --url "$operator_url/internal/workers" --kind operator-missing \
  >"$proof_tmp/operator-missing.json"
python3 benchmarks/public_edge_probe.py request \
  --url "$operator_url/internal/workers" --kind operator-public \
  --bearer-env V28_PUBLIC_A >"$proof_tmp/operator-public.json"
python3 benchmarks/public_edge_probe.py request \
  --url "$operator_url/internal/workers" --kind operator-authorized \
  --bearer-env V28_OPERATOR --projection operator-status \
  >"$proof_tmp/operator-authorized.json"

open_route_paths=(/ /assets/og-inferlab.png /health /readyz)
open_route_files=()
open_index=0
for route_path in "${open_route_paths[@]}"; do
  open_file="$proof_tmp/open-$open_index.json"
  python3 benchmarks/public_edge_probe.py request \
    --url "$public_url$route_path" --kind public-open --discard-body >"$open_file"
  open_route_files+=("$open_file")
  open_index="$((open_index + 1))"
done

python3 benchmarks/public_edge_probe.py request \
  --url "$public_url/showcase/status" --kind showcase-missing \
  >"$proof_tmp/showcase-missing.json"
python3 benchmarks/public_edge_probe.py request \
  --url "$public_url/showcase/status" --kind showcase-operator \
  --bearer-env V28_OPERATOR >"$proof_tmp/showcase-operator.json"
python3 benchmarks/public_edge_probe.py request \
  --url "$public_url/showcase/status" --kind showcase-public \
  --bearer-env V28_PUBLIC_A --projection showcase-status \
  >"$proof_tmp/showcase-public.json"

python3 - "$proof_tmp" "${open_route_files[@]}" >"$results_dir/route-isolation.json" <<'PY'
import json
import sys
from pathlib import Path

root = Path(sys.argv[1])
load = lambda name: json.loads((root / name).read_text(encoding="utf-8"))
open_routes = [json.loads(Path(path).read_text(encoding="utf-8")) for path in sys.argv[2:]]
print(json.dumps({
    "schema": "inferlab.public-edge-route-isolation.v0.28",
    "public_internal": {
        "missing": load("public-internal-missing.json"),
        "public": load("public-internal-public.json"),
        "operator": load("public-internal-operator.json"),
    },
    "operator_internal": {
        "missing": load("operator-missing.json"),
        "public": load("operator-public.json"),
        "operator": load("operator-authorized.json"),
    },
    "public_open_routes": open_routes,
    "public_showcase": {
        "missing": load("showcase-missing.json"),
        "operator": load("showcase-operator.json"),
        "public": load("showcase-public.json"),
    },
}, indent=2, sort_keys=True))
PY

curl --noproxy '*' --fail --silent --show-error --connect-timeout 1 --max-time 3 \
  "$gateway_metrics_url/metrics" \
  >"$results_dir/attempts-before-rejections-gateway.prom"
curl --noproxy '*' --fail --silent --show-error --connect-timeout 1 --max-time 3 \
  "$worker_metrics_url/metrics" \
  >"$results_dir/attempts-before-rejections-worker.prom"

python3 benchmarks/public_edge_probe.py request \
  --url "$public_url/v1/chat/completions" --method POST \
  --body-file "$proof_tmp/valid-small.json" --kind auth-missing \
  >"$proof_tmp/auth-missing.json"
python3 benchmarks/public_edge_probe.py request \
  --url "$public_url/v1/chat/completions" --method POST \
  --body-file "$proof_tmp/oversize.bin" --kind auth-missing-oversize \
  >"$proof_tmp/auth-missing-oversize.json"
python3 benchmarks/public_edge_probe.py request \
  --url "$public_url/v1/chat/completions" --method POST \
  --body-file "$proof_tmp/valid-small.json" --kind auth-wrong \
  --bearer-env V28_WRONG >"$proof_tmp/auth-wrong.json"
python3 benchmarks/public_edge_probe.py request \
  --url "$public_url/v1/chat/completions" --method POST \
  --body-file "$proof_tmp/valid-small.json" --kind auth-wrong-scheme \
  --authorization-env V28_WRONG_SCHEME >"$proof_tmp/auth-wrong-scheme.json"
python3 benchmarks/public_edge_probe.py duplicate-auth \
  --url "$public_url/v1/chat/completions" --body-file "$proof_tmp/valid-small.json" \
  --bearer-env V28_PUBLIC_A >"$proof_tmp/auth-duplicate.json"
python3 - "$proof_tmp" >"$results_dir/authentication-rejections.json" <<'PY'
import json
import sys
from pathlib import Path
root = Path(sys.argv[1])
load = lambda name: json.loads((root / name).read_text(encoding="utf-8"))
print(json.dumps({
    "schema": "inferlab.public-edge-authentication-rejections.v0.28",
    "cases": {
        "missing": load("auth-missing.json"),
        "missing_oversize": load("auth-missing-oversize.json"),
        "wrong": load("auth-wrong.json"),
        "wrong_scheme": load("auth-wrong-scheme.json"),
        "duplicate": load("auth-duplicate.json"),
    },
}, indent=2, sort_keys=True))
PY

input_names=(fixed_oversize malformed_json missing_messages invalid_message_content too_many_messages prompt_too_large invalid_max_tokens_zero invalid_max_tokens_string max_output_tokens_exceeded)
input_files=(oversize.bin malformed.json missing-messages.json invalid-content.json too-many.json prompt-large.json max-zero.json max-string.json max-large.json)
input_outputs=()
for ((input_index = 0; input_index < ${#input_names[@]}; input_index++)); do
  input_output="$proof_tmp/input-${input_names[$input_index]}.json"
  python3 benchmarks/public_edge_probe.py request \
    --url "$public_url/v1/chat/completions" --method POST \
    --body-file "$proof_tmp/${input_files[$input_index]}" --kind "input-${input_names[$input_index]}" \
    --bearer-env V28_PUBLIC_A >"$input_output"
  input_outputs+=("${input_names[$input_index]}" "$input_output")
done
python3 benchmarks/public_edge_probe.py chunked-oversize \
  --url "$public_url/v1/chat/completions" --size 65537 --chunk-size 4096 \
  --bearer-env V28_PUBLIC_A >"$proof_tmp/input-chunked-oversize.json"
input_outputs+=(chunked_oversize "$proof_tmp/input-chunked-oversize.json")
python3 - "${input_outputs[@]}" >"$results_dir/input-rejections.json" <<'PY'
import json
import sys
from pathlib import Path
values = sys.argv[1:]
if len(values) % 2:
    raise SystemExit("input evidence requires name/path pairs")
cases = {
    values[index]: json.loads(Path(values[index + 1]).read_text(encoding="utf-8"))
    for index in range(0, len(values), 2)
}
print(json.dumps({
    "schema": "inferlab.public-edge-input-rejections.v0.28",
    "cases": cases,
}, indent=2, sort_keys=True))
PY

curl --noproxy '*' --fail --silent --show-error --connect-timeout 1 --max-time 3 \
  "$gateway_metrics_url/metrics" \
  >"$results_dir/attempts-after-rejections-gateway.prom"
curl --noproxy '*' --fail --silent --show-error --connect-timeout 1 --max-time 3 \
  "$worker_metrics_url/metrics" \
  >"$results_dir/attempts-after-rejections-worker.prom"

python3 benchmarks/public_edge_probe.py request \
  --url "$public_url/v1/chat/completions" --method POST \
  --body-file "$proof_tmp/request-boundary.json" --kind exact-request-boundary \
  --bearer-env V28_PUBLIC_A >"$proof_tmp/request-boundary-observation.json"
python3 - "$proof_tmp/request-boundary.json" \
  "$proof_tmp/request-boundary-observation.json" >"$results_dir/request-boundary.json" <<'PY'
import json
import sys
from pathlib import Path
fixture = Path(sys.argv[1])
observation = json.loads(Path(sys.argv[2]).read_text(encoding="utf-8"))
print(json.dumps({
    "schema": "inferlab.public-edge-request-boundary.v0.28",
    "request_body_bytes": len(fixture.read_bytes()),
    "observation": observation,
}, indent=2, sort_keys=True))
PY
sleep 2.1

python3 benchmarks/public_edge_probe.py rate-sequence \
  --url "$public_url/v1/chat/completions" --body-file "$proof_tmp/valid-small.json" \
  --public-a-env V28_PUBLIC_A --public-b-env V28_PUBLIC_B \
  --rate-requests-per-minute 60 --rate-burst 2 --refill-wait-ms 1100 \
  >"$results_dir/rate-limit.json"

sleep 2.1
python3 benchmarks/public_edge_probe.py request \
  --url "$public_url/v1/chat/completions" --method POST \
  --body-file "$proof_tmp/valid-json.json" --kind real-json \
  --bearer-env V28_PUBLIC_A --request-id-env V28_REQUEST_ID \
  >"$results_dir/json-completion.json"
"$proof_python" benchmarks/public_edge_probe.py sse \
  --url "$public_url/v1/chat/completions" --body-file "$proof_tmp/valid-sse.json" \
  --bearer-env V28_PUBLIC_B >"$results_dir/sse-completion.json"

sleep 2.1
ready_file="$proof_tmp/disconnect-ready"
release_file="$proof_tmp/disconnect-release"
stream_probe_started_ns="$(monotonic_ns)"
"$proof_python" benchmarks/public_edge_probe.py sse \
  --url "$public_url/v1/chat/completions" --body-file "$proof_tmp/valid-sse.json" \
  --bearer-env V28_PUBLIC_A --disconnect-after-content 1 \
  --ready-file "$ready_file" --release-file "$release_file" --release-timeout-ms 5000 \
  >"$proof_tmp/disconnect-stream.json" &
disconnect_probe_pid="$!"
live_pids+=("$disconnect_probe_pid")
disconnect_deadline=$((SECONDS + 15))
while [[ ! -s "$ready_file" ]]; do
  if ! is_owned_child "$disconnect_probe_pid"; then
    echo 'disconnect probe exited before observing content' >&2
    exit 1
  fi
  if ((SECONDS >= disconnect_deadline)); then
    echo 'timed out waiting for disconnect content' >&2
    exit 1
  fi
  sleep 0.01
done
content_ready_ns="$(tr -d '[:space:]' <"$ready_file")"
if [[ ! "$content_ready_ns" =~ ^[1-9][0-9]*$ ]]; then
  echo 'disconnect probe retained an invalid content-ready monotonic timestamp' >&2
  exit 1
fi
during_status_started_ns="$(monotonic_ns)"
python3 benchmarks/public_edge_probe.py request \
  --url "$operator_url/internal/workers" --bearer-env V28_OPERATOR \
  --kind disconnect-during --projection operator-status \
  >"$results_dir/sse-disconnect-during-status.json"
during_status_completed_ns="$(monotonic_ns)"
admission_started_ns="$(monotonic_ns)"
python3 benchmarks/public_edge_probe.py request \
  --url "$public_url/v1/chat/completions" --method POST \
  --body-file "$proof_tmp/valid-small.json" --kind admission-full \
  --bearer-env V28_PUBLIC_B >"$proof_tmp/admission-full.json"
admission_completed_ns="$(monotonic_ns)"
release_signaled_ns="$(monotonic_ns)"
printf 'release\n' >"$release_file"
wait "$disconnect_probe_pid"
stream_probe_completed_ns="$(monotonic_ns)"
forget_pid "$disconnect_probe_pid"
disconnect_probe_pid=''

idle_deadline=$((SECONDS + 15))
while true; do
  after_status_started_ns="$(monotonic_ns)"
  python3 benchmarks/public_edge_probe.py request \
    --url "$operator_url/internal/workers" --bearer-env V28_OPERATOR \
    --kind disconnect-after --projection operator-status \
    >"$proof_tmp/disconnect-after-candidate.json"
  after_status_completed_ns="$(monotonic_ns)"
  if python3 - "$proof_tmp/disconnect-after-candidate.json" <<'PY'
import json
import sys
value = json.load(open(sys.argv[1], encoding="utf-8"))
body = value.get("body", {})
admission = body.get("admission", {})
workers = body.get("workers", [])
raise SystemExit(0 if (
    value.get("status") == 200
    and admission.get("outstanding") == 0
    and admission.get("executing") == 0
    and admission.get("queued") == 0
    and workers
    and all(worker.get("in_flight") == 0 and worker.get("executing") == 0 for worker in workers)
) else 1)
PY
  then
    mv "$proof_tmp/disconnect-after-candidate.json" \
      "$results_dir/sse-disconnect-after-status.json"
    break
  fi
  if ((SECONDS >= idle_deadline)); then
    echo 'disconnect did not release all bounded ownership' >&2
    exit 1
  fi
  sleep 0.02
done

first_started_ns="$(monotonic_ns)"
python3 benchmarks/public_edge_probe.py request \
  --url "$public_url/v1/chat/completions" --method POST \
  --body-file "$proof_tmp/valid-small.json" --kind no-refund-first \
  --bearer-env V28_PUBLIC_B >"$proof_tmp/no-refund-first.json"
first_completed_ns="$(monotonic_ns)"
limited_started_ns="$(monotonic_ns)"
python3 benchmarks/public_edge_probe.py request \
  --url "$public_url/v1/chat/completions" --method POST \
  --body-file "$proof_tmp/valid-small.json" --kind no-refund-limited \
  --bearer-env V28_PUBLIC_B >"$proof_tmp/no-refund-limited.json"
limited_completed_ns="$(monotonic_ns)"
python3 - "$proof_tmp" "$stream_probe_started_ns" "$content_ready_ns" \
  "$during_status_started_ns" "$during_status_completed_ns" \
  "$admission_started_ns" "$admission_completed_ns" "$release_signaled_ns" \
  "$stream_probe_completed_ns" "$after_status_started_ns" \
  "$after_status_completed_ns" "$first_started_ns" "$first_completed_ns" \
  "$limited_started_ns" "$limited_completed_ns" \
  >"$results_dir/sse-disconnect.json" <<'PY'
import json
import sys
from pathlib import Path
root = Path(sys.argv[1])
load = lambda name: json.loads((root / name).read_text(encoding="utf-8"))
names = [
    "stream_probe_started_ns",
    "content_ready_ns",
    "during_status_started_ns",
    "during_status_completed_ns",
    "admission_started_ns",
    "admission_completed_ns",
    "release_signaled_ns",
    "stream_probe_completed_ns",
    "after_status_started_ns",
    "after_status_completed_ns",
    "first_started_ns",
    "first_completed_ns",
    "limited_started_ns",
    "limited_completed_ns",
]
values = sys.argv[2:]
if len(values) != len(names):
    raise SystemExit("disconnect timeline arity mismatch")
timeline = {name: int(value) for name, value in zip(names, values)}
print(json.dumps({
    "schema": "inferlab.public-edge-disconnect.v0.28",
    "admission_to_second_request_ms": round(
        (timeline["limited_started_ns"] - timeline["admission_completed_ns"])
        / 1_000_000,
        3,
    ),
    "timeline": timeline,
    "stream": load("disconnect-stream.json"),
    "admission_full": load("admission-full.json"),
    "after_release_first": load("no-refund-first.json"),
    "after_release_limited": load("no-refund-limited.json"),
}, indent=2, sort_keys=True))
PY

python3 benchmarks/public_edge_probe.py request \
  --url "$operator_url/internal/workers" --bearer-env V28_OPERATOR \
  --kind final-operator-status --projection operator-status \
  >"$results_dir/operator-status-final.json"
curl --noproxy '*' --fail --silent --show-error --connect-timeout 1 --max-time 3 \
  "$gateway_metrics_url/metrics" \
  >"$results_dir/final-gateway.prom"
curl --noproxy '*' --fail --silent --show-error --connect-timeout 1 --max-time 3 \
  "$worker_metrics_url/metrics" \
  >"$results_dir/final-worker.prom"

production_filters=(
  'public_edge::tests::config_bounds_and_mode_are_explicit'
  'public_edge::tests::exact_burst_refill_and_slot_isolation_use_a_deterministic_clock'
  'public_edge::tests::input_policy_distinguishes_json_messages_prompt_and_output_limits'
  'metrics::tests::gateway_theoretical_series_stay_within_the_hard_target_budget'
  'worker_execution_admission_rejection_is_counted_as_zero_attempt_public_edge_work'
)
production_targets=(lib lib lib lib public_edge)
production_arguments=()
for ((test_index = 0; test_index < ${#production_filters[@]}; test_index++)); do
  test_filter="${production_filters[$test_index]}"
  test_target="${production_targets[$test_index]}"
  test_log="$proof_tmp/production-$test_index.log"
  set +e
  if [[ "$test_target" == 'lib' ]]; then
    CARGO_TERM_COLOR=never cargo test --locked -p gateway --lib "$test_filter" -- --exact \
      >"$test_log" 2>&1
  else
    CARGO_TERM_COLOR=never cargo test --locked -p gateway --test "$test_target" \
      "$test_filter" -- --exact >"$test_log" 2>&1
  fi
  test_status="$?"
  set -e
  production_arguments+=("$test_filter" "$test_target" "$test_status" "$test_log")
done
python3 - "${production_arguments[@]}" >"$results_dir/production-tests.json" <<'PY'
import json
import re
import sys
from pathlib import Path

values = sys.argv[1:]
if len(values) % 4:
    raise SystemExit("production evidence requires filter/target/status/log quartets")
summary_pattern = re.compile(
    r"^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; "
    r"[0-9]+ filtered out; finished in [0-9]+(?:\.[0-9]+)?s$"
)
tests = []
for index in range(0, len(values), 4):
    test_filter, target, raw_status, raw_path = values[index:index + 4]
    output = Path(raw_path).read_text(encoding="utf-8", errors="replace")
    lines = output.splitlines()
    summaries = [line for line in lines if summary_pattern.fullmatch(line)]
    exact_test_line = f"test {test_filter} ... ok"
    projected_output = ""
    if lines.count("running 1 test") == 1 and lines.count(exact_test_line) == 1 and len(summaries) == 1:
        projected_output = "\n".join(["running 1 test", exact_test_line, summaries[0]]) + "\n"
    target_args = ["--lib"] if target == "lib" else ["--test", target]
    tests.append({
        "command": ["cargo", "test", "--locked", "-p", "gateway", *target_args,
                    test_filter, "--", "--exact"],
        "environment": {"CARGO_TERM_COLOR": "never"},
        "test_filter": test_filter,
        "exit_code": int(raw_status),
        "running_one_test": lines.count("running 1 test") == 1,
        "exact_test_line": lines.count(exact_test_line) == 1,
        "exact_summary": len(summaries) == 1,
        "summary_line": summaries[0] if len(summaries) == 1 else None,
        "output": projected_output,
    })
result = {
    "schema": "inferlab.public-edge-production-tests.v0.28",
    "test_count": len(tests),
    "tests": tests,
}
print(json.dumps(result, indent=2, sort_keys=True))
if len(tests) != 5 or not all(
    item["exit_code"] == 0 and item["running_one_test"]
    and item["exact_test_line"] and item["exact_summary"]
    for item in tests
):
    raise SystemExit("an exact production regression did not run once and pass")
PY

python3 - "$proof_tmp" "$public_key_a" "$public_key_b" "$operator_key" \
  "$wrong_key" "$proof_prompt" >"$results_dir/discarded-log-scan.json" <<'PY'
import hashlib
import json
import re
import sys
import urllib.parse
from pathlib import Path

root = Path(sys.argv[1])
credentials = sys.argv[2:6]
prompt = sys.argv[6]
startup_names = [
    "startup-bind_collision.log",
    "startup-credential_overlap.log",
    "startup-missing_public_keys.log",
]
runtime_names = ["cpu-worker.log", "gateway.log"]
host_path = re.compile(
    r"(?:/Users|/home|/private/var|/var/folders|/tmp|/workspace|/workspaces|"
    r"/github/workspace)/[^\s\"'<>]+"
)
credential_position = re.compile(
    r"(?:credential|api[ _-]?key).{0,32}(?:slot|entry|index|position)"
    r"\s*[=:]?\s*[0-9]+",
    re.IGNORECASE,
)
private_markers = ("-----BEGIN", "PRIVATE KEY", "CERTIFICATE-----")
credential_encodings = [
    candidate
    for credential in credentials
    for candidate in (
        credential,
        urllib.parse.quote(credential, safe=""),
    )
]
credential_hashes = [
    hashlib.sha256(credential.encode()).hexdigest() for credential in credentials
]
violations = []
for name in startup_names:
    raw = (root / name).read_text(encoding="utf-8", errors="replace")
    if any(value in raw for value in credential_encodings) or any(
        value in raw.lower() for value in credential_hashes
    ):
        violations.append(f"{name}:credential-or-hash")
    if credential_position.search(raw):
        violations.append(f"{name}:credential-position")
    if host_path.search(raw):
        violations.append(f"{name}:host-path")
    if any(marker in raw.upper() for marker in private_markers):
        violations.append(f"{name}:private-marker")
for name in runtime_names:
    raw = (root / name).read_text(encoding="utf-8", errors="replace")
    if (
        any(value in raw for value in credential_encodings)
        or any(value in raw.lower() for value in credential_hashes)
        or prompt in raw
    ):
        violations.append(f"{name}:credential-or-prompt")
    if credential_position.search(raw):
        violations.append(f"{name}:credential-position")
    if any(marker in raw.upper() for marker in private_markers):
        violations.append(f"{name}:private-marker")
if violations:
    raise SystemExit("discarded proof log leak: " + ", ".join(violations))
print(json.dumps({
    "schema": "inferlab.public-edge-discarded-log-scan.v0.28",
    "startup_files_scanned": startup_names,
    "runtime_files_scanned": runtime_names,
    "credential_count": len(credentials),
    "credential_encodings_checked": ["literal", "percent-encoded", "sha256"],
    "prompt_checked_in_runtime_logs": True,
    "credential_position_checked_in_all_logs": True,
    "host_path_checked_in_startup_logs": True,
    "private_marker_checks": len(private_markers),
    "request_ids_allowed_in_runtime_logs": True,
    "violations": 0,
}, indent=2, sort_keys=True))
PY

record_process_continuity

python3 - "$proof_tmp/forbidden-values.json" "$public_key_a" "$public_key_b" \
  "$operator_key" "$wrong_key" "$proof_prompt" "$request_id_marker" <<'PY'
import json
import sys
from pathlib import Path
Path(sys.argv[1]).write_text(json.dumps({
    "public_key_a": sys.argv[2],
    "public_key_b": sys.argv[3],
    "operator_key": sys.argv[4],
    "wrong_key": sys.argv[5],
    "prompt": sys.argv[6],
    "request_id_marker": sys.argv[7],
}, indent=2, sort_keys=True) + "\n", encoding="utf-8")
PY

# Placeholders make the sanitizer/private scan inventories exact before the
# first checker/renderer pass. They are replaced immediately and never retained.
printf '{}\n' >"$results_dir/assertions.json"
printf '<svg/>\n' >"$results_dir/public-edge-proof.svg"
printf '{}\n' >"$results_dir/private-material-scan.json"
python3 benchmarks/public_edge_probe.py sanitize-evidence \
  --evidence-dir "$results_dir" --forbidden-values-file "$proof_tmp/forbidden-values.json" \
  --proof-root "$proof_tmp" --project-root "$project_root" \
  >"$results_dir/sanitizer.json"
scan_private_material "$proof_tmp/private-preliminary.json"
mv "$proof_tmp/private-preliminary.json" "$results_dir/private-material-scan.json"

set +e
python3 benchmarks/check_public_edge.py \
  --evidence-dir "$results_dir" --output "$results_dir/assertions.json"
checker_status="$?"
set -e
if [[ "$checker_status" != '0' ]]; then
  python3 - "$results_dir" <<'PY'
import json
import sys
from pathlib import Path

root = Path(sys.argv[1])
report = json.loads((root / "assertions.json").read_text(encoding="utf-8"))
for item in report.get("assertions", []):
    if item.get("passed") is not True:
        print(f"failed v0.28 assertion: {item.get('name')}", file=sys.stderr)
        if "observations" in item:
            print(
                "  observations="
                + json.dumps(item["observations"], sort_keys=True),
                file=sys.stderr,
            )
for name in (
    "authentication-rejections.json",
    "sse-disconnect.json",
    "operator-status-final.json",
):
    print(
        f"  diagnostic {name}="
        + json.dumps(json.loads((root / name).read_text(encoding="utf-8")), sort_keys=True),
        file=sys.stderr,
    )
PY
  exit "$checker_status"
fi
python3 benchmarks/render_public_edge_svg.py \
  --evidence-dir "$results_dir" --output "$results_dir/public-edge-proof.svg"

python3 benchmarks/public_edge_probe.py sanitize-evidence \
  --evidence-dir "$results_dir" --forbidden-values-file "$proof_tmp/forbidden-values.json" \
  --proof-root "$proof_tmp" --project-root "$project_root" \
  >"$proof_tmp/sanitizer-stable.json"
mv "$proof_tmp/sanitizer-stable.json" "$results_dir/sanitizer.json"
scan_private_material "$proof_tmp/private-stable.json"
mv "$proof_tmp/private-stable.json" "$results_dir/private-material-scan.json"
python3 benchmarks/check_public_edge.py \
  --evidence-dir "$results_dir" --output "$results_dir/assertions.json"
python3 benchmarks/render_public_edge_svg.py \
  --evidence-dir "$results_dir" --output "$results_dir/public-edge-proof.svg"

python3 benchmarks/check_public_edge.py \
  --evidence-dir "$results_dir" --output "$proof_tmp/replay-assertions.json"
cmp "$results_dir/assertions.json" "$proof_tmp/replay-assertions.json"
python3 benchmarks/render_public_edge_svg.py \
  --evidence-dir "$results_dir" --output "$proof_tmp/replay-public-edge-proof.svg"
cmp "$results_dir/public-edge-proof.svg" "$proof_tmp/replay-public-edge-proof.svg"
final_leak_scan

expected_files=(
  assertions.json
  authentication-rejections.json
  attempts-after-rejections-gateway.prom
  attempts-after-rejections-worker.prom
  attempts-before-rejections-gateway.prom
  attempts-before-rejections-worker.prom
  discarded-log-scan.json
  final-gateway.prom
  final-worker.prom
  input-rejections.json
  json-completion.json
  manifest.json
  operator-status-final.json
  private-material-scan.json
  process-continuity.json
  production-tests.json
  proof-contract.json
  public-edge-proof.svg
  rate-limit.json
  request-boundary.json
  route-isolation.json
  sanitizer.json
  sse-completion.json
  sse-disconnect-after-status.json
  sse-disconnect-during-status.json
  sse-disconnect.json
  startup-contract.json
)
write_manifest "${expected_files[@]}"
python3 benchmarks/check_public_edge.py \
  --evidence-dir "$results_dir" --require-manifest \
  --output "$proof_tmp/post-manifest-assertions.json"
cmp "$results_dir/assertions.json" "$proof_tmp/post-manifest-assertions.json"
python3 benchmarks/render_public_edge_svg.py \
  --evidence-dir "$results_dir" --output "$proof_tmp/post-manifest-public-edge-proof.svg"
cmp "$results_dir/public-edge-proof.svg" "$proof_tmp/post-manifest-public-edge-proof.svg"

retain_results "${expected_files[@]}"
if [[ -n "${INFERLAB_V28_OUTPUT_DIR:-}" ]]; then
  python3 benchmarks/check_public_edge.py \
    --evidence-dir "$INFERLAB_V28_OUTPUT_DIR" --require-manifest \
    --output "$proof_tmp/retained-assertions.json"
  cmp "$results_dir/assertions.json" "$proof_tmp/retained-assertions.json"
  python3 benchmarks/render_public_edge_svg.py \
    --evidence-dir "$INFERLAB_V28_OUTPUT_DIR" \
    --output "$proof_tmp/retained-public-edge-proof.svg"
  cmp "$results_dir/public-edge-proof.svg" "$proof_tmp/retained-public-edge-proof.svg"
fi

python3 - "$results_dir/assertions.json" <<'PY'
import json
import sys
report = json.load(open(sys.argv[1], encoding="utf-8"))
print(
    f"v0.28 exact-process public-edge proof complete: "
    f"{report['passed']}/{report['total']} assertions passed"
)
PY
