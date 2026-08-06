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
old_snapshot="$proof_tmp/old-key-routing.json"
new_snapshot="$proof_tmp/new-key-routing.json"
tampered_snapshot="$proof_tmp/tampered-routing.json"
mkdir -p "$results_dir"
live_pids=(0)
started_pid=0
gateway_pid=0
node_a_pid=0
node_b_pid=0
node_c_pid=0
lease_ms=700
primary_cluster='inferlab-primary'
old_key_id='primary-2026-a'
old_seed='nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A='
old_public='11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo='
new_key_id='primary-2026-b'
new_seed='TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs='
new_public='PUAXw+hDiVqStwqnTRt+vJyYLM8uxJaMwM1V8Sr0Zgw='
rogue_key_id='rogue-2026-x'
rogue_seed='xaqN9D+fg3vtt0QvMdy3sWbThTUHbwlLhc46LgtEWPc='
trusted_keys="$old_key_id=$old_public,$new_key_id=$new_public"

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
  if [[ -n "${INFERLAB_V18_OUTPUT_DIR:-}" && -d "$results_dir" ]]; then
    mkdir -p "$INFERLAB_V18_OUTPUT_DIR"
    cp "$results_dir"/*.json "$INFERLAB_V18_OUTPUT_DIR/" 2>/dev/null || true
    cp "$results_dir"/*.svg "$INFERLAB_V18_OUTPUT_DIR/" 2>/dev/null || true
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
                f"refusing to start v0.18 proof: 127.0.0.1:{port} is busy: {error}"
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
    "schema": "inferlab.signed-control-event.v0.18",
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
  local key_id="$3"
  python3 - "$output" "$event" "$primary_cluster" "$key_id" "$node_a_pid" "$node_b_pid" "$node_c_pid" <<'PY'
import json
import sys
import time

output, event, cluster_id, key_id, *pids = sys.argv[1:]
record = {
    "schema": "inferlab.signed-control-cluster-event.v0.18",
    "at_ms": round(time.time() * 1000, 3),
    "event": event,
    "cluster_id": cluster_id,
    "signing_key_id": key_id,
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
  local key_id="$3"
  record_control_event "$output" "$event" "$key_id"
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
  local key_id="$8"
  local private_key="$9"
  mkdir -p "$data_dir"
  INFERLAB_RAFT_NODE_ID="$node_id" \
  INFERLAB_RAFT_CLUSTER_ID="$primary_cluster" \
  INFERLAB_RAFT_BIND="127.0.0.1:$port" \
  INFERLAB_RAFT_PEERS="$peers" \
  INFERLAB_RAFT_DATA_DIR="$data_dir" \
  INFERLAB_RAFT_ELECTION_MIN_MS="$election_min_ms" \
  INFERLAB_RAFT_ELECTION_MAX_MS="$election_max_ms" \
  INFERLAB_RAFT_HEARTBEAT_MS=50 \
  INFERLAB_RAFT_RPC_TIMEOUT_MS=100 \
  INFERLAB_RAFT_COMMIT_TIMEOUT_MS=1500 \
  INFERLAB_CONTROL_SIGNING_KEY_ID="$key_id" \
  INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64="$private_key" \
    target/debug/control-plane >"$proof_tmp/$log_name.log" 2>&1 &
  started_pid="$!"
  register_pid "$started_pid"
  wait_for_health "http://127.0.0.1:$port/healthz"
}

start_nodes() {
  local directory_prefix="$1"
  local log_prefix="$2"
  local key_id="$3"
  local private_key="$4"
  start_node node-a 9901 \
    'node-b=http://127.0.0.1:9902,node-c=http://127.0.0.1:9903' \
    180 240 "$proof_tmp/$directory_prefix-node-a" "$log_prefix-node-a" "$key_id" "$private_key"
  node_a_pid="$started_pid"
  start_node node-b 9902 \
    'node-a=http://127.0.0.1:9901,node-c=http://127.0.0.1:9903' \
    300 360 "$proof_tmp/$directory_prefix-node-b" "$log_prefix-node-b" "$key_id" "$private_key"
  node_b_pid="$started_pid"
  start_node node-c 9903 \
    'node-a=http://127.0.0.1:9901,node-b=http://127.0.0.1:9902' \
    420 480 "$proof_tmp/$directory_prefix-node-c" "$log_prefix-node-c" "$key_id" "$private_key"
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
  local revoked_keys="${3:-}"
  local lease_enabled="${4:-yes}"
  (
    export INFERLAB_BIND=127.0.0.1:9900
    export INFERLAB_CONTROL_PLANE_URLS="$urls"
    export INFERLAB_CONTROL_CLUSTER_ID="$primary_cluster"
    export INFERLAB_CONTROL_TRUSTED_KEYS="$trusted_keys"
    export INFERLAB_CONTROL_REVOKED_KEY_IDS="$revoked_keys"
    export INFERLAB_CONTROL_BOOTSTRAP_WAIT_MS=150
    export INFERLAB_CONTROL_POLL_MS=25
    export INFERLAB_ROUTING_SNAPSHOT_PATH="$snapshot_path"
    export INFERLAB_ROUTING_SNAPSHOT_MAX_AGE_MS=60000
    export INFERLAB_ROUTING_SNAPSHOT_MAX_FUTURE_SKEW_MS=100
    export INFERLAB_WORKER_CONCURRENCY=4
    export INFERLAB_ADMISSION_QUEUE_CAPACITY=8
    export INFERLAB_REQUEST_DEADLINE_MS=10000
    export INFERLAB_ATTEMPT_TIMEOUT_MS=10000
    export INFERLAB_MAX_RETRIES=0
    if [[ "$lease_enabled" == yes ]]; then
      export INFERLAB_ROUTING_LEASE_MS="$lease_ms"
      export INFERLAB_ROUTING_LEASE_EXPIRY_ACTION=reject-new
    else
      unset INFERLAB_ROUTING_LEASE_MS
      unset INFERLAB_ROUTING_LEASE_EXPIRY_ACTION
    fi
    exec target/debug/gateway
  ) >"$proof_tmp/$log_name.log" 2>&1 &
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

mutate_snapshot_route() {
  python3 - "$new_snapshot" "$tampered_snapshot" "$results_dir/tampered-snapshot-fixture.json" <<'PY'
import json
import sys
from pathlib import Path

source, destination, record_path = sys.argv[1:]
document = json.loads(Path(source).read_text())
original_worker = document["configuration"]["workers"][0]["id"]
document["configuration"]["workers"][0]["id"] = "cpu-tampered"
Path(destination).write_text(json.dumps(document, indent=2) + "\n")
record = {
    "schema": "inferlab.signed-control-tamper-fixture.v0.18",
    "cluster_id": document["cluster_id"],
    "revision": document["revision"],
    "term": document["term"],
    "signing_key_id": document["authentication"]["key_id"],
    "original_worker_id": original_worker,
    "tampered_worker_id": document["configuration"]["workers"][0]["id"],
    "signature_unchanged": True,
}
Path(record_path).write_text(json.dumps(record, indent=2) + "\n")
PY
}

run_rejected_bootstrap() {
  local snapshot_path="$1"
  local revoked_keys="$2"
  local log_name="$3"
  local output_name="$4"
  local log_path="$proof_tmp/$log_name.log"
  set +e
  INFERLAB_BIND=127.0.0.1:9906 \
  INFERLAB_CONTROL_PLANE_URLS="$urls" \
  INFERLAB_CONTROL_CLUSTER_ID="$primary_cluster" \
  INFERLAB_CONTROL_TRUSTED_KEYS="$trusted_keys" \
  INFERLAB_CONTROL_REVOKED_KEY_IDS="$revoked_keys" \
  INFERLAB_CONTROL_BOOTSTRAP_WAIT_MS=100 \
  INFERLAB_ROUTING_SNAPSHOT_PATH="$snapshot_path" \
  INFERLAB_ROUTING_SNAPSHOT_MAX_AGE_MS=60000 \
    target/debug/gateway >"$log_path" 2>&1
  local exit_code="$?"
  set -e
  python3 - "$exit_code" "$log_path" "$results_dir/$output_name.json" <<'PY'
import json
import sys
from pathlib import Path

exit_code, log_path, output = sys.argv[1:]
result = {
    "schema": "inferlab.signed-control-bootstrap-rejection.v0.18",
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
    "schema": "inferlab.signed-control-directory.v0.18",
    "entries": entries,
    "temporary_snapshot_files": [
        name for name in entries if name.startswith(".primary-routing.json")
    ],
}
Path(sys.argv[2]).write_text(json.dumps(result, indent=2) + "\n")
PY
}

urls='http://127.0.0.1:9901,http://127.0.0.1:9902,http://127.0.0.1:9903'
gateway_url='http://127.0.0.1:9900'
primary_workers='cpu-primary=http://127.0.0.1:9904@1'
rogue_workers='cpu-rogue=http://127.0.0.1:9905@1'

check_ports_are_free 9900 9901 9902 9903 9904 9905 9906

echo "Building the v0.18 signed-control boundary..."
cargo build --workspace --quiet
start_worker cpu-primary 9904 250
start_worker cpu-rogue 9905 0

start_nodes primary initial-primary "$old_key_id" "$old_seed"
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" >"$results_dir/initial-primary-election.json"
python3 benchmarks/full_stack_probe.py write-config \
  --urls "$urls" \
  --policy round-robin \
  --workers "$primary_workers" >"$results_dir/config-primary-old-key.json"
revision="$(json_field "$results_dir/config-primary-old-key.json" committed.revision)"

start_gateway "$primary_snapshot" gateway-primary
python3 benchmarks/runtime_lease_probe.py wait-state \
  --gateway-url "$gateway_url" \
  --state fresh \
  --revision "$revision" \
  --minimum-renewals 1 >"$results_dir/gateway-old-key-fresh.json"
cp "$primary_snapshot" "$old_snapshot"
cp "$primary_snapshot" "$results_dir/snapshot-old-key.json"
python3 benchmarks/runtime_lease_probe.py request \
  --gateway-url "$gateway_url" \
  --prompt 'signed primary route' >"$results_dir/request-old-key.json"

python3 benchmarks/full_stack_probe.py worker-health \
  --workers "$primary_workers" >"$results_dir/worker-primary-before-stream.json"
primary_requests="$(json_field "$results_dir/worker-primary-before-stream.json" workers.cpu-primary.body.requests)"
python3 benchmarks/full_stack_probe.py stream \
  --gateway-url "$gateway_url" \
  --prompt 'teach me authenticated streaming' \
  --speculative-tokens 0 >"$results_dir/stream-crossing-rogue-key.json" &
stream_pid="$!"
register_pid "$stream_pid"
wait_for_worker_requests 9904 "$((primary_requests + 1))"

stop_control_cluster "$results_dir/primary-control-outage.json" primary_cluster_stopped "$old_key_id"
start_nodes rogue rogue "$rogue_key_id" "$rogue_seed"
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" >"$results_dir/rogue-election.json"
python3 benchmarks/full_stack_probe.py write-config \
  --urls "$urls" \
  --policy round-robin \
  --workers "$rogue_workers" >"$results_dir/config-rogue-key.json"

python3 benchmarks/runtime_lease_probe.py wait-state \
  --gateway-url "$gateway_url" \
  --state expired-rejecting-new \
  --revision "$revision" >"$results_dir/gateway-rogue-rejected.json"
python3 benchmarks/runtime_lease_probe.py readiness \
  --gateway-url "$gateway_url" >"$results_dir/readiness-rogue-rejected.json"
python3 benchmarks/full_stack_probe.py worker-health \
  --workers "$primary_workers" >"$results_dir/worker-primary-before-rejection.json"
python3 benchmarks/full_stack_probe.py worker-health \
  --workers "$rogue_workers" >"$results_dir/worker-rogue-before-rejection.json"
python3 benchmarks/runtime_lease_probe.py request \
  --gateway-url "$gateway_url" \
  --prompt 'rogue signature must not route' >"$results_dir/request-rogue-rejected.json"
python3 benchmarks/full_stack_probe.py worker-health \
  --workers "$primary_workers" >"$results_dir/worker-primary-after-rejection.json"
python3 benchmarks/full_stack_probe.py worker-health \
  --workers "$rogue_workers" >"$results_dir/worker-rogue-after-rejection.json"

wait "$stream_pid"
unregister_pid "$stream_pid"

stop_control_cluster "$results_dir/rogue-control-stop.json" rogue_cluster_stopped "$rogue_key_id"
start_nodes primary rotated-primary "$new_key_id" "$new_seed"
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" >"$results_dir/rotated-primary-election.json"
python3 benchmarks/runtime_lease_probe.py wait-state \
  --gateway-url "$gateway_url" \
  --state fresh \
  --revision "$revision" >"$results_dir/gateway-new-key-renewed.json"
python3 benchmarks/runtime_lease_probe.py readiness \
  --gateway-url "$gateway_url" >"$results_dir/readiness-new-key-renewed.json"
cp "$primary_snapshot" "$new_snapshot"
cp "$primary_snapshot" "$results_dir/snapshot-new-key.json"
python3 benchmarks/runtime_lease_probe.py request \
  --gateway-url "$gateway_url" \
  --prompt 'rotated signing key accepted' >"$results_dir/request-new-key.json"

stop_control_cluster "$results_dir/rotated-primary-control-stop.json" primary_new_key_stopped "$new_key_id"
start_nodes primary rollback-primary "$old_key_id" "$old_seed"
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" >"$results_dir/rollback-old-key-election.json"
python3 benchmarks/runtime_lease_probe.py wait-state \
  --gateway-url "$gateway_url" \
  --state expired-rejecting-new \
  --revision "$revision" >"$results_dir/gateway-key-downgrade-rejected.json"

stop_control_cluster "$results_dir/rollback-old-key-control-stop.json" rollback_old_key_stopped "$old_key_id"
start_nodes primary restored-new-key "$new_key_id" "$new_seed"
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" >"$results_dir/restored-new-key-election.json"
python3 benchmarks/runtime_lease_probe.py wait-state \
  --gateway-url "$gateway_url" \
  --state fresh \
  --revision "$revision" >"$results_dir/gateway-new-key-rerenewed.json"

record_process_event "$results_dir/gateway-primary-stop.json" gateway_stopped gateway "$gateway_pid"
stop_owned_process "$gateway_pid" gateway
stop_control_cluster "$results_dir/primary-control-final-stop.json" primary_cluster_stopped "$new_key_id"

mutate_snapshot_route
run_rejected_bootstrap "$tampered_snapshot" '' tampered-disk-rejection tampered-disk-bootstrap-rejected
run_rejected_bootstrap "$old_snapshot" "$old_key_id" revoked-old-key-rejection revoked-old-key-bootstrap-rejected

start_gateway "$new_snapshot" gateway-new-key-disk "$old_key_id" no
python3 benchmarks/full_stack_probe.py wait-gateway \
  --gateway-url "$gateway_url" \
  --policy round-robin \
  --revision "$revision" \
  --worker-ids cpu-primary >"$results_dir/gateway-new-key-disk.json"
python3 benchmarks/runtime_lease_probe.py request \
  --gateway-url "$gateway_url" \
  --prompt 'new key disk survives old key revocation' >"$results_dir/request-new-key-disk.json"
python3 benchmarks/full_stack_probe.py stream \
  --gateway-url "$gateway_url" \
  --prompt 'teach me authenticated streaming' \
  --speculative-tokens 3 >"$results_dir/stream-final.json"

record_process_event "$results_dir/gateway-final-stop.json" gateway_stopped gateway "$gateway_pid"
stop_owned_process "$gateway_pid" gateway
record_snapshot_directory

python3 benchmarks/check_signed_control.py \
  --evidence-dir "$results_dir" \
  --output "$results_dir/signed-control-check.json"
python3 benchmarks/render_signed_control_svg.py \
  --evidence-dir "$results_dir" \
  --check "$results_dir/signed-control-check.json" \
  --output "$results_dir/signed-control-proof.svg"

if [[ -n "${INFERLAB_V18_OUTPUT_DIR:-}" ]]; then
  mkdir -p "$INFERLAB_V18_OUTPUT_DIR"
  cp "$results_dir"/*.json "$INFERLAB_V18_OUTPUT_DIR/"
  cp "$results_dir/signed-control-proof.svg" "$INFERLAB_V18_OUTPUT_DIR/"
  echo "Retained evidence in $INFERLAB_V18_OUTPUT_DIR"
fi

echo "v0.18 signed-control proof passed"
