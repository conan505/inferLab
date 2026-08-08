#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

if [[ -x "$project_root/.tools/cargo/bin/cargo" ]]; then
  export RUSTUP_HOME="$project_root/.tools/rustup"
  export CARGO_HOME="$project_root/.tools/cargo"
  export PATH="$CARGO_HOME/bin:$PATH"
fi

export NO_PROXY='127.0.0.1,localhost'
export no_proxy="$NO_PROXY"
unset HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy || true
umask 077

proof_tmp_root="${TMPDIR:-/tmp}"
proof_tmp="$(mktemp -d "$proof_tmp_root/inferlab-v026.XXXXXX")"
results_dir="$proof_tmp/results"
mkdir -p "$results_dir"

# A permanent non-numeric sentinel keeps Bash 3.2's `set -u` handling of empty
# arrays from turning early-failure cleanup into a second error.
live_pids=(sentinel)
control_a_pid=''
control_b_pid=''
control_c_pid=''
worker_pid=''
batch_pid=''
trust_pid=''
link_pid=''
gateway_primary_pid=''
gateway_retry_pid=''
retry_server_pid=''

cluster_id='inferlab-observability'
control_urls='http://127.0.0.1:10063,http://127.0.0.1:10064,http://127.0.0.1:10065'
gateway_primary_url='http://127.0.0.1:10060'
gateway_retry_url='http://127.0.0.1:10068'
worker_url='http://127.0.0.1:10061'
batch_url='http://127.0.0.1:10062'
trust_url='http://127.0.0.1:10066'
link_url='http://127.0.0.1:10067'
retry_first_url='http://127.0.0.1:10069'
retry_second_url='http://127.0.0.1:10070'

route_key_id='route-2026-b'
route_seed='TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs='
route_public='PUAXw+hDiVqStwqnTRt+vJyYLM8uxJaMwM1V8Sr0Zgw='
writer_id='deploy-bot'
writer_seed='nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A='
writer_public='11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo='
control_a_seed='TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs='
control_b_seed='nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A='
control_c_seed='xaqN9D+fg3vtt0QvMdy3sWbThTUHbwlLhc46LgtEWPc='
gateway_seed='AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA='
trust_root_id='trust-root-a'
trust_root_seed='BQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQU='
public_api_key='inferlab-observability-proof-key'
export INFERLAB_PROBE_BEARER="$public_api_key"

forget_pid() {
  local forgotten="$1" pid
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
  local pid="$1" attempt state
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
  local owned_pids=("${live_pids[@]}") index pid
  for ((index = ${#owned_pids[@]} - 1; index >= 0; index--)); do
    pid="${owned_pids[$index]}"
    shutdown_child "$pid"
    forget_pid "$pid"
  done
  if [[ -n "$proof_tmp" && -d "$proof_tmp" ]]; then
    case "$(basename "$proof_tmp")" in
      inferlab-v026.*) rm -rf -- "$proof_tmp" ;;
      *) echo "refusing to remove unexpected proof path: $proof_tmp" >&2 ;;
    esac
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

prepare_output_dir() {
  if [[ -z "${INFERLAB_V26_OUTPUT_DIR:-}" ]]; then
    return
  fi
  mkdir -p "$INFERLAB_V26_OUTPUT_DIR"
  if find "$INFERLAB_V26_OUTPUT_DIR" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
    echo 'INFERLAB_V26_OUTPUT_DIR must be empty so stale evidence cannot survive' >&2
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
            raise SystemExit(f"refusing v0.26 proof: 127.0.0.1:{port} is busy: {error}")
PY
}

wait_endpoint() {
  local url="$1" pid="$2" label="$3" status="${4:-200}"
  local deadline=$((SECONDS + 60)) observed
  while true; do
    observed="$(curl --noproxy '*' --connect-timeout 0.1 --max-time 0.25 \
      --silent --output /dev/null --write-out '%{http_code}' "$url" 2>/dev/null || true)"
    if [[ "$observed" == "$status" ]]; then
      return
    fi
    if ! is_owned_child "$pid"; then
      echo "$label exited before becoming healthy" >&2
      tail -n 40 "$proof_tmp/$label.log" 2>/dev/null || true
      return 1
    fi
    if ((SECONDS >= deadline)); then
      echo "timed out waiting for $label at $url; last status=$observed" >&2
      tail -n 40 "$proof_tmp/$label.log" 2>/dev/null || true
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

now_ms() {
  python3 - <<'PY'
import time
print(time.time_ns() // 1_000_000)
PY
}

public_key() {
  local service_id="$1" seed="$2"
  INFERLAB_SERVICE_ID="$service_id" \
  INFERLAB_SERVICE_CREDENTIAL_ID='key-a' \
  INFERLAB_SERVICE_PRIVATE_KEY_B64="$seed" \
    target/debug/service_public_key
}

node_port() {
  case "$1" in
    control-a) printf '10063' ;;
    control-b) printf '10064' ;;
    control-c) printf '10065' ;;
    *) return 1 ;;
  esac
}

node_peers() {
  case "$1" in
    control-a) printf 'control-b=http://127.0.0.1:10064,control-c=http://127.0.0.1:10065' ;;
    control-b) printf 'control-a=http://127.0.0.1:10063,control-c=http://127.0.0.1:10065' ;;
    control-c) printf 'control-a=http://127.0.0.1:10063,control-b=http://127.0.0.1:10064' ;;
    *) return 1 ;;
  esac
}

node_seed() {
  case "$1" in
    control-a) printf '%s' "$control_a_seed" ;;
    control-b) printf '%s' "$control_b_seed" ;;
    control-c) printf '%s' "$control_c_seed" ;;
    *) return 1 ;;
  esac
}

node_election_min() {
  case "$1" in
    control-a) printf '220' ;;
    control-b) printf '1500' ;;
    control-c) printf '2500' ;;
    *) return 1 ;;
  esac
}

set_node_pid() {
  case "$1" in
    control-a) control_a_pid="$2" ;;
    control-b) control_b_pid="$2" ;;
    control-c) control_c_pid="$2" ;;
    *) return 1 ;;
  esac
}

start_control() {
  local node_id="$1" port peers seed election_min election_max metrics_port pid
  port="$(node_port "$node_id")"
  metrics_port="$((port + 100))"
  peers="$(node_peers "$node_id")"
  seed="$(node_seed "$node_id")"
  election_min="$(node_election_min "$node_id")"
  election_max="$((election_min + 80))"
  mkdir -p "$proof_tmp/$node_id"
  INFERLAB_RAFT_NODE_ID="$node_id" \
  INFERLAB_RAFT_CLUSTER_ID="$cluster_id" \
  INFERLAB_RAFT_BIND="127.0.0.1:$port" \
  INFERLAB_RAFT_PEERS="$peers" \
  INFERLAB_RAFT_DATA_DIR="$proof_tmp/$node_id" \
  INFERLAB_RAFT_ELECTION_MIN_MS="$election_min" \
  INFERLAB_RAFT_ELECTION_MAX_MS="$election_max" \
  INFERLAB_RAFT_HEARTBEAT_MS=50 \
  INFERLAB_RAFT_RPC_TIMEOUT_MS=250 \
  INFERLAB_RAFT_COMMIT_TIMEOUT_MS=2500 \
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
  INFERLAB_METRICS_BIND="127.0.0.1:$metrics_port" \
  INFERLAB_LOG_FORMAT=json \
  RUST_LOG=info \
    target/debug/control-plane >"$proof_tmp/$node_id.log" 2>&1 &
  pid="$!"
  live_pids+=("$pid")
  set_node_pid "$node_id" "$pid"
  wait_endpoint "http://127.0.0.1:$port/healthz" "$pid" "$node_id"
  wait_endpoint "http://127.0.0.1:$metrics_port/healthz" "$pid" "$node_id-metrics"
}

start_worker() {
  INFERLAB_CPU_WORKER_ID='cpu-observability-canary' \
  INFERLAB_CPU_BIND=127.0.0.1:10061 \
  INFERLAB_MODEL_PATH=models/tiny-inferlab-v2.bin \
  INFERLAB_CPU_DECODER_MODE=paged-kv-cache \
  INFERLAB_CPU_QUANTIZATION=fp32 \
  INFERLAB_CPU_SPECULATIVE_DRAFT_QUANTIZATION=int8 \
  INFERLAB_CPU_ATTENTION_KERNEL=online-tiled \
  INFERLAB_CPU_ATTENTION_PRECISION=fp32 \
  INFERLAB_CPU_ATTENTION_TILE_TOKENS=32 \
  INFERLAB_CPU_MAX_BATCH_SIZE=4 \
  INFERLAB_CPU_SCHEDULER_QUEUE_CAPACITY=32 \
  INFERLAB_CPU_KV_PAGE_TOKENS=4 \
  INFERLAB_CPU_KV_PAGE_COUNT=64 \
  INFERLAB_CPU_PREFIX_CACHE_CAPACITY=32 \
  INFERLAB_CPU_BATCH_TICK_MS=20 \
  INFERLAB_METRICS_BIND=127.0.0.1:10161 \
  INFERLAB_LOG_FORMAT=json \
  RUST_LOG=info \
    target/debug/cpu-worker >"$proof_tmp/cpu-worker.log" 2>&1 &
  worker_pid="$!"
  live_pids+=("$worker_pid")
  wait_endpoint "$worker_url/health" "$worker_pid" 'cpu-worker'
  wait_endpoint 'http://127.0.0.1:10161/healthz' "$worker_pid" 'cpu-worker-metrics'
}

start_batch() {
  INFERLAB_BATCH_BIND=127.0.0.1:10062 \
  INFERLAB_BATCH_WAL="$proof_tmp/batch-queue.wal" \
  INFERLAB_METRICS_BIND=127.0.0.1:10162 \
  INFERLAB_LOG_FORMAT=json \
  RUST_LOG=info \
    target/debug/batch-queue >"$proof_tmp/batch-queue.log" 2>&1 &
  batch_pid="$!"
  live_pids+=("$batch_pid")
  wait_endpoint "$batch_url/healthz" "$batch_pid" 'batch-queue'
  wait_endpoint 'http://127.0.0.1:10162/healthz' "$batch_pid" 'batch-queue-metrics'
}

start_trust() {
  INFERLAB_TRUST_DISTRIBUTOR_BIND=127.0.0.1:10066 \
  INFERLAB_TRUST_DISTRIBUTOR_CLUSTER_ID="$cluster_id" \
  INFERLAB_SERVICE_TRUST_ROOT_KEYS="$trust_root_id=$trust_root_public" \
  INFERLAB_SERVICE_TRUST_REVOKED_ROOT_KEY_IDS='' \
  INFERLAB_TRUST_DISTRIBUTOR_STATE_PATH="$proof_tmp/trust-state.json" \
  INFERLAB_TRUST_DISTRIBUTOR_EXPECTED_RECEIVERS='observability-receiver/key-a' \
  INFERLAB_METRICS_BIND=127.0.0.1:10166 \
  INFERLAB_LOG_FORMAT=json \
  RUST_LOG=info \
    target/debug/trust-distributor >"$proof_tmp/trust-distributor.log" 2>&1 &
  trust_pid="$!"
  live_pids+=("$trust_pid")
  wait_endpoint "$trust_url/health" "$trust_pid" 'trust-distributor'
  wait_endpoint 'http://127.0.0.1:10166/healthz' "$trust_pid" 'trust-distributor-metrics'
}

start_retry_servers() {
  python3 benchmarks/observability_probe.py retry-servers \
    --first-bind 127.0.0.1:10069 \
    --second-bind 127.0.0.1:10070 \
    --second-delay-ms 25 \
    --events "$proof_tmp/retry-events.jsonl" \
    >"$proof_tmp/retry-servers.log" 2>&1 &
  retry_server_pid="$!"
  live_pids+=("$retry_server_pid")
  wait_endpoint "$retry_first_url/health" "$retry_server_pid" 'retry-servers'
  wait_endpoint "$retry_second_url/health" "$retry_server_pid" 'retry-servers'
}

start_link() {
  INFERLAB_RAFT_LINK_ID='observability-link' \
  INFERLAB_RAFT_LINK_SOURCE_ID='proof-source' \
  INFERLAB_RAFT_LINK_TARGET_ID='proof-target' \
  INFERLAB_RAFT_LINK_BIND=127.0.0.1:10067 \
  INFERLAB_RAFT_LINK_UPSTREAM="$retry_second_url" \
  INFERLAB_RAFT_LINK_EVENT_PATH="$proof_tmp/link-events.jsonl" \
  INFERLAB_METRICS_BIND=127.0.0.1:10167 \
  INFERLAB_LOG_FORMAT=json \
  RUST_LOG=info \
    target/debug/raft-link-proxy >"$proof_tmp/raft-link-proxy.log" 2>&1 &
  link_pid="$!"
  live_pids+=("$link_pid")
  wait_endpoint "$link_url/healthz" "$link_pid" 'raft-link-proxy'
  wait_endpoint 'http://127.0.0.1:10167/healthz' "$link_pid" 'raft-link-proxy-metrics'
}

start_gateway_primary() {
  INFERLAB_BIND=127.0.0.1:10060 \
  INFERLAB_CONTROL_PLANE_URLS="$control_urls" \
  INFERLAB_CONTROL_CLUSTER_ID="$cluster_id" \
  INFERLAB_CONTROL_TRUSTED_KEYS="$route_key_id=$route_public" \
  INFERLAB_CONTROL_BOOTSTRAP_WAIT_MS=3000 \
  INFERLAB_CONTROL_POLL_MS=50 \
  INFERLAB_GATEWAY_SERVICE_ID='gateway-primary' \
  INFERLAB_GATEWAY_SERVICE_CREDENTIAL_ID='key-a' \
  INFERLAB_GATEWAY_SERVICE_PRIVATE_KEY_B64="$gateway_seed" \
  INFERLAB_CONTROL_SERVICE_TARGETS='control-a=http://127.0.0.1:10063,control-b=http://127.0.0.1:10064,control-c=http://127.0.0.1:10065' \
  INFERLAB_ROUTING_SNAPSHOT_PATH="$proof_tmp/gateway-routing.json" \
  INFERLAB_ROUTING_SNAPSHOT_MAX_AGE_MS=60000 \
  INFERLAB_ROUTING_LEASE_MS=5000 \
  INFERLAB_WORKER_CONCURRENCY=4 \
  INFERLAB_ADMISSION_QUEUE_CAPACITY=16 \
  INFERLAB_REQUEST_DEADLINE_MS=15000 \
  INFERLAB_ATTEMPT_TIMEOUT_MS=12000 \
  INFERLAB_MAX_RETRIES=0 \
  INFERLAB_PUBLIC_API_KEYS="$public_api_key" \
  INFERLAB_METRICS_BIND=127.0.0.1:10160 \
  INFERLAB_LOG_FORMAT=json \
  RUST_LOG=info \
    target/debug/gateway >"$proof_tmp/gateway-primary.log" 2>&1 &
  gateway_primary_pid="$!"
  live_pids+=("$gateway_primary_pid")
  wait_endpoint "$gateway_primary_url/health" "$gateway_primary_pid" 'gateway-primary'
  wait_endpoint 'http://127.0.0.1:10160/healthz' "$gateway_primary_pid" 'gateway-primary-metrics'
}

start_gateway_retry() {
  INFERLAB_BIND=127.0.0.1:10068 \
  INFERLAB_WORKERS="retry-first=$retry_first_url,retry-second=$retry_second_url" \
  INFERLAB_ROUTING_POLICY=round-robin \
  INFERLAB_WORKER_CONCURRENCY=2 \
  INFERLAB_ADMISSION_QUEUE_CAPACITY=4 \
  INFERLAB_REQUEST_DEADLINE_MS=5000 \
  INFERLAB_ATTEMPT_TIMEOUT_MS=1000 \
  INFERLAB_MAX_RETRIES=1 \
  INFERLAB_RETRY_BUDGET_PERCENT=100 \
  INFERLAB_RETRY_BASE_DELAY_MS=1 \
  INFERLAB_RETRY_MAX_DELAY_MS=1 \
  INFERLAB_JITTER_SEED=7 \
  INFERLAB_METRICS_BIND=127.0.0.1:10168 \
  INFERLAB_LOG_FORMAT=json \
  RUST_LOG=info \
    target/debug/gateway >"$proof_tmp/gateway-retry.log" 2>&1 &
  gateway_retry_pid="$!"
  live_pids+=("$gateway_retry_pid")
  wait_endpoint "$gateway_retry_url/health" "$gateway_retry_pid" 'gateway-retry'
  wait_endpoint 'http://127.0.0.1:10168/healthz' "$gateway_retry_pid" 'gateway-retry-metrics'
}

create_target_inventory() {
  python3 - "$results_dir/target-inventory.json" <<'PY'
import json
import sys
from pathlib import Path

targets = [
    {"name": "gateway-primary", "service": "gateway", "metrics_url": "http://127.0.0.1:10160/metrics", "status_url": "http://127.0.0.1:10060/internal/workers", "status_requires_bearer": True},
    {"name": "gateway-retry", "service": "gateway", "metrics_url": "http://127.0.0.1:10168/metrics", "status_url": "http://127.0.0.1:10068/internal/workers"},
    {"name": "cpu-worker", "service": "cpu-worker", "metrics_url": "http://127.0.0.1:10161/metrics", "status_url": "http://127.0.0.1:10061/health"},
    {"name": "batch-queue", "service": "batch-queue", "metrics_url": "http://127.0.0.1:10162/metrics", "status_url": "http://127.0.0.1:10062/internal/status"},
    {"name": "control-a", "service": "control-plane", "metrics_url": "http://127.0.0.1:10163/metrics", "status_url": "http://127.0.0.1:10063/v1/control/status"},
    {"name": "control-b", "service": "control-plane", "metrics_url": "http://127.0.0.1:10164/metrics", "status_url": "http://127.0.0.1:10064/v1/control/status"},
    {"name": "control-c", "service": "control-plane", "metrics_url": "http://127.0.0.1:10165/metrics", "status_url": "http://127.0.0.1:10065/v1/control/status"},
    {"name": "trust-distributor", "service": "trust-distributor", "metrics_url": "http://127.0.0.1:10166/metrics", "status_url": "http://127.0.0.1:10066/v1/service-trust/status"},
    {"name": "raft-link-proxy", "service": "raft-link-proxy", "metrics_url": "http://127.0.0.1:10167/metrics", "status_url": "http://127.0.0.1:10067/v1/link/status"},
]
Path(sys.argv[1]).write_text(json.dumps({
    "schema": "inferlab.observability-target-inventory.v0.26",
    "target_count": len(targets),
    "targets": targets,
}, indent=2, sort_keys=True) + "\n", encoding="utf-8")
PY
}

create_control_configuration() {
  python3 - "$proof_tmp/control-config.json" <<'PY'
import json
import sys
from pathlib import Path

Path(sys.argv[1]).write_text(json.dumps({
    "routing_policy": "round-robin",
    "workers": [{
        "id": "cpu-observability-canary",
        "base_url": "http://127.0.0.1:10061",
        "weight": 1,
    }],
}, indent=2) + "\n", encoding="utf-8")
PY
  INFERLAB_CONTROL_WRITER_ID="$writer_id" \
  INFERLAB_CONTROL_WRITER_PRIVATE_KEY_B64="$writer_seed" \
    target/debug/sign_control_write \
      "$cluster_id" 0 now observability-routing-v1 "$proof_tmp/control-config.json" \
      >"$proof_tmp/control-write.json"
}

create_trust_snapshots() {
  local issued_at receiver_public
  issued_at="$(now_ms)"
  receiver_public="$(public_key observability-receiver "$gateway_seed")"
  python3 - "$issued_at" "$receiver_public" "$proof_tmp/trust-policy.json" <<'PY'
import json
import sys
from pathlib import Path

issued_at = int(sys.argv[1])
receiver_public = sys.argv[2]
Path(sys.argv[3]).write_text(json.dumps({
    "schema": "inferlab.service-trust-policy.v1",
    "cluster_id": "inferlab-observability",
    "generation": 1,
    "issued_at_ms": issued_at,
    "trusted_credentials": [{
        "service_id": "observability-receiver",
        "credential_id": "key-a",
        "public_key_base64": receiver_public,
    }],
    "revoked_service_ids": [],
    "revoked_credentials": [],
    "gateway_service_ids": ["observability-receiver"],
}, indent=2) + "\n", encoding="utf-8")
PY
  INFERLAB_SERVICE_TRUST_ROOT_KEY_ID="$trust_root_id" \
  INFERLAB_SERVICE_TRUST_ROOT_PRIVATE_KEY_B64="$trust_root_seed" \
    target/debug/sign_service_trust "$proof_tmp/trust-policy.json" \
    >"$proof_tmp/trust-snapshot.json"
  python3 - "$proof_tmp/trust-snapshot.json" "$proof_tmp/trust-snapshot-tampered.json" <<'PY'
import json
import sys
from pathlib import Path

snapshot = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
snapshot["generation"] = 2
Path(sys.argv[2]).write_text(json.dumps(snapshot, indent=2) + "\n", encoding="utf-8")
PY
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
    gateway-primary "$gateway_primary_pid" gateway-retry "$gateway_retry_pid" \
    cpu-worker "$worker_pid" batch-queue "$batch_pid" \
    control-a "$control_a_pid" control-b "$control_b_pid" control-c "$control_c_pid" \
    trust-distributor "$trust_pid" raft-link-proxy "$link_pid"
  python3 - "$proof_tmp/initial-processes.json" "$$" \
    >"$results_dir/process-continuity.json" <<'PY'
import json
import subprocess
import sys
from pathlib import Path

initial = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
proof_shell_pid = int(sys.argv[2])

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
for name, prior in initial["processes"].items():
    current = observe(prior["pid"])
    processes[name] = {
        "initial_pid": prior["pid"],
        "current_pid": current["pid"],
        "same_pid": prior["pid"] == current["pid"],
        "initial_start_token": prior["start_token"],
        "current_start_token": current["start_token"],
        "same_start_token": prior["start_token"] == current["start_token"],
        "initial_command": prior["command"],
        "current_command": current["command"],
        "same_command": prior["command"] == current["command"],
        "parent_pid": current["parent_pid"],
        "process_state": current["process_state"],
        "alive": current["alive"],
        "owned_child": current["owned_child"],
        "non_zombie": current["non_zombie"],
    }
print(json.dumps({
    "schema": "inferlab.observability-process-continuity.v0.26",
    "proof_shell_pid": proof_shell_pid,
    "processes": processes,
}, indent=2, sort_keys=True))
PY
}

scan_private_material() {
  local output="$1"
  python3 - "$results_dir" \
    "$route_seed" "$writer_seed" "$control_a_seed" "$control_b_seed" \
    "$control_c_seed" "$gateway_seed" "$trust_root_seed" "$public_api_key" \
    >"$output" <<'PY'
import json
import sys
from pathlib import Path

evidence = Path(sys.argv[1])
seed_labels = [
    "route_seed", "writer_seed", "control_a_seed", "control_b_seed",
    "control_c_seed", "gateway_seed", "trust_root_seed",
]
secret_labels = seed_labels + ["public_api_key"]
values = sys.argv[2:]

def compact(value: str) -> str:
    return "".join(value.replace("\\n", "").replace("\\r", "").split())

candidates = []
for label, value in zip(secret_labels, values):
    normalized = compact(value)
    candidates.extend([(label, normalized), (f"{label}_unpadded", normalized.rstrip("="))])

files = sorted(
    path for path in evidence.iterdir()
    if path.suffix in {".json", ".svg", ".prom"}
)
matches = []
for path in files:
    retained = compact(path.read_text(encoding="utf-8", errors="replace"))
    for label, candidate in candidates:
        if len(candidate) >= 24 and candidate in retained:
            matches.append({"file": path.name, "candidate_label": label})
if matches:
    raise SystemExit(
        "private material leaked into retained evidence: "
        + ", ".join(f"{item['candidate_label']} in {item['file']}" for item in matches)
    )
print(json.dumps({
    "schema": "inferlab.private-material-scan.v0.26",
    "files_scanned": [path.name for path in files],
    "known_seed_labels": seed_labels,
    "known_seed_count": len(seed_labels),
    "additional_secret_labels": ["public_api_key"],
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
    if path.suffix not in {".json", ".svg", ".prom"}:
        continue
    text = path.read_text(encoding="utf-8", errors="replace")
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
if "manifest.json" not in expected or len(expected) != len(set(expected)):
    raise SystemExit("manifest expected set must contain one unique manifest.json")
before = sorted(path.name for path in directory.iterdir())
without_manifest = sorted(name for name in expected if name != "manifest.json")
if before != without_manifest:
    raise SystemExit(f"unexpected pre-manifest evidence set: {before}")
files = []
for name in without_manifest:
    content = (directory / name).read_bytes()
    files.append({
        "path": name,
        "sha256": hashlib.sha256(content).hexdigest(),
        "bytes": len(content),
    })
(directory / "manifest.json").write_text(json.dumps({
    "schema": "inferlab.evidence-manifest.v0.26",
    "expected_files": expected,
    "file_count": len(expected),
    "hashed_file_count": len(files),
    "files": files,
}, indent=2, sort_keys=True) + "\n", encoding="utf-8")
if sorted(path.name for path in directory.iterdir()) != expected:
    raise SystemExit("final evidence set differs from its exact manifest")
PY
}

retain_results() {
  if [[ -z "${INFERLAB_V26_OUTPUT_DIR:-}" ]]; then
    return
  fi
  local name
  for name in "$@"; do
    if [[ "$name" != 'manifest.json' ]]; then
      cp "$results_dir/$name" "$INFERLAB_V26_OUTPUT_DIR/$name"
    fi
  done
  python3 - "$results_dir/manifest.json" "$INFERLAB_V26_OUTPUT_DIR" <<'PY'
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
  # The manifest is the completion marker. It appears only after all evidence
  # files are copied and verified byte-for-byte.
  cp "$results_dir/manifest.json" "$INFERLAB_V26_OUTPUT_DIR/manifest.json"
}

prepare_output_dir
check_ports_are_free \
  10060 10061 10062 10063 10064 10065 10066 10067 10068 10069 10070 \
  10160 10161 10162 10163 10164 10165 10166 10167 10168
cargo build --workspace --bins --quiet

control_a_public="$(public_key control-a "$control_a_seed")"
control_b_public="$(public_key control-b "$control_b_seed")"
control_c_public="$(public_key control-c "$control_c_seed")"
gateway_public="$(public_key gateway-primary "$gateway_seed")"
service_keys="control-a/key-a=$control_a_public,control-b/key-a=$control_b_public,control-c/key-a=$control_c_public,gateway-primary/key-a=$gateway_public"
trust_root_public="$(
  INFERLAB_SERVICE_TRUST_ROOT_KEY_ID="$trust_root_id" \
  INFERLAB_SERVICE_TRUST_ROOT_PRIVATE_KEY_B64="$trust_root_seed" \
    target/debug/service_trust_public_key
)"

create_target_inventory
create_trust_snapshots
start_retry_servers
start_link
start_worker
start_batch
start_trust

# B and C serve RPCs before A starts its short election timer. Exact term
# numbers remain scheduling details; the retained invariant is one leader and
# one committed revision shared by all three controls.
start_control control-b
start_control control-c
start_control control-a
python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$control_urls" --timeout 10 >"$proof_tmp/leader.json"
leader_url="$(json_field "$proof_tmp/leader.json" leader_url)"
create_control_configuration
python3 benchmarks/control_write_probe.py submit \
  --url "$leader_url" --body "$proof_tmp/control-write.json" \
  >"$proof_tmp/control-write-response.json"
python3 benchmarks/raft_partition_probe.py wait-cluster \
  --nodes 'control-a=http://127.0.0.1:10063,control-b=http://127.0.0.1:10064,control-c=http://127.0.0.1:10065' \
  --commit-index 2 --revision 2 --policy round-robin --timeout 10 \
  >"$proof_tmp/control-cluster.json"

start_gateway_primary
start_gateway_retry
wait_endpoint "$gateway_primary_url/readyz" "$gateway_primary_pid" 'gateway-primary-ready'
wait_endpoint "$gateway_retry_url/readyz" "$gateway_retry_pid" 'gateway-retry-ready'
capture_process_snapshot "$proof_tmp/initial-processes.json" \
  gateway-primary "$gateway_primary_pid" gateway-retry "$gateway_retry_pid" \
  cpu-worker "$worker_pid" batch-queue "$batch_pid" \
  control-a "$control_a_pid" control-b "$control_b_pid" control-c "$control_c_pid" \
  trust-distributor "$trust_pid" raft-link-proxy "$link_pid"

python3 benchmarks/observability_probe.py scrape-set \
  --targets-file "$results_dir/target-inventory.json" \
  --checkpoint baseline --raw-dir "$results_dir" \
  >"$results_dir/baseline-scrapes.json"

python3 - "$proof_tmp/request.json" <<'PY'
import json
import sys
from pathlib import Path

Path(sys.argv[1]).write_text(json.dumps({
    "model": "inferlab-tiny",
    "stream": False,
    "temperature": 0,
    "max_tokens": 6,
    "messages": [{"role": "user", "content": "explain bounded metrics"}],
}, separators=(",", ":")) + "\n", encoding="utf-8")
PY
python3 benchmarks/observability_probe.py request \
  --url "$gateway_primary_url/v1/chat/completions" --method POST \
  --body "$proof_tmp/request.json" --request-id 'obs.valid.001' \
  --use-bearer-env --expect-status 200 --timeout 15 \
  >"$results_dir/request-id-valid.json"
python3 benchmarks/observability_probe.py request \
  --url "$gateway_primary_url/v1/chat/completions" --method POST \
  --body "$proof_tmp/request.json" --request-id 'obs/invalid/request' \
  --use-bearer-env --expect-status 200 --timeout 15 \
  >"$results_dir/request-id-invalid.json"
python3 benchmarks/observability_probe.py stream \
  --url "$gateway_primary_url/v1/chat/completions" \
  --prompt 'explain fixed histogram buckets' --request-id 'obs.stream.001' \
  --max-tokens 8 --speculative-tokens 2 --use-bearer-env --timeout 20 \
  >"$results_dir/stream.json"

python3 benchmarks/observability_probe.py request \
  --url "$gateway_retry_url/v1/chat/completions" --method POST \
  --body "$proof_tmp/request.json" --request-id 'obs.retry.stable' \
  --expect-status 200 --timeout 8 >"$results_dir/request-id-retry.json"

python3 - "$proof_tmp/link-request.json" "$proof_tmp/link-drop.json" "$proof_tmp/link-allow.json" <<'PY'
import json
import sys
from pathlib import Path

Path(sys.argv[1]).write_text("{}\n", encoding="utf-8")
Path(sys.argv[2]).write_text(json.dumps({"mode": "drop", "reason": "observability-exact-drop"}) + "\n", encoding="utf-8")
Path(sys.argv[3]).write_text(json.dumps({"mode": "allow", "reason": "observability-exact-heal"}) + "\n", encoding="utf-8")
PY
python3 benchmarks/observability_probe.py request \
  --url "$link_url/raft/append-entries" --method POST \
  --body "$proof_tmp/link-request.json" --expect-status 200 \
  >"$proof_tmp/link-forwarded.json"
python3 benchmarks/observability_probe.py request \
  --url "$link_url/v1/link/mode" --method PUT \
  --body "$proof_tmp/link-drop.json" --expect-status 200 \
  >"$proof_tmp/link-drop-mode.json"
python3 benchmarks/observability_probe.py request \
  --url "$link_url/raft/append-entries" --method POST \
  --body "$proof_tmp/link-request.json" --expect-status 503 \
  >"$proof_tmp/link-dropped.json"
python3 benchmarks/observability_probe.py request \
  --url "$link_url/v1/link/mode" --method PUT \
  --body "$proof_tmp/link-allow.json" --expect-status 200 \
  >"$proof_tmp/link-allow-mode.json"

# The retry fixture is not a retained service target. Stop it deliberately so
# one subsequent link RPC produces exactly one observed upstream-failure delta.
shutdown_child "$retry_server_pid"
forget_pid "$retry_server_pid"
retry_server_pid=''
python3 benchmarks/observability_probe.py capture-retry-events \
  --events "$proof_tmp/retry-events.jsonl" >"$results_dir/retry-events.json"
python3 benchmarks/observability_probe.py request \
  --url "$link_url/raft/append-entries" --method POST \
  --body "$proof_tmp/link-request.json" --expect-status 502 \
  >"$proof_tmp/link-upstream-failure.json"
python3 - "$proof_tmp/link-forwarded.json" "$proof_tmp/link-drop-mode.json" \
  "$proof_tmp/link-dropped.json" "$proof_tmp/link-allow-mode.json" \
  "$proof_tmp/link-upstream-failure.json" >"$results_dir/link-scenario.json" <<'PY'
import json
import sys
from pathlib import Path

captures = [json.loads(Path(path).read_text(encoding="utf-8")) for path in sys.argv[1:]]
forwarded, drop_mode, dropped, allow_mode, failed = captures
scenario = {
    "schema": "inferlab.observability-link-scenario.v0.26",
    "forwarded_status": forwarded["response"]["status"],
    "dropped_status": dropped["response"]["status"],
    "upstream_failure_status": failed["response"]["status"],
    "mode_sequence": [
        "allow",
        drop_mode["response"]["body"].get("mode"),
        allow_mode["response"]["body"].get("mode"),
    ],
    "error_codes": {
        "dropped": dropped["response"]["body"].get("error", {}).get("code"),
        "upstream_failure": failed["response"]["body"].get("error", {}).get("code"),
    },
}
if scenario["mode_sequence"] != ["allow", "drop", "allow"]:
    raise SystemExit("link mode sequence did not match allow/drop/allow")
print(json.dumps(scenario, indent=2, sort_keys=True))
PY

python3 benchmarks/observability_probe.py batch-scenario \
  --base-url "$batch_url" >"$results_dir/batch-scenario.json"
python3 benchmarks/observability_probe.py trust-scenario \
  --base-url "$trust_url" --snapshot "$proof_tmp/trust-snapshot.json" \
  --tampered-snapshot "$proof_tmp/trust-snapshot-tampered.json" \
  >"$results_dir/trust-scenario.json"

python3 benchmarks/observability_probe.py scrape-set \
  --targets-file "$results_dir/target-inventory.json" \
  --checkpoint before-cardinality --raw-dir "$results_dir" \
  >"$results_dir/before-cardinality-scrapes.json"
python3 benchmarks/observability_probe.py unique-prompts \
  --url "$gateway_primary_url/v1/chat/completions" --count 24 \
  --prompt-prefix 'observability-cardinality-canary' \
  --request-id-prefix 'obs.cardinality' --max-tokens 4 \
  --use-bearer-env --timeout 15 >"$results_dir/unique-prompts.json"
python3 benchmarks/observability_probe.py scrape-set \
  --targets-file "$results_dir/target-inventory.json" \
  --checkpoint after-cardinality --raw-dir "$results_dir" \
  >"$results_dir/after-cardinality-scrapes.json"

replacement_id="$(json_field "$results_dir/request-id-invalid.json" response.headers.x-inferlab-request-id)"
python3 benchmarks/observability_probe.py extract-log-events \
  --log-file "$proof_tmp/cpu-worker.log" \
  --request-ids "obs.valid.001,$replacement_id,obs.stream.001,obs/invalid/request" \
  >"$results_dir/worker-request-id-events.json"
python3 benchmarks/observability_probe.py capture-json-set \
  --targets-file "$results_dir/target-inventory.json" --url-field status_url \
  --use-bearer-env >"$results_dir/final-statuses.json"
record_process_continuity
python3 benchmarks/observability_probe.py scrape-set \
  --targets-file "$results_dir/target-inventory.json" \
  --checkpoint final --raw-dir "$results_dir" \
  >"$results_dir/final-scrapes.json"

# Sanitize before any retained report consumes the evidence. The preliminary
# private scan feeds the first checker pass; the retained report then feeds a
# second checker/render pass, and a final discarded scan covers those outputs.
python3 benchmarks/observability_probe.py sanitize-evidence \
  --evidence-dir "$results_dir" --proof-root "$proof_tmp" --project-root "$project_root" \
  >"$proof_tmp/sanitizer.json"
mv "$proof_tmp/sanitizer.json" "$results_dir/sanitizer.json"
scan_private_material "$proof_tmp/private-preliminary.json"
mv "$proof_tmp/private-preliminary.json" "$results_dir/private-material-scan.json"
python3 benchmarks/check_observability.py \
  --evidence-dir "$results_dir" \
  --output "$results_dir/assertions.json" \
  --contract-output "$results_dir/contract.json" \
  --cardinality-output "$results_dir/cardinality.json" \
  --histogram-output "$results_dir/histograms.json" \
  --delta-output "$results_dir/deltas.json"
python3 benchmarks/render_observability_svg.py \
  --evidence-dir "$results_dir" --output "$results_dir/observability-proof.svg"
scan_private_material "$proof_tmp/private-retained.json"
mv "$proof_tmp/private-retained.json" "$results_dir/private-material-scan.json"
python3 benchmarks/check_observability.py \
  --evidence-dir "$results_dir" \
  --output "$results_dir/assertions.json" \
  --contract-output "$results_dir/contract.json" \
  --cardinality-output "$results_dir/cardinality.json" \
  --histogram-output "$results_dir/histograms.json" \
  --delta-output "$results_dir/deltas.json"
python3 benchmarks/render_observability_svg.py \
  --evidence-dir "$results_dir" --output "$results_dir/observability-proof.svg"
# Re-run into disposable paths and compare every derived artifact byte-for-byte.
# Unlike an in-place overwrite, these comparisons prove deterministic replay
# over the exact retained inputs.
python3 benchmarks/check_observability.py \
  --evidence-dir "$results_dir" \
  --output "$proof_tmp/replay-assertions.json" \
  --contract-output "$proof_tmp/replay-contract.json" \
  --cardinality-output "$proof_tmp/replay-cardinality.json" \
  --histogram-output "$proof_tmp/replay-histograms.json" \
  --delta-output "$proof_tmp/replay-deltas.json"
cmp "$results_dir/assertions.json" "$proof_tmp/replay-assertions.json"
cmp "$results_dir/contract.json" "$proof_tmp/replay-contract.json"
cmp "$results_dir/cardinality.json" "$proof_tmp/replay-cardinality.json"
cmp "$results_dir/histograms.json" "$proof_tmp/replay-histograms.json"
cmp "$results_dir/deltas.json" "$proof_tmp/replay-deltas.json"
python3 benchmarks/render_observability_svg.py \
  --evidence-dir "$results_dir" --output "$proof_tmp/replay-observability-proof.svg"
cmp "$results_dir/observability-proof.svg" "$proof_tmp/replay-observability-proof.svg"
scan_private_material "$proof_tmp/private-final-discarded.json"
final_leak_scan

expected_files=(
  after-cardinality-scrapes.json
  assertions.json
  baseline-scrapes.json
  batch-scenario.json
  before-cardinality-scrapes.json
  cardinality.json
  contract.json
  deltas.json
  final-scrapes.json
  final-statuses.json
  histograms.json
  link-scenario.json
  manifest.json
  observability-proof.svg
  private-material-scan.json
  process-continuity.json
  request-id-invalid.json
  request-id-retry.json
  request-id-valid.json
  retry-events.json
  sanitizer.json
  stream.json
  target-inventory.json
  trust-scenario.json
  unique-prompts.json
  worker-request-id-events.json
)
for checkpoint in baseline before-cardinality after-cardinality final; do
  for target in gateway-primary gateway-retry cpu-worker batch-queue \
    control-a control-b control-c trust-distributor raft-link-proxy; do
    expected_files+=("$checkpoint-$target.prom")
  done
done
write_manifest "${expected_files[@]}"
retain_results "${expected_files[@]}"

python3 - "$results_dir/assertions.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    assertions = json.load(source)
print(
    f"v0.26 exact-process observability proof complete: "
    f"{assertions['passed']}/{assertions['total']} assertions passed"
)
PY
