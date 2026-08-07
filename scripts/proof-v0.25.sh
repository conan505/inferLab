#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

if [[ -x "$project_root/.tools/cargo/bin/cargo" ]]; then
  export RUSTUP_HOME="$project_root/.tools/rustup"
  export CARGO_HOME="$project_root/.tools/cargo"
  export PATH="$CARGO_HOME/bin:$PATH"
fi

unset INFERLAB_PUBLIC_API_KEYS || true
export NO_PROXY='127.0.0.1,localhost'
export no_proxy="$NO_PROXY"
umask 077

proof_tmp_root="${TMPDIR:-/tmp}"
proof_tmp="$(mktemp -d "$proof_tmp_root/inferlab-v025.XXXXXX")"
results_dir="$proof_tmp/results"
mkdir -p "$results_dir"

live_pids=()
node_a_pid=''
node_b_pid=''
node_c_pid=''
proxy_a_b_pid=''
proxy_a_c_pid=''
proxy_b_a_pid=''
proxy_b_c_pid=''
proxy_c_a_pid=''
proxy_c_b_pid=''
gateway_pid=''
worker_pid=''

cluster_id='inferlab-primary'
node_urls='node-a=http://127.0.0.1:9961,node-b=http://127.0.0.1:9962,node-c=http://127.0.0.1:9963'
control_urls='http://127.0.0.1:9961,http://127.0.0.1:9962,http://127.0.0.1:9963'
majority_urls='node-b=http://127.0.0.1:9962,node-c=http://127.0.0.1:9963'
link_urls='a-to-b=http://127.0.0.1:9971,a-to-c=http://127.0.0.1:9972,b-to-a=http://127.0.0.1:9973,b-to-c=http://127.0.0.1:9974,c-to-a=http://127.0.0.1:9975,c-to-b=http://127.0.0.1:9976'
partition_links='b-to-a=http://127.0.0.1:9973,c-to-a=http://127.0.0.1:9975,a-to-b=http://127.0.0.1:9971,a-to-c=http://127.0.0.1:9972'
healing_links='a-to-b=http://127.0.0.1:9971,a-to-c=http://127.0.0.1:9972,b-to-a=http://127.0.0.1:9973,c-to-a=http://127.0.0.1:9975'
link_events="a-to-b=$proof_tmp/a-to-b.jsonl,a-to-c=$proof_tmp/a-to-c.jsonl,b-to-a=$proof_tmp/b-to-a.jsonl,b-to-c=$proof_tmp/b-to-c.jsonl,c-to-a=$proof_tmp/c-to-a.jsonl,c-to-b=$proof_tmp/c-to-b.jsonl"
gateway_url='http://127.0.0.1:9960'

route_key_id='route-2026-b'
route_seed='TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs='
route_public='PUAXw+hDiVqStwqnTRt+vJyYLM8uxJaMwM1V8Sr0Zgw='
writer_id='deploy-bot'
writer_seed='nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A='
writer_public='11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo='
node_a_seed='TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs='
node_b_seed='nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A='
node_c_seed='xaqN9D+fg3vtt0QvMdy3sWbThTUHbwlLhc46LgtEWPc='
gateway_seed='AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA='

forget_pid() {
  local forgotten="$1"
  local pid
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
  local pid="$1"
  local attempt state
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
  local owned_pids=("${live_pids[@]}")
  local index pid
  for ((index = ${#owned_pids[@]} - 1; index >= 0; index--)); do
    pid="${owned_pids[$index]}"
    shutdown_child "$pid"
    forget_pid "$pid"
  done
  if [[ -n "$proof_tmp" && -d "$proof_tmp" ]]; then
    case "$(basename "$proof_tmp")" in
      inferlab-v025.*) rm -rf -- "$proof_tmp" ;;
      *) echo "refusing to remove unexpected proof path: $proof_tmp" >&2 ;;
    esac
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

prepare_output_dir() {
  if [[ -z "${INFERLAB_V25_OUTPUT_DIR:-}" ]]; then
    return
  fi
  mkdir -p "$INFERLAB_V25_OUTPUT_DIR"
  if find "$INFERLAB_V25_OUTPUT_DIR" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
    echo 'INFERLAB_V25_OUTPUT_DIR must be empty so stale evidence cannot survive' >&2
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
            raise SystemExit(f"refusing v0.25 proof: 127.0.0.1:{port} is busy: {error}")
PY
}

wait_for_health() {
  local url="$1" pid="$2" label="$3"
  local deadline=$((SECONDS + 60))
  until curl --noproxy '*' --connect-timeout 0.05 --max-time 0.15 --fail --silent "$url" >/dev/null; do
    if [[ -n "$pid" ]] && ! kill -0 "$pid" 2>/dev/null; then
      echo "$label exited before becoming healthy" >&2
      return 1
    fi
    if ((SECONDS >= deadline)); then
      echo "timed out waiting for $label" >&2
      return 1
    fi
    sleep 0.05
  done
}

json_field() {
  python3 - "$1" "$2" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    value = json.load(source)
for component in sys.argv[2].split("."):
    value = value[int(component)] if component.isdigit() else value[component]
print(value)
PY
}

service_seed() {
  case "$1" in
    node-a) printf '%s' "$node_a_seed" ;;
    node-b) printf '%s' "$node_b_seed" ;;
    node-c) printf '%s' "$node_c_seed" ;;
    gateway-primary) printf '%s' "$gateway_seed" ;;
    *) return 1 ;;
  esac
}

public_key() {
  local service_id="$1" seed="$2"
  INFERLAB_SERVICE_ID="$service_id" \
  INFERLAB_SERVICE_CREDENTIAL_ID='key-a' \
  INFERLAB_SERVICE_PRIVATE_KEY_B64="$seed" \
    target/debug/service_public_key
}

proxy_port() {
  case "$1" in
    a-to-b) printf '9971' ;;
    a-to-c) printf '9972' ;;
    b-to-a) printf '9973' ;;
    b-to-c) printf '9974' ;;
    c-to-a) printf '9975' ;;
    c-to-b) printf '9976' ;;
    *) return 1 ;;
  esac
}

proxy_source() {
  case "$1" in
    a-to-b|a-to-c) printf 'node-a' ;;
    b-to-a|b-to-c) printf 'node-b' ;;
    c-to-a|c-to-b) printf 'node-c' ;;
    *) return 1 ;;
  esac
}

proxy_target() {
  case "$1" in
    b-to-a|c-to-a) printf 'node-a' ;;
    a-to-b|c-to-b) printf 'node-b' ;;
    a-to-c|b-to-c) printf 'node-c' ;;
    *) return 1 ;;
  esac
}

target_port() {
  case "$1" in
    node-a) printf '9961' ;;
    node-b) printf '9962' ;;
    node-c) printf '9963' ;;
    *) return 1 ;;
  esac
}

set_proxy_pid() {
  case "$1" in
    a-to-b) proxy_a_b_pid="$2" ;;
    a-to-c) proxy_a_c_pid="$2" ;;
    b-to-a) proxy_b_a_pid="$2" ;;
    b-to-c) proxy_b_c_pid="$2" ;;
    c-to-a) proxy_c_a_pid="$2" ;;
    c-to-b) proxy_c_b_pid="$2" ;;
    *) return 1 ;;
  esac
}

start_proxy() {
  local link_id="$1" source target port upstream pid
  source="$(proxy_source "$link_id")"
  target="$(proxy_target "$link_id")"
  port="$(proxy_port "$link_id")"
  upstream="$(target_port "$target")"
  INFERLAB_RAFT_LINK_ID="$link_id" \
  INFERLAB_RAFT_LINK_SOURCE_ID="$source" \
  INFERLAB_RAFT_LINK_TARGET_ID="$target" \
  INFERLAB_RAFT_LINK_BIND="127.0.0.1:$port" \
  INFERLAB_RAFT_LINK_UPSTREAM="http://127.0.0.1:$upstream" \
  INFERLAB_RAFT_LINK_EVENT_PATH="$proof_tmp/$link_id.jsonl" \
    target/debug/raft-link-proxy >"$proof_tmp/$link_id.log" 2>&1 &
  pid="$!"
  live_pids+=("$pid")
  set_proxy_pid "$link_id" "$pid"
  wait_for_health "http://127.0.0.1:$port/healthz" "$pid" "link proxy $link_id"
}

node_port() {
  case "$1" in
    node-a) printf '9961' ;;
    node-b) printf '9962' ;;
    node-c) printf '9963' ;;
    *) return 1 ;;
  esac
}

node_peers() {
  case "$1" in
    node-a) printf 'node-b=http://127.0.0.1:9971,node-c=http://127.0.0.1:9972' ;;
    node-b) printf 'node-a=http://127.0.0.1:9973,node-c=http://127.0.0.1:9974' ;;
    node-c) printf 'node-a=http://127.0.0.1:9975,node-b=http://127.0.0.1:9976' ;;
    *) return 1 ;;
  esac
}

node_election_min() {
  case "$1" in
    node-a) printf '180' ;;
    node-b) printf '1500' ;;
    node-c) printf '2500' ;;
    *) return 1 ;;
  esac
}

set_node_pid() {
  case "$1" in
    node-a) node_a_pid="$2" ;;
    node-b) node_b_pid="$2" ;;
    node-c) node_c_pid="$2" ;;
    *) return 1 ;;
  esac
}

start_node() {
  local node_id="$1" port peers election_min election_max seed pid
  port="$(node_port "$node_id")"
  peers="$(node_peers "$node_id")"
  election_min="$(node_election_min "$node_id")"
  election_max="$((election_min + 60))"
  seed="$(service_seed "$node_id")"
  mkdir -p "$proof_tmp/$node_id"
  INFERLAB_RAFT_NODE_ID="$node_id" \
  INFERLAB_RAFT_CLUSTER_ID="$cluster_id" \
  INFERLAB_RAFT_BIND="127.0.0.1:$port" \
  INFERLAB_RAFT_PEERS="$peers" \
  INFERLAB_RAFT_DATA_DIR="$proof_tmp/$node_id" \
  INFERLAB_RAFT_ELECTION_MIN_MS="$election_min" \
  INFERLAB_RAFT_ELECTION_MAX_MS="$election_max" \
  INFERLAB_RAFT_HEARTBEAT_MS=50 \
  INFERLAB_RAFT_RPC_TIMEOUT_MS=150 \
  INFERLAB_RAFT_COMMIT_TIMEOUT_MS=1600 \
  INFERLAB_CONTROL_SIGNING_KEY_ID="$route_key_id" \
  INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64="$route_seed" \
  INFERLAB_CONTROL_WRITER_KEYS="$writer_id=$writer_public" \
  INFERLAB_CONTROL_WRITE_MAX_AGE_MS=5000 \
  INFERLAB_CONTROL_WRITE_MAX_FUTURE_SKEW_MS=250 \
  INFERLAB_SERVICE_ID="$node_id" \
  INFERLAB_SERVICE_CREDENTIAL_ID='key-a' \
  INFERLAB_SERVICE_PRIVATE_KEY_B64="$seed" \
  INFERLAB_SERVICE_TRUSTED_KEYS="$service_keys" \
  INFERLAB_GATEWAY_SERVICE_IDS='gateway-primary' \
  INFERLAB_SERVICE_AUTH_MAX_AGE_MS=5000 \
  INFERLAB_SERVICE_AUTH_MAX_FUTURE_SKEW_MS=250 \
    target/debug/control-plane >"$proof_tmp/$node_id.log" 2>&1 &
  pid="$!"
  live_pids+=("$pid")
  set_node_pid "$node_id" "$pid"
  wait_for_health "http://127.0.0.1:$port/healthz" "$pid" "$node_id"
}

create_configuration() {
  local policy="$1" weight="$2" output="$3"
  python3 - "$policy" "$weight" "$output" <<'PY'
import json
import sys
from pathlib import Path

policy, weight, output = sys.argv[1], int(sys.argv[2]), Path(sys.argv[3])
output.write_text(json.dumps({
    "routing_policy": policy,
    "workers": [{
        "id": "cpu-raft-partition",
        "base_url": "http://127.0.0.1:9964",
        "weight": weight,
    }],
}, indent=2) + "\n", encoding="utf-8")
PY
}

sign_configuration() {
  local expected_revision="$1" nonce="$2" configuration="$3" output="$4"
  INFERLAB_CONTROL_WRITER_ID="$writer_id" \
  INFERLAB_CONTROL_WRITER_PRIVATE_KEY_B64="$writer_seed" \
    target/debug/sign_control_write \
      "$cluster_id" "$expected_revision" now "$nonce" "$configuration" >"$output"
}

start_worker() {
  INFERLAB_CPU_WORKER_ID=cpu-raft-partition \
  INFERLAB_CPU_BIND=127.0.0.1:9964 \
  INFERLAB_MODEL_PATH=models/tiny-inferlab-v2.bin \
  INFERLAB_CPU_DECODER_MODE=paged-kv-cache \
  INFERLAB_CPU_QUANTIZATION=fp32 \
  INFERLAB_CPU_SPECULATIVE_DRAFT_QUANTIZATION=int8 \
  INFERLAB_CPU_ATTENTION_KERNEL=online-tiled \
  INFERLAB_CPU_ATTENTION_PRECISION=fp32 \
  INFERLAB_CPU_ATTENTION_TILE_TOKENS=32 \
  INFERLAB_CPU_MAX_BATCH_SIZE=4 \
  INFERLAB_CPU_SCHEDULER_QUEUE_CAPACITY=16 \
  INFERLAB_CPU_KV_PAGE_TOKENS=4 \
  INFERLAB_CPU_KV_PAGE_COUNT=64 \
  INFERLAB_CPU_PREFIX_CACHE_CAPACITY=32 \
  INFERLAB_CPU_BATCH_TICK_MS=20 \
    target/debug/cpu-worker >"$proof_tmp/worker.log" 2>&1 &
  worker_pid="$!"
  live_pids+=("$worker_pid")
  wait_for_health 'http://127.0.0.1:9964/health' "$worker_pid" 'real CPU worker'
}

start_gateway() {
  INFERLAB_BIND=127.0.0.1:9960 \
  INFERLAB_CONTROL_PLANE_URLS="$control_urls" \
  INFERLAB_CONTROL_CLUSTER_ID="$cluster_id" \
  INFERLAB_CONTROL_TRUSTED_KEYS="$route_key_id=$route_public" \
  INFERLAB_CONTROL_BOOTSTRAP_WAIT_MS=2000 \
  INFERLAB_CONTROL_POLL_MS=50 \
  INFERLAB_GATEWAY_SERVICE_ID='gateway-primary' \
  INFERLAB_GATEWAY_SERVICE_CREDENTIAL_ID='key-a' \
  INFERLAB_GATEWAY_SERVICE_PRIVATE_KEY_B64="$gateway_seed" \
  INFERLAB_CONTROL_SERVICE_TARGETS='node-a=http://127.0.0.1:9961,node-b=http://127.0.0.1:9962,node-c=http://127.0.0.1:9963' \
  INFERLAB_ROUTING_SNAPSHOT_PATH="$proof_tmp/gateway-routing.json" \
  INFERLAB_ROUTING_SNAPSHOT_MAX_AGE_MS=60000 \
  INFERLAB_ROUTING_LEASE_MS=3000 \
  INFERLAB_WORKER_CONCURRENCY=4 \
  INFERLAB_ADMISSION_QUEUE_CAPACITY=8 \
  INFERLAB_REQUEST_DEADLINE_MS=10000 \
  INFERLAB_ATTEMPT_TIMEOUT_MS=10000 \
  INFERLAB_MAX_RETRIES=0 \
    target/debug/gateway >"$proof_tmp/gateway.log" 2>&1 &
  gateway_pid="$!"
  live_pids+=("$gateway_pid")
  wait_for_health "$gateway_url/health" "$gateway_pid" 'gateway'
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
        check=False,
        capture_output=True,
        text=True,
    )
    fields = result.stdout.strip().split(None, 7)
    ppid = int(fields[0]) if len(fields) >= 1 and fields[0].isdigit() else None
    state = fields[1] if len(fields) >= 2 else None
    started = " ".join(fields[2:7]) if len(fields) >= 7 else None
    command = fields[7] if len(fields) >= 8 else None
    return {
        "pid": int(raw_pid),
        "parent_pid": ppid,
        "process_state": state,
        "start_token": started,
        "command": command,
        "alive": result.returncode == 0 and ppid is not None,
        "owned_child": ppid == proof_shell_pid,
        "non_zombie": state is not None and "Z" not in state,
    }

processes = {
    values[offset]: observe(values[offset + 1])
    for offset in range(0, len(values), 2)
}
if not all(
    item["alive"] and item["owned_child"] and item["non_zombie"] and item["start_token"]
    for item in processes.values()
):
    raise SystemExit("could not capture an owned non-zombie proof child")
print(json.dumps({"proof_shell_pid": proof_shell_pid, "processes": processes}, indent=2, sort_keys=True))
PY
}

record_process_continuity() {
  assert_owned_processes_alive \
    node-a "$node_a_pid" node-b "$node_b_pid" node-c "$node_c_pid" \
    a-to-b "$proxy_a_b_pid" a-to-c "$proxy_a_c_pid" \
    b-to-a "$proxy_b_a_pid" b-to-c "$proxy_b_c_pid" \
    c-to-a "$proxy_c_a_pid" c-to-b "$proxy_c_b_pid" \
    gateway "$gateway_pid" cpu-worker "$worker_pid"
  python3 - "$proof_tmp/initial-partition-processes.json" \
    "$proof_tmp/initial-product-processes.json" "$$" \
    >"$results_dir/process-continuity.json" <<'PY'
import json
import subprocess
import sys
from pathlib import Path

partition = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
product = json.loads(Path(sys.argv[2]).read_text(encoding="utf-8"))
proof_shell_pid = int(sys.argv[3])
initial_processes = {**partition["processes"], **product["processes"]}

def observe(pid: int) -> dict:
    result = subprocess.run(
        ["ps", "-o", "ppid=", "-o", "stat=", "-o", "lstart=", "-o", "command=", "-p", str(pid)],
        check=False,
        capture_output=True,
        text=True,
    )
    fields = result.stdout.strip().split(None, 7)
    ppid = int(fields[0]) if len(fields) >= 1 and fields[0].isdigit() else None
    state = fields[1] if len(fields) >= 2 else None
    started = " ".join(fields[2:7]) if len(fields) >= 7 else None
    command = fields[7] if len(fields) >= 8 else None
    return {
        "pid": pid,
        "parent_pid": ppid,
        "process_state": state,
        "start_token": started,
        "command": command,
        "alive": result.returncode == 0 and ppid is not None,
        "owned_child": ppid == proof_shell_pid,
        "non_zombie": state is not None and "Z" not in state,
    }

processes = {}
for name, initial in initial_processes.items():
    current = observe(initial["pid"])
    processes[name] = {
        "initial_pid": initial["pid"],
        "current_pid": current["pid"],
        "same_pid": initial["pid"] == current["pid"],
        "same_start_token": initial["start_token"] == current["start_token"],
        "alive": current["alive"],
        "owned_child": current["owned_child"],
        "non_zombie": current["non_zombie"],
        "initial_start_token": initial["start_token"],
        "current_start_token": current["start_token"],
        "parent_pid": current["parent_pid"],
        "process_state": current["process_state"],
        "command": current["command"],
    }
print(json.dumps({
    "schema": "inferlab.raft-partition-process-continuity.v0.25",
    "proof_shell_pid": proof_shell_pid,
    "partition_participants": [
        "node-a", "node-b", "node-c",
        "a-to-b", "a-to-c", "b-to-a", "b-to-c", "c-to-a", "c-to-b",
    ],
    "processes": processes,
}, indent=2, sort_keys=True))
PY
}

scan_private_material() {
  local output="$1"
  python3 - "$results_dir" \
    "$route_seed" "$writer_seed" "$node_a_seed" "$node_b_seed" "$node_c_seed" "$gateway_seed" \
    >"$output" <<'PY'
import json
import sys
from pathlib import Path

evidence = Path(sys.argv[1])
labels = ["route_seed", "writer_seed", "node_a_seed", "node_b_seed", "node_c_seed", "gateway_seed"]
values = sys.argv[2:]

def compact(value: str) -> str:
    return "".join(value.replace("\\n", "").replace("\\r", "").split())

candidates = []
for label, value in zip(labels, values):
    normalized = compact(value)
    candidates.extend([(label, normalized), (f"{label}_unpadded", normalized.rstrip("="))])

files = sorted(path for path in evidence.iterdir() if path.suffix in {".json", ".svg"})
matches = []
for path in files:
    retained = compact(path.read_text(encoding="utf-8"))
    for label, candidate in candidates:
        if len(candidate) >= 32 and candidate in retained:
            matches.append({"file": path.name, "candidate_label": label})
if matches:
    raise SystemExit(
        "private seed leaked into retained evidence: "
        + ", ".join(f"{item['candidate_label']} in {item['file']}" for item in matches)
    )
print(json.dumps({
    "schema": "inferlab.private-material-scan.v0.25",
    "files_scanned": [path.name for path in files],
    "known_ed25519_seed_labels_scanned": labels,
    "known_ed25519_seed_count": len(labels),
    "normalized_base64_and_escaped_newlines": True,
    "matches": 0,
}, indent=2, sort_keys=True))
PY
}

final_leak_scan() {
  python3 - "$results_dir" "$proof_tmp" "$project_root" <<'PY'
import re
import sys
from pathlib import Path

evidence, proof_root, project_root = Path(sys.argv[1]), sys.argv[2], sys.argv[3]
for path in sorted(evidence.iterdir()):
    if path.suffix not in {".json", ".svg"}:
        continue
    text = path.read_text(encoding="utf-8")
    if proof_root in text or project_root in text:
        raise SystemExit(f"host path leaked into {path.name}")
    if any(marker in text for marker in ["-----BEGIN", "PRIVATE KEY", "CERTIFICATE-----"]):
        raise SystemExit(f"private material marker leaked into {path.name}")
    if re.search(r"(?:/Users|/home|/private/var|/tmp)/[^\s\"'<>]+", text):
        raise SystemExit(f"absolute host path leaked into {path.name}")
PY
}

write_manifest() {
  python3 - "$results_dir" "$@" <<'PY'
import hashlib
import json
import sys
from pathlib import Path

directory = Path(sys.argv[1])
expected = sorted(sys.argv[2:])
if "manifest.json" not in expected:
    raise SystemExit("manifest expected set must include manifest.json")
before = sorted(path.name for path in directory.iterdir())
if before != sorted(name for name in expected if name != "manifest.json"):
    raise SystemExit(f"unexpected pre-manifest evidence set: {before}")
files = []
for name in expected:
    if name == "manifest.json":
        continue
    content = (directory / name).read_bytes()
    files.append({"path": name, "sha256": hashlib.sha256(content).hexdigest(), "bytes": len(content)})
(directory / "manifest.json").write_text(json.dumps({
    "schema": "inferlab.evidence-manifest.v0.25",
    "expected_files": expected,
    "file_count": len(expected),
    "hashed_file_count": len(files),
    "files": files,
}, indent=2, sort_keys=True) + "\n", encoding="utf-8")
after = sorted(path.name for path in directory.iterdir())
if after != expected:
    raise SystemExit(f"unexpected final evidence set: {after}")
PY
}

prepare_output_dir
check_ports_are_free 9960 9961 9962 9963 9964 9971 9972 9973 9974 9975 9976
cargo build --workspace --bins --quiet

# The deterministic five-server replay is separate from the live three-node run.
cargo run -p control-plane --bin raft-figure-eight-proof --quiet \
  >"$results_dir/figure-eight.json"
set +e
cargo test -p control-plane --lib \
  figure_eight::tests::figure_eight_requires_a_current_term_entry_before_counting_replicas_is_safe \
  -- --exact >"$proof_tmp/figure-test.stdout" 2>"$proof_tmp/figure-test.stderr"
figure_test_status="$?"
set -e
python3 benchmarks/raft_partition_probe.py command-evidence \
  --command-json '["cargo","test","-p","control-plane","--lib","figure_eight::tests::figure_eight_requires_a_current_term_entry_before_counting_replicas_is_safe","--","--exact"]' \
  --status "$figure_test_status" \
  --stdout-file "$proof_tmp/figure-test.stdout" \
  --stderr-file "$proof_tmp/figure-test.stderr" \
  >"$results_dir/figure-eight-test.json"

node_a_public="$(public_key node-a "$node_a_seed")"
node_b_public="$(public_key node-b "$node_b_seed")"
node_c_public="$(public_key node-c "$node_c_seed")"
gateway_public="$(public_key gateway-primary "$gateway_seed")"
service_keys="node-a/key-a=$node_a_public,node-b/key-a=$node_b_public,node-c/key-a=$node_c_public,gateway-primary/key-a=$gateway_public"

for link_id in a-to-b a-to-c b-to-a b-to-c c-to-a c-to-b; do
  start_proxy "$link_id"
done
start_node node-b
start_node node-c
start_node node-a

create_configuration round-robin 1 "$proof_tmp/baseline-config.json"
python3 benchmarks/raft_partition_probe.py wait-cluster \
  --nodes "$node_urls" --expected-leader node-a --minimum-term 1 --commit-index 1 --timeout 10 \
  >"$proof_tmp/initial-election.json"
sign_configuration 0 partition-baseline "$proof_tmp/baseline-config.json" "$proof_tmp/baseline-write.json"
python3 benchmarks/raft_partition_probe.py submit-write \
  --url 'http://127.0.0.1:9961' --body "$proof_tmp/baseline-write.json" --status 200 \
  >"$results_dir/baseline-write.json"
python3 benchmarks/raft_partition_probe.py wait-cluster \
  --nodes "$node_urls" --expected-leader node-a --minimum-term 1 \
  --commit-index 2 --revision 2 --policy round-robin --timeout 10 \
  >"$results_dir/baseline-cluster.json"
python3 benchmarks/raft_partition_probe.py capture-state \
  --data-root "$proof_tmp" >"$results_dir/baseline-state.json"
python3 benchmarks/raft_partition_probe.py capture-links \
  --links "$link_urls" >"$results_dir/baseline-links.json"
capture_process_snapshot "$proof_tmp/initial-partition-processes.json" \
  node-a "$node_a_pid" node-b "$node_b_pid" node-c "$node_c_pid" \
  a-to-b "$proxy_a_b_pid" a-to-c "$proxy_a_c_pid" \
  b-to-a "$proxy_b_a_pid" b-to-c "$proxy_b_c_pid" \
  c-to-a "$proxy_c_a_pid" c-to-b "$proxy_c_b_pid"
baseline_term="$(json_field "$results_dir/baseline-cluster.json" statuses.node-a.observation.body.term)"

# Close inbound paths first so a newly campaigning majority cannot raise A's
# term during the small transition window; then close A's outbound heartbeats.
python3 benchmarks/raft_partition_probe.py set-links \
  --links "$partition_links" --mode drop --reason 'isolate-old-leader-a' \
  >"$results_dir/partition-transition.json"

create_configuration least-in-flight 1 "$proof_tmp/minority-config.json"
sign_configuration 2 isolated-a-proposal "$proof_tmp/minority-config.json" "$proof_tmp/minority-write.json"
python3 benchmarks/raft_partition_probe.py submit-write \
  --url 'http://127.0.0.1:9961' --body "$proof_tmp/minority-write.json" \
  --status 503 --timeout 3 \
  >"$results_dir/isolated-write.json"

python3 benchmarks/raft_partition_probe.py wait-cluster \
  --nodes "$majority_urls" --minimum-term "$((baseline_term + 1))" --commit-index 3 \
  --revision 2 --policy round-robin --timeout 10 \
  >"$results_dir/majority-election.json"
majority_leader="$(json_field "$results_dir/majority-election.json" leader_id)"
case "$majority_leader" in
  node-b) majority_leader_url='http://127.0.0.1:9962' ;;
  node-c) majority_leader_url='http://127.0.0.1:9963' ;;
  *) echo "unexpected majority leader: $majority_leader" >&2; exit 1 ;;
esac
create_configuration weighted-round-robin 3 "$proof_tmp/majority-config.json"
sign_configuration 2 majority-config-v1 "$proof_tmp/majority-config.json" "$proof_tmp/majority-write.json"
python3 benchmarks/raft_partition_probe.py submit-write \
  --url "$majority_leader_url" --body "$proof_tmp/majority-write.json" --status 200 \
  >"$results_dir/majority-write.json"
python3 benchmarks/raft_partition_probe.py wait-cluster \
  --nodes "$majority_urls" --minimum-term "$((baseline_term + 1))" --commit-index 4 \
  --revision 4 --policy weighted-round-robin --timeout 10 \
  >"$results_dir/majority-cluster.json"
python3 benchmarks/raft_partition_probe.py capture-cluster \
  --nodes "$node_urls" >"$results_dir/partition-cluster.json"
python3 benchmarks/raft_partition_probe.py capture-state \
  --data-root "$proof_tmp" >"$results_dir/partition-state.json"
python3 benchmarks/raft_partition_probe.py capture-links \
  --links "$link_urls" >"$results_dir/partition-links.json"

# Restore A's outbound paths first, then inbound paths. A observes the higher
# term and the majority leader repairs its conflicting uncommitted suffix.
python3 benchmarks/raft_partition_probe.py set-links \
  --links "$healing_links" --mode allow --reason 'heal-old-leader-cut' \
  >"$results_dir/healing-transition.json"
python3 benchmarks/raft_partition_probe.py wait-cluster \
  --nodes "$node_urls" --required-follower node-a --minimum-term "$((baseline_term + 1))" \
  --commit-index 4 --revision 4 --policy weighted-round-robin --timeout 10 \
  >"$results_dir/healed-cluster.json"
python3 benchmarks/raft_partition_probe.py wait-state \
  --data-root "$proof_tmp" --commit-index 4 --policy weighted-round-robin --timeout 10 \
  >"$results_dir/healed-state.json"
python3 benchmarks/raft_partition_probe.py capture-links \
  --links "$link_urls" >"$results_dir/healed-links.json"

start_worker
start_gateway
capture_process_snapshot "$proof_tmp/initial-product-processes.json" \
  gateway "$gateway_pid" cpu-worker "$worker_pid"
python3 benchmarks/full_stack_probe.py wait-gateway \
  --gateway-url "$gateway_url" --policy weighted-round-robin \
  --revision 4 --worker-ids cpu-raft-partition --timeout 10 \
  >"$results_dir/gateway-ready.json"
python3 benchmarks/full_stack_probe.py requests \
  --gateway-url "$gateway_url" --requests 1 --prompt 'explain Raft quorum' \
  >"$results_dir/request.json"
python3 benchmarks/full_stack_probe.py stream \
  --gateway-url "$gateway_url" --prompt 'explain Raft repair' --speculative-tokens 3 \
  >"$results_dir/stream.json"

python3 benchmarks/raft_partition_probe.py capture-events \
  --events "$link_events" >"$results_dir/link-events.json"
record_process_continuity

# Sanitize before any retained report consumes the evidence.
python3 benchmarks/raft_partition_probe.py sanitize-evidence \
  --evidence-dir "$results_dir" --proof-root "$proof_tmp" --project-root "$project_root" \
  >"$proof_tmp/sanitizer.json"
mv "$proof_tmp/sanitizer.json" "$results_dir/sanitizer.json"

# Preliminary scan -> checker/render -> retained scan -> checker/render again.
# The last discarded scan covers the final assertions/SVG without changing an
# input that the checker itself consumes.
scan_private_material "$proof_tmp/private-preliminary.json"
mv "$proof_tmp/private-preliminary.json" "$results_dir/private-material-scan.json"
python3 benchmarks/check_raft_partition.py \
  --evidence-dir "$results_dir" --output "$results_dir/assertions.json"
python3 benchmarks/render_raft_partition_svg.py \
  --evidence-dir "$results_dir" --output "$results_dir/raft-partition-proof.svg"
scan_private_material "$proof_tmp/private-retained.json"
mv "$proof_tmp/private-retained.json" "$results_dir/private-material-scan.json"
python3 benchmarks/check_raft_partition.py \
  --evidence-dir "$results_dir" --output "$results_dir/assertions.json"
python3 benchmarks/render_raft_partition_svg.py \
  --evidence-dir "$results_dir" --output "$results_dir/raft-partition-proof.svg"
scan_private_material "$proof_tmp/private-final-discarded.json"
final_leak_scan

expected_files=(
  assertions.json
  baseline-cluster.json
  baseline-links.json
  baseline-state.json
  baseline-write.json
  figure-eight-test.json
  figure-eight.json
  gateway-ready.json
  healed-cluster.json
  healed-links.json
  healed-state.json
  healing-transition.json
  isolated-write.json
  link-events.json
  majority-cluster.json
  majority-election.json
  majority-write.json
  manifest.json
  partition-cluster.json
  partition-links.json
  partition-state.json
  partition-transition.json
  private-material-scan.json
  process-continuity.json
  raft-partition-proof.svg
  request.json
  sanitizer.json
  stream.json
)
write_manifest "${expected_files[@]}"

if [[ -n "${INFERLAB_V25_OUTPUT_DIR:-}" ]]; then
  for name in "${expected_files[@]}"; do
    if [[ "$name" != 'manifest.json' ]]; then
      cp "$results_dir/$name" "$INFERLAB_V25_OUTPUT_DIR/$name"
    fi
  done
  python3 - "$results_dir/manifest.json" "$INFERLAB_V25_OUTPUT_DIR" <<'PY'
import hashlib
import json
import sys
from pathlib import Path

manifest = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
destination = Path(sys.argv[2])
expected = sorted(name for name in manifest["expected_files"] if name != "manifest.json")
observed = sorted(path.name for path in destination.iterdir())
if observed != expected:
    raise SystemExit(f"partial retained evidence set before manifest publication: {observed}")
for item in manifest["files"]:
    content = (destination / item["path"]).read_bytes()
    if len(content) != item["bytes"] or hashlib.sha256(content).hexdigest() != item["sha256"]:
        raise SystemExit(f"retained evidence hash mismatch: {item['path']}")
PY
  # The manifest is the completion marker and is published only after every
  # evidence file has been copied and its exact hash verified.
  cp "$results_dir/manifest.json" "$INFERLAB_V25_OUTPUT_DIR/manifest.json"
fi

python3 - "$results_dir/assertions.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    assertions = json.load(source)
print(f"v0.25 exact-process proof complete: {assertions['passed']}/{assertions['total']} assertions passed")
PY
