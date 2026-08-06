#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

if [[ -x "$project_root/.tools/cargo/bin/cargo" ]]; then
  export RUSTUP_HOME="$project_root/.tools/rustup"
  export CARGO_HOME="$project_root/.tools/cargo"
  export PATH="$CARGO_HOME/bin:$PATH"
fi

proof_tmp="$(mktemp -d)"
results_dir="$proof_tmp/results"
snapshot_path="$proof_tmp/gateway-routing.json"
mkdir -p "$results_dir"
live_pids=(0)
started_pid=0
gateway_pid=0
node_a_pid=0
node_b_pid=0
node_c_pid=0
lease_ms=700
snapshot_max_age_ms=60000

register_pid() {
  live_pids+=("$1")
}

unregister_pid() {
  local target="$1"
  local retained=()
  local pid
  for pid in "${live_pids[@]}"; do
    if [[ "$pid" != "$target" ]]; then
      retained+=("$pid")
    fi
  done
  live_pids=("${retained[@]}")
}

is_owned_child() {
  local pid="$1"
  local parent
  [[ "$pid" =~ ^[1-9][0-9]*$ ]] || return 1
  parent="$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d '[:space:]')"
  [[ "$parent" == "$$" ]]
}

stop_owned_process() {
  local pid="$1"
  local label="$2"
  if ! is_owned_child "$pid"; then
    echo "refusing to stop $label: PID $pid is not a live child of this harness" >&2
    return 1
  fi
  kill "$pid"
  wait "$pid" 2>/dev/null || true
  unregister_pid "$pid"
}

cleanup() {
  local pid
  for pid in "${live_pids[@]}"; do
    if is_owned_child "$pid"; then
      kill "$pid" 2>/dev/null || true
    fi
  done
  for pid in "${live_pids[@]}"; do
    wait "$pid" 2>/dev/null || true
  done
  if [[ -n "${INFERLAB_V16_OUTPUT_DIR:-}" && -d "$results_dir" ]]; then
    mkdir -p "$INFERLAB_V16_OUTPUT_DIR"
    cp "$results_dir"/*.json "$INFERLAB_V16_OUTPUT_DIR/" 2>/dev/null || true
    cp "$results_dir"/*.svg "$INFERLAB_V16_OUTPUT_DIR/" 2>/dev/null || true
  fi
  rm -rf "$proof_tmp"
}
trap cleanup EXIT INT TERM

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
            raise SystemExit(
                f"refusing to start v0.16 proof: 127.0.0.1:{port} is busy: {error}"
            )
PY
}

wait_for_health() {
  local url="$1"
  local attempts=0
  until curl --fail --silent "$url" >/dev/null; do
    attempts=$((attempts + 1))
    if ((attempts >= 300)); then
      echo "timed out waiting for $url" >&2
      return 1
    fi
    sleep 0.025
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

record_process_event() {
  local output="$1"
  local event="$2"
  local target="$3"
  local pid="$4"
  python3 - "$output" "$event" "$target" "$pid" <<'PY'
import json
import sys
import time

output, event, target, pid = sys.argv[1:]
record = {
    "schema": "inferlab.runtime-routing-lease-event.v0.16",
    "at_ms": round(time.time() * 1000, 3),
    "event": event,
    "target": target,
    "pid": int(pid),
    "scope": "owned-child-process",
}
with open(output, "w", encoding="utf-8") as destination:
    json.dump(record, destination, indent=2, sort_keys=True)
    destination.write("\n")
PY
}

record_control_event() {
  local output="$1"
  local event="$2"
  python3 - "$output" "$event" "$node_a_pid" "$node_b_pid" "$node_c_pid" <<'PY'
import json
import sys
import time

output, event, *pids = sys.argv[1:]
record = {
    "schema": "inferlab.runtime-routing-lease-control-event.v0.16",
    "at_ms": round(time.time() * 1000, 3),
    "event": event,
    "scope": "owned-child-processes",
    "targets": ["node-a", "node-b", "node-c"],
    "pids": [int(pid) for pid in pids],
}
with open(output, "w", encoding="utf-8") as destination:
    json.dump(record, destination, indent=2, sort_keys=True)
    destination.write("\n")
PY
}

stop_control_cluster() {
  local output="$1"
  local event="$2"
  record_control_event "$output" "$event"
  stop_owned_process "$node_a_pid" node-a
  stop_owned_process "$node_b_pid" node-b
  stop_owned_process "$node_c_pid" node-c
}

start_node() {
  local node_id="$1"
  local port="$2"
  local peers="$3"
  local election_min_ms="$4"
  local election_max_ms="$5"
  local data_dir="$6"
  local log_name="$7"
  mkdir -p "$data_dir"
  INFERLAB_RAFT_NODE_ID="$node_id" \
  INFERLAB_RAFT_BIND="127.0.0.1:$port" \
  INFERLAB_RAFT_PEERS="$peers" \
  INFERLAB_RAFT_DATA_DIR="$data_dir" \
  INFERLAB_RAFT_ELECTION_MIN_MS="$election_min_ms" \
  INFERLAB_RAFT_ELECTION_MAX_MS="$election_max_ms" \
  INFERLAB_RAFT_HEARTBEAT_MS=50 \
  INFERLAB_RAFT_RPC_TIMEOUT_MS=100 \
  INFERLAB_RAFT_COMMIT_TIMEOUT_MS=1500 \
    target/debug/control-plane >"$proof_tmp/$log_name.log" 2>&1 &
  started_pid="$!"
  register_pid "$started_pid"
  wait_for_health "http://127.0.0.1:$port/healthz"
}

start_nodes() {
  local log_prefix="$1"
  start_node node-a 9881 \
    'node-b=http://127.0.0.1:9882,node-c=http://127.0.0.1:9883' \
    180 240 "$proof_tmp/node-a" "$log_prefix-node-a"
  node_a_pid="$started_pid"
  start_node node-b 9882 \
    'node-a=http://127.0.0.1:9881,node-c=http://127.0.0.1:9883' \
    300 360 "$proof_tmp/node-b" "$log_prefix-node-b"
  node_b_pid="$started_pid"
  start_node node-c 9883 \
    'node-a=http://127.0.0.1:9881,node-b=http://127.0.0.1:9882' \
    420 480 "$proof_tmp/node-c" "$log_prefix-node-c"
  node_c_pid="$started_pid"
}

start_worker() {
  INFERLAB_CPU_WORKER_ID=cpu-lease \
  INFERLAB_CPU_BIND=127.0.0.1:9884 \
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
  INFERLAB_CPU_BATCH_TICK_MS=200 \
    target/debug/cpu-worker >"$proof_tmp/cpu-lease.log" 2>&1 &
  started_pid="$!"
  register_pid "$started_pid"
  wait_for_health 'http://127.0.0.1:9884/health'
}

start_gateway() {
  local action="$1"
  local log_name="$2"
  INFERLAB_BIND=127.0.0.1:9880 \
  INFERLAB_CONTROL_PLANE_URLS="$urls" \
  INFERLAB_CONTROL_BOOTSTRAP_WAIT_MS=150 \
  INFERLAB_CONTROL_POLL_MS=25 \
  INFERLAB_ROUTING_SNAPSHOT_PATH="$snapshot_path" \
  INFERLAB_ROUTING_SNAPSHOT_MAX_AGE_MS="$snapshot_max_age_ms" \
  INFERLAB_ROUTING_SNAPSHOT_MAX_FUTURE_SKEW_MS=100 \
  INFERLAB_ROUTING_LEASE_MS="$lease_ms" \
  INFERLAB_ROUTING_LEASE_EXPIRY_ACTION="$action" \
  INFERLAB_WORKER_CONCURRENCY=4 \
  INFERLAB_ADMISSION_QUEUE_CAPACITY=8 \
  INFERLAB_REQUEST_DEADLINE_MS=10000 \
  INFERLAB_ATTEMPT_TIMEOUT_MS=10000 \
  INFERLAB_MAX_RETRIES=0 \
    target/debug/gateway >"$proof_tmp/$log_name.log" 2>&1 &
  gateway_pid="$!"
  register_pid "$gateway_pid"
  wait_for_health "$gateway_url/health"
}

wait_for_worker_requests() {
  local minimum="$1"
  python3 - "$minimum" <<'PY'
import json
import sys
import time
import urllib.request

minimum = int(sys.argv[1])
for _ in range(200):
    with urllib.request.urlopen("http://127.0.0.1:9884/health", timeout=1) as response:
        requests = json.load(response)["requests"]
    if requests >= minimum:
        raise SystemExit(0)
    time.sleep(0.025)
raise SystemExit(f"worker never reached {minimum} requests")
PY
}

record_snapshot_directory() {
  python3 - "$proof_tmp" "$results_dir/snapshot-directory.json" <<'PY'
import json
import sys
from pathlib import Path

directory = Path(sys.argv[1])
entries = sorted(path.name for path in directory.iterdir())
result = {
    "schema": "inferlab.runtime-routing-lease-directory.v0.16",
    "entries": entries,
    "temporary_snapshot_files": [
        name for name in entries if name.startswith(".gateway-routing.json")
    ],
}
Path(sys.argv[2]).write_text(json.dumps(result, indent=2) + "\n")
PY
}

urls='http://127.0.0.1:9881,http://127.0.0.1:9882,http://127.0.0.1:9883'
gateway_url='http://127.0.0.1:9880'
workers='cpu-lease=http://127.0.0.1:9884@1'

check_ports_are_free 9880 9881 9882 9883 9884

echo "Building the v0.16 runtime routing-lease gateway..."
cargo build --workspace --quiet

start_nodes initial
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" >"$results_dir/initial-election.json"
start_worker

python3 benchmarks/full_stack_probe.py write-config \
  --urls "$urls" \
  --policy round-robin \
  --workers "$workers" >"$results_dir/config-initial.json"
revision="$(json_field "$results_dir/config-initial.json" committed.revision)"

start_gateway reject-new gateway-reject-new
python3 benchmarks/runtime_lease_probe.py wait-state \
  --gateway-url "$gateway_url" \
  --state fresh \
  --revision "$revision" \
  --minimum-renewals 1 >"$results_dir/lease-live-fresh.json"
python3 benchmarks/runtime_lease_probe.py readiness \
  --gateway-url "$gateway_url" >"$results_dir/readiness-live.json"
python3 benchmarks/runtime_lease_probe.py request \
  --gateway-url "$gateway_url" \
  --prompt 'runtime lease live' >"$results_dir/request-live.json"

python3 benchmarks/full_stack_probe.py worker-health \
  --workers "$workers" >"$results_dir/worker-before-crossing-stream.json"
worker_requests="$(json_field "$results_dir/worker-before-crossing-stream.json" workers.cpu-lease.body.requests)"
python3 benchmarks/full_stack_probe.py stream \
  --gateway-url "$gateway_url" \
  --prompt 'teach me streaming' \
  --speculative-tokens 0 >"$results_dir/stream-crossing-expiry.json" &
stream_pid="$!"
register_pid "$stream_pid"
wait_for_worker_requests "$((worker_requests + 1))"

stop_control_cluster "$results_dir/control-outage.json" control_cluster_stopped
python3 benchmarks/runtime_lease_probe.py wait-state \
  --gateway-url "$gateway_url" \
  --state expired-rejecting-new \
  --revision "$revision" >"$results_dir/lease-expired-rejecting.json"
python3 benchmarks/runtime_lease_probe.py readiness \
  --gateway-url "$gateway_url" >"$results_dir/readiness-expired.json"
python3 benchmarks/full_stack_probe.py worker-health \
  --workers "$workers" >"$results_dir/worker-before-rejection.json"
python3 benchmarks/runtime_lease_probe.py request \
  --gateway-url "$gateway_url" \
  --prompt 'must not reach worker' >"$results_dir/request-rejected.json"
python3 benchmarks/full_stack_probe.py worker-health \
  --workers "$workers" >"$results_dir/worker-after-rejection.json"

wait "$stream_pid"
unregister_pid "$stream_pid"

start_nodes recovered
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" >"$results_dir/recovered-election.json"
python3 benchmarks/runtime_lease_probe.py wait-state \
  --gateway-url "$gateway_url" \
  --state fresh \
  --revision "$revision" >"$results_dir/lease-renewed.json"
python3 benchmarks/runtime_lease_probe.py readiness \
  --gateway-url "$gateway_url" >"$results_dir/readiness-renewed.json"
python3 benchmarks/runtime_lease_probe.py request \
  --gateway-url "$gateway_url" \
  --prompt 'same revision renewed' >"$results_dir/request-renewed.json"

record_process_event "$results_dir/gateway-reject-stop.json" gateway_stopped gateway "$gateway_pid"
stop_owned_process "$gateway_pid" gateway
stop_control_cluster "$results_dir/control-second-outage.json" control_cluster_stopped

start_gateway serve-stale gateway-serve-stale
python3 benchmarks/runtime_lease_probe.py wait-state \
  --gateway-url "$gateway_url" \
  --state expired-serving-stale \
  --revision "$revision" >"$results_dir/lease-expired-serving-stale.json"
python3 benchmarks/runtime_lease_probe.py readiness \
  --gateway-url "$gateway_url" >"$results_dir/readiness-serving-stale.json"
python3 benchmarks/full_stack_probe.py worker-health \
  --workers "$workers" >"$results_dir/worker-before-stale-request.json"
python3 benchmarks/runtime_lease_probe.py request \
  --gateway-url "$gateway_url" \
  --prompt 'operator chose availability' >"$results_dir/request-serving-stale.json"
python3 benchmarks/full_stack_probe.py worker-health \
  --workers "$workers" >"$results_dir/worker-after-stale-request.json"
python3 benchmarks/full_stack_probe.py stream \
  --gateway-url "$gateway_url" \
  --prompt 'teach me streaming' \
  --speculative-tokens 3 >"$results_dir/stream-final.json"

record_process_event "$results_dir/gateway-final-stop.json" gateway_stopped gateway "$gateway_pid"
stop_owned_process "$gateway_pid" gateway
record_snapshot_directory

python3 benchmarks/check_runtime_lease.py \
  --evidence-dir "$results_dir" \
  --output "$results_dir/runtime-routing-lease-check.json"
python3 benchmarks/render_runtime_lease_svg.py \
  --evidence-dir "$results_dir" \
  --check "$results_dir/runtime-routing-lease-check.json" \
  --output "$results_dir/runtime-routing-lease-proof.svg"

if [[ -n "${INFERLAB_V16_OUTPUT_DIR:-}" ]]; then
  mkdir -p "$INFERLAB_V16_OUTPUT_DIR"
  cp "$results_dir"/*.json "$INFERLAB_V16_OUTPUT_DIR/"
  cp "$results_dir/runtime-routing-lease-proof.svg" "$INFERLAB_V16_OUTPUT_DIR/"
  echo "Retained evidence in $INFERLAB_V16_OUTPUT_DIR"
fi

echo "v0.16 runtime routing lease proof passed"
