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
worker_pid=0
node_a_pid=0
node_b_pid=0
node_c_pid=0

cluster_id='inferlab-primary'
urls='http://127.0.0.1:9911,http://127.0.0.1:9912,http://127.0.0.1:9913'
gateway_url='http://127.0.0.1:9910'
route_key_id='route-2026-b'
route_seed='TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs='
route_public='PUAXw+hDiVqStwqnTRt+vJyYLM8uxJaMwM1V8Sr0Zgw='
trusted_writer_id='deploy-bot'
trusted_writer_seed='nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A='
trusted_writer_public='11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo='
revoked_writer_id='revoked-bot'
revoked_writer_seed='xaqN9D+fg3vtt0QvMdy3sWbThTUHbwlLhc46LgtEWPc='
revoked_writer_public='/FHNjmIYoaONpH7QAjDwWAgW7RO6MwOsXeuRFUiQgCU='
unknown_writer_id='rogue-bot'
unknown_writer_seed="$route_seed"
writer_keys="$trusted_writer_id=$trusted_writer_public,$revoked_writer_id=$revoked_writer_public"

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
  if [[ -n "${INFERLAB_V19_OUTPUT_DIR:-}" && -d "$results_dir" ]]; then
    mkdir -p "$INFERLAB_V19_OUTPUT_DIR"
    cp "$results_dir"/*.json "$INFERLAB_V19_OUTPUT_DIR/" 2>/dev/null || true
    cp "$results_dir"/*.svg "$INFERLAB_V19_OUTPUT_DIR/" 2>/dev/null || true
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
                f"refusing to start v0.19 proof: 127.0.0.1:{port} is busy: {error}"
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

now_ms() {
  python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
}

start_node() {
  local node_id="$1"
  local port="$2"
  local peers="$3"
  local election_min_ms="$4"
  local election_max_ms="$5"
  local log_name="$6"
  local data_dir="$proof_tmp/$node_id"
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
  INFERLAB_CONTROL_SIGNING_KEY_ID="$route_key_id" \
  INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64="$route_seed" \
  INFERLAB_CONTROL_WRITER_KEYS="$writer_keys" \
  INFERLAB_CONTROL_REVOKED_WRITER_IDS="$revoked_writer_id" \
  INFERLAB_CONTROL_WRITE_MAX_AGE_MS=1000 \
  INFERLAB_CONTROL_WRITE_MAX_FUTURE_SKEW_MS=100 \
    target/debug/control-plane >"$proof_tmp/$log_name.log" 2>&1 &
  started_pid="$!"
  register_pid "$started_pid"
  wait_for_health "http://127.0.0.1:$port/healthz"
}

start_worker() {
  INFERLAB_CPU_WORKER_ID=cpu-authorized \
  INFERLAB_CPU_BIND=127.0.0.1:9914 \
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
    target/debug/cpu-worker >"$proof_tmp/cpu-authorized.log" 2>&1 &
  worker_pid="$!"
  register_pid "$worker_pid"
  wait_for_health 'http://127.0.0.1:9914/health'
}

start_gateway() {
  INFERLAB_BIND=127.0.0.1:9910 \
  INFERLAB_CONTROL_PLANE_URLS="$urls" \
  INFERLAB_CONTROL_CLUSTER_ID="$cluster_id" \
  INFERLAB_CONTROL_TRUSTED_KEYS="$route_key_id=$route_public" \
  INFERLAB_CONTROL_BOOTSTRAP_WAIT_MS=1000 \
  INFERLAB_CONTROL_POLL_MS=25 \
  INFERLAB_ROUTING_SNAPSHOT_PATH="$snapshot_path" \
  INFERLAB_ROUTING_SNAPSHOT_MAX_AGE_MS=60000 \
  INFERLAB_ROUTING_SNAPSHOT_MAX_FUTURE_SKEW_MS=100 \
  INFERLAB_ROUTING_LEASE_MS=3000 \
  INFERLAB_ROUTING_LEASE_EXPIRY_ACTION=reject-new \
  INFERLAB_WORKER_CONCURRENCY=4 \
  INFERLAB_ADMISSION_QUEUE_CAPACITY=8 \
  INFERLAB_REQUEST_DEADLINE_MS=10000 \
  INFERLAB_ATTEMPT_TIMEOUT_MS=10000 \
  INFERLAB_MAX_RETRIES=0 \
    target/debug/gateway >"$proof_tmp/gateway.log" 2>&1 &
  gateway_pid="$!"
  register_pid "$gateway_pid"
  wait_for_health "$gateway_url/health"
}

sign_write() {
  local writer_id="$1"
  local private_seed="$2"
  local expected_revision="$3"
  local issued_at_ms="$4"
  local nonce="$5"
  local configuration="$6"
  local output="$7"
  INFERLAB_CONTROL_WRITER_ID="$writer_id" \
  INFERLAB_CONTROL_WRITER_PRIVATE_KEY_B64="$private_seed" \
    target/debug/sign_control_write \
      "$cluster_id" "$expected_revision" "$issued_at_ms" "$nonce" "$configuration" \
      >"$output"
}

submit_write() {
  local leader_url="$1"
  local body="$2"
  local output="$3"
  python3 benchmarks/control_write_probe.py submit \
    --url "$leader_url" --body "$body" >"$output"
}

record_stop_event() {
  python3 - "$results_dir/process-stop.json" "$gateway_pid" "$worker_pid" "$node_a_pid" "$node_b_pid" "$node_c_pid" <<'PY'
import json
import sys
import time

output, *pids = sys.argv[1:]
record = {
    "schema": "inferlab.control-writer-process-event.v0.19",
    "observed_at_ms": round(time.time() * 1000, 3),
    "event": "proof_processes_stopped",
    "scope": "owned-child-processes",
    "targets": ["gateway", "cpu-authorized", "node-a", "node-b", "node-c"],
    "pids": [int(pid) for pid in pids],
}
with open(output, "w", encoding="utf-8") as destination:
    json.dump(record, destination, indent=2, sort_keys=True)
    destination.write("\n")
PY
}

record_snapshot_directory() {
  python3 - "$proof_tmp" "$results_dir/snapshot-directory.json" <<'PY'
import json
import sys
from pathlib import Path

directory = Path(sys.argv[1])
record = {
    "schema": "inferlab.control-writer-directory.v0.19",
    "entries": sorted(path.name for path in directory.iterdir()),
    "temporary_snapshot_files": sorted(
        path.name for path in directory.iterdir()
        if path.name.startswith(".gateway-routing.json.tmp-")
    ),
}
Path(sys.argv[2]).write_text(json.dumps(record, indent=2) + "\n")
PY
}

check_ports_are_free 9910 9911 9912 9913 9914
cargo build --workspace --bins --quiet

cat >"$proof_tmp/config-initial.json" <<'JSON'
{
  "routing_policy": "round-robin",
  "workers": [
    {"id": "cpu-authorized", "base_url": "http://127.0.0.1:9914", "weight": 1}
  ]
}
JSON
cat >"$proof_tmp/config-updated.json" <<'JSON'
{
  "routing_policy": "least-in-flight",
  "workers": [
    {"id": "cpu-authorized", "base_url": "http://127.0.0.1:9914", "weight": 2}
  ]
}
JSON

start_node node-a 9911 \
  'node-b=http://127.0.0.1:9912,node-c=http://127.0.0.1:9913' \
  180 240 node-a
node_a_pid="$started_pid"
start_node node-b 9912 \
  'node-a=http://127.0.0.1:9911,node-c=http://127.0.0.1:9913' \
  300 360 node-b
node_b_pid="$started_pid"
start_node node-c 9913 \
  'node-a=http://127.0.0.1:9911,node-b=http://127.0.0.1:9912' \
  420 480 node-c
node_c_pid="$started_pid"

python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" --timeout 5 >"$results_dir/initial-election.json"
leader_url="$(json_field "$results_dir/initial-election.json" leader_url)"
python3 benchmarks/control_write_probe.py status \
  --url "$leader_url" >"$results_dir/status-initial.json"

submit_write "$leader_url" "$proof_tmp/config-initial.json" \
  "$results_dir/write-unsigned-rejected.json"

sign_write "$unknown_writer_id" "$unknown_writer_seed" 0 now \
  rogue-write-000001 "$proof_tmp/config-initial.json" "$proof_tmp/write-unknown.json"
submit_write "$leader_url" "$proof_tmp/write-unknown.json" \
  "$results_dir/write-unknown-rejected.json"

sign_write "$trusted_writer_id" "$trusted_writer_seed" 0 now \
  tamper-write-00001 "$proof_tmp/config-initial.json" "$proof_tmp/write-before-tamper.json"
python3 benchmarks/control_write_probe.py mutate-worker \
  --body "$proof_tmp/write-before-tamper.json" --worker-id cpu-tampered \
  >"$proof_tmp/write-tampered.json"
submit_write "$leader_url" "$proof_tmp/write-tampered.json" \
  "$results_dir/write-tampered-rejected.json"

stale_ms="$(( $(now_ms) - 5000 ))"
sign_write "$trusted_writer_id" "$trusted_writer_seed" 0 "$stale_ms" \
  stale-write-000001 "$proof_tmp/config-initial.json" "$proof_tmp/write-stale.json"
submit_write "$leader_url" "$proof_tmp/write-stale.json" \
  "$results_dir/write-stale-rejected.json"

sign_write "$revoked_writer_id" "$revoked_writer_seed" 0 now \
  revoked-write-0001 "$proof_tmp/config-initial.json" "$proof_tmp/write-revoked.json"
submit_write "$leader_url" "$proof_tmp/write-revoked.json" \
  "$results_dir/write-revoked-rejected.json"

python3 benchmarks/control_write_probe.py status \
  --url "$leader_url" >"$results_dir/status-after-rejections.json"

sign_write "$trusted_writer_id" "$trusted_writer_seed" 0 now \
  deploy-write-00001 "$proof_tmp/config-initial.json" "$proof_tmp/write-valid.json"
submit_write "$leader_url" "$proof_tmp/write-valid.json" \
  "$results_dir/write-valid-committed.json"
submit_write "$leader_url" "$proof_tmp/write-valid.json" \
  "$results_dir/write-replay-rejected.json"

start_worker
start_gateway
python3 benchmarks/full_stack_probe.py wait-gateway \
  --gateway-url "$gateway_url" --policy round-robin --revision 2 \
  --worker-ids cpu-authorized --timeout 5 >"$results_dir/gateway-revision-2.json"
python3 benchmarks/full_stack_probe.py requests \
  --gateway-url "$gateway_url" --requests 1 --prompt 'authorized write reaches worker' \
  >"$results_dir/request-revision-2.json"

sign_write "$trusted_writer_id" "$trusted_writer_seed" 2 now \
  deploy-update-0001 "$proof_tmp/config-updated.json" "$proof_tmp/write-update.json"
submit_write "$leader_url" "$proof_tmp/write-update.json" \
  "$results_dir/write-update-committed.json"
python3 benchmarks/full_stack_probe.py wait-gateway \
  --gateway-url "$gateway_url" --policy least-in-flight --revision 3 \
  --worker-ids cpu-authorized --timeout 5 >"$results_dir/gateway-revision-3.json"
python3 benchmarks/full_stack_probe.py stream \
  --gateway-url "$gateway_url" --prompt 'authorized changes are auditable' \
  --speculative-tokens 3 >"$results_dir/stream-final.json"
python3 benchmarks/control_write_probe.py status \
  --url "$leader_url" >"$results_dir/status-final.json"
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" --timeout 5 >"$results_dir/final-cluster.json"
cp "$snapshot_path" "$results_dir/gateway-routing-snapshot.json"

record_stop_event
stop_owned_process "$gateway_pid" gateway
stop_owned_process "$worker_pid" cpu-authorized
stop_owned_process "$node_a_pid" node-a
stop_owned_process "$node_b_pid" node-b
stop_owned_process "$node_c_pid" node-c
record_snapshot_directory

python3 benchmarks/check_control_write_auth.py \
  --evidence-dir "$results_dir" >"$results_dir/control-write-auth-check.json"
python3 benchmarks/render_control_write_auth_svg.py \
  --evidence-dir "$results_dir" \
  --check "$results_dir/control-write-auth-check.json" \
  --output "$results_dir/control-write-auth-proof.svg"

if [[ -n "${INFERLAB_V19_OUTPUT_DIR:-}" ]]; then
  mkdir -p "$INFERLAB_V19_OUTPUT_DIR"
  cp "$results_dir"/*.json "$INFERLAB_V19_OUTPUT_DIR/"
  cp "$results_dir"/*.svg "$INFERLAB_V19_OUTPUT_DIR/"
fi

cat "$results_dir/control-write-auth-check.json"
