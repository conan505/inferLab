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
primary_snapshot="$proof_tmp/primary-routing.json"
foreign_snapshot="$proof_tmp/foreign-routing.json"
mkdir -p "$results_dir"
live_pids=(0)
started_pid=0
gateway_pid=0
node_a_pid=0
node_b_pid=0
node_c_pid=0
lease_ms=700
primary_cluster='inferlab-primary'
foreign_cluster='inferlab-foreign'

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
  if [[ -n "${INFERLAB_V17_OUTPUT_DIR:-}" && -d "$results_dir" ]]; then
    mkdir -p "$INFERLAB_V17_OUTPUT_DIR"
    cp "$results_dir"/*.json "$INFERLAB_V17_OUTPUT_DIR/" 2>/dev/null || true
    cp "$results_dir"/*.svg "$INFERLAB_V17_OUTPUT_DIR/" 2>/dev/null || true
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
                f"refusing to start v0.17 proof: 127.0.0.1:{port} is busy: {error}"
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
    "schema": "inferlab.control-cluster-identity-event.v0.17",
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
  local cluster_id="$3"
  python3 - "$output" "$event" "$cluster_id" "$node_a_pid" "$node_b_pid" "$node_c_pid" <<'PY'
import json
import sys
import time

output, event, cluster_id, *pids = sys.argv[1:]
record = {
    "schema": "inferlab.control-cluster-identity-control-event.v0.17",
    "at_ms": round(time.time() * 1000, 3),
    "event": event,
    "cluster_id": cluster_id,
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
  local cluster_id="$3"
  record_control_event "$output" "$event" "$cluster_id"
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
  local cluster_id="$8"
  mkdir -p "$data_dir"
  INFERLAB_RAFT_NODE_ID="$node_id" \
  INFERLAB_RAFT_CLUSTER_ID="$cluster_id" \
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
  local cluster_id="$1"
  local directory_prefix="$2"
  local log_prefix="$3"
  start_node node-a 9891 \
    'node-b=http://127.0.0.1:9892,node-c=http://127.0.0.1:9893' \
    180 240 "$proof_tmp/$directory_prefix-node-a" "$log_prefix-node-a" "$cluster_id"
  node_a_pid="$started_pid"
  start_node node-b 9892 \
    'node-a=http://127.0.0.1:9891,node-c=http://127.0.0.1:9893' \
    300 360 "$proof_tmp/$directory_prefix-node-b" "$log_prefix-node-b" "$cluster_id"
  node_b_pid="$started_pid"
  start_node node-c 9893 \
    'node-a=http://127.0.0.1:9891,node-b=http://127.0.0.1:9892' \
    420 480 "$proof_tmp/$directory_prefix-node-c" "$log_prefix-node-c" "$cluster_id"
  node_c_pid="$started_pid"
}

start_worker() {
  local worker_id="$1"
  local port="$2"
  local tick_ms="$3"
  INFERLAB_CPU_WORKER_ID="$worker_id" \
  INFERLAB_CPU_BIND="127.0.0.1:$port" \
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
  INFERLAB_CPU_BATCH_TICK_MS="$tick_ms" \
    target/debug/cpu-worker >"$proof_tmp/$worker_id.log" 2>&1 &
  started_pid="$!"
  register_pid "$started_pid"
  wait_for_health "http://127.0.0.1:$port/health"
}

start_gateway() {
  local snapshot_path="$1"
  local log_name="$2"
  INFERLAB_BIND=127.0.0.1:9890 \
  INFERLAB_CONTROL_PLANE_URLS="$urls" \
  INFERLAB_CONTROL_CLUSTER_ID="$primary_cluster" \
  INFERLAB_CONTROL_BOOTSTRAP_WAIT_MS=150 \
  INFERLAB_CONTROL_POLL_MS=25 \
  INFERLAB_ROUTING_SNAPSHOT_PATH="$snapshot_path" \
  INFERLAB_ROUTING_SNAPSHOT_MAX_AGE_MS=60000 \
  INFERLAB_ROUTING_SNAPSHOT_MAX_FUTURE_SKEW_MS=100 \
  INFERLAB_ROUTING_LEASE_MS="$lease_ms" \
  INFERLAB_ROUTING_LEASE_EXPIRY_ACTION=reject-new \
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
  local port="$1"
  local minimum="$2"
  python3 - "$port" "$minimum" <<'PY'
import json
import sys
import time
import urllib.request

port, minimum = int(sys.argv[1]), int(sys.argv[2])
for _ in range(240):
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=1) as response:
        requests = json.load(response)["requests"]
    if requests >= minimum:
        raise SystemExit(0)
    time.sleep(0.025)
raise SystemExit(f"worker on {port} never reached {minimum} requests")
PY
}

mutate_snapshot_cluster() {
  python3 - "$primary_snapshot" "$foreign_snapshot" "$foreign_cluster" "$results_dir/foreign-snapshot-fixture.json" <<'PY'
import json
import sys
from pathlib import Path

source, destination, foreign_cluster, record_path = sys.argv[1:]
document = json.loads(Path(source).read_text())
original_cluster = document["cluster_id"]
document["cluster_id"] = foreign_cluster
Path(destination).write_text(json.dumps(document, indent=2) + "\n")
record = {
    "schema": "inferlab.control-cluster-identity-fixture.v0.17",
    "original_cluster_id": original_cluster,
    "mutated_cluster_id": foreign_cluster,
    "revision": document["revision"],
    "term": document["term"],
    "configuration": document["configuration"],
}
Path(record_path).write_text(json.dumps(record, indent=2) + "\n")
PY
}

run_rejected_bootstrap() {
  local log_path="$proof_tmp/foreign-disk-rejection.log"
  set +e
  INFERLAB_BIND=127.0.0.1:9896 \
  INFERLAB_CONTROL_PLANE_URLS="$urls" \
  INFERLAB_CONTROL_CLUSTER_ID="$primary_cluster" \
  INFERLAB_CONTROL_BOOTSTRAP_WAIT_MS=100 \
  INFERLAB_ROUTING_SNAPSHOT_PATH="$foreign_snapshot" \
  INFERLAB_ROUTING_SNAPSHOT_MAX_AGE_MS=60000 \
    target/debug/gateway >"$log_path" 2>&1
  local exit_code="$?"
  set -e
  python3 - "$exit_code" "$log_path" "$results_dir/foreign-disk-bootstrap-rejected.json" <<'PY'
import json
import sys
from pathlib import Path

exit_code, log_path, output = sys.argv[1:]
result = {
    "schema": "inferlab.control-cluster-identity-bootstrap-rejection.v0.17",
    "exit_code": int(exit_code),
    "log": Path(log_path).read_text(),
}
Path(output).write_text(json.dumps(result, indent=2) + "\n")
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
    "schema": "inferlab.control-cluster-identity-directory.v0.17",
    "entries": entries,
    "temporary_snapshot_files": [
        name for name in entries
        if name.startswith(".primary-routing.json") or name.startswith(".foreign-routing.json")
    ],
}
Path(sys.argv[2]).write_text(json.dumps(result, indent=2) + "\n")
PY
}

urls='http://127.0.0.1:9891,http://127.0.0.1:9892,http://127.0.0.1:9893'
gateway_url='http://127.0.0.1:9890'
primary_workers='cpu-primary=http://127.0.0.1:9894@1'
foreign_workers='cpu-foreign=http://127.0.0.1:9895@1'

check_ports_are_free 9890 9891 9892 9893 9894 9895 9896

echo "Building the v0.17 control-cluster identity fence..."
cargo build --workspace --quiet
start_worker cpu-primary 9894 250
start_worker cpu-foreign 9895 0

start_nodes "$primary_cluster" primary initial-primary
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" >"$results_dir/initial-primary-election.json"
python3 benchmarks/full_stack_probe.py write-config \
  --urls "$urls" \
  --policy round-robin \
  --workers "$primary_workers" >"$results_dir/config-primary.json"
revision="$(json_field "$results_dir/config-primary.json" committed.revision)"

start_gateway "$primary_snapshot" gateway-primary
python3 benchmarks/runtime_lease_probe.py wait-state \
  --gateway-url "$gateway_url" \
  --state fresh \
  --revision "$revision" \
  --minimum-renewals 1 >"$results_dir/gateway-primary-fresh.json"
cp "$primary_snapshot" "$results_dir/snapshot-primary.json"
python3 benchmarks/runtime_lease_probe.py request \
  --gateway-url "$gateway_url" \
  --prompt 'primary cluster identity' >"$results_dir/request-primary.json"

python3 benchmarks/full_stack_probe.py worker-health \
  --workers "$primary_workers" >"$results_dir/worker-primary-before-stream.json"
primary_requests="$(json_field "$results_dir/worker-primary-before-stream.json" workers.cpu-primary.body.requests)"
python3 benchmarks/full_stack_probe.py stream \
  --gateway-url "$gateway_url" \
  --prompt 'teach me streaming' \
  --speculative-tokens 0 >"$results_dir/stream-crossing-foreign-cluster.json" &
stream_pid="$!"
register_pid "$stream_pid"
wait_for_worker_requests 9894 "$((primary_requests + 1))"

stop_control_cluster "$results_dir/primary-control-outage.json" primary_cluster_stopped "$primary_cluster"
start_nodes "$foreign_cluster" foreign foreign
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" >"$results_dir/foreign-election.json"
python3 benchmarks/full_stack_probe.py write-config \
  --urls "$urls" \
  --policy round-robin \
  --workers "$foreign_workers" >"$results_dir/config-foreign.json"
python3 benchmarks/runtime_lease_probe.py wait-state \
  --gateway-url "$gateway_url" \
  --state expired-rejecting-new \
  --revision "$revision" >"$results_dir/gateway-foreign-rejected.json"
python3 benchmarks/runtime_lease_probe.py readiness \
  --gateway-url "$gateway_url" >"$results_dir/readiness-foreign-rejected.json"
python3 benchmarks/full_stack_probe.py worker-health \
  --workers "$primary_workers" >"$results_dir/worker-primary-before-rejection.json"
python3 benchmarks/full_stack_probe.py worker-health \
  --workers "$foreign_workers" >"$results_dir/worker-foreign-before-rejection.json"
python3 benchmarks/runtime_lease_probe.py request \
  --gateway-url "$gateway_url" \
  --prompt 'foreign cluster must not route' >"$results_dir/request-foreign-rejected.json"
python3 benchmarks/full_stack_probe.py worker-health \
  --workers "$primary_workers" >"$results_dir/worker-primary-after-rejection.json"
python3 benchmarks/full_stack_probe.py worker-health \
  --workers "$foreign_workers" >"$results_dir/worker-foreign-after-rejection.json"

wait "$stream_pid"
unregister_pid "$stream_pid"

stop_control_cluster "$results_dir/foreign-control-stop.json" foreign_cluster_stopped "$foreign_cluster"
start_nodes "$primary_cluster" primary recovered-primary
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" >"$results_dir/recovered-primary-election.json"
python3 benchmarks/runtime_lease_probe.py wait-state \
  --gateway-url "$gateway_url" \
  --state fresh \
  --revision "$revision" >"$results_dir/gateway-primary-renewed.json"
python3 benchmarks/runtime_lease_probe.py readiness \
  --gateway-url "$gateway_url" >"$results_dir/readiness-primary-renewed.json"
python3 benchmarks/runtime_lease_probe.py request \
  --gateway-url "$gateway_url" \
  --prompt 'primary cluster restored' >"$results_dir/request-primary-renewed.json"

record_process_event "$results_dir/gateway-primary-stop.json" gateway_stopped gateway "$gateway_pid"
stop_owned_process "$gateway_pid" gateway
stop_control_cluster "$results_dir/primary-control-second-stop.json" primary_cluster_stopped "$primary_cluster"

mutate_snapshot_cluster
run_rejected_bootstrap

start_nodes "$primary_cluster" primary second-recovered-primary
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" >"$results_dir/second-recovered-primary-election.json"
start_gateway "$foreign_snapshot" gateway-live-repair
python3 benchmarks/runtime_lease_probe.py wait-state \
  --gateway-url "$gateway_url" \
  --state fresh \
  --revision "$revision" >"$results_dir/gateway-live-repair.json"
cp "$foreign_snapshot" "$results_dir/snapshot-live-repaired.json"
python3 benchmarks/runtime_lease_probe.py request \
  --gateway-url "$gateway_url" \
  --prompt 'live primary repairs disk identity' >"$results_dir/request-live-repair.json"
python3 benchmarks/full_stack_probe.py stream \
  --gateway-url "$gateway_url" \
  --prompt 'teach me streaming' \
  --speculative-tokens 3 >"$results_dir/stream-final.json"

record_process_event "$results_dir/gateway-final-stop.json" gateway_stopped gateway "$gateway_pid"
stop_owned_process "$gateway_pid" gateway
stop_control_cluster "$results_dir/primary-control-final-stop.json" primary_cluster_stopped "$primary_cluster"
record_snapshot_directory

python3 benchmarks/check_cluster_identity.py \
  --evidence-dir "$results_dir" \
  --output "$results_dir/control-cluster-identity-check.json"
python3 benchmarks/render_cluster_identity_svg.py \
  --evidence-dir "$results_dir" \
  --check "$results_dir/control-cluster-identity-check.json" \
  --output "$results_dir/control-cluster-identity-proof.svg"

if [[ -n "${INFERLAB_V17_OUTPUT_DIR:-}" ]]; then
  mkdir -p "$INFERLAB_V17_OUTPUT_DIR"
  cp "$results_dir"/*.json "$INFERLAB_V17_OUTPUT_DIR/"
  cp "$results_dir/control-cluster-identity-proof.svg" "$INFERLAB_V17_OUTPUT_DIR/"
  echo "Retained evidence in $INFERLAB_V17_OUTPUT_DIR"
fi

echo "v0.17 control-cluster identity proof passed"
