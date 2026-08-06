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
gateway_started_ms=0
node_a_pid=0
node_b_pid=0
node_c_pid=0
max_age_ms=5000
future_skew_ms=100

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
  if [[ -n "${INFERLAB_V15_OUTPUT_DIR:-}" && -d "$results_dir" ]]; then
    mkdir -p "$INFERLAB_V15_OUTPUT_DIR"
    cp "$results_dir"/*.json "$INFERLAB_V15_OUTPUT_DIR/" 2>/dev/null || true
    cp "$results_dir"/*.svg "$INFERLAB_V15_OUTPUT_DIR/" 2>/dev/null || true
  fi
  rm -rf "$proof_tmp"
}
trap cleanup EXIT INT TERM

now_ms() {
  python3 - <<'PY'
import time
print(round(time.time() * 1000, 3))
PY
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
            raise SystemExit(
                f"refusing to start v0.15 proof: 127.0.0.1:{port} is busy: {error}"
            )
PY
}

wait_for_health() {
  local url="$1"
  local attempts=0
  until curl --fail --silent "$url" >/dev/null; do
    attempts=$((attempts + 1))
    if ((attempts >= 200)); then
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
    "schema": "inferlab.snapshot-freshness-event.v0.15",
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
    "schema": "inferlab.snapshot-freshness-control-event.v0.15",
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
  start_node node-a 9871 \
    'node-b=http://127.0.0.1:9872,node-c=http://127.0.0.1:9873' \
    180 240 "$proof_tmp/live-node-a" "$log_prefix-node-a"
  node_a_pid="$started_pid"
  start_node node-b 9872 \
    'node-a=http://127.0.0.1:9871,node-c=http://127.0.0.1:9873' \
    300 360 "$proof_tmp/live-node-b" "$log_prefix-node-b"
  node_b_pid="$started_pid"
  start_node node-c 9873 \
    'node-a=http://127.0.0.1:9871,node-b=http://127.0.0.1:9872' \
    420 480 "$proof_tmp/live-node-c" "$log_prefix-node-c"
  node_c_pid="$started_pid"
}

start_worker() {
  local worker_id="$1"
  local port="$2"
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
  INFERLAB_CPU_BATCH_TICK_MS=2 \
    target/debug/cpu-worker >"$proof_tmp/$worker_id.log" 2>&1 &
  started_pid="$!"
  register_pid "$started_pid"
  wait_for_health "http://127.0.0.1:$port/health"
}

start_gateway() {
  local log_name="$1"
  gateway_started_ms="$(now_ms)"
  INFERLAB_BIND=127.0.0.1:9870 \
  INFERLAB_CONTROL_PLANE_URLS="$urls" \
  INFERLAB_CONTROL_BOOTSTRAP_WAIT_MS=150 \
  INFERLAB_CONTROL_POLL_MS=25 \
  INFERLAB_ROUTING_SNAPSHOT_PATH="$snapshot_path" \
  INFERLAB_ROUTING_SNAPSHOT_MAX_AGE_MS="$max_age_ms" \
  INFERLAB_ROUTING_SNAPSHOT_MAX_FUTURE_SKEW_MS="$future_skew_ms" \
  INFERLAB_WORKER_CONCURRENCY=4 \
  INFERLAB_ADMISSION_QUEUE_CAPACITY=8 \
  INFERLAB_REQUEST_DEADLINE_MS=5000 \
  INFERLAB_ATTEMPT_TIMEOUT_MS=1000 \
  INFERLAB_MAX_RETRIES=2 \
  INFERLAB_RETRY_BUDGET_PERCENT=100 \
  INFERLAB_RETRY_BASE_DELAY_MS=1 \
  INFERLAB_RETRY_MAX_DELAY_MS=2 \
    target/debug/gateway >"$proof_tmp/$log_name.log" 2>&1 &
  gateway_pid="$!"
  register_pid "$gateway_pid"
  wait_for_health "$gateway_url/health"
}

run_rejected_bootstrap() {
  local fixture="$1"
  local output="$2"
  local log_path="$proof_tmp/$fixture-gateway.log"
  set +e
  INFERLAB_BIND=127.0.0.1:9876 \
  INFERLAB_CONTROL_PLANE_URLS="$urls" \
  INFERLAB_CONTROL_BOOTSTRAP_WAIT_MS=100 \
  INFERLAB_ROUTING_SNAPSHOT_PATH="$snapshot_path" \
  INFERLAB_ROUTING_SNAPSHOT_MAX_AGE_MS="$max_age_ms" \
  INFERLAB_ROUTING_SNAPSHOT_MAX_FUTURE_SKEW_MS="$future_skew_ms" \
    target/debug/gateway >"$log_path" 2>&1
  local exit_code="$?"
  set -e
  python3 - "$fixture" "$exit_code" "$log_path" "$output" <<'PY'
import json
import sys
from pathlib import Path

fixture, exit_code, log_path, output = sys.argv[1:]
result = {
    "schema": "inferlab.snapshot-freshness-rejection.v0.15",
    "fixture": fixture,
    "exit_code": int(exit_code),
    "log": Path(log_path).read_text(),
}
Path(output).write_text(json.dumps(result, indent=2) + "\n")
PY
}

mutate_snapshot_time() {
  local mode="$1"
  local output="$2"
  python3 - "$snapshot_path" "$mode" "$max_age_ms" "$future_skew_ms" "$output" <<'PY'
import json
import sys
import time
from pathlib import Path

snapshot_path, mode, max_age, future_skew, output = sys.argv[1:]
path = Path(snapshot_path)
document = json.loads(path.read_text())
now_ms = round(time.time() * 1000)
maximum_age_ms = int(max_age)
maximum_future_skew_ms = int(future_skew)
if mode == "expired":
    document["saved_at_ms"] = now_ms - maximum_age_ms - 1000
elif mode == "future":
    document["saved_at_ms"] = now_ms + maximum_future_skew_ms + 5000
else:
    raise SystemExit(f"unknown fixture mode: {mode}")
path.write_text(json.dumps(document, indent=2) + "\n")
record = {
    "schema": "inferlab.snapshot-freshness-fixture.v0.15",
    "mode": mode,
    "observed_at_ms": now_ms,
    "saved_at_ms": document["saved_at_ms"],
    "observed_age_ms": max(now_ms - document["saved_at_ms"], 0),
    "future_delta_ms": max(document["saved_at_ms"] - now_ms, 0),
    "maximum_age_ms": maximum_age_ms,
    "maximum_future_skew_ms": maximum_future_skew_ms,
    "revision": document["revision"],
}
Path(output).write_text(json.dumps(record, indent=2) + "\n")
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
    "schema": "inferlab.snapshot-freshness-directory.v0.15",
    "entries": entries,
    "temporary_snapshot_files": [
        name for name in entries if name.startswith(".gateway-routing.json")
    ],
}
Path(sys.argv[2]).write_text(json.dumps(result, indent=2) + "\n")
PY
}

urls='http://127.0.0.1:9871,http://127.0.0.1:9872,http://127.0.0.1:9873'
gateway_url='http://127.0.0.1:9870'
workers='cpu-fresh-a=http://127.0.0.1:9874@1,cpu-fresh-b=http://127.0.0.1:9875@1'

check_ports_are_free 9870 9871 9872 9873 9874 9875 9876

echo "Building the v0.15 bounded-age snapshot gateway..."
cargo build --workspace --quiet

start_nodes initial
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" >"$results_dir/initial-election.json"

start_worker cpu-fresh-a 9874
start_worker cpu-fresh-b 9875

python3 benchmarks/full_stack_probe.py write-config \
  --urls "$urls" \
  --policy round-robin \
  --workers "$workers" >"$results_dir/config-initial.json"
initial_revision="$(json_field "$results_dir/config-initial.json" committed.revision)"

start_gateway gateway-live
python3 benchmarks/gateway_restart_probe.py wait-status \
  --gateway-url "$gateway_url" \
  --revision "$initial_revision" \
  --policy round-robin \
  --worker-ids cpu-fresh-a,cpu-fresh-b \
  --bootstrap-source live-control-plane \
  --persisted-revision "$initial_revision" \
  --snapshot-max-age-ms "$max_age_ms" \
  --snapshot-max-future-skew-ms "$future_skew_ms" \
  --started-at-ms "$gateway_started_ms" >"$results_dir/gateway-live.json"
cp "$snapshot_path" "$results_dir/snapshot-initial.json"
python3 benchmarks/full_stack_probe.py requests \
  --gateway-url "$gateway_url" \
  --requests 2 \
  --prompt 'freshness live ' >"$results_dir/requests-live.json"

record_process_event "$results_dir/gateway-first-stop.json" gateway_stopped gateway "$gateway_pid"
stop_owned_process "$gateway_pid" gateway
stop_control_cluster "$results_dir/control-first-outage.json" control_cluster_stopped

start_gateway gateway-fresh-disk
python3 benchmarks/gateway_restart_probe.py wait-status \
  --gateway-url "$gateway_url" \
  --revision "$initial_revision" \
  --policy round-robin \
  --worker-ids cpu-fresh-a,cpu-fresh-b \
  --bootstrap-source disk-snapshot \
  --persisted-revision "$initial_revision" \
  --snapshot-max-age-ms "$max_age_ms" \
  --snapshot-max-future-skew-ms "$future_skew_ms" \
  --require-bootstrap-snapshot-age \
  --started-at-ms "$gateway_started_ms" >"$results_dir/gateway-fresh-disk.json"
python3 benchmarks/full_stack_probe.py requests \
  --gateway-url "$gateway_url" \
  --requests 3 \
  --prompt 'fresh disk ' >"$results_dir/requests-fresh-disk.json"

record_process_event "$results_dir/gateway-second-stop.json" gateway_stopped gateway "$gateway_pid"
stop_owned_process "$gateway_pid" gateway

mutate_snapshot_time expired "$results_dir/expired-fixture.json"
run_rejected_bootstrap expired "$results_dir/expired-bootstrap.json"

mutate_snapshot_time future "$results_dir/future-fixture.json"
run_rejected_bootstrap future "$results_dir/future-bootstrap.json"

start_nodes recovered
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" >"$results_dir/recovered-election.json"

start_gateway gateway-live-repair
python3 benchmarks/gateway_restart_probe.py wait-status \
  --gateway-url "$gateway_url" \
  --revision "$initial_revision" \
  --policy round-robin \
  --worker-ids cpu-fresh-a,cpu-fresh-b \
  --bootstrap-source live-control-plane \
  --persisted-revision "$initial_revision" \
  --snapshot-max-age-ms "$max_age_ms" \
  --snapshot-max-future-skew-ms "$future_skew_ms" \
  --started-at-ms "$gateway_started_ms" >"$results_dir/gateway-live-repair.json"
cp "$snapshot_path" "$results_dir/snapshot-repaired.json"
python3 benchmarks/full_stack_probe.py requests \
  --gateway-url "$gateway_url" \
  --requests 2 \
  --prompt 'live repairs time ' >"$results_dir/requests-live-repair.json"
python3 benchmarks/full_stack_probe.py stream \
  --gateway-url "$gateway_url" \
  --prompt 'teach me streaming' \
  --speculative-tokens 3 >"$results_dir/stream-final.json"

record_process_event "$results_dir/gateway-final-stop.json" gateway_stopped gateway "$gateway_pid"
stop_owned_process "$gateway_pid" gateway
stop_control_cluster "$results_dir/control-final-outage.json" control_cluster_stopped
record_snapshot_directory

python3 benchmarks/check_snapshot_freshness.py \
  --evidence-dir "$results_dir" \
  --output "$results_dir/snapshot-freshness-check.json"
python3 benchmarks/render_snapshot_freshness_svg.py \
  --evidence-dir "$results_dir" \
  --check "$results_dir/snapshot-freshness-check.json" \
  --output "$results_dir/snapshot-freshness-proof.svg"

if [[ -n "${INFERLAB_V15_OUTPUT_DIR:-}" ]]; then
  mkdir -p "$INFERLAB_V15_OUTPUT_DIR"
  cp "$results_dir"/*.json "$INFERLAB_V15_OUTPUT_DIR/"
  cp "$results_dir/snapshot-freshness-proof.svg" "$INFERLAB_V15_OUTPUT_DIR/"
  echo "Retained evidence in $INFERLAB_V15_OUTPUT_DIR"
fi

echo "v0.15 bounded-age routing snapshot proof passed"
