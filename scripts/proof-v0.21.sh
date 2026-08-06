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
node_a_pid=''
node_b_pid=''
node_c_pid=''
gateway_pid=''

cluster_id='inferlab-primary'
urls='http://127.0.0.1:9931,http://127.0.0.1:9932,http://127.0.0.1:9933'
gateway_url='http://127.0.0.1:9930'
route_key_id='route-2026-b'
route_seed='TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs='
route_public='PUAXw+hDiVqStwqnTRt+vJyYLM8uxJaMwM1V8Sr0Zgw='
writer_id='deploy-bot'
writer_seed='nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A='
writer_public='11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo='
node_a_key_a_seed='TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs='
node_b_key_a_seed='nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A='
node_c_key_a_seed='xaqN9D+fg3vtt0QvMdy3sWbThTUHbwlLhc46LgtEWPc='
gateway_key_a_seed='AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA='
node_a_key_b_seed='AgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgI='
node_b_key_b_seed='AwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwM='
node_c_key_b_seed='BAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQ='
gateway_key_b_seed='AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE='
revoked_key_a_credentials='node-a/key-a,node-b/key-a,node-c/key-a,gateway-primary/key-a'

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
  if [[ -n "${INFERLAB_V21_OUTPUT_DIR:-}" && -d "$results_dir" ]]; then
    mkdir -p "$INFERLAB_V21_OUTPUT_DIR"
    cp "$results_dir"/*.json "$INFERLAB_V21_OUTPUT_DIR/" 2>/dev/null || true
    cp "$results_dir"/*.svg "$INFERLAB_V21_OUTPUT_DIR/" 2>/dev/null || true
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
            raise SystemExit(f"refusing v0.21 proof: 127.0.0.1:{port} is busy: {error}")
PY
}

wait_for_health() {
  local url="$1"
  local attempts=0
  until curl --fail --silent "$url" >/dev/null; do
    attempts=$((attempts + 1))
    if ((attempts >= 400)); then
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

seed_for() {
  case "$1/$2" in
    node-a/key-a) printf '%s' "$node_a_key_a_seed" ;;
    node-a/key-b) printf '%s' "$node_a_key_b_seed" ;;
    node-b/key-a) printf '%s' "$node_b_key_a_seed" ;;
    node-b/key-b) printf '%s' "$node_b_key_b_seed" ;;
    node-c/key-a) printf '%s' "$node_c_key_a_seed" ;;
    node-c/key-b) printf '%s' "$node_c_key_b_seed" ;;
    gateway-primary/key-a) printf '%s' "$gateway_key_a_seed" ;;
    gateway-primary/key-b) printf '%s' "$gateway_key_b_seed" ;;
    *) return 1 ;;
  esac
}

public_key() {
  local service_id="$1"
  local credential_id="$2"
  local seed="$3"
  INFERLAB_SERVICE_ID="$service_id" \
  INFERLAB_SERVICE_CREDENTIAL_ID="$credential_id" \
  INFERLAB_SERVICE_PRIVATE_KEY_B64="$seed" \
    target/debug/service_public_key
}

node_port() {
  case "$1" in
    node-a) printf '9931' ;;
    node-b) printf '9932' ;;
    node-c) printf '9933' ;;
    *) return 1 ;;
  esac
}

node_peers() {
  case "$1" in
    node-a) printf 'node-b=http://127.0.0.1:9932,node-c=http://127.0.0.1:9933' ;;
    node-b) printf 'node-a=http://127.0.0.1:9931,node-c=http://127.0.0.1:9933' ;;
    node-c) printf 'node-a=http://127.0.0.1:9931,node-b=http://127.0.0.1:9932' ;;
    *) return 1 ;;
  esac
}

node_election_min() {
  case "$1" in
    node-a) printf '180' ;;
    node-b) printf '300' ;;
    node-c) printf '420' ;;
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

get_node_pid() {
  case "$1" in
    node-a) printf '%s' "$node_a_pid" ;;
    node-b) printf '%s' "$node_b_pid" ;;
    node-c) printf '%s' "$node_c_pid" ;;
    *) return 1 ;;
  esac
}

start_node() {
  local node_id="$1"
  local credential_id="$2"
  local revoked_credentials="$3"
  local port peers election_min election_max seed
  port="$(node_port "$node_id")"
  peers="$(node_peers "$node_id")"
  election_min="$(node_election_min "$node_id")"
  election_max="$((election_min + 60))"
  seed="$(seed_for "$node_id" "$credential_id")"
  mkdir -p "$proof_tmp/$node_id"
  INFERLAB_RAFT_NODE_ID="$node_id" \
  INFERLAB_RAFT_CLUSTER_ID="$cluster_id" \
  INFERLAB_RAFT_BIND="127.0.0.1:$port" \
  INFERLAB_RAFT_PEERS="$peers" \
  INFERLAB_RAFT_DATA_DIR="$proof_tmp/$node_id" \
  INFERLAB_RAFT_ELECTION_MIN_MS="$election_min" \
  INFERLAB_RAFT_ELECTION_MAX_MS="$election_max" \
  INFERLAB_RAFT_HEARTBEAT_MS=50 \
  INFERLAB_RAFT_RPC_TIMEOUT_MS=100 \
  INFERLAB_RAFT_COMMIT_TIMEOUT_MS=1500 \
  INFERLAB_CONTROL_SIGNING_KEY_ID="$route_key_id" \
  INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64="$route_seed" \
  INFERLAB_CONTROL_WRITER_KEYS="$writer_id=$writer_public" \
  INFERLAB_CONTROL_WRITE_MAX_AGE_MS=1000 \
  INFERLAB_CONTROL_WRITE_MAX_FUTURE_SKEW_MS=100 \
  INFERLAB_SERVICE_ID="$node_id" \
  INFERLAB_SERVICE_CREDENTIAL_ID="$credential_id" \
  INFERLAB_SERVICE_PRIVATE_KEY_B64="$seed" \
  INFERLAB_SERVICE_TRUSTED_KEYS="$service_keys" \
  INFERLAB_SERVICE_REVOKED_CREDENTIALS="$revoked_credentials" \
  INFERLAB_GATEWAY_SERVICE_IDS='gateway-primary' \
  INFERLAB_SERVICE_AUTH_MAX_AGE_MS=1000 \
  INFERLAB_SERVICE_AUTH_MAX_FUTURE_SKEW_MS=100 \
    target/debug/control-plane >"$proof_tmp/$node_id.log" 2>&1 &
  local pid="$!"
  live_pids+=("$pid")
  set_node_pid "$node_id" "$pid"
  wait_for_health "http://127.0.0.1:$port/healthz"
}

stop_node() {
  local node_id="$1"
  local pid
  pid="$(get_node_pid "$node_id")"
  if [[ -n "$pid" ]]; then
    kill "$pid"
    wait "$pid" 2>/dev/null || true
    set_node_pid "$node_id" ''
  fi
}

sign_service_request() {
  local service_id="$1"
  local credential_id="$2"
  local method="$3"
  local path="$4"
  local audience_id="$5"
  local issued_at_ms="$6"
  local nonce="$7"
  local body="$8"
  local output="$9"
  local seed
  seed="$(seed_for "$service_id" "$credential_id")"
  INFERLAB_SERVICE_ID="$service_id" \
  INFERLAB_SERVICE_CREDENTIAL_ID="$credential_id" \
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
  INFERLAB_CPU_WORKER_ID=cpu-credential-rotation \
  INFERLAB_CPU_BIND=127.0.0.1:9934 \
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
  wait_for_health 'http://127.0.0.1:9934/health'
}

start_gateway() {
  local credential_id="$1"
  local seed
  seed="$(seed_for gateway-primary "$credential_id")"
  INFERLAB_BIND=127.0.0.1:9930 \
  INFERLAB_CONTROL_PLANE_URLS="$urls" \
  INFERLAB_CONTROL_CLUSTER_ID="$cluster_id" \
  INFERLAB_CONTROL_TRUSTED_KEYS="$route_key_id=$route_public" \
  INFERLAB_CONTROL_BOOTSTRAP_WAIT_MS=1500 \
  INFERLAB_CONTROL_POLL_MS=50 \
  INFERLAB_GATEWAY_SERVICE_ID='gateway-primary' \
  INFERLAB_GATEWAY_SERVICE_CREDENTIAL_ID="$credential_id" \
  INFERLAB_GATEWAY_SERVICE_PRIVATE_KEY_B64="$seed" \
  INFERLAB_CONTROL_SERVICE_TARGETS='node-a=http://127.0.0.1:9931,node-b=http://127.0.0.1:9932,node-c=http://127.0.0.1:9933' \
  INFERLAB_ROUTING_SNAPSHOT_PATH="$snapshot_path" \
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
  wait_for_health "$gateway_url/health"
}

stop_gateway() {
  if [[ -n "$gateway_pid" ]]; then
    kill "$gateway_pid"
    wait "$gateway_pid" 2>/dev/null || true
    gateway_pid=''
  fi
}

roll_control_nodes() {
  local leader_id="$1"
  local credential_id="$2"
  local revoked_credentials="$3"
  local phase="$4"
  local node_id
  for node_id in node-a node-b node-c; do
    if [[ "$node_id" == "$leader_id" ]]; then
      continue
    fi
    stop_node "$node_id"
    start_node "$node_id" "$credential_id" "$revoked_credentials"
    python3 benchmarks/full_stack_probe.py wait-leader \
      --urls "$urls" --timeout 5 >"$results_dir/$phase-$node_id.json"
  done
  stop_node "$leader_id"
  start_node "$leader_id" "$credential_id" "$revoked_credentials"
  python3 benchmarks/full_stack_probe.py wait-leader \
    --urls "$urls" --timeout 5 >"$results_dir/$phase-$leader_id.json"
}

check_ports_are_free 9930 9931 9932 9933 9934
cargo build --workspace --bins --quiet

node_a_key_a_public="$(public_key node-a key-a "$node_a_key_a_seed")"
node_a_key_b_public="$(public_key node-a key-b "$node_a_key_b_seed")"
node_b_key_a_public="$(public_key node-b key-a "$node_b_key_a_seed")"
node_b_key_b_public="$(public_key node-b key-b "$node_b_key_b_seed")"
node_c_key_a_public="$(public_key node-c key-a "$node_c_key_a_seed")"
node_c_key_b_public="$(public_key node-c key-b "$node_c_key_b_seed")"
gateway_key_a_public="$(public_key gateway-primary key-a "$gateway_key_a_seed")"
gateway_key_b_public="$(public_key gateway-primary key-b "$gateway_key_b_seed")"
service_keys="node-a/key-a=$node_a_key_a_public,node-a/key-b=$node_a_key_b_public,node-b/key-a=$node_b_key_a_public,node-b/key-b=$node_b_key_b_public,node-c/key-a=$node_c_key_a_public,node-c/key-b=$node_c_key_b_public,gateway-primary/key-a=$gateway_key_a_public,gateway-primary/key-b=$gateway_key_b_public"

cat >"$proof_tmp/config.json" <<'JSON'
{
  "routing_policy": "round-robin",
  "workers": [
    {"id": "cpu-credential-rotation", "base_url": "http://127.0.0.1:9934", "weight": 1}
  ]
}
JSON

start_node node-a key-a ''
start_node node-b key-a ''
start_node node-c key-a ''

python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" --timeout 5 >"$results_dir/initial-cluster.json"
leader_url="$(json_field "$results_dir/initial-cluster.json" leader_url)"

INFERLAB_CONTROL_WRITER_ID="$writer_id" \
INFERLAB_CONTROL_WRITER_PRIVATE_KEY_B64="$writer_seed" \
  target/debug/sign_control_write \
    "$cluster_id" 0 now credential-rotation-route-1 "$proof_tmp/config.json" \
    >"$proof_tmp/write.json"
python3 benchmarks/control_write_probe.py submit \
  --url "$leader_url" --body "$proof_tmp/write.json" \
  >"$results_dir/write-committed.json"

start_worker
start_gateway key-a
python3 benchmarks/full_stack_probe.py wait-gateway \
  --gateway-url "$gateway_url" --policy round-robin --revision 2 \
  --worker-ids cpu-credential-rotation --timeout 5 >"$results_dir/gateway-key-a-ready.json"

initial_leader_id="$(json_field "$results_dir/initial-cluster.json" leader_id)"
roll_control_nodes "$initial_leader_id" key-b '' control-key-b-step
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" --timeout 5 >"$results_dir/after-control-key-b.json"

overlap_leader_url="$(json_field "$results_dir/after-control-key-b.json" leader_url)"
overlap_leader_id="$(json_field "$results_dir/after-control-key-b.json" leader_id)"
sign_service_request gateway-primary key-a GET /v1/control/config \
  "$overlap_leader_id" "$(now_ms)" overlap-key-a-still-valid - \
  "$proof_tmp/auth-overlap-key-a.json"
probe_service GET "$overlap_leader_url/v1/control/config" \
  "$proof_tmp/auth-overlap-key-a.json" - "$results_dir/overlap-key-a-valid.json"

stop_gateway
start_gateway key-b
python3 benchmarks/full_stack_probe.py wait-gateway \
  --gateway-url "$gateway_url" --policy round-robin --revision 2 \
  --worker-ids cpu-credential-rotation --timeout 5 >"$results_dir/gateway-key-b-ready.json"

pre_revoke_leader_id="$(json_field "$results_dir/after-control-key-b.json" leader_id)"
roll_control_nodes "$pre_revoke_leader_id" key-b "$revoked_key_a_credentials" revoke-key-a-step
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" --timeout 5 >"$results_dir/after-key-a-revocation.json"

final_leader_url="$(json_field "$results_dir/after-key-a-revocation.json" leader_url)"
final_leader_id="$(json_field "$results_dir/after-key-a-revocation.json" leader_id)"
python3 benchmarks/control_write_probe.py status \
  --url "$final_leader_url" >"$results_dir/before-revoked-attacks.json"
final_term="$(json_field "$results_dir/before-revoked-attacks.json" response.body.term)"

sign_service_request gateway-primary key-a GET /v1/control/config \
  "$final_leader_id" "$(now_ms)" revoked-gateway-key-a-request - \
  "$proof_tmp/auth-revoked-gateway.json"
probe_service GET "$final_leader_url/v1/control/config" \
  "$proof_tmp/auth-revoked-gateway.json" - "$results_dir/revoked-gateway-key-a.json"

attacker_id='node-a'
if [[ "$final_leader_id" == 'node-a' ]]; then
  attacker_id='node-b'
fi
cat >"$proof_tmp/revoked-vote.json" <<JSON
{"cluster_id": "$cluster_id", "term": $((final_term + 50)), "candidate_id": "$attacker_id", "last_log_index": 0, "last_log_term": 0}
JSON
sign_service_request "$attacker_id" key-a POST /raft/request-vote \
  "$final_leader_id" "$(now_ms)" revoked-peer-key-a-request \
  "$proof_tmp/revoked-vote.json" "$proof_tmp/auth-revoked-peer.json"
probe_service POST "$final_leader_url/raft/request-vote" \
  "$proof_tmp/auth-revoked-peer.json" "$proof_tmp/revoked-vote.json" \
  "$results_dir/revoked-peer-key-a.json"

sign_service_request gateway-primary key-b GET /v1/control/config \
  "$final_leader_id" "$(now_ms)" valid-gateway-key-b-request - \
  "$proof_tmp/auth-valid-gateway-key-b.json"
probe_service GET "$final_leader_url/v1/control/config" \
  "$proof_tmp/auth-valid-gateway-key-b.json" - "$results_dir/valid-gateway-key-b.json"

python3 benchmarks/control_write_probe.py status \
  --url "$final_leader_url" >"$results_dir/after-revoked-attacks.json"
python3 benchmarks/full_stack_probe.py requests \
  --gateway-url "$gateway_url" --requests 1 --prompt 'rotated credential route' \
  --speculative-tokens 2 >"$results_dir/request.json"
python3 benchmarks/full_stack_probe.py stream \
  --gateway-url "$gateway_url" --prompt 'revoked old credential stream' \
  --speculative-tokens 2 >"$results_dir/stream.json"
python3 benchmarks/full_stack_probe.py wait-gateway \
  --gateway-url "$gateway_url" --policy round-robin --revision 2 \
  --worker-ids cpu-credential-rotation --timeout 5 >"$results_dir/final-gateway.json"
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" --timeout 5 >"$results_dir/final-cluster.json"

python3 benchmarks/check_service_credential_rotation.py \
  --evidence-dir "$results_dir" >"$results_dir/assertions.json"
python3 benchmarks/render_service_credential_rotation_svg.py \
  --evidence-dir "$results_dir" \
  --output "$results_dir/service-credential-rotation-proof.svg"

cat "$results_dir/assertions.json"
