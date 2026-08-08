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
unset INFERLAB_PUBLIC_API_KEYS || true
# The v1 startup case proves the signed-receiver default, so an ambient legacy
# compatibility switch must not silently change the experiment.
unset INFERLAB_SERVICE_TRUST_ALLOW_LEGACY_V1 || true
umask 077

# Apple's system Python can use an older LibreSSL. The mTLS proof needs an
# interpreter with an exact TLS 1.3 client.
proof_python="$(command -v python3)"
if ! "$proof_python" -c 'import ssl; assert ssl.HAS_TLSv1_3' >/dev/null 2>&1; then
  for candidate in python3.13 python3.12 /opt/homebrew/bin/python3; do
    if command -v "$candidate" >/dev/null 2>&1 && \
      "$(command -v "$candidate")" -c 'import ssl; assert ssl.HAS_TLSv1_3' >/dev/null 2>&1; then
      proof_python="$(command -v "$candidate")"
      break
    fi
  done
fi
if ! "$proof_python" -c 'import ssl; assert ssl.HAS_TLSv1_3' >/dev/null 2>&1; then
  echo 'v0.27 proof requires Python linked to a TLS 1.3-capable OpenSSL' >&2
  exit 1
fi
python3() {
  "$proof_python" "$@"
}

proof_tmp_root="${TMPDIR:-/tmp}"
proof_tmp="$(mktemp -d "$proof_tmp_root/inferlab-v027.XXXXXX")"
results_dir="$proof_tmp/results"
policy_dir="$proof_tmp/policies"
pki_dir="$proof_tmp/pki"
mkdir -p "$results_dir" "$policy_dir" "$pki_dir"

# A sentinel keeps Bash 3.2 + set -u safe when cleanup runs before a child is
# launched. Every signal is revalidated as an exact child of this proof shell.
live_pids=(sentinel)
control_a_pid=''
control_b_pid=''
control_c_pid=''
distributor_pid=''
worker_pid=''
gateway_pid=''
expired_restart_failed_pid=''

cluster_id='inferlab-primary'
control_urls='http://127.0.0.1:10081,http://127.0.0.1:10082,http://127.0.0.1:10083'
gateway_url='http://127.0.0.1:10080'
worker_url='http://127.0.0.1:10084'
distributor_url='https://localhost:10085'

route_key_id='route-2026-b'
route_seed='TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs='
route_public='PUAXw+hDiVqStwqnTRt+vJyYLM8uxJaMwM1V8Sr0Zgw='
writer_id='deploy-bot'
writer_seed='nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A='
writer_public='11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo='
trust_root_id='service-trust-root-a'
trust_root_seed='BQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQU='
control_a_seed='TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs='
control_b_seed='nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A='
control_c_seed='xaqN9D+fg3vtt0QvMdy3sWbThTUHbwlLhc46LgtEWPc='
gateway_seed='AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA='

policy_max_lifetime_ms=45000
policy_future_skew_ms=250
g1_lifetime_ms=45000
g2_lifetime_ms=30000

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
      inferlab-v027.*) rm -rf -- "$proof_tmp" ;;
      *) echo "refusing to remove unexpected proof path: $proof_tmp" >&2 ;;
    esac
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

prepare_output_dir() {
  if [[ -z "${INFERLAB_V27_OUTPUT_DIR:-}" ]]; then
    return
  fi
  mkdir -p "$INFERLAB_V27_OUTPUT_DIR"
  if find "$INFERLAB_V27_OUTPUT_DIR" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
    echo 'INFERLAB_V27_OUTPUT_DIR must be empty so stale evidence cannot survive' >&2
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
            raise SystemExit(f"refusing v0.27 proof: 127.0.0.1:{port} is busy: {error}")
PY
}

wait_endpoint() {
  local url="$1" pid="$2" label="$3" status="${4:-200}"
  local deadline=$((SECONDS + 60)) observed
  while true; do
    observed="$(curl --noproxy '*' --connect-timeout 0.1 --max-time 0.3 \
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

wait_distributor_health() {
  local deadline=$((SECONDS + 60)) observed
  while true; do
    observed="$(curl --noproxy '*' --connect-timeout 0.1 --max-time 0.4 \
      --silent --output /dev/null --write-out '%{http_code}' \
      --cacert "$pki_dir/ca.crt" --cert "$pki_dir/publisher.crt" \
      --key "$pki_dir/publisher.key" "$distributor_url/health" 2>/dev/null || true)"
    if [[ "$observed" == '200' ]]; then
      return
    fi
    if ! is_owned_child "$distributor_pid"; then
      echo 'trust-distributor exited before becoming healthy' >&2
      tail -n 40 "$proof_tmp/trust-distributor.log" 2>/dev/null || true
      return 1
    fi
    if ((SECONDS >= deadline)); then
      echo "timed out waiting for trust-distributor; last status=$observed" >&2
      tail -n 40 "$proof_tmp/trust-distributor.log" 2>/dev/null || true
      return 1
    fi
    sleep 0.05
  done
}

listener_is_open() {
  local port="$1"
  (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null
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

node_port() {
  case "$1" in
    control-a) printf '10081' ;;
    control-b) printf '10082' ;;
    control-c) printf '10083' ;;
    *) return 1 ;;
  esac
}

node_peers() {
  case "$1" in
    control-a) printf 'control-b=http://127.0.0.1:10082,control-c=http://127.0.0.1:10083' ;;
    control-b) printf 'control-a=http://127.0.0.1:10081,control-c=http://127.0.0.1:10083' ;;
    control-c) printf 'control-a=http://127.0.0.1:10081,control-b=http://127.0.0.1:10082' ;;
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
    control-a) printf '300' ;;
    control-b) printf '15000' ;;
    control-c) printf '18000' ;;
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

get_node_pid() {
  case "$1" in
    control-a) printf '%s' "$control_a_pid" ;;
    control-b) printf '%s' "$control_b_pid" ;;
    control-c) printf '%s' "$control_c_pid" ;;
    *) return 1 ;;
  esac
}

public_key() {
  local service_id="$1" seed="$2"
  INFERLAB_SERVICE_ID="$service_id" \
  INFERLAB_SERVICE_CREDENTIAL_ID='key-a' \
  INFERLAB_SERVICE_PRIVATE_KEY_B64="$seed" \
    target/debug/service_public_key
}

service_seed() {
  case "$1" in
    control-a) printf '%s' "$control_a_seed" ;;
    control-b) printf '%s' "$control_b_seed" ;;
    control-c) printf '%s' "$control_c_seed" ;;
    gateway-primary) printf '%s' "$gateway_seed" ;;
    *) return 1 ;;
  esac
}

generate_pki() {
  python3 - "$pki_dir" <<'PY'
import sys
from pathlib import Path

directory = Path(sys.argv[1])
(directory / "ca.cnf").write_text("""[req]
prompt = no
distinguished_name = dn
x509_extensions = ca_ext
[dn]
CN = InferLab v0.27 disposable proof CA
[ca_ext]
basicConstraints = critical,CA:TRUE
keyUsage = critical,keyCertSign,cRLSign
subjectKeyIdentifier = hash
""", encoding="utf-8")
(directory / "server.cnf").write_text("""[req]
prompt = no
distinguished_name = dn
req_extensions = leaf_ext
[dn]
CN = localhost
[leaf_ext]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature,keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:localhost
""", encoding="utf-8")
for name in ["publisher", "control-a", "control-b", "control-c"]:
    (directory / f"{name}.cnf").write_text(f"""[req]
prompt = no
distinguished_name = dn
req_extensions = leaf_ext
[dn]
CN = {name}
[leaf_ext]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature,keyEncipherment
extendedKeyUsage = clientAuth
""", encoding="utf-8")
PY
  openssl req -x509 -newkey rsa:2048 -nodes -days 1 -sha256 \
    -keyout "$pki_dir/ca.key" -out "$pki_dir/ca.crt" \
    -config "$pki_dir/ca.cnf" >/dev/null 2>&1
  local leaf
  for leaf in server publisher control-a control-b control-c; do
    openssl req -new -newkey rsa:2048 -nodes \
      -keyout "$pki_dir/$leaf.key" -out "$pki_dir/$leaf.csr" \
      -config "$pki_dir/$leaf.cnf" >/dev/null 2>&1
    openssl x509 -req -days 1 -sha256 \
      -in "$pki_dir/$leaf.csr" -CA "$pki_dir/ca.crt" -CAkey "$pki_dir/ca.key" \
      -CAcreateserial -out "$pki_dir/$leaf.crt" \
      -extfile "$pki_dir/$leaf.cnf" -extensions leaf_ext >/dev/null 2>&1
  done
}

start_distributor() {
  INFERLAB_TRUST_DISTRIBUTOR_BIND='127.0.0.1:10085' \
  INFERLAB_TRUST_DISTRIBUTOR_CLUSTER_ID="$cluster_id" \
  INFERLAB_SERVICE_TRUST_ROOT_KEYS="$trust_root_id=$trust_root_public" \
  INFERLAB_SERVICE_TRUST_REVOKED_ROOT_KEY_IDS='' \
  INFERLAB_TRUST_DISTRIBUTOR_STATE_PATH="$proof_tmp/distributor-state.json" \
  INFERLAB_TRUST_DISTRIBUTOR_EXPECTED_RECEIVERS='control-a/key-a,control-b/key-a,control-c/key-a' \
  INFERLAB_TRUST_DISTRIBUTOR_MAX_BODY_BYTES=262144 \
  INFERLAB_TRUST_DISTRIBUTOR_TLS_CERT_PATH="$pki_dir/server.crt" \
  INFERLAB_TRUST_DISTRIBUTOR_TLS_KEY_PATH="$pki_dir/server.key" \
  INFERLAB_TRUST_DISTRIBUTOR_TLS_CLIENT_CA_PATH="$pki_dir/ca.crt" \
    target/debug/trust-distributor >"$proof_tmp/trust-distributor.log" 2>&1 &
  distributor_pid="$!"
  live_pids+=("$distributor_pid")
  wait_distributor_health
}

stop_distributor() {
  if [[ -n "$distributor_pid" ]]; then
    local pid="$distributor_pid"
    shutdown_child "$pid"
    forget_pid "$pid"
    distributor_pid=''
  fi
}

start_node() {
  local node_id="$1" port peers seed election_min election_max pid
  port="$(node_port "$node_id")"
  peers="$(node_peers "$node_id")"
  seed="$(node_seed "$node_id")"
  election_min="$(node_election_min "$node_id")"
  election_max="$((election_min + 100))"
  mkdir -p "$proof_tmp/$node_id"
  INFERLAB_RAFT_NODE_ID="$node_id" \
  INFERLAB_RAFT_CLUSTER_ID="$cluster_id" \
  INFERLAB_RAFT_BIND="127.0.0.1:$port" \
  INFERLAB_RAFT_PEERS="$peers" \
  INFERLAB_RAFT_DATA_DIR="$proof_tmp/$node_id" \
  INFERLAB_RAFT_ELECTION_MIN_MS="$election_min" \
  INFERLAB_RAFT_ELECTION_MAX_MS="$election_max" \
  INFERLAB_RAFT_HEARTBEAT_MS=50 \
  INFERLAB_RAFT_RPC_TIMEOUT_MS=500 \
  INFERLAB_RAFT_COMMIT_TIMEOUT_MS=2500 \
  INFERLAB_CONTROL_SIGNING_KEY_ID="$route_key_id" \
  INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64="$route_seed" \
  INFERLAB_CONTROL_WRITER_KEYS="$writer_id=$writer_public" \
  INFERLAB_CONTROL_WRITE_MAX_AGE_MS=5000 \
  INFERLAB_CONTROL_WRITE_MAX_FUTURE_SKEW_MS=250 \
  INFERLAB_SERVICE_ID="$node_id" \
  INFERLAB_SERVICE_CREDENTIAL_ID='key-a' \
  INFERLAB_SERVICE_PRIVATE_KEY_B64="$seed" \
  INFERLAB_SERVICE_TRUST_DISTRIBUTOR_URL="$distributor_url" \
  INFERLAB_SERVICE_TRUST_CACHE_PATH="$proof_tmp/$node_id/service-trust-cache.json" \
  INFERLAB_SERVICE_TRUST_STATE_PATH="$proof_tmp/$node_id/service-trust-floor.json" \
  INFERLAB_SERVICE_TRUST_ROOT_KEYS="$trust_root_id=$trust_root_public" \
  INFERLAB_SERVICE_TRUST_REVOKED_ROOT_KEY_IDS='' \
  INFERLAB_SERVICE_TRUST_MAX_POLICY_LIFETIME_MS="$policy_max_lifetime_ms" \
  INFERLAB_SERVICE_TRUST_POLICY_MAX_FUTURE_SKEW_MS="$policy_future_skew_ms" \
  INFERLAB_SERVICE_TRUST_TLS_CA_CERT_PATH="$pki_dir/ca.crt" \
  INFERLAB_SERVICE_TRUST_TLS_CLIENT_CERT_PATH="$pki_dir/$node_id.crt" \
  INFERLAB_SERVICE_TRUST_TLS_CLIENT_KEY_PATH="$pki_dir/$node_id.key" \
  INFERLAB_SERVICE_TRUST_POLL_MS=25 \
  INFERLAB_SERVICE_TRUST_REQUEST_TIMEOUT_MS=1000 \
  INFERLAB_SERVICE_TRUST_MAX_BACKOFF_MS=200 \
  INFERLAB_SERVICE_AUTH_MAX_AGE_MS=5000 \
  INFERLAB_SERVICE_AUTH_MAX_FUTURE_SKEW_MS=250 \
    target/debug/control-plane >"$proof_tmp/$node_id.log" 2>&1 &
  pid="$!"
  live_pids+=("$pid")
  set_node_pid "$node_id" "$pid"
  wait_endpoint "http://127.0.0.1:$port/healthz" "$pid" "$node_id"
}

stop_node() {
  local node_id="$1" pid
  pid="$(get_node_pid "$node_id")"
  if [[ -n "$pid" ]]; then
    shutdown_child "$pid"
    forget_pid "$pid"
    set_node_pid "$node_id" ''
  fi
}

start_worker() {
  INFERLAB_CPU_WORKER_ID='cpu-trust-expiry' \
  INFERLAB_CPU_BIND='127.0.0.1:10084' \
  INFERLAB_MODEL_PATH='models/tiny-inferlab-v2.bin' \
  INFERLAB_CPU_DECODER_MODE='paged-kv-cache' \
  INFERLAB_CPU_QUANTIZATION='fp32' \
  INFERLAB_CPU_SPECULATIVE_DRAFT_QUANTIZATION='int8' \
  INFERLAB_CPU_ATTENTION_KERNEL='online-tiled' \
  INFERLAB_CPU_ATTENTION_PRECISION='fp32' \
  INFERLAB_CPU_ATTENTION_TILE_TOKENS=32 \
  INFERLAB_CPU_MAX_BATCH_SIZE=4 \
  INFERLAB_CPU_SCHEDULER_QUEUE_CAPACITY=16 \
  INFERLAB_CPU_KV_PAGE_TOKENS=4 \
  INFERLAB_CPU_KV_PAGE_COUNT=64 \
  INFERLAB_CPU_PREFIX_CACHE_CAPACITY=32 \
  INFERLAB_CPU_BATCH_TICK_MS=500 \
    target/debug/cpu-worker >"$proof_tmp/cpu-worker.log" 2>&1 &
  worker_pid="$!"
  live_pids+=("$worker_pid")
  wait_endpoint "$worker_url/health" "$worker_pid" 'cpu-worker'
}

start_gateway() {
  INFERLAB_BIND='127.0.0.1:10080' \
  INFERLAB_CONTROL_PLANE_URLS="$control_urls" \
  INFERLAB_CONTROL_CLUSTER_ID="$cluster_id" \
  INFERLAB_CONTROL_TRUSTED_KEYS="$route_key_id=$route_public" \
  INFERLAB_CONTROL_BOOTSTRAP_WAIT_MS=5000 \
  INFERLAB_CONTROL_POLL_MS=50 \
  INFERLAB_GATEWAY_SERVICE_ID='gateway-primary' \
  INFERLAB_GATEWAY_SERVICE_CREDENTIAL_ID='key-a' \
  INFERLAB_GATEWAY_SERVICE_PRIVATE_KEY_B64="$gateway_seed" \
  INFERLAB_CONTROL_SERVICE_TARGETS='control-a=http://127.0.0.1:10081,control-b=http://127.0.0.1:10082,control-c=http://127.0.0.1:10083' \
  INFERLAB_ROUTING_SNAPSHOT_PATH="$proof_tmp/gateway-routing.json" \
  INFERLAB_ROUTING_SNAPSHOT_MAX_AGE_MS=120000 \
  INFERLAB_ROUTING_LEASE_MS=60000 \
  INFERLAB_WORKER_CONCURRENCY=4 \
  INFERLAB_ADMISSION_QUEUE_CAPACITY=8 \
  INFERLAB_REQUEST_DEADLINE_MS=15000 \
  INFERLAB_ATTEMPT_TIMEOUT_MS=12000 \
  INFERLAB_MAX_RETRIES=0 \
    target/debug/gateway >"$proof_tmp/gateway.log" 2>&1 &
  gateway_pid="$!"
  live_pids+=("$gateway_pid")
  wait_endpoint "$gateway_url/health" "$gateway_pid" 'gateway'
}

sign_policy() {
  local policy="$1" output="$2"
  INFERLAB_SERVICE_TRUST_ROOT_KEY_ID="$trust_root_id" \
  INFERLAB_SERVICE_TRUST_ROOT_PRIVATE_KEY_B64="$trust_root_seed" \
    target/debug/sign_service_trust "$policy" >"$output"
}

sign_service_request() {
  local service_id="$1" method="$2" path="$3" audience="$4"
  local issued_at_ms="$5" nonce="$6" body="$7" output="$8" seed
  seed="$(service_seed "$service_id")"
  INFERLAB_SERVICE_ID="$service_id" \
  INFERLAB_SERVICE_CREDENTIAL_ID='key-a' \
  INFERLAB_SERVICE_PRIVATE_KEY_B64="$seed" \
    target/debug/sign_service_request \
      "$method" "$path" "$cluster_id" "$audience" "$issued_at_ms" "$nonce" "$body" \
      >"$output"
}

post_snapshot() {
  local body="$1" status="$2" output="$3"
  python3 benchmarks/trust_expiry_probe.py capture \
    --url "$distributor_url/v1/service-trust/snapshot" --method POST \
    --body "$body" --expect-status "$status" \
    --ca-cert "$pki_dir/ca.crt" --client-cert "$pki_dir/publisher.crt" \
    --client-key "$pki_dir/publisher.key" >"$output"
}

capture_distributor() {
  local output="$1"
  python3 benchmarks/trust_expiry_probe.py capture \
    --url "$distributor_url/v1/service-trust/status" --expect-status 200 \
    --ca-cert "$pki_dir/ca.crt" --client-cert "$pki_dir/publisher.crt" \
    --client-key "$pki_dir/publisher.key" >"$output"
}

capture_startup_failure() {
  local scenario="$1" snapshot="$2" expected_kind="$3" output="$4"
  local max_lifetime="$5" max_skew="$6" scenario_dir pid status state deadline
  local listener_ever_open=0 listener_probe_count=0
  scenario_dir="$proof_tmp/startup-$scenario"
  mkdir -p "$scenario_dir"
  INFERLAB_RAFT_NODE_ID='control-c' \
  INFERLAB_RAFT_CLUSTER_ID="$cluster_id" \
  INFERLAB_RAFT_BIND='127.0.0.1:10086' \
  INFERLAB_RAFT_PEERS='control-a=http://127.0.0.1:10081,control-b=http://127.0.0.1:10082' \
  INFERLAB_RAFT_DATA_DIR="$scenario_dir" \
  INFERLAB_RAFT_ELECTION_MIN_MS=5000 \
  INFERLAB_RAFT_ELECTION_MAX_MS=5100 \
  INFERLAB_RAFT_HEARTBEAT_MS=50 \
  INFERLAB_RAFT_RPC_TIMEOUT_MS=500 \
  INFERLAB_RAFT_COMMIT_TIMEOUT_MS=2500 \
  INFERLAB_CONTROL_SIGNING_KEY_ID="$route_key_id" \
  INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64="$route_seed" \
  INFERLAB_CONTROL_WRITER_KEYS="$writer_id=$writer_public" \
  INFERLAB_SERVICE_ID='control-c' \
  INFERLAB_SERVICE_CREDENTIAL_ID='key-a' \
  INFERLAB_SERVICE_PRIVATE_KEY_B64="$control_c_seed" \
  INFERLAB_SERVICE_TRUST_SNAPSHOT_PATH="$snapshot" \
  INFERLAB_SERVICE_TRUST_STATE_PATH="$scenario_dir/service-trust-floor.json" \
  INFERLAB_SERVICE_TRUST_ROOT_KEYS="$trust_root_id=$trust_root_public" \
  INFERLAB_SERVICE_TRUST_REVOKED_ROOT_KEY_IDS='' \
  INFERLAB_SERVICE_TRUST_MAX_POLICY_LIFETIME_MS="$max_lifetime" \
  INFERLAB_SERVICE_TRUST_POLICY_MAX_FUTURE_SKEW_MS="$max_skew" \
  INFERLAB_SERVICE_TRUST_POLL_MS=25 \
  INFERLAB_SERVICE_AUTH_MAX_AGE_MS=5000 \
  INFERLAB_SERVICE_AUTH_MAX_FUTURE_SKEW_MS=250 \
    target/debug/control-plane >"$scenario_dir/process.log" 2>&1 &
  pid="$!"
  live_pids+=("$pid")
  deadline=$((SECONDS + 30))
  while ((SECONDS < deadline)); do
    listener_probe_count="$((listener_probe_count + 1))"
    if listener_is_open 10086; then
      listener_ever_open=1
      break
    fi
    state="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
    if ! is_owned_child "$pid" || [[ "$state" == *Z* ]]; then
      break
    fi
    sleep 0.025
  done
  listener_probe_count="$((listener_probe_count + 1))"
  if listener_is_open 10086; then
    listener_ever_open=1
  fi
  if is_owned_child "$pid"; then
    state="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
    if [[ "$state" != *Z* ]]; then
      shutdown_child "$pid"
      forget_pid "$pid"
      echo "startup scenario unexpectedly remained live: $scenario" >&2
      return 1
    fi
  fi
  set +e
  wait "$pid"
  status="$?"
  set -e
  forget_pid "$pid"
  python3 - "$scenario" "$expected_kind" "$status" "$pid" \
    "$scenario_dir/process.log" "$scenario_dir" "$listener_ever_open" \
    "$listener_probe_count" >"$output" <<'PY'
import json
import socket
import sys
from pathlib import Path

(
    scenario, expected_kind, raw_status, raw_pid, raw_log, scenario_dir,
    raw_listener_ever_open, raw_listener_probe_count,
) = sys.argv[1:]
log = Path(raw_log).read_text(encoding="utf-8", errors="replace")
expected_diagnostics = {
    "issued_in_future": (
        'Error: Custom { kind: InvalidData, error: "service-trust policy validity '
        'rejected (issued_in_future): service-trust policy issue time exceeds the '
        'configured future-skew allowance of 250 ms" }'
    ),
    "lifetime_exceeded": (
        'Error: Custom { kind: InvalidData, error: "service-trust policy validity '
        'rejected (lifetime_exceeded): service-trust policy lifetime exceeds the '
        'configured 45000 ms maximum" }'
    ),
    "legacy_v1_disallowed": (
        'Error: Custom { kind: InvalidData, error: "service-trust policy validity '
        'rejected (legacy_v1_disallowed): legacy non-expiring service-trust policy '
        'v1 is disabled" }'
    ),
}
expected_diagnostic = expected_diagnostics.get(expected_kind)
if expected_diagnostic is None:
    raise SystemExit(f"unrecognized startup validity error kind: {expected_kind}")
listener_open = False
with socket.socket() as probe:
    probe.settimeout(0.1)
    listener_open = probe.connect_ex(("127.0.0.1", 10086)) == 0
result = {
    "schema": "inferlab.trust-expiry-startup-failure.v0.27",
    "scenario": scenario,
    "expected_error_kind": expected_kind,
    "exit_code": int(raw_status),
    "listener_ever_open": bool(int(raw_listener_ever_open)),
    "listener_probe_count": int(raw_listener_probe_count),
    "listener_open_after_exit": listener_open,
    "failed_before_listener": not bool(int(raw_listener_ever_open)) and not listener_open,
    "error_kind_observed": log.splitlines().count(expected_diagnostic) == 1,
    "durable_floor_created": (Path(scenario_dir) / "service-trust-floor.json").exists(),
    "log_excerpt": "\n".join(log.splitlines()[-30:]),
    "failed_pid": int(raw_pid),
}
print(json.dumps(result, indent=2, sort_keys=True))
if (
    result["exit_code"] == 0
    or not result["failed_before_listener"]
    or result["listener_ever_open"]
    or result["listener_probe_count"] < 1
    or not result["error_kind_observed"]
    or result["durable_floor_created"]
):
    raise SystemExit(1)
PY
}

capture_expired_cache_restart_failure() {
  local node_id='control-c' port peers seed election_min election_max pid status state deadline
  local listener_ever_open=0 listener_probe_count=0
  port="$(node_port "$node_id")"
  peers="$(node_peers "$node_id")"
  seed="$(node_seed "$node_id")"
  election_min="$(node_election_min "$node_id")"
  election_max="$((election_min + 100))"
  INFERLAB_RAFT_NODE_ID="$node_id" \
  INFERLAB_RAFT_CLUSTER_ID="$cluster_id" \
  INFERLAB_RAFT_BIND="127.0.0.1:$port" \
  INFERLAB_RAFT_PEERS="$peers" \
  INFERLAB_RAFT_DATA_DIR="$proof_tmp/$node_id" \
  INFERLAB_RAFT_ELECTION_MIN_MS="$election_min" \
  INFERLAB_RAFT_ELECTION_MAX_MS="$election_max" \
  INFERLAB_RAFT_HEARTBEAT_MS=50 \
  INFERLAB_RAFT_RPC_TIMEOUT_MS=500 \
  INFERLAB_RAFT_COMMIT_TIMEOUT_MS=2500 \
  INFERLAB_CONTROL_SIGNING_KEY_ID="$route_key_id" \
  INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64="$route_seed" \
  INFERLAB_CONTROL_WRITER_KEYS="$writer_id=$writer_public" \
  INFERLAB_SERVICE_ID="$node_id" \
  INFERLAB_SERVICE_CREDENTIAL_ID='key-a' \
  INFERLAB_SERVICE_PRIVATE_KEY_B64="$seed" \
  INFERLAB_SERVICE_TRUST_DISTRIBUTOR_URL="$distributor_url" \
  INFERLAB_SERVICE_TRUST_CACHE_PATH="$proof_tmp/$node_id/service-trust-cache.json" \
  INFERLAB_SERVICE_TRUST_STATE_PATH="$proof_tmp/$node_id/service-trust-floor.json" \
  INFERLAB_SERVICE_TRUST_ROOT_KEYS="$trust_root_id=$trust_root_public" \
  INFERLAB_SERVICE_TRUST_REVOKED_ROOT_KEY_IDS='' \
  INFERLAB_SERVICE_TRUST_MAX_POLICY_LIFETIME_MS="$policy_max_lifetime_ms" \
  INFERLAB_SERVICE_TRUST_POLICY_MAX_FUTURE_SKEW_MS="$policy_future_skew_ms" \
  INFERLAB_SERVICE_TRUST_TLS_CA_CERT_PATH="$pki_dir/ca.crt" \
  INFERLAB_SERVICE_TRUST_TLS_CLIENT_CERT_PATH="$pki_dir/$node_id.crt" \
  INFERLAB_SERVICE_TRUST_TLS_CLIENT_KEY_PATH="$pki_dir/$node_id.key" \
  INFERLAB_SERVICE_TRUST_POLL_MS=25 \
  INFERLAB_SERVICE_TRUST_REQUEST_TIMEOUT_MS=300 \
  INFERLAB_SERVICE_TRUST_MAX_BACKOFF_MS=200 \
  INFERLAB_SERVICE_AUTH_MAX_AGE_MS=5000 \
  INFERLAB_SERVICE_AUTH_MAX_FUTURE_SKEW_MS=250 \
    target/debug/control-plane >"$proof_tmp/expired-cache-restart.log" 2>&1 &
  pid="$!"
  expired_restart_failed_pid="$pid"
  live_pids+=("$pid")
  deadline=$((SECONDS + 30))
  while ((SECONDS < deadline)); do
    listener_probe_count="$((listener_probe_count + 1))"
    if listener_is_open "$port"; then
      listener_ever_open=1
      break
    fi
    state="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
    if ! is_owned_child "$pid" || [[ "$state" == *Z* ]]; then
      break
    fi
    sleep 0.025
  done
  listener_probe_count="$((listener_probe_count + 1))"
  if listener_is_open "$port"; then
    listener_ever_open=1
  fi
  if is_owned_child "$pid"; then
    state="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
    if [[ "$state" != *Z* ]]; then
      shutdown_child "$pid"
      forget_pid "$pid"
      echo 'expired-cache restart unexpectedly remained live' >&2
      return 1
    fi
  fi
  set +e
  wait "$pid"
  status="$?"
  set -e
  forget_pid "$pid"
  python3 - "$status" "$pid" "$proof_tmp/expired-cache-restart.log" \
    "$proof_tmp/control-c/service-trust-cache.json" \
    "$proof_tmp/control-c/service-trust-floor.json" \
    "$listener_ever_open" "$listener_probe_count" \
    >"$results_dir/expired-cache-restart.json" <<'PY'
import hashlib
import json
import socket
import sys
from pathlib import Path

status, pid, raw_log, raw_cache, raw_floor, raw_listener_ever_open, raw_listener_probe_count = sys.argv[1:]
log = Path(raw_log).read_text(encoding="utf-8", errors="replace")
with socket.socket() as probe:
    probe.settimeout(0.1)
    listener_open = probe.connect_ex(("127.0.0.1", 10083)) == 0

def digest(path: str) -> str:
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()

result = {
    "schema": "inferlab.trust-expiry-cache-restart.v0.27",
    "exit_code": int(status),
    "failed_pid": int(pid),
    "listener_ever_open": bool(int(raw_listener_ever_open)),
    "listener_probe_count": int(raw_listener_probe_count),
    "listener_open_after_exit": listener_open,
    "failed_before_listener": not bool(int(raw_listener_ever_open)) and not listener_open,
    "expired_error_observed": (
        "service-trust policy validity rejected (expired): "
        "service-trust policy is expired"
    ) in log,
    "cache_sha256": digest(raw_cache),
    "floor_sha256": digest(raw_floor),
    "log_excerpt": "\n".join(log.splitlines()[-30:]),
}
print(json.dumps(result, indent=2, sort_keys=True))
if (
    result["exit_code"] == 0
    or not result["failed_before_listener"]
    or result["listener_ever_open"]
    or result["listener_probe_count"] < 1
    or not result["expired_error_observed"]
):
    raise SystemExit(1)
PY
}

record_durable_state() {
  local output="$1"
  python3 - "$proof_tmp" >"$output" <<'PY'
import hashlib
import json
import sys
from pathlib import Path

root = Path(sys.argv[1])
nodes = {}
for node in ["control-a", "control-b", "control-c"]:
    cache_path = root / node / "service-trust-cache.json"
    floor_path = root / node / "service-trust-floor.json"
    cache_bytes = cache_path.read_bytes()
    floor_bytes = floor_path.read_bytes()
    cache = json.loads(cache_bytes)
    floor = json.loads(floor_bytes)
    snapshot = cache["snapshot"]
    nodes[node] = {
        "cache_sha256": hashlib.sha256(cache_bytes).hexdigest(),
        "floor_sha256": hashlib.sha256(floor_bytes).hexdigest(),
        "cache_schema": cache["schema"],
        "policy_schema": snapshot["schema"],
        "authentication_schema": snapshot["authentication"]["schema"],
        "authentication_algorithm": snapshot["authentication"]["algorithm"],
        "root_key_id": snapshot["authentication"]["key_id"],
        "snapshot_signature_sha256": hashlib.sha256(
            snapshot["authentication"]["signature"].encode("utf-8")
        ).hexdigest(),
        "generation": snapshot["generation"],
        "issued_at_ms": snapshot["issued_at_ms"],
        "expires_at_ms": snapshot.get("expires_at_ms"),
        "floor_schema": floor["schema"],
        "floor_generation": floor["generation"],
        "floor_signature_matches_cache": floor["signature"] == snapshot["authentication"]["signature"],
    }
print(json.dumps({
    "schema": "inferlab.trust-expiry-durable-state.v0.27",
    "nodes": nodes,
}, indent=2, sort_keys=True))
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
        check=False, capture_output=True, text=True,
    )
    fields = result.stdout.strip().split(None, 7)
    ppid = int(fields[0]) if len(fields) >= 1 and fields[0].isdigit() else None
    state = fields[1] if len(fields) >= 2 else None
    started = " ".join(fields[2:7]) if len(fields) >= 7 else None
    command = fields[7] if len(fields) >= 8 else None
    return {
        "pid": int(raw_pid), "parent_pid": ppid, "process_state": state,
        "start_token": started, "command": command,
        "alive": result.returncode == 0 and ppid is not None,
        "owned_child": ppid == proof_shell_pid,
        "non_zombie": state is not None and "Z" not in state,
    }

processes = {values[index]: observe(values[index + 1]) for index in range(0, len(values), 2)}
if not all(item["alive"] and item["owned_child"] and item["non_zombie"] and item["start_token"] for item in processes.values()):
    raise SystemExit("could not capture an owned non-zombie proof child")
print(json.dumps({
    "schema": "inferlab.trust-expiry-process-snapshot.v0.27",
    "proof_shell_pid": proof_shell_pid,
    "processes": processes,
}, indent=2, sort_keys=True))
PY
}

record_process_continuity() {
  assert_owned_processes_alive \
    control-a "$control_a_pid" control-b "$control_b_pid" control-c "$control_c_pid" \
    trust-distributor "$distributor_pid" cpu-worker "$worker_pid" gateway "$gateway_pid"
  python3 - "$proof_tmp/initial-processes.json" "$$" \
    "$expired_restart_failed_pid" \
    "$control_a_pid" "$control_b_pid" "$control_c_pid" "$distributor_pid" \
    "$worker_pid" "$gateway_pid" >"$results_dir/process-continuity.json" <<'PY'
import json
import subprocess
import sys
from pathlib import Path

initial = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
proof_shell_pid = int(sys.argv[2])
failed_restart_pid = int(sys.argv[3])

def observe(pid: int) -> dict:
    result = subprocess.run(
        ["ps", "-o", "ppid=", "-o", "stat=", "-o", "lstart=", "-o", "command=", "-p", str(pid)],
        check=False, capture_output=True, text=True,
    )
    fields = result.stdout.strip().split(None, 7)
    ppid = int(fields[0]) if len(fields) >= 1 and fields[0].isdigit() else None
    state = fields[1] if len(fields) >= 2 else None
    started = " ".join(fields[2:7]) if len(fields) >= 7 else None
    command = fields[7] if len(fields) >= 8 else None
    return {
        "pid": pid, "parent_pid": ppid, "process_state": state,
        "start_token": started, "command": command,
        "alive": result.returncode == 0 and ppid is not None,
        "owned_child": ppid == proof_shell_pid,
        "non_zombie": state is not None and "Z" not in state,
    }

current_pids = {
    "control-a": int(sys.argv[4]), "control-b": int(sys.argv[5]),
    "control-c": int(sys.argv[6]), "trust-distributor": int(sys.argv[7]),
    "cpu-worker": int(sys.argv[8]), "gateway": int(sys.argv[9]),
}
processes = {}
for name, current_pid in current_pids.items():
    before = initial["processes"][name]
    current = observe(current_pid)
    processes[name] = {
        "initial_pid": before["pid"], "current_pid": current_pid,
        "same_pid": before["pid"] == current_pid,
        "initial_start_token": before["start_token"],
        "current_start_token": current["start_token"],
        "same_start_token": before["start_token"] == current["start_token"],
        "initial_command": before["command"], "current_command": current["command"],
        "same_command": before["command"] == current["command"],
        "initial_parent_pid": before["parent_pid"],
        "initial_process_state": before["process_state"],
        "initial_alive": before["alive"],
        "initial_owned_child": before["owned_child"],
        "initial_non_zombie": before["non_zombie"],
        "parent_pid": current["parent_pid"], "process_state": current["process_state"],
        "alive": current["alive"], "owned_child": current["owned_child"],
        "non_zombie": current["non_zombie"],
    }
print(json.dumps({
    "schema": "inferlab.trust-expiry-process-continuity.v0.27",
    "proof_shell_pid": proof_shell_pid,
    "expected_restarts": ["control-c", "trust-distributor"],
    "failed_expired_cache_restart_pid": failed_restart_pid,
    "failed_restart_pid_is_not_live_participant": failed_restart_pid not in current_pids.values(),
    "processes": processes,
}, indent=2, sort_keys=True))
PY
}

scan_private_material() {
  local output="$1"
  python3 - "$results_dir" "$pki_dir" \
    "$route_seed" "$writer_seed" "$trust_root_seed" \
    "$control_a_seed" "$control_b_seed" "$control_c_seed" "$gateway_seed" \
    >"$output" <<'PY'
import json
import sys
from pathlib import Path

evidence = Path(sys.argv[1])
pki = Path(sys.argv[2])
seed_labels = [
    "route_seed", "writer_seed", "trust_root_seed", "control_a_seed",
    "control_b_seed", "control_c_seed", "gateway_seed",
]
seed_values = sys.argv[3:]

def compact(value: str) -> str:
    return "".join(value.replace("\\n", "").replace("\\r", "").split())

candidates = []
for label, value in zip(seed_labels, seed_values):
    normalized = compact(value)
    candidates.extend([(label, normalized), (f"{label}_unpadded", normalized.rstrip("="))])
key_files = sorted(pki.glob("*.key"))
for path in key_files:
    payload = compact("".join(
        line.strip() for line in path.read_text(encoding="utf-8").splitlines()
        if not line.startswith("-----")
    ))
    if not payload:
        raise SystemExit(f"generated PKI key payload is empty: {path.name}")
    candidates.extend([(f"pki:{path.name}", payload), (f"pki:{path.name}:unpadded", payload.rstrip("="))])
files = sorted(path for path in evidence.iterdir() if path.suffix in {".json", ".svg"})
matches = []
for path in files:
    retained = compact(path.read_text(encoding="utf-8", errors="replace"))
    for label, candidate in candidates:
        if len(candidate) >= 24 and candidate in retained:
            matches.append({"file": path.name, "candidate_label": label})
if matches:
    raise SystemExit("private material leaked into retained evidence: " + ", ".join(
        f"{item['candidate_label']} in {item['file']}" for item in matches
    ))
print(json.dumps({
    "schema": "inferlab.private-material-scan.v0.27",
    "files_scanned": [path.name for path in files],
    "known_ed25519_seed_labels": seed_labels,
    "known_ed25519_seed_count": len(seed_labels),
    "generated_pki_private_key_files": [path.name for path in key_files],
    "generated_pki_private_key_count": len(key_files),
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
    if path.suffix not in {".json", ".svg"}:
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
without_manifest = sorted(name for name in expected if name != "manifest.json")
before = sorted(path.name for path in directory.iterdir())
if before != without_manifest:
    raise SystemExit(f"unexpected pre-manifest evidence set: {before}")
files = []
for name in without_manifest:
    content = (directory / name).read_bytes()
    files.append({"path": name, "sha256": hashlib.sha256(content).hexdigest(), "bytes": len(content)})
(directory / "manifest.json").write_text(json.dumps({
    "schema": "inferlab.evidence-manifest.v0.27",
    "expected_files": expected, "file_count": len(expected),
    "hashed_file_count": len(files), "files": files,
}, indent=2, sort_keys=True) + "\n", encoding="utf-8")
if sorted(path.name for path in directory.iterdir()) != expected:
    raise SystemExit("final evidence set differs from its exact manifest")
PY
}

retain_results() {
  if [[ -z "${INFERLAB_V27_OUTPUT_DIR:-}" ]]; then
    return
  fi
  local name
  for name in "$@"; do
    if [[ "$name" != 'manifest.json' ]]; then
      cp "$results_dir/$name" "$INFERLAB_V27_OUTPUT_DIR/$name"
    fi
  done
  python3 - "$results_dir/manifest.json" "$INFERLAB_V27_OUTPUT_DIR" <<'PY'
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
  # The manifest is the completion marker and is published last.
  cp "$results_dir/manifest.json" "$INFERLAB_V27_OUTPUT_DIR/manifest.json"
}

prepare_output_dir
check_ports_are_free 10080 10081 10082 10083 10084 10085 10086
command -v openssl >/dev/null
cargo build --workspace --bins --quiet
generate_pki

trust_root_public="$(
  INFERLAB_SERVICE_TRUST_ROOT_KEY_ID="$trust_root_id" \
  INFERLAB_SERVICE_TRUST_ROOT_PRIVATE_KEY_B64="$trust_root_seed" \
    target/debug/service_trust_public_key
)"
control_a_public="$(public_key control-a "$control_a_seed")"
control_b_public="$(public_key control-b "$control_b_seed")"
control_c_public="$(public_key control-c "$control_c_seed")"
gateway_public="$(public_key gateway-primary "$gateway_seed")"

fixture_issued_at="$(now_ms)"
fixture_expires_at="$((fixture_issued_at + g1_lifetime_ms))"
python3 - "$fixture_issued_at" "$fixture_expires_at" "$control_a_public" \
  "$control_b_public" "$control_c_public" "$gateway_public" "$policy_dir" <<'PY'
import json
import sys
from pathlib import Path

issued_at = int(sys.argv[1])
expires_at = int(sys.argv[2])
control_a, control_b, control_c, gateway = sys.argv[3:7]
directory = Path(sys.argv[7])
credentials = [
    {"service_id": "control-a", "credential_id": "key-a", "public_key_base64": control_a},
    {"service_id": "control-b", "credential_id": "key-a", "public_key_base64": control_b},
    {"service_id": "control-c", "credential_id": "key-a", "public_key_base64": control_c},
    {"service_id": "gateway-primary", "credential_id": "key-a", "public_key_base64": gateway},
]

def write(name: str, schema: str, generation: int, issued: int, expiry) -> None:
    policy = {
        "schema": schema,
        "cluster_id": "inferlab-primary",
        "generation": generation,
        "issued_at_ms": issued,
        "trusted_credentials": credentials,
        "revoked_service_ids": [],
        "revoked_credentials": [],
        "gateway_service_ids": ["gateway-primary"],
    }
    if expiry is not None:
        policy["expires_at_ms"] = expiry
    (directory / f"policy-{name}.json").write_text(
        json.dumps(policy, indent=2) + "\n", encoding="utf-8"
    )

write("g1", "inferlab.service-trust-policy.v2", 1, issued_at, expires_at)
write("g1-fork", "inferlab.service-trust-policy.v2", 1, issued_at, expires_at + 1_000)
write("future", "inferlab.service-trust-policy.v2", 91, issued_at + 10_000, issued_at + 20_000)
write("excess", "inferlab.service-trust-policy.v2", 92, issued_at, issued_at + 60_000)
write("legacy-v1", "inferlab.service-trust-policy.v1", 93, issued_at, None)
PY

for name in future excess legacy-v1; do
  sign_policy "$policy_dir/policy-$name.json" "$policy_dir/snapshot-$name.json"
done

# Receiver-only validity failures run as bounded transient OS processes. They
# cannot advance the main distributor and must fail before opening a listener.
capture_startup_failure future-issued "$policy_dir/snapshot-future.json" \
  issued_in_future "$results_dir/future-issued-startup.json" 45000 250
capture_startup_failure excessive-lifetime "$policy_dir/snapshot-excess.json" \
  lifetime_exceeded "$results_dir/excessive-lifetime-startup.json" 45000 250
capture_startup_failure legacy-v1-default "$policy_dir/snapshot-legacy-v1.json" \
  legacy_v1_disallowed "$results_dir/legacy-v1-startup.json" 45000 250

# Give the live g1 schedule its complete signed lifetime after the independent
# startup-rejection processes finish; a slow runner cannot spend that window on
# setup evidence that is unrelated to the cutoff experiment.
g1_issued_at="$(now_ms)"
g1_expires_at="$((g1_issued_at + g1_lifetime_ms))"
python3 - "$policy_dir" "$g1_issued_at" "$g1_expires_at" <<'PY'
import json
import sys
from pathlib import Path

directory = Path(sys.argv[1])
issued_at, expires_at = map(int, sys.argv[2:4])
for name, expiry in [("g1", expires_at), ("g1-fork", expires_at + 1_000)]:
    path = directory / f"policy-{name}.json"
    policy = json.loads(path.read_text(encoding="utf-8"))
    policy["issued_at_ms"] = issued_at
    policy["expires_at_ms"] = expiry
    path.write_text(json.dumps(policy, indent=2) + "\n", encoding="utf-8")
PY
for name in g1 g1-fork; do
  sign_policy "$policy_dir/policy-$name.json" "$policy_dir/snapshot-$name.json"
done
python3 - "$policy_dir/snapshot-g1.json" "$policy_dir" <<'PY'
import json
import sys
from pathlib import Path

source = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
directory = Path(sys.argv[2])
tampered = json.loads(json.dumps(source))
tampered["expires_at_ms"] += 1
(directory / "snapshot-expiry-tampered.json").write_text(
    json.dumps(tampered, indent=2) + "\n", encoding="utf-8"
)
malformed = json.loads(json.dumps(source))
malformed["expires_at_ms"] = malformed["issued_at_ms"]
(directory / "snapshot-malformed-window.json").write_text(
    json.dumps(malformed, indent=2) + "\n", encoding="utf-8"
)
PY

start_distributor
initial_distributor_pid="$distributor_pid"
post_snapshot "$policy_dir/snapshot-g1.json" 201 "$results_dir/publish-g1.json"

# B and C are serving with long election windows before short-timeout A starts,
# so A can deterministically obtain its initial majority without a startup race.
start_node control-b
start_node control-c
start_node control-a
initial_control_a_pid="$control_a_pid"
initial_control_b_pid="$control_b_pid"
initial_control_c_pid="$control_c_pid"

python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$control_urls" --timeout 10 >"$results_dir/initial-cluster.json"
python3 benchmarks/trust_expiry_probe.py wait-controls \
  --urls "$control_urls" --generation 1 --validity valid \
  --expires-at-ms "$g1_expires_at" --bootstrap-source remote --timeout 10 \
  >"$results_dir/generation-1-controls.json"
python3 benchmarks/trust_expiry_probe.py wait-distributor \
  --url "$distributor_url" --generation 1 \
  --policy-schema 'inferlab.service-trust-policy.v2' \
  --expires-at-ms "$g1_expires_at" \
  --acked-receivers 'control-a/key-a,control-b/key-a,control-c/key-a' --timeout 10 \
  --ca-cert "$pki_dir/ca.crt" --client-cert "$pki_dir/publisher.crt" \
  --client-key "$pki_dir/publisher.key" \
  >"$results_dir/generation-1-receipts.json"

python3 - "$proof_tmp/control-config.json" <<'PY'
import json
import sys
from pathlib import Path

Path(sys.argv[1]).write_text(json.dumps({
    "routing_policy": "round-robin",
    "workers": [{"id": "cpu-trust-expiry", "base_url": "http://127.0.0.1:10084", "weight": 1}],
}, indent=2) + "\n", encoding="utf-8")
PY
leader_url="$(json_field "$results_dir/initial-cluster.json" leader_url)"
leader_id="$(json_field "$results_dir/initial-cluster.json" leader_id)"
INFERLAB_CONTROL_WRITER_ID="$writer_id" \
INFERLAB_CONTROL_WRITER_PRIVATE_KEY_B64="$writer_seed" \
  target/debug/sign_control_write \
    "$cluster_id" 0 now trust-expiry-route-1 "$proof_tmp/control-config.json" \
    >"$proof_tmp/control-write.json"
python3 benchmarks/control_write_probe.py submit \
  --url "$leader_url" --body "$proof_tmp/control-write.json" \
  >"$results_dir/write-committed.json"

start_worker
start_gateway
python3 benchmarks/full_stack_probe.py wait-gateway \
  --gateway-url "$gateway_url" --policy round-robin --revision 2 \
  --worker-ids cpu-trust-expiry --timeout 10 >"$results_dir/gateway-ready.json"
python3 benchmarks/full_stack_probe.py requests \
  --gateway-url "$gateway_url" --requests 1 \
  --prompt 'valid signed policy generation one' --speculative-tokens 2 \
  >"$results_dir/generation-1-request.json"

capture_process_snapshot "$proof_tmp/initial-processes.json" \
  control-a "$control_a_pid" control-b "$control_b_pid" control-c "$control_c_pid" \
  trust-distributor "$distributor_pid" cpu-worker "$worker_pid" gateway "$gateway_pid"
record_durable_state "$results_dir/durable-generation-1.json"

# The main distributor stays on g1 throughout the attacks. Structural and
# signature failures are invalid_snapshot; an authentic same-generation
# deadline fork is a distinct 409 conflict.
post_snapshot "$policy_dir/snapshot-expiry-tampered.json" 400 \
  "$results_dir/expiry-tamper.json"
post_snapshot "$policy_dir/snapshot-malformed-window.json" 400 \
  "$results_dir/malformed-window.json"
post_snapshot "$policy_dir/snapshot-g1-fork.json" 409 \
  "$results_dir/same-generation-deadline-fork.json"
capture_distributor "$results_dir/post-candidate-attacks.json"
record_durable_state "$results_dir/durable-after-candidate-attacks.json"

# An observed 200 followed by If-None-Match 304 binds that transport success
# to the exact unchanged signed deadline. It is not a renewal.
python3 benchmarks/trust_expiry_probe.py conditional-get \
  --url "$distributor_url" --generation 1 --expires-at-ms "$g1_expires_at" \
  --ca-cert "$pki_dir/ca.crt" --client-cert "$pki_dir/publisher.crt" \
  --client-key "$pki_dir/publisher.key" \
  >"$results_dir/not-modified-does-not-renew.json"
python3 benchmarks/trust_expiry_probe.py wait-controls \
  --urls "$control_urls" --generation 1 --validity valid \
  --expires-at-ms "$g1_expires_at" --last-fetch-outcome not-modified --timeout 10 \
  >"$results_dir/pre-expiry-controls.json"

auth_issued_at="$((g1_expires_at - 1600))"
sign_service_request gateway-primary GET /v1/control/config "$leader_id" \
  "$auth_issued_at" 'expiry-pre-request-0001' - "$proof_tmp/pre-expiry-auth.json"
sign_service_request gateway-primary GET /v1/control/config "$leader_id" \
  "$auth_issued_at" 'expiry-post-request-0002' - "$proof_tmp/post-expiry-auth.json"
python3 benchmarks/trust_expiry_probe.py cutoff \
  --gateway-url "$gateway_url" --control-url "$leader_url" \
  --expires-at-ms "$g1_expires_at" \
  --pre-authentication "$proof_tmp/pre-expiry-auth.json" \
  --post-authentication "$proof_tmp/post-expiry-auth.json" \
  --stream-start-before-ms 1500 --pre-request-before-ms 400 \
  --post-request-after-ms 25 --timeout 15 \
  >"$results_dir/request-time-cutoff-and-admitted-stream.json"
python3 benchmarks/trust_expiry_probe.py wait-controls \
  --urls "$control_urls" --generation 1 --validity expired \
  --expires-at-ms "$g1_expires_at" --timeout 10 \
  >"$results_dir/expired-controls.json"
record_durable_state "$results_dir/durable-expired-generation-1.json"

old_control_c_pid="$control_c_pid"
stop_node control-c
old_distributor_pid="$distributor_pid"
stop_distributor
python3 benchmarks/trust_expiry_probe.py expect-transport-failure \
  --scenario distributor-withheld-and-stopped \
  --url "$distributor_url/health" --timeout 0.5 \
  --ca-cert "$pki_dir/ca.crt" --client-cert "$pki_dir/publisher.crt" \
  --client-key "$pki_dir/publisher.key" >"$results_dir/distributor-outage.json"
capture_expired_cache_restart_failure

# Recovery is a distinct higher generation with its own freshly signed
# deadline. The expired cache/floor are preserved and advanced, not deleted.
start_distributor
g2_issued_at="$(now_ms)"
g2_expires_at="$((g2_issued_at + g2_lifetime_ms))"
python3 - "$g2_issued_at" "$g2_expires_at" "$control_a_public" \
  "$control_b_public" "$control_c_public" "$gateway_public" \
  "$policy_dir/policy-g2.json" <<'PY'
import json
import sys
from pathlib import Path

issued_at, expires_at = map(int, sys.argv[1:3])
public_keys = sys.argv[3:7]
ids = ["control-a", "control-b", "control-c", "gateway-primary"]
policy = {
    "schema": "inferlab.service-trust-policy.v2",
    "cluster_id": "inferlab-primary",
    "generation": 2,
    "issued_at_ms": issued_at,
    "expires_at_ms": expires_at,
    "trusted_credentials": [
        {"service_id": service_id, "credential_id": "key-a", "public_key_base64": key}
        for service_id, key in zip(ids, public_keys)
    ],
    "revoked_service_ids": [],
    "revoked_credentials": [],
    "gateway_service_ids": ["gateway-primary"],
}
Path(sys.argv[7]).write_text(json.dumps(policy, indent=2) + "\n", encoding="utf-8")
PY
sign_policy "$policy_dir/policy-g2.json" "$policy_dir/snapshot-g2.json"
post_snapshot "$policy_dir/snapshot-g2.json" 201 "$results_dir/publish-g2.json"
start_node control-c
python3 benchmarks/trust_expiry_probe.py wait-controls \
  --urls "$control_urls" --generation 2 --validity valid \
  --expires-at-ms "$g2_expires_at" --timeout 15 \
  >"$results_dir/generation-2-controls.json"
python3 benchmarks/trust_expiry_probe.py wait-distributor \
  --url "$distributor_url" --generation 2 \
  --policy-schema 'inferlab.service-trust-policy.v2' \
  --expires-at-ms "$g2_expires_at" \
  --acked-receivers 'control-a/key-a,control-b/key-a,control-c/key-a' --timeout 15 \
  --ca-cert "$pki_dir/ca.crt" --client-cert "$pki_dir/publisher.crt" \
  --client-key "$pki_dir/publisher.key" \
  >"$results_dir/generation-2-receipts.json"
record_durable_state "$results_dir/durable-generation-2.json"

python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$control_urls" --timeout 15 >"$results_dir/final-cluster.json"
python3 benchmarks/full_stack_probe.py wait-gateway \
  --gateway-url "$gateway_url" --policy round-robin --revision 2 \
  --worker-ids cpu-trust-expiry --timeout 15 >"$results_dir/final-gateway.json"
python3 benchmarks/full_stack_probe.py requests \
  --gateway-url "$gateway_url" --requests 1 \
  --prompt 'valid generation two recovery json' --speculative-tokens 2 \
  >"$results_dir/final-request.json"
python3 benchmarks/full_stack_probe.py stream \
  --gateway-url "$gateway_url" --prompt 'valid generation two recovery stream' \
  --speculative-tokens 2 >"$results_dir/final-stream.json"
record_process_continuity

validity_test_filters=(
  'service_authentication::tests::signed_policy_expiry_is_exclusive_latched_and_recovers_on_higher_generation'
  'service_trust::tests::post_persist_expiry_advances_floor_without_activation_or_rollback'
  'service_trust::tests::local_future_issued_snapshot_is_retried_when_unchanged_bytes_become_eligible'
  'service_trust::tests::unchanged_304_does_not_renew_expiry_and_valid_higher_generation_recovers'
  'service_trust::tests::unchanged_local_poll_latches_expiry_against_backward_clock'
  'service_trust::tests::remote_post_persist_expiry_advances_floor_without_activation_or_receipt'
  'service_trust::tests::remote_etag_update_failure_and_receipt_paths_keep_last_known_good'
)
production_test_arguments=()
production_test_index=0
for validity_test_filter in "${validity_test_filters[@]}"; do
  production_test_log="$proof_tmp/production-validity-test-$production_test_index.log"
  set +e
  CARGO_TERM_COLOR=never cargo test -p control-plane --lib \
    "$validity_test_filter" -- --exact \
    >"$production_test_log" 2>&1
  production_test_status="$?"
  set -e
  production_test_arguments+=(
    "$validity_test_filter" "$production_test_status" "$production_test_log"
  )
  production_test_index="$((production_test_index + 1))"
done
python3 - "${production_test_arguments[@]}" \
  >"$results_dir/production-validity-tests.json" <<'PY'
import json
import re
import sys
from pathlib import Path

values = sys.argv[1:]
if len(values) % 3:
    raise SystemExit("production validity evidence requires filter/status/log triples")
tests = []
for index in range(0, len(values), 3):
    test_filter, status, path = values[index:index + 3]
    output = Path(path).read_text(encoding="utf-8", errors="replace")
    lines = output.splitlines()
    summary_pattern = re.compile(
        r"^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; "
        r"[0-9]+ filtered out; finished in [0-9]+(?:\.[0-9]+)?s$"
    )
    running_one_test = lines.count("running 1 test") == 1
    exact_test_line = lines.count(f"test {test_filter} ... ok") == 1
    matching_summaries = [line for line in lines if summary_pattern.fullmatch(line)]
    exact_summary = len(matching_summaries) == 1
    tests.append({
        "command": [
            "cargo", "test", "-p", "control-plane", "--lib", test_filter,
            "--", "--exact",
        ],
        "environment": {"CARGO_TERM_COLOR": "never"},
        "test_filter": test_filter,
        "exit_code": int(status),
        "running_one_test": running_one_test,
        "exact_test_line": exact_test_line,
        "exact_summary": exact_summary,
        "summary_line": matching_summaries[0] if exact_summary else None,
        "output": output,
    })
result = {
    "schema": "inferlab.trust-expiry-production-tests.v0.27",
    "test_count": len(tests),
    "tests": tests,
}
print(json.dumps(result, indent=2, sort_keys=True))
if len(tests) != 7 or not all(
    item["exit_code"] == 0
    and item["running_one_test"]
    and item["exact_test_line"]
    and item["exact_summary"]
    for item in tests
):
    raise SystemExit(1)
PY

python3 benchmarks/trust_expiry_probe.py sanitize-evidence \
  --evidence-dir "$results_dir" --proof-root "$proof_tmp" \
  --project-root "$project_root" >"$proof_tmp/sanitizer.json"
mv "$proof_tmp/sanitizer.json" "$results_dir/sanitizer.json"
scan_private_material "$proof_tmp/private-preliminary.json"
mv "$proof_tmp/private-preliminary.json" "$results_dir/private-material-scan.json"
python3 benchmarks/check_trust_expiry.py \
  --evidence-dir "$results_dir" --output "$results_dir/assertions.json"
python3 benchmarks/render_trust_expiry_svg.py \
  --evidence-dir "$results_dir" --output "$results_dir/trust-expiry-proof.svg"
scan_private_material "$proof_tmp/private-retained.json"
mv "$proof_tmp/private-retained.json" "$results_dir/private-material-scan.json"
python3 benchmarks/check_trust_expiry.py \
  --evidence-dir "$results_dir" --output "$results_dir/assertions.json"
python3 benchmarks/render_trust_expiry_svg.py \
  --evidence-dir "$results_dir" --output "$results_dir/trust-expiry-proof.svg"
python3 benchmarks/check_trust_expiry.py \
  --evidence-dir "$results_dir" --output "$proof_tmp/replay-assertions.json"
cmp "$results_dir/assertions.json" "$proof_tmp/replay-assertions.json"
python3 benchmarks/render_trust_expiry_svg.py \
  --evidence-dir "$results_dir" --output "$proof_tmp/replay-trust-expiry-proof.svg"
cmp "$results_dir/trust-expiry-proof.svg" "$proof_tmp/replay-trust-expiry-proof.svg"
scan_private_material "$proof_tmp/private-final-discarded.json"
final_leak_scan

expected_files=(
  assertions.json
  distributor-outage.json
  durable-after-candidate-attacks.json
  durable-expired-generation-1.json
  durable-generation-1.json
  durable-generation-2.json
  excessive-lifetime-startup.json
  expired-cache-restart.json
  expired-controls.json
  expiry-tamper.json
  final-cluster.json
  final-gateway.json
  final-request.json
  final-stream.json
  future-issued-startup.json
  gateway-ready.json
  generation-1-controls.json
  generation-1-receipts.json
  generation-1-request.json
  generation-2-controls.json
  generation-2-receipts.json
  initial-cluster.json
  legacy-v1-startup.json
  malformed-window.json
  manifest.json
  not-modified-does-not-renew.json
  post-candidate-attacks.json
  pre-expiry-controls.json
  private-material-scan.json
  process-continuity.json
  production-validity-tests.json
  publish-g1.json
  publish-g2.json
  request-time-cutoff-and-admitted-stream.json
  same-generation-deadline-fork.json
  sanitizer.json
  trust-expiry-proof.svg
  write-committed.json
)
write_manifest "${expected_files[@]}"
python3 benchmarks/check_trust_expiry.py \
  --evidence-dir "$results_dir" --require-manifest \
  --output "$proof_tmp/post-manifest-assertions.json"
cmp "$results_dir/assertions.json" "$proof_tmp/post-manifest-assertions.json"
python3 benchmarks/render_trust_expiry_svg.py \
  --evidence-dir "$results_dir" --output "$proof_tmp/post-manifest-trust-expiry-proof.svg"
cmp "$results_dir/trust-expiry-proof.svg" "$proof_tmp/post-manifest-trust-expiry-proof.svg"
retain_results "${expected_files[@]}"
if [[ -n "${INFERLAB_V27_OUTPUT_DIR:-}" ]]; then
  python3 benchmarks/check_trust_expiry.py \
    --evidence-dir "$INFERLAB_V27_OUTPUT_DIR" --require-manifest \
    --output "$proof_tmp/retained-assertions.json"
  cmp "$results_dir/assertions.json" "$proof_tmp/retained-assertions.json"
  python3 benchmarks/render_trust_expiry_svg.py \
    --evidence-dir "$INFERLAB_V27_OUTPUT_DIR" \
    --output "$proof_tmp/retained-trust-expiry-proof.svg"
  cmp "$results_dir/trust-expiry-proof.svg" \
    "$proof_tmp/retained-trust-expiry-proof.svg"
fi

python3 - "$results_dir/assertions.json" <<'PY'
import json
import sys

assertions = json.loads(open(sys.argv[1], encoding="utf-8").read())
print(
    f"v0.27 exact-process signed trust-expiry proof complete: "
    f"{assertions['passed']}/{assertions['total']} assertions passed"
)
PY
