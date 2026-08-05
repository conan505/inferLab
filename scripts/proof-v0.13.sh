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
mkdir -p "$results_dir"
live_pids=(0)
started_pid=0
node_a_pid=0
node_b_pid=0
node_c_pid=0
worker_a_pid=0
worker_b_pid=0
worker_c_pid=0

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
  if [[ -n "${INFERLAB_V13_OUTPUT_DIR:-}" && -d "$results_dir" ]]; then
    mkdir -p "$INFERLAB_V13_OUTPUT_DIR"
    cp "$results_dir"/*.json "$INFERLAB_V13_OUTPUT_DIR/" 2>/dev/null || true
    cp "$results_dir"/*.svg "$INFERLAB_V13_OUTPUT_DIR/" 2>/dev/null || true
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
                f"refusing to start v0.13 proof: 127.0.0.1:{port} is busy: {error}"
            )
PY
}

wait_for_health() {
  local url="$1"
  local attempts=0
  until curl --fail --silent "$url" >/dev/null; do
    attempts=$((attempts + 1))
    if ((attempts >= 160)); then
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

record_event() {
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
    "schema": "inferlab.full-stack-fault.v0.13",
    "at_ms": round(time.time() * 1000, 3),
    "event": event,
    "target": target,
    "pid": int(pid),
    "scope": "owned-child-process",
    "bind": "127.0.0.1",
}
with open(output, "w", encoding="utf-8") as destination:
    json.dump(record, destination, indent=2, sort_keys=True)
    destination.write("\n")
PY
}

start_node() {
  local node_id="$1"
  local port="$2"
  local peers="$3"
  local election_min_ms="$4"
  local election_max_ms="$5"
  mkdir -p "$proof_tmp/$node_id"
  INFERLAB_RAFT_NODE_ID="$node_id" \
  INFERLAB_RAFT_BIND="127.0.0.1:$port" \
  INFERLAB_RAFT_PEERS="$peers" \
  INFERLAB_RAFT_DATA_DIR="$proof_tmp/$node_id" \
  INFERLAB_RAFT_ELECTION_MIN_MS="$election_min_ms" \
  INFERLAB_RAFT_ELECTION_MAX_MS="$election_max_ms" \
  INFERLAB_RAFT_HEARTBEAT_MS=50 \
  INFERLAB_RAFT_RPC_TIMEOUT_MS=100 \
  INFERLAB_RAFT_COMMIT_TIMEOUT_MS=1500 \
  RUST_LOG=info \
    target/debug/control-plane >"$proof_tmp/$node_id.log" 2>&1 &
  started_pid="$!"
  register_pid "$started_pid"
  wait_for_health "http://127.0.0.1:$port/healthz"
}

start_nodes() {
  start_node node-a 9811 \
    'node-b=http://127.0.0.1:9812,node-c=http://127.0.0.1:9813' \
    180 240
  node_a_pid="$started_pid"
  start_node node-b 9812 \
    'node-a=http://127.0.0.1:9811,node-c=http://127.0.0.1:9813' \
    300 360
  node_b_pid="$started_pid"
  start_node node-c 9813 \
    'node-a=http://127.0.0.1:9811,node-b=http://127.0.0.1:9812' \
    420 480
  node_c_pid="$started_pid"
}

pid_for_node() {
  case "$1" in
    node-a) echo "$node_a_pid" ;;
    node-b) echo "$node_b_pid" ;;
    node-c) echo "$node_c_pid" ;;
    *) return 1 ;;
  esac
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

pid_for_worker() {
  case "$1" in
    cpu-real-a) echo "$worker_a_pid" ;;
    cpu-real-b) echo "$worker_b_pid" ;;
    cpu-real-c) echo "$worker_c_pid" ;;
    *) return 1 ;;
  esac
}

oracle_python="${INFERLAB_ORACLE_PYTHON:-}"
if [[ -z "$oracle_python" && -x "$project_root/.tools/v0.7-python/bin/python" ]]; then
  oracle_python="$project_root/.tools/v0.7-python/bin/python"
fi
if [[ -z "$oracle_python" ]]; then
  oracle_python="$(command -v python3)"
fi
if ! "$oracle_python" -c 'import torch' >/dev/null 2>&1; then
  echo "v0.13 proof requires PyTorch 2.2.2 or compatible for environment evidence." >&2
  exit 1
fi

urls='http://127.0.0.1:9811,http://127.0.0.1:9812,http://127.0.0.1:9813'
gateway_url='http://127.0.0.1:9820'
initial_workers='cpu-real-a=http://127.0.0.1:9821@1,cpu-real-b=http://127.0.0.1:9822@1,cpu-real-c=http://127.0.0.1:9823@1'

check_ports_are_free 9811 9812 9813 9820 9821 9822 9823

echo "Regenerating the deterministic v2 checkpoint..."
python3 oracle/generate_tiny_model_v2.py \
  --model "$proof_tmp/tiny-inferlab-v2.bin" \
  --metadata "$proof_tmp/tiny-inferlab-v2.json"
cmp models/tiny-inferlab-v2.bin "$proof_tmp/tiny-inferlab-v2.bin"
cmp models/tiny-inferlab-v2.json "$proof_tmp/tiny-inferlab-v2.json"

echo "Building the v0.13 full stack..."
cargo build --workspace --quiet

start_nodes
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" >"$results_dir/initial-election.json"

start_worker cpu-real-a 9821
worker_a_pid="$started_pid"
start_worker cpu-real-b 9822
worker_b_pid="$started_pid"
start_worker cpu-real-c 9823
worker_c_pid="$started_pid"

python3 benchmarks/full_stack_probe.py write-config \
  --urls "$urls" \
  --policy consistent-hash \
  --workers "$initial_workers" >"$results_dir/config-initial.json"
initial_revision="$(json_field "$results_dir/config-initial.json" committed.revision)"

INFERLAB_BIND=127.0.0.1:9820 \
INFERLAB_CONTROL_PLANE_URLS="$urls" \
INFERLAB_CONTROL_POLL_MS=25 \
INFERLAB_WORKER_CONCURRENCY=4 \
INFERLAB_ADMISSION_QUEUE_CAPACITY=8 \
INFERLAB_REQUEST_DEADLINE_MS=5000 \
INFERLAB_ATTEMPT_TIMEOUT_MS=1000 \
INFERLAB_MAX_RETRIES=2 \
INFERLAB_RETRY_BUDGET_PERCENT=100 \
INFERLAB_RETRY_BASE_DELAY_MS=1 \
INFERLAB_RETRY_MAX_DELAY_MS=2 \
INFERLAB_CIRCUIT_WINDOW_SIZE=2 \
INFERLAB_CIRCUIT_MIN_REQUESTS=1 \
INFERLAB_CIRCUIT_FAILURE_RATE_PERCENT=50 \
INFERLAB_CIRCUIT_OPEN_MS=1000 \
  target/debug/gateway >"$proof_tmp/gateway.log" 2>&1 &
gateway_pid="$!"
register_pid "$gateway_pid"
wait_for_health "$gateway_url/health"

python3 benchmarks/full_stack_probe.py wait-gateway \
  --gateway-url "$gateway_url" \
  --policy consistent-hash \
  --revision "$initial_revision" \
  --worker-ids cpu-real-a,cpu-real-b,cpu-real-c \
  >"$results_dir/gateway-initial.json"
python3 benchmarks/full_stack_probe.py worker-health \
  --workers "$initial_workers" >"$results_dir/worker-health.json"
python3 benchmarks/full_stack_probe.py affinity \
  --gateway-url "$gateway_url" \
  --cache-key v0.13/shared-prefix >"$results_dir/affinity.json"

failed_worker="$(json_field "$results_dir/affinity.json" requests.0.worker)"
failed_worker_pid="$(pid_for_worker "$failed_worker")"
record_event "$results_dir/worker-fault.json" worker_killed "$failed_worker" "$failed_worker_pid"
stop_owned_process "$failed_worker_pid" "$failed_worker"

python3 benchmarks/full_stack_probe.py requests \
  --gateway-url "$gateway_url" \
  --requests 1 \
  --cache-key v0.13/shared-prefix >"$results_dir/failover.json"

case "$failed_worker" in
  cpu-real-a)
    live_workers='cpu-real-b=http://127.0.0.1:9822@1,cpu-real-c=http://127.0.0.1:9823@1'
    live_ids='cpu-real-b,cpu-real-c'
    weighted_workers='cpu-real-b=http://127.0.0.1:9822@3,cpu-real-c=http://127.0.0.1:9823@1'
    ;;
  cpu-real-b)
    live_workers='cpu-real-a=http://127.0.0.1:9821@1,cpu-real-c=http://127.0.0.1:9823@1'
    live_ids='cpu-real-a,cpu-real-c'
    weighted_workers='cpu-real-a=http://127.0.0.1:9821@3,cpu-real-c=http://127.0.0.1:9823@1'
    ;;
  cpu-real-c)
    live_workers='cpu-real-a=http://127.0.0.1:9821@1,cpu-real-b=http://127.0.0.1:9822@1'
    live_ids='cpu-real-a,cpu-real-b'
    weighted_workers='cpu-real-a=http://127.0.0.1:9821@3,cpu-real-b=http://127.0.0.1:9822@1'
    ;;
  *)
    echo "unexpected affinity owner $failed_worker" >&2
    exit 1
    ;;
esac

python3 benchmarks/full_stack_probe.py write-config \
  --urls "$urls" \
  --policy least-in-flight \
  --workers "$live_workers" >"$results_dir/config-live.json"
live_revision="$(json_field "$results_dir/config-live.json" committed.revision)"
python3 benchmarks/full_stack_probe.py wait-gateway \
  --gateway-url "$gateway_url" \
  --policy least-in-flight \
  --revision "$live_revision" \
  --worker-ids "$live_ids" >"$results_dir/gateway-live.json"
python3 benchmarks/full_stack_probe.py requests \
  --gateway-url "$gateway_url" \
  --requests 4 \
  --prompt 'post reconfiguration ' >"$results_dir/post-reconfigure.json"

python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" >"$results_dir/control-leader.json"
control_leader="$(json_field "$results_dir/control-leader.json" leader_id)"
control_leader_pid="$(pid_for_node "$control_leader")"
record_event "$results_dir/control-fault.json" leader_killed "$control_leader" "$control_leader_pid"
control_fault_ms="$(json_field "$results_dir/control-fault.json" at_ms)"
stop_owned_process "$control_leader_pid" "$control_leader"

python3 benchmarks/full_stack_probe.py requests \
  --gateway-url "$gateway_url" \
  --requests 6 \
  --prompt 'control election continuity ' >"$results_dir/election-continuity.json"
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" \
  --since-ms "$control_fault_ms" >"$results_dir/re-election.json"

python3 benchmarks/full_stack_probe.py write-config \
  --urls "$urls" \
  --policy weighted-round-robin \
  --workers "$weighted_workers" >"$results_dir/config-weighted.json"
weighted_revision="$(json_field "$results_dir/config-weighted.json" committed.revision)"
python3 benchmarks/full_stack_probe.py wait-gateway \
  --gateway-url "$gateway_url" \
  --policy weighted-round-robin \
  --revision "$weighted_revision" \
  --worker-ids "$live_ids" >"$results_dir/gateway-weighted.json"
python3 benchmarks/full_stack_probe.py requests \
  --gateway-url "$gateway_url" \
  --requests 8 \
  --prompt 'weighted request ' >"$results_dir/weighted.json"
python3 benchmarks/full_stack_probe.py stream \
  --gateway-url "$gateway_url" \
  --prompt 'teach me streaming' \
  --speculative-tokens 3 >"$results_dir/stream.json"

"$oracle_python" benchmarks/attention_environment.py \
  --model models/tiny-inferlab-v2.bin \
  --benchmark-note \
  'The v0.13 proof runs three real online-attention decoder workers on the host CPU behind the Raft-configured gateway; it is an integration and fault-continuity experiment, not a throughput benchmark.' \
  --milestone-boundary \
  'The proof integrates previously validated CPU runtime and distributed-system mechanisms. CUDA remains v1.0 because this host has no NVIDIA toolchain or device.' \
  --output "$results_dir/environment.json"

python3 benchmarks/check_full_stack.py \
  --evidence-dir "$results_dir" \
  --output "$results_dir/full-stack-check.json"
python3 benchmarks/render_full_stack_svg.py \
  --evidence-dir "$results_dir" \
  --check "$results_dir/full-stack-check.json" \
  --output "$results_dir/full-stack-proof.svg"

if [[ -n "${INFERLAB_V13_OUTPUT_DIR:-}" ]]; then
  output_dir="$INFERLAB_V13_OUTPUT_DIR"
  mkdir -p "$output_dir"
  cp "$results_dir"/*.json "$output_dir/"
  cp "$results_dir/full-stack-proof.svg" "$output_dir/"
  echo "Retained evidence in $output_dir"
fi

echo "v0.13 proof passed"
