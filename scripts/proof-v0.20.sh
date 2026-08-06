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
live_pids=()

cluster_id='inferlab-primary'
urls='http://127.0.0.1:9921,http://127.0.0.1:9922,http://127.0.0.1:9923'
gateway_url='http://127.0.0.1:9920'
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
rogue_seed='//////////////////////////////////////////8='

cleanup() {
  local pid
  for pid in "${live_pids[@]}"; do
    if [[ "$pid" =~ ^[1-9][0-9]*$ ]]; then
      kill "$pid" 2>/dev/null || true
    fi
  done
  for pid in "${live_pids[@]}"; do
    wait "$pid" 2>/dev/null || true
  done
  if [[ -n "${INFERLAB_V20_OUTPUT_DIR:-}" && -d "$results_dir" ]]; then
    mkdir -p "$INFERLAB_V20_OUTPUT_DIR"
    cp "$results_dir"/*.json "$INFERLAB_V20_OUTPUT_DIR/" 2>/dev/null || true
    cp "$results_dir"/*.svg "$INFERLAB_V20_OUTPUT_DIR/" 2>/dev/null || true
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
            raise SystemExit(f"refusing v0.20 proof: 127.0.0.1:{port} is busy: {error}")
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

public_key() {
  local service_id="$1"
  local seed="$2"
  INFERLAB_SERVICE_ID="$service_id" \
  INFERLAB_SERVICE_PRIVATE_KEY_B64="$seed" \
    target/debug/service_public_key
}

service_seed() {
  case "$1" in
    node-a) printf '%s' "$node_a_seed" ;;
    node-b) printf '%s' "$node_b_seed" ;;
    node-c) printf '%s' "$node_c_seed" ;;
    gateway-primary) printf '%s' "$gateway_seed" ;;
    rogue-service) printf '%s' "$rogue_seed" ;;
    *) return 1 ;;
  esac
}

start_node() {
  local node_id="$1"
  local port="$2"
  local peers="$3"
  local election_min_ms="$4"
  local election_max_ms="$5"
  local seed
  seed="$(service_seed "$node_id")"
  mkdir -p "$proof_tmp/$node_id"
  INFERLAB_RAFT_NODE_ID="$node_id" \
  INFERLAB_RAFT_CLUSTER_ID="$cluster_id" \
  INFERLAB_RAFT_BIND="127.0.0.1:$port" \
  INFERLAB_RAFT_PEERS="$peers" \
  INFERLAB_RAFT_DATA_DIR="$proof_tmp/$node_id" \
  INFERLAB_RAFT_ELECTION_MIN_MS="$election_min_ms" \
  INFERLAB_RAFT_ELECTION_MAX_MS="$election_max_ms" \
  INFERLAB_RAFT_HEARTBEAT_MS=50 \
  INFERLAB_RAFT_RPC_TIMEOUT_MS=100 \
  INFERLAB_RAFT_COMMIT_TIMEOUT_MS=1500 \
  INFERLAB_CONTROL_SIGNING_KEY_ID="$route_key_id" \
  INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64="$route_seed" \
  INFERLAB_CONTROL_WRITER_KEYS="$writer_id=$writer_public" \
  INFERLAB_CONTROL_WRITE_MAX_AGE_MS=1000 \
  INFERLAB_CONTROL_WRITE_MAX_FUTURE_SKEW_MS=100 \
  INFERLAB_SERVICE_ID="$node_id" \
  INFERLAB_SERVICE_PRIVATE_KEY_B64="$seed" \
  INFERLAB_SERVICE_TRUSTED_KEYS="$service_keys" \
  INFERLAB_GATEWAY_SERVICE_IDS='gateway-primary' \
  INFERLAB_SERVICE_AUTH_MAX_AGE_MS=1000 \
  INFERLAB_SERVICE_AUTH_MAX_FUTURE_SKEW_MS=100 \
    target/debug/control-plane >"$proof_tmp/$node_id.log" 2>&1 &
  live_pids+=("$!")
  wait_for_health "http://127.0.0.1:$port/healthz"
}

sign_service_request() {
  local service_id="$1"
  local seed="$2"
  local method="$3"
  local path="$4"
  local audience_id="$5"
  local issued_at_ms="$6"
  local nonce="$7"
  local body="$8"
  local output="$9"
  INFERLAB_SERVICE_ID="$service_id" \
  INFERLAB_SERVICE_PRIVATE_KEY_B64="$seed" \
    target/debug/sign_service_request \
      "$method" "$path" "$cluster_id" "$audience_id" "$issued_at_ms" "$nonce" "$body" \
      >"$output"
}

probe_service() {
  local method="$1"
  local url="$2"
  local authentication="$3"
  local body="$4"
  local output="$5"
  local arguments=(--method "$method" --url "$url")
  if [[ "$authentication" != '-' ]]; then
    arguments+=(--authentication "$authentication")
  fi
  if [[ "$body" != '-' ]]; then
    arguments+=(--body "$body")
  fi
  python3 benchmarks/service_request_probe.py "${arguments[@]}" >"$output"
}

start_worker() {
  INFERLAB_CPU_WORKER_ID=cpu-service-auth \
  INFERLAB_CPU_BIND=127.0.0.1:9924 \
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
  live_pids+=("$!")
  wait_for_health 'http://127.0.0.1:9924/health'
}

start_gateway() {
  INFERLAB_BIND=127.0.0.1:9920 \
  INFERLAB_CONTROL_PLANE_URLS="$urls" \
  INFERLAB_CONTROL_CLUSTER_ID="$cluster_id" \
  INFERLAB_CONTROL_TRUSTED_KEYS="$route_key_id=$route_public" \
  INFERLAB_CONTROL_BOOTSTRAP_WAIT_MS=1000 \
  INFERLAB_CONTROL_POLL_MS=50 \
  INFERLAB_GATEWAY_SERVICE_ID='gateway-primary' \
  INFERLAB_GATEWAY_SERVICE_PRIVATE_KEY_B64="$gateway_seed" \
  INFERLAB_CONTROL_SERVICE_TARGETS='node-a=http://127.0.0.1:9921,node-b=http://127.0.0.1:9922,node-c=http://127.0.0.1:9923' \
  INFERLAB_ROUTING_SNAPSHOT_PATH="$snapshot_path" \
  INFERLAB_ROUTING_SNAPSHOT_MAX_AGE_MS=60000 \
  INFERLAB_ROUTING_LEASE_MS=3000 \
  INFERLAB_WORKER_CONCURRENCY=4 \
  INFERLAB_ADMISSION_QUEUE_CAPACITY=8 \
  INFERLAB_REQUEST_DEADLINE_MS=10000 \
  INFERLAB_ATTEMPT_TIMEOUT_MS=10000 \
  INFERLAB_MAX_RETRIES=0 \
    target/debug/gateway >"$proof_tmp/gateway.log" 2>&1 &
  live_pids+=("$!")
  wait_for_health "$gateway_url/health"
}

check_ports_are_free 9920 9921 9922 9923 9924
cargo build --workspace --bins --quiet

node_a_public="$(public_key node-a "$node_a_seed")"
node_b_public="$(public_key node-b "$node_b_seed")"
node_c_public="$(public_key node-c "$node_c_seed")"
gateway_public="$(public_key gateway-primary "$gateway_seed")"
service_keys="node-a=$node_a_public,node-b=$node_b_public,node-c=$node_c_public,gateway-primary=$gateway_public"

cat >"$proof_tmp/config.json" <<'JSON'
{
  "routing_policy": "round-robin",
  "workers": [
    {"id": "cpu-service-auth", "base_url": "http://127.0.0.1:9924", "weight": 1}
  ]
}
JSON

start_node node-a 9921 \
  'node-b=http://127.0.0.1:9922,node-c=http://127.0.0.1:9923' 180 240
start_node node-b 9922 \
  'node-a=http://127.0.0.1:9921,node-c=http://127.0.0.1:9923' 300 360
start_node node-c 9923 \
  'node-a=http://127.0.0.1:9921,node-b=http://127.0.0.1:9922' 420 480

python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" --timeout 5 >"$results_dir/election.json"
leader_url="$(json_field "$results_dir/election.json" leader_url)"
leader_id="$(json_field "$results_dir/election.json" leader_id)"
leader_term="$(json_field "$results_dir/election.json" term)"

INFERLAB_CONTROL_WRITER_ID="$writer_id" \
INFERLAB_CONTROL_WRITER_PRIVATE_KEY_B64="$writer_seed" \
  target/debug/sign_control_write \
    "$cluster_id" 0 now service-auth-route-1 "$proof_tmp/config.json" \
    >"$proof_tmp/write.json"
python3 benchmarks/control_write_probe.py submit \
  --url "$leader_url" --body "$proof_tmp/write.json" \
  >"$results_dir/write-committed.json"

probe_service GET "$leader_url/v1/control/config" - - \
  "$results_dir/missing-rejected.json"

sign_service_request rogue-service "$rogue_seed" GET /v1/control/config \
  "$leader_id" "$(now_ms)" rogue-service-request-1 - "$proof_tmp/auth-rogue.json"
probe_service GET "$leader_url/v1/control/config" "$proof_tmp/auth-rogue.json" - \
  "$results_dir/unknown-rejected.json"

stale_ms="$(( $(now_ms) - 5000 ))"
sign_service_request gateway-primary "$gateway_seed" GET /v1/control/config \
  "$leader_id" "$stale_ms" gateway-stale-request-1 - "$proof_tmp/auth-stale.json"
probe_service GET "$leader_url/v1/control/config" "$proof_tmp/auth-stale.json" - \
  "$results_dir/stale-rejected.json"

sign_service_request node-a "$node_a_seed" GET /v1/control/config \
  "$leader_id" "$(now_ms)" node-a-gateway-read-1 - "$proof_tmp/auth-peer-read.json"
probe_service GET "$leader_url/v1/control/config" "$proof_tmp/auth-peer-read.json" - \
  "$results_dir/peer-read-forbidden.json"

sign_service_request gateway-primary "$gateway_seed" GET /v1/control/config \
  "$leader_id" "$(now_ms)" gateway-valid-request-1 - "$proof_tmp/auth-valid.json"
probe_service GET "$leader_url/v1/control/config" "$proof_tmp/auth-valid.json" - \
  "$results_dir/gateway-read-valid.json"
probe_service GET "$leader_url/v1/control/config" "$proof_tmp/auth-valid.json" - \
  "$results_dir/replay-rejected.json"

attacker_peer_id="$(python3 - "$leader_id" <<'PY'
import sys
print(next(node for node in ("node-a", "node-b", "node-c") if node != sys.argv[1]))
PY
)"
cat >"$proof_tmp/vote-original.json" <<JSON
{"cluster_id": "$cluster_id", "term": $((leader_term + 50)), "candidate_id": "$attacker_peer_id", "last_log_index": 0, "last_log_term": 0}
JSON
python3 - "$proof_tmp/vote-original.json" "$proof_tmp/vote-tampered.json" <<'PY'
import json
import sys
from pathlib import Path

body = json.loads(Path(sys.argv[1]).read_text())
body["term"] += 1
Path(sys.argv[2]).write_text(json.dumps(body, indent=2) + "\n")
PY
sign_service_request node-a "$node_a_seed" POST /raft/request-vote \
  "$leader_id" "$(now_ms)" tampered-peer-request-1 "$proof_tmp/vote-original.json" \
  "$proof_tmp/auth-tampered-vote.json"
probe_service POST "$leader_url/raft/request-vote" "$proof_tmp/auth-tampered-vote.json" \
  "$proof_tmp/vote-tampered.json" "$results_dir/tampered-raft-rejected.json"

sign_service_request gateway-primary "$gateway_seed" POST /raft/request-vote \
  "$leader_id" "$(now_ms)" gateway-peer-request-1 "$proof_tmp/vote-original.json" \
  "$proof_tmp/auth-gateway-vote.json"
probe_service POST "$leader_url/raft/request-vote" "$proof_tmp/auth-gateway-vote.json" \
  "$proof_tmp/vote-original.json" "$results_dir/gateway-peer-forbidden.json"

python3 benchmarks/control_write_probe.py status \
  --url "$leader_url" >"$results_dir/leader-after-rejections.json"

start_worker
start_gateway
python3 benchmarks/full_stack_probe.py wait-gateway \
  --gateway-url "$gateway_url" --policy round-robin --revision 2 \
  --worker-ids cpu-service-auth --timeout 5 >"$results_dir/gateway-ready.json"
python3 benchmarks/full_stack_probe.py requests \
  --gateway-url "$gateway_url" --requests 1 --prompt 'service identity route' \
  --speculative-tokens 2 >"$results_dir/request.json"
python3 benchmarks/full_stack_probe.py stream \
  --gateway-url "$gateway_url" --prompt 'authenticated service stream' \
  --speculative-tokens 2 >"$results_dir/stream.json"
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" --timeout 5 >"$results_dir/final-cluster.json"
python3 benchmarks/control_write_probe.py status \
  --url "$leader_url" >"$results_dir/final-leader.json"

python3 benchmarks/check_service_auth.py \
  --evidence-dir "$results_dir" >"$results_dir/assertions.json"
python3 benchmarks/render_service_auth_svg.py \
  --evidence-dir "$results_dir" \
  --output "$results_dir/service-auth-proof.svg"

cat "$results_dir/assertions.json"
