#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

if [[ -x "$project_root/.tools/cargo/bin/cargo" ]]; then
  export RUSTUP_HOME="$project_root/.tools/rustup"
  export CARGO_HOME="$project_root/.tools/cargo"
  export PATH="$CARGO_HOME/bin:$PATH"
fi

# Do not let a caller's optional public-demo key change this local proof API.
unset INFERLAB_PUBLIC_API_KEYS || true

proof_tmp_root="${TMPDIR:-/tmp}"
proof_tmp="$(mktemp -d "$proof_tmp_root/inferlab-v023.XXXXXX")"
results_dir="$proof_tmp/results"
policy_dir="$proof_tmp/policies"
snapshot_path="$proof_tmp/gateway-routing.json"
mkdir -p "$results_dir" "$policy_dir"

live_pids=()
node_a_pid=''
node_b_pid=''
node_c_pid=''
gateway_pid=''
distributor_pid=''
relay_pid=''

cluster_id='inferlab-primary'
urls='http://127.0.0.1:9941,http://127.0.0.1:9942,http://127.0.0.1:9943'
gateway_url='http://127.0.0.1:9940'
distributor_url='http://127.0.0.1:9945'
relay_url='http://127.0.0.1:9946'
route_key_id='route-2026-b'
route_seed='TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs='
route_public='PUAXw+hDiVqStwqnTRt+vJyYLM8uxJaMwM1V8Sr0Zgw='
writer_id='deploy-bot'
writer_seed='nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A='
writer_public='11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo='
trust_root_id='service-trust-root-a'
trust_root_seed='BQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQU='
node_a_seed='TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs='
node_b_seed='nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A='
node_c_seed='xaqN9D+fg3vtt0QvMdy3sWbThTUHbwlLhc46LgtEWPc='
gateway_key_a_seed='AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA='
gateway_key_b_seed='AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE='

cleanup() {
  local owned_pids=("${live_pids[@]}")
  local pid
  for pid in "${owned_pids[@]}"; do
    shutdown_child "$pid"
    forget_pid "$pid"
  done
  if [[ -n "${INFERLAB_V23_OUTPUT_DIR:-}" && -d "$results_dir" ]]; then
    mkdir -p "$INFERLAB_V23_OUTPUT_DIR"
    cp "$results_dir"/*.json "$INFERLAB_V23_OUTPUT_DIR/" 2>/dev/null || true
    cp "$results_dir"/*.svg "$INFERLAB_V23_OUTPUT_DIR/" 2>/dev/null || true
  fi
  if [[ -n "$proof_tmp" && -d "$proof_tmp" ]]; then
    case "$(basename "$proof_tmp")" in
      inferlab-v023.*) rm -rf -- "$proof_tmp" ;;
      *) echo "refusing to remove unexpected proof path: $proof_tmp" >&2 ;;
    esac
  fi
}
trap cleanup EXIT INT TERM

forget_pid() {
  local forgotten="$1"
  local pid
  # Bash 3.2 with `set -u` treats an empty array expansion as unbound. Keep a
  # private sentinel and slice it away so forgetting the last PID stays safe.
  local retained=(sentinel)
  for pid in "${live_pids[@]}"; do
    if [[ "$pid" != "$forgotten" ]]; then
      retained+=("$pid")
    fi
  done
  live_pids=("${retained[@]:1}")
}

shutdown_child() {
  local pid="$1"
  local attempt
  local state
  if [[ ! "$pid" =~ ^[1-9][0-9]*$ ]]; then
    return
  fi
  kill -TERM "$pid" 2>/dev/null || true
  for ((attempt = 0; attempt < 100; attempt++)); do
    if ! kill -0 "$pid" 2>/dev/null; then
      break
    fi
    state="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
    if [[ "$state" == *Z* ]]; then
      break
    fi
    sleep 0.02
  done
  if kill -0 "$pid" 2>/dev/null; then
    state="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
    if [[ "$state" != *Z* ]]; then
      kill -KILL "$pid" 2>/dev/null || true
    fi
  fi
  wait "$pid" 2>/dev/null || true
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
                f"refusing v0.23 proof: 127.0.0.1:{port} is busy: {error}"
            )
PY
}

wait_for_health() {
  local url="$1"
  local attempts=0
  until curl --connect-timeout 0.05 --max-time 0.10 --fail --silent "$url" >/dev/null; do
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

now_ms() {
  python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
}

service_seed() {
  case "$1/$2" in
    node-a/key-a) printf '%s' "$node_a_seed" ;;
    node-b/key-a) printf '%s' "$node_b_seed" ;;
    node-c/key-a) printf '%s' "$node_c_seed" ;;
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
    node-a) printf '9941' ;;
    node-b) printf '9942' ;;
    node-c) printf '9943' ;;
    *) return 1 ;;
  esac
}

node_peers() {
  case "$1" in
    node-a) printf 'node-b=http://127.0.0.1:9942,node-c=http://127.0.0.1:9943' ;;
    node-b) printf 'node-a=http://127.0.0.1:9941,node-c=http://127.0.0.1:9943' ;;
    node-c) printf 'node-a=http://127.0.0.1:9941,node-b=http://127.0.0.1:9942' ;;
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

node_distributor_url() {
  case "$1" in
    node-c) printf '%s' "$relay_url" ;;
    node-a|node-b) printf '%s' "$distributor_url" ;;
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

start_distributor() {
  INFERLAB_TRUST_DISTRIBUTOR_BIND='127.0.0.1:9945' \
  INFERLAB_TRUST_DISTRIBUTOR_CLUSTER_ID="$cluster_id" \
  INFERLAB_SERVICE_TRUST_ROOT_KEYS="$trust_root_id=$trust_root_public" \
  INFERLAB_SERVICE_TRUST_REVOKED_ROOT_KEY_IDS='' \
  INFERLAB_TRUST_DISTRIBUTOR_STATE_PATH="$proof_tmp/distributor-state.json" \
  INFERLAB_TRUST_DISTRIBUTOR_EXPECTED_RECEIVERS='node-a/key-a,node-b/key-a,node-c/key-a' \
  INFERLAB_TRUST_DISTRIBUTOR_MAX_BODY_BYTES=262144 \
    target/debug/trust-distributor >"$proof_tmp/distributor.log" 2>&1 &
  distributor_pid="$!"
  live_pids+=("$distributor_pid")
  wait_for_health "$distributor_url/health"
}

stop_distributor() {
  if [[ -n "$distributor_pid" ]]; then
    local stopped_pid="$distributor_pid"
    shutdown_child "$stopped_pid"
    forget_pid "$stopped_pid"
    distributor_pid=''
  fi
}

start_relay() {
  python3 benchmarks/trust_distribution_probe.py proxy \
    --bind 127.0.0.1:9946 --target "$distributor_url" \
    >"$proof_tmp/relay.log" 2>&1 &
  relay_pid="$!"
  live_pids+=("$relay_pid")
  wait_for_health "$relay_url/health"
}

stop_relay() {
  if [[ -n "$relay_pid" ]]; then
    local stopped_pid="$relay_pid"
    shutdown_child "$stopped_pid"
    forget_pid "$stopped_pid"
    relay_pid=''
  fi
}

post_snapshot() {
  local snapshot="$1"
  local output="$2"
  python3 - "$distributor_url/v1/service-trust/snapshot" "$snapshot" >"$output" <<'PY'
import json
import sys
import urllib.error
import urllib.request
from pathlib import Path

request = urllib.request.Request(
    sys.argv[1],
    data=Path(sys.argv[2]).read_bytes(),
    headers={"content-type": "application/json"},
    method="POST",
)
try:
    response = urllib.request.urlopen(request, timeout=2)
except urllib.error.HTTPError as error:
    response = error
with response:
    raw = response.read()
    try:
        body = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        body = raw.decode("utf-8", errors="replace")
    print(json.dumps({
        "schema": "inferlab.trust-distribution-publication-capture.v0.23",
        "status": response.status,
        "etag": response.headers.get("etag"),
        "body": body,
    }, indent=2, sort_keys=True))
PY
}

start_node() {
  local node_id="$1"
  local port peers election_min election_max seed remote
  port="$(node_port "$node_id")"
  peers="$(node_peers "$node_id")"
  election_min="$(node_election_min "$node_id")"
  election_max="$((election_min + 60))"
  seed="$(service_seed "$node_id" key-a)"
  remote="$(node_distributor_url "$node_id")"
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
  INFERLAB_SERVICE_CREDENTIAL_ID='key-a' \
  INFERLAB_SERVICE_PRIVATE_KEY_B64="$seed" \
  INFERLAB_SERVICE_TRUST_DISTRIBUTOR_URL="$remote" \
  INFERLAB_SERVICE_TRUST_CACHE_PATH="$proof_tmp/$node_id/service-trust-cache.json" \
  INFERLAB_SERVICE_TRUST_STATE_PATH="$proof_tmp/$node_id/service-trust-floor.json" \
  INFERLAB_SERVICE_TRUST_ROOT_KEYS="$trust_root_id=$trust_root_public" \
  INFERLAB_SERVICE_TRUST_REVOKED_ROOT_KEY_IDS='' \
  INFERLAB_SERVICE_TRUST_POLL_MS=25 \
  INFERLAB_SERVICE_TRUST_REQUEST_TIMEOUT_MS=100 \
  INFERLAB_SERVICE_TRUST_MAX_BACKOFF_MS=200 \
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
    shutdown_child "$pid"
    forget_pid "$pid"
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
  seed="$(service_seed "$service_id" "$credential_id")"
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
  local signed_authentication="$3"
  local body="$4"
  local output="$5"
  local arguments=(--method "$method" --url "$url")
  if [[ "$signed_authentication" != '-' ]]; then
    arguments+=(--authentication "$signed_authentication")
  fi
  if [[ "$body" != '-' ]]; then
    arguments+=(--body "$body")
  fi
  python3 benchmarks/service_request_probe.py "${arguments[@]}" >"$output"
}

sign_policy() {
  local policy="$1"
  local output="$2"
  INFERLAB_SERVICE_TRUST_ROOT_KEY_ID="$trust_root_id" \
  INFERLAB_SERVICE_TRUST_ROOT_PRIVATE_KEY_B64="$trust_root_seed" \
    target/debug/sign_service_trust "$policy" >"$output"
}

start_worker() {
  INFERLAB_CPU_WORKER_ID=cpu-distributed-trust \
  INFERLAB_CPU_BIND=127.0.0.1:9944 \
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
  wait_for_health 'http://127.0.0.1:9944/health'
}

start_gateway() {
  local credential_id="$1"
  local seed
  seed="$(service_seed gateway-primary "$credential_id")"
  INFERLAB_BIND=127.0.0.1:9940 \
  INFERLAB_CONTROL_PLANE_URLS="$urls" \
  INFERLAB_CONTROL_CLUSTER_ID="$cluster_id" \
  INFERLAB_CONTROL_TRUSTED_KEYS="$route_key_id=$route_public" \
  INFERLAB_CONTROL_BOOTSTRAP_WAIT_MS=1500 \
  INFERLAB_CONTROL_POLL_MS=50 \
  INFERLAB_GATEWAY_SERVICE_ID='gateway-primary' \
  INFERLAB_GATEWAY_SERVICE_CREDENTIAL_ID="$credential_id" \
  INFERLAB_GATEWAY_SERVICE_PRIVATE_KEY_B64="$seed" \
  INFERLAB_CONTROL_SERVICE_TARGETS='node-a=http://127.0.0.1:9941,node-b=http://127.0.0.1:9942,node-c=http://127.0.0.1:9943' \
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
    local stopped_pid="$gateway_pid"
    shutdown_child "$stopped_pid"
    forget_pid "$stopped_pid"
    gateway_pid=''
  fi
}

check_ports_are_free 9940 9941 9942 9943 9944 9945 9946
cargo build --workspace --bins --quiet

trust_root_public="$(
  INFERLAB_SERVICE_TRUST_ROOT_KEY_ID="$trust_root_id" \
  INFERLAB_SERVICE_TRUST_ROOT_PRIVATE_KEY_B64="$trust_root_seed" \
    target/debug/service_trust_public_key
)"
node_a_public="$(public_key node-a key-a "$node_a_seed")"
node_b_public="$(public_key node-b key-a "$node_b_seed")"
node_c_public="$(public_key node-c key-a "$node_c_seed")"
gateway_key_a_public="$(public_key gateway-primary key-a "$gateway_key_a_seed")"
gateway_key_b_public="$(public_key gateway-primary key-b "$gateway_key_b_seed")"

issued_at="$(now_ms)"
python3 - "$issued_at" "$node_a_public" "$node_b_public" "$node_c_public" \
  "$gateway_key_a_public" "$gateway_key_b_public" "$policy_dir" <<'PY'
import json
import sys
from pathlib import Path

issued = int(sys.argv[1])
node_a, node_b, node_c, gateway_a, gateway_b = sys.argv[2:7]
directory = Path(sys.argv[7])
base = [
    {"service_id": "node-a", "credential_id": "key-a", "public_key_base64": node_a},
    {"service_id": "node-b", "credential_id": "key-a", "public_key_base64": node_b},
    {"service_id": "node-c", "credential_id": "key-a", "public_key_base64": node_c},
    {"service_id": "gateway-primary", "credential_id": "key-a", "public_key_base64": gateway_a},
]
overlap = base + [
    {"service_id": "gateway-primary", "credential_id": "key-b", "public_key_base64": gateway_b}
]
policies = {
    "g1": (1, issued + 1, base, []),
    "g2": (2, issued + 2, overlap, []),
    "g3": (3, issued + 3, overlap, [{"service_id": "gateway-primary", "credential_id": "key-a"}]),
    "g3-fork": (3, issued + 30, overlap, [{"service_id": "gateway-primary", "credential_id": "key-a"}]),
    "g4": (4, issued + 4, overlap, [{"service_id": "gateway-primary", "credential_id": "key-a"}]),
}
for name, (generation, policy_time, credentials, revoked) in policies.items():
    payload = {
        "schema": "inferlab.service-trust-policy.v1",
        "cluster_id": "inferlab-primary",
        "generation": generation,
        "issued_at_ms": policy_time,
        "trusted_credentials": credentials,
        "revoked_service_ids": [],
        "revoked_credentials": revoked,
        "gateway_service_ids": ["gateway-primary"],
    }
    (directory / f"policy-{name}.json").write_text(
        json.dumps(payload, indent=2) + "\n", encoding="utf-8"
    )
PY

sign_policy "$policy_dir/policy-g1.json" "$policy_dir/snapshot-g1.json"
sign_policy "$policy_dir/policy-g2.json" "$policy_dir/snapshot-g2.json"
sign_policy "$policy_dir/policy-g3.json" "$policy_dir/snapshot-g3.json"
sign_policy "$policy_dir/policy-g3-fork.json" "$policy_dir/snapshot-g3-fork.json"
sign_policy "$policy_dir/policy-g4.json" "$policy_dir/snapshot-g4-valid.json"
python3 - "$policy_dir/snapshot-g4-valid.json" "$policy_dir/snapshot-g4-tampered.json" <<'PY'
import json
import sys
from pathlib import Path

snapshot = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
snapshot["generation"] = 5
Path(sys.argv[2]).write_text(json.dumps(snapshot, indent=2) + "\n", encoding="utf-8")
PY

start_distributor
python3 benchmarks/trust_distribution_probe.py capture \
  --url "$distributor_url/v1/service-trust/status" \
  >"$results_dir/distributor-empty.json"
post_snapshot "$policy_dir/snapshot-g1.json" "$results_dir/publish-g1.json"
wait_for_health "$distributor_url/readyz"
start_relay

start_node node-a
start_node node-b
start_node node-c
initial_node_a_pid="$node_a_pid"
initial_node_b_pid="$node_b_pid"
initial_node_c_pid="$node_c_pid"

python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" --timeout 5 >"$proof_tmp/initial-leader.json"
python3 benchmarks/trust_distribution_probe.py wait-controls \
  --urls "$urls" --generation 1 --bootstrap-source remote --timeout 5 \
  >"$results_dir/initial-controls.json"
python3 benchmarks/trust_distribution_probe.py wait-distributor \
  --url "$distributor_url" --generation 1 \
  --acked-receivers 'node-a/key-a,node-b/key-a,node-c/key-a' --timeout 5 \
  >"$results_dir/generation-1-receipts.json"

cat >"$proof_tmp/config.json" <<'JSON'
{
  "routing_policy": "round-robin",
  "workers": [
    {"id": "cpu-distributed-trust", "base_url": "http://127.0.0.1:9944", "weight": 1}
  ]
}
JSON

python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" --timeout 5 >"$proof_tmp/leader.json"
leader_url="$(json_field "$proof_tmp/leader.json" leader_url)"
INFERLAB_CONTROL_WRITER_ID="$writer_id" \
INFERLAB_CONTROL_WRITER_PRIVATE_KEY_B64="$writer_seed" \
  target/debug/sign_control_write \
    "$cluster_id" 0 now distributed-trust-route-1 "$proof_tmp/config.json" \
    >"$proof_tmp/write.json"
python3 benchmarks/control_write_probe.py submit \
  --url "$leader_url" --body "$proof_tmp/write.json" \
  >"$results_dir/write-committed.json"

start_worker
start_gateway key-a
python3 benchmarks/full_stack_probe.py wait-gateway \
  --gateway-url "$gateway_url" --policy round-robin --revision 2 \
  --worker-ids cpu-distributed-trust --timeout 5 \
  >"$results_dir/gateway-key-a-ready.json"

# Partition only node C's distributor transport. Raft and inference traffic are
# unaffected because the proof owns and stops only this relay PID.
stop_relay
post_snapshot "$policy_dir/snapshot-g2.json" "$results_dir/publish-g2.json"
python3 benchmarks/trust_distribution_probe.py wait-controls \
  --urls 'http://127.0.0.1:9941,http://127.0.0.1:9942' \
  --generation 2 --timeout 5 >"$results_dir/generation-2-partial-controls.json"
python3 benchmarks/trust_distribution_probe.py wait-controls \
  --urls 'http://127.0.0.1:9943' --generation 1 --minimum-rejections 1 \
  --timeout 5 >"$results_dir/generation-2-withheld-c-wait.json"
python3 benchmarks/trust_distribution_probe.py capture \
  --url 'http://127.0.0.1:9943/v1/control/status' \
  >"$results_dir/generation-2-withheld-c.json"
python3 benchmarks/trust_distribution_probe.py wait-distributor \
  --url "$distributor_url" --generation 2 \
  --acked-receivers 'node-a/key-a,node-b/key-a' \
  --pending-receivers 'node-c/key-a' --timeout 5 \
  >"$results_dir/generation-2-partial-receipts.json"

start_relay
python3 benchmarks/trust_distribution_probe.py wait-controls \
  --urls "$urls" --generation 2 --timeout 5 \
  >"$results_dir/generation-2-convergence.json"
python3 benchmarks/trust_distribution_probe.py wait-distributor \
  --url "$distributor_url" --generation 2 \
  --acked-receivers 'node-a/key-a,node-b/key-a,node-c/key-a' --timeout 5 \
  >"$results_dir/generation-2-receipts.json"

generation_2_leader_id="$(python3 - "$results_dir/generation-2-convergence.json" <<'PY'
import json
import sys
sample = json.load(open(sys.argv[1], encoding="utf-8"))
print(next(status["body"]["node_id"] for status in sample["statuses"] if status["body"]["role"] == "leader"))
PY
)"
generation_2_leader_url="$(python3 - "$results_dir/generation-2-convergence.json" <<'PY'
import json
import sys
sample = json.load(open(sys.argv[1], encoding="utf-8"))
print(next(status["url"].removesuffix("/v1/control/status") for status in sample["statuses"] if status["body"]["role"] == "leader"))
PY
)"
sign_service_request gateway-primary key-b GET /v1/control/config \
  "$generation_2_leader_id" "$(now_ms)" distributed-g2-key-b-read - \
  "$proof_tmp/auth-key-b-g2.json"
probe_service GET "$generation_2_leader_url/v1/control/config" \
  "$proof_tmp/auth-key-b-g2.json" - "$results_dir/generation-2-key-b-valid.json"

stop_gateway
start_gateway key-b
python3 benchmarks/full_stack_probe.py wait-gateway \
  --gateway-url "$gateway_url" --policy round-robin --revision 2 \
  --worker-ids cpu-distributed-trust --timeout 5 \
  >"$results_dir/gateway-key-b-ready.json"

post_snapshot "$policy_dir/snapshot-g3.json" "$results_dir/publish-g3.json"
python3 benchmarks/trust_distribution_probe.py wait-controls \
  --urls "$urls" --generation 3 --timeout 5 \
  >"$results_dir/generation-3-convergence.json"
python3 benchmarks/trust_distribution_probe.py wait-distributor \
  --url "$distributor_url" --generation 3 \
  --acked-receivers 'node-a/key-a,node-b/key-a,node-c/key-a' --timeout 5 \
  >"$results_dir/generation-3-receipts.json"

python3 - "$initial_node_a_pid" "$initial_node_b_pid" "$initial_node_c_pid" \
  "$node_a_pid" "$node_b_pid" "$node_c_pid" \
  >"$results_dir/online-process-continuity.json" <<'PY'
import json
import sys

names = ["node-a", "node-b", "node-c"]
before = dict(zip(names, map(int, sys.argv[1:4])))
after = dict(zip(names, map(int, sys.argv[4:7])))
print(json.dumps({
    "schema": "inferlab.distributed-service-trust-process-continuity.v0.23",
    "before": before,
    "after_generation_3": after,
    "unchanged_before_cache_restart": before == after,
}, indent=2, sort_keys=True))
PY

generation_3_leader_id="$(python3 - "$results_dir/generation-3-convergence.json" <<'PY'
import json
import sys
sample = json.load(open(sys.argv[1], encoding="utf-8"))
print(next(status["body"]["node_id"] for status in sample["statuses"] if status["body"]["role"] == "leader"))
PY
)"
generation_3_leader_url="$(python3 - "$results_dir/generation-3-convergence.json" <<'PY'
import json
import sys
sample = json.load(open(sys.argv[1], encoding="utf-8"))
print(next(status["url"].removesuffix("/v1/control/status") for status in sample["statuses"] if status["body"]["role"] == "leader"))
PY
)"
sign_service_request gateway-primary key-a GET /v1/control/config \
  "$generation_3_leader_id" "$(now_ms)" distributed-g3-key-a-revoked - \
  "$proof_tmp/auth-key-a-g3.json"
probe_service GET "$generation_3_leader_url/v1/control/config" \
  "$proof_tmp/auth-key-a-g3.json" - "$results_dir/generation-3-key-a-revoked.json"
sign_service_request gateway-primary key-b GET /v1/control/config \
  "$generation_3_leader_id" "$(now_ms)" distributed-g3-key-b-valid - \
  "$proof_tmp/auth-key-b-g3.json"
probe_service GET "$generation_3_leader_url/v1/control/config" \
  "$proof_tmp/auth-key-b-g3.json" - "$results_dir/generation-3-key-b-valid.json"

post_snapshot "$policy_dir/snapshot-g2.json" "$results_dir/rollback-publication.json"
post_snapshot "$policy_dir/snapshot-g3-fork.json" "$results_dir/fork-publication.json"
post_snapshot "$policy_dir/snapshot-g4-tampered.json" "$results_dir/tamper-publication.json"
python3 benchmarks/trust_distribution_probe.py wait-controls \
  --urls "$urls" --generation 3 --timeout 5 \
  >"$results_dir/after-attacks-controls.json"

restart_node="$(python3 - "$results_dir/generation-3-convergence.json" <<'PY'
import json
import sys
sample = json.load(open(sys.argv[1], encoding="utf-8"))
print(next(status["body"]["node_id"] for status in sample["statuses"] if status["body"]["role"] == "follower"))
PY
)"
old_restart_pid="$(get_node_pid "$restart_node")"
stop_node "$restart_node"
stop_distributor
if curl --fail --silent "$distributor_url/health" >/dev/null 2>&1; then
  echo 'distributor unexpectedly remained available after its exact PID stopped' >&2
  exit 1
fi
start_node "$restart_node"
new_restart_pid="$(get_node_pid "$restart_node")"
restart_url="http://127.0.0.1:$(node_port "$restart_node")"
python3 benchmarks/trust_distribution_probe.py wait-controls \
  --urls "$restart_url" --generation 3 --bootstrap-source cache \
  --minimum-receipt-failures 1 --timeout 5 \
  >"$results_dir/cache-restart-wait.json"
python3 - "$old_restart_pid" "$new_restart_pid" "$restart_node" \
  "$results_dir/cache-restart-wait.json" >"$results_dir/cache-restart.json" <<'PY'
import json
import sys

sample = json.load(open(sys.argv[4], encoding="utf-8"))
print(json.dumps({
    "schema": "inferlab.distributed-service-trust-cache-restart.v0.23",
    "node_id": sys.argv[3],
    "old_pid": int(sys.argv[1]),
    "new_pid": int(sys.argv[2]),
    "distributor_unavailable": True,
    "status": sample["statuses"][0],
}, indent=2, sort_keys=True))
PY

python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" --timeout 5 >"$results_dir/final-cluster.json"
python3 benchmarks/full_stack_probe.py requests \
  --gateway-url "$gateway_url" --requests 1 \
  --prompt 'distributed trust cache route' --speculative-tokens 2 \
  >"$results_dir/request.json"
python3 benchmarks/full_stack_probe.py stream \
  --gateway-url "$gateway_url" --prompt 'receipt fenced trust stream' \
  --speculative-tokens 2 >"$results_dir/stream.json"
python3 benchmarks/full_stack_probe.py wait-gateway \
  --gateway-url "$gateway_url" --policy round-robin --revision 2 \
  --worker-ids cpu-distributed-trust --timeout 5 \
  >"$results_dir/final-gateway.json"

python3 benchmarks/trust_distribution_probe.py sanitize-evidence \
  --evidence-dir "$results_dir" --proof-root "$proof_tmp" \
  >"$proof_tmp/evidence-sanitization.json"
mv "$proof_tmp/evidence-sanitization.json" "$results_dir/evidence-sanitization.json"
python3 benchmarks/check_distributed_service_trust.py \
  --evidence-dir "$results_dir" >"$results_dir/assertions.json"
python3 benchmarks/render_distributed_service_trust_svg.py \
  --evidence-dir "$results_dir" \
  --output "$results_dir/distributed-service-trust-proof.svg"

cat "$results_dir/assertions.json"
