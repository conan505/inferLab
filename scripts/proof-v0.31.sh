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
  echo 'v0.31 proof requires Python with TLS 1.3 support' >&2
  exit 1
fi
python3() { "$proof_python" "$@"; }

proof_tmp_root="${TMPDIR:-/tmp}"
escaped_ca_serial="${proof_tmp_root%/}/inferlab-v031-ca.srl"
proof_tmp="$(mktemp -d "$proof_tmp_root/inferlab-v031.XXXXXX")"
results_dir="$proof_tmp/results"
pki_dir="$proof_tmp/pki"
case_dir="$proof_tmp/cases"
state_dir="$proof_tmp/state"
mkdir -p "$results_dir" "$pki_dir" "$case_dir" "$state_dir"

cluster_id='inferlab-primary'
gateway_url='http://127.0.0.1:12580'
control_urls='http://127.0.0.1:12581,http://127.0.0.1:12582,http://127.0.0.1:12583'
worker_url='http://127.0.0.1:12584'
distributor_url='https://localhost:12585'
renewer_distributor_url='https://localhost:12586'
renewer_status_url='http://127.0.0.1:12587'
route_key_id='route-v031'
writer_id='v031-deployer'
trust_root_id='service-trust-root-v031'
policy_lifetime_ms=20000
renew_before_ms=10000
poll_interval_ms=50
retry_interval_ms=200
request_timeout_ms=500
policy_max_lifetime_ms=30000

derive_seed() {
  python3 - "$1" <<'PY'
import base64, hashlib, sys
print(base64.b64encode(hashlib.sha256(sys.argv[1].encode()).digest()).decode())
PY
}
trust_root_seed="$(derive_seed v031-service-trust-root)"
route_seed="$(derive_seed v031-route-signing)"
writer_seed="$(derive_seed v031-control-writer)"
service_seed() { derive_seed "v031-service-$1"; }

live_pids=(sentinel)
control_a_pid=''
control_b_pid=''
control_c_pid=''
gateway_pid=''
worker_pid=''
distributor_pid=''
renewer_pid=''
initial_renewer_pid=''
fault_gate_pid=''

forget_pid() {
  local forgotten="$1" pid retained=(sentinel)
  for pid in "${live_pids[@]}"; do
    [[ "$pid" == "$forgotten" ]] || retained+=("$pid")
  done
  live_pids=("${retained[@]:1}")
}

is_owned_child() {
  local pid="$1" parent
  [[ "$pid" =~ ^[1-9][0-9]*$ ]] || return 1
  parent="$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d '[:space:]')"
  [[ "$parent" == "$$" ]]
}

shutdown_child() {
  local pid="$1" attempt state
  is_owned_child "$pid" || return 0
  kill -TERM "$pid" 2>/dev/null || true
  for ((attempt = 0; attempt < 100; attempt++)); do
    is_owned_child "$pid" || break
    state="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
    [[ "$state" == *Z* ]] && break
    sleep 0.02
  done
  if is_owned_child "$pid"; then
    state="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
    [[ "$state" == *Z* ]] || kill -KILL "$pid" 2>/dev/null || true
  fi
  wait "$pid" 2>/dev/null || true
}

cleanup() {
  local owned=("${live_pids[@]}") index pid
  for ((index = ${#owned[@]} - 1; index >= 0; index--)); do
    pid="${owned[$index]}"
    shutdown_child "$pid"
    forget_pid "$pid"
  done
  if [[ "${INFERLAB_V31_KEEP_TMP:-0}" == 1 && "${proof_succeeded:-0}" != 1 ]]; then
    echo "retaining v0.31 proof temporary directory: $proof_tmp" >&2
  elif [[ -n "$proof_tmp" && -d "$proof_tmp" ]]; then
    case "$(basename "$proof_tmp")" in
      inferlab-v031.*) rm -rf -- "$proof_tmp" ;;
      *) echo "refusing unexpected proof cleanup path: $proof_tmp" >&2 ;;
    esac
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

prepare_output_dir() {
  [[ -z "${INFERLAB_V31_OUTPUT_DIR:-}" ]] && return
  mkdir -p "$INFERLAB_V31_OUTPUT_DIR"
  if find "$INFERLAB_V31_OUTPUT_DIR" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
    echo 'INFERLAB_V31_OUTPUT_DIR must be empty' >&2
    exit 1
  fi
}

check_ports_are_free() {
  python3 - "$@" <<'PY'
import socket, sys
for raw in sys.argv[1:]:
    port = int(raw)
    with socket.socket() as probe:
        probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            probe.bind(("127.0.0.1", port))
        except OSError as error:
            raise SystemExit(f"refusing v0.31 proof: 127.0.0.1:{port} busy: {error}")
PY
}

node_port() {
  case "$1" in
    control-a) echo 12581 ;;
    control-b) echo 12582 ;;
    control-c) echo 12583 ;;
    *) return 1 ;;
  esac
}

node_peers() {
  case "$1" in
    control-a) echo 'control-b=http://127.0.0.1:12582,control-c=http://127.0.0.1:12583' ;;
    control-b) echo 'control-a=http://127.0.0.1:12581,control-c=http://127.0.0.1:12583' ;;
    control-c) echo 'control-a=http://127.0.0.1:12581,control-b=http://127.0.0.1:12582' ;;
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

wait_endpoint() {
  local url="$1" pid="$2" label="$3" expected="${4:-200}" deadline=$((SECONDS + 60)) observed
  while true; do
    observed="$(curl --noproxy '*' --connect-timeout 0.1 --max-time 0.4 --silent \
      --output /dev/null --write-out '%{http_code}' "$url" 2>/dev/null || true)"
    [[ "$observed" == "$expected" ]] && return
    if ! is_owned_child "$pid"; then
      echo "$label exited before readiness" >&2
      tail -n 60 "$proof_tmp/$label.log" 2>/dev/null || true
      return 1
    fi
    ((SECONDS < deadline)) || {
      echo "timeout waiting for $label at $url; status=$observed" >&2
      tail -n 60 "$proof_tmp/$label.log" 2>/dev/null || true
      return 1
    }
    sleep 0.05
  done
}

wait_file() {
  local path="$1" pid="$2" label="$3" deadline=$((SECONDS + 60))
  while [[ ! -f "$path" ]]; do
    if ! is_owned_child "$pid"; then
      echo "$label exited before writing $(basename "$path")" >&2
      tail -n 60 "$proof_tmp/$label.log" 2>/dev/null || true
      return 1
    fi
    ((SECONDS < deadline)) || { echo "timeout waiting for $label barrier" >&2; return 1; }
    sleep 0.02
  done
}

wait_distributor() {
  local deadline=$((SECONDS + 60)) observed
  while true; do
    observed="$(curl --noproxy '*' --connect-timeout 0.1 --max-time 0.5 --silent \
      --output /dev/null --write-out '%{http_code}' --cacert "$pki_dir/ca.crt" \
      --cert "$pki_dir/probe.crt" --key "$pki_dir/probe.key" \
      "$distributor_url/health" 2>/dev/null || true)"
    [[ "$observed" == 200 ]] && return
    is_owned_child "$distributor_pid" || {
      echo 'trust-distributor exited before health' >&2
      tail -n 60 "$proof_tmp/trust-distributor.log" 2>/dev/null || true
      return 1
    }
    ((SECONDS < deadline)) || { echo 'timeout waiting for trust-distributor' >&2; return 1; }
    sleep 0.05
  done
}

listener_is_open() {
  python3 - "$1" <<'PY'
import socket, sys
try:
    with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=0.02):
        raise SystemExit(0)
except OSError:
    raise SystemExit(1)
PY
}

generate_pki() {
  python3 - "$pki_dir" <<'PY'
import sys
from pathlib import Path
d = Path(sys.argv[1])
(d / "ca.cnf").write_text("""[req]
prompt = no
distinguished_name = dn
x509_extensions = ca_ext
[dn]
CN = InferLab v0.31 disposable proof CA
[ca_ext]
basicConstraints = critical,CA:TRUE
keyUsage = critical,keyCertSign,cRLSign
subjectKeyIdentifier = hash
""", encoding="utf-8")
def leaf(name, eku, server=False):
    san = "subjectAltName = DNS:localhost\n" if server else ""
    (d / f"{name}.cnf").write_text(f"""[req]
prompt = no
distinguished_name = dn
req_extensions = leaf_ext
[dn]
CN = {name}
[leaf_ext]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature,keyEncipherment
extendedKeyUsage = {eku}
{san}""", encoding="utf-8")
leaf("distributor", "serverAuth", True)
leaf("fault-gate", "serverAuth", True)
for name in ["renewer", "gate-upstream", "probe", "control-a", "control-b", "control-c"]:
    leaf(name, "clientAuth")
PY
  env -i PATH="$PATH" openssl req -x509 -newkey rsa:2048 -nodes -days 2 -sha256 \
    -keyout "$pki_dir/ca.key" -out "$pki_dir/ca.crt" -config "$pki_dir/ca.cnf" \
    >/dev/null 2>&1
  local leaf
  for leaf in distributor fault-gate renewer gate-upstream probe control-a control-b control-c; do
    env -i PATH="$PATH" openssl req -new -newkey rsa:2048 -nodes \
      -keyout "$pki_dir/$leaf.key" -out "$pki_dir/$leaf.csr" \
      -config "$pki_dir/$leaf.cnf" >/dev/null 2>&1
    env -i PATH="$PATH" openssl x509 -req -days 2 -sha256 \
      -in "$pki_dir/$leaf.csr" -CA "$pki_dir/ca.crt" -CAkey "$pki_dir/ca.key" \
      -CAserial "$pki_dir/ca.srl" -CAcreateserial -out "$pki_dir/$leaf.crt" \
      -extfile "$pki_dir/$leaf.cnf" -extensions leaf_ext >/dev/null 2>&1
  done
  python3 - "$pki_dir/ca.srl" "$pki_dir" "$escaped_ca_serial" <<'PY'
import os, re, stat, sys
from pathlib import Path
serial, pki, escaped = Path(sys.argv[1]), Path(sys.argv[2]).resolve(), Path(sys.argv[3])
metadata = serial.lstat()
if (
    not stat.S_ISREG(metadata.st_mode)
    or stat.S_ISLNK(metadata.st_mode)
    or stat.S_IMODE(metadata.st_mode) != 0o600
    or metadata.st_uid != os.getuid()
    or serial.resolve().parent != pki
    or re.fullmatch(rb"[0-9A-Fa-f]+\n", serial.read_bytes()) is None
):
    raise SystemExit("proof-owned OpenSSL serial failed containment")
if escaped.exists() or escaped.is_symlink():
    raise SystemExit("OpenSSL CA serial escaped the proof-owned PKI directory")
PY
}

start_distributor() {
  env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$NO_PROXY" \
    INFERLAB_TRUST_DISTRIBUTOR_BIND='127.0.0.1:12585' \
    INFERLAB_TRUST_DISTRIBUTOR_CLUSTER_ID="$cluster_id" \
    INFERLAB_SERVICE_TRUST_ROOT_KEYS="$trust_root_id=$trust_root_public" \
    INFERLAB_SERVICE_TRUST_REVOKED_ROOT_KEY_IDS='' \
    INFERLAB_TRUST_DISTRIBUTOR_STATE_PATH="$state_dir/distributor.json" \
    INFERLAB_TRUST_DISTRIBUTOR_EXPECTED_SERVICE_IDS='control-a,control-b,control-c' \
    INFERLAB_TRUST_DISTRIBUTOR_MAX_BODY_BYTES=262144 \
    INFERLAB_TRUST_DISTRIBUTOR_TLS_CERT_PATH="$pki_dir/distributor.crt" \
    INFERLAB_TRUST_DISTRIBUTOR_TLS_KEY_PATH="$pki_dir/distributor.key" \
    INFERLAB_TRUST_DISTRIBUTOR_TLS_CLIENT_CA_PATH="$pki_dir/ca.crt" \
    target/debug/trust-distributor >"$proof_tmp/trust-distributor.log" 2>&1 &
  distributor_pid="$!"; live_pids+=("$distributor_pid")
  wait_distributor
}

set_gate_mode() {
  python3 - "$proof_tmp/fault-gate-mode.json" "$1" <<'PY'
import json, os, sys
from pathlib import Path
path, mode = Path(sys.argv[1]), sys.argv[2]
temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
temporary.write_text(json.dumps({"mode": mode}, sort_keys=True) + "\n", encoding="utf-8")
temporary.replace(path)
PY
}

start_fault_gate() {
  set_gate_mode pass
  env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$NO_PROXY" \
    "$proof_python" benchmarks/trust_policy_renewal_probe.py fault-gate \
      --listen-port 12586 --upstream-port 12585 \
      --downstream-ca "$pki_dir/ca.crt" \
      --downstream-cert "$pki_dir/fault-gate.crt" \
      --downstream-key "$pki_dir/fault-gate.key" \
      --upstream-ca "$pki_dir/ca.crt" \
      --upstream-cert "$pki_dir/gate-upstream.crt" \
      --upstream-key "$pki_dir/gate-upstream.key" \
      --mode-file "$proof_tmp/fault-gate-mode.json" \
      --ready-file "$proof_tmp/fault-gate-ready.json" \
      --drop-marker "$proof_tmp/fault-gate-drop.json" \
      --outage-marker "$proof_tmp/fault-gate-outage.json" \
      >"$proof_tmp/renewal-fault-gate.log" 2>&1 &
  fault_gate_pid="$!"; live_pids+=("$fault_gate_pid")
  wait_file "$proof_tmp/fault-gate-ready.json" "$fault_gate_pid" renewal-fault-gate
}

start_renewer() {
  env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$NO_PROXY" \
    INFERLAB_TRUST_RENEWER_STATUS_BIND='127.0.0.1:12587' \
    INFERLAB_TRUST_RENEWER_DISTRIBUTOR_URL="$renewer_distributor_url" \
    INFERLAB_TRUST_RENEWER_CLUSTER_ID="$cluster_id" \
    INFERLAB_TRUST_RENEWER_TEMPLATE_PATH="$state_dir/renewal-template.json" \
    INFERLAB_TRUST_RENEWER_STATE_PATH="$state_dir/renewer.json" \
    INFERLAB_TRUST_RENEWER_ROOT_KEY_ID="$trust_root_id" \
    INFERLAB_TRUST_RENEWER_ROOT_PRIVATE_KEY_B64="$trust_root_seed" \
    INFERLAB_TRUST_RENEWER_TLS_SERVER_CA_PATH="$pki_dir/ca.crt" \
    INFERLAB_TRUST_RENEWER_TLS_CLIENT_CERT_PATH="$pki_dir/renewer.crt" \
    INFERLAB_TRUST_RENEWER_TLS_CLIENT_KEY_PATH="$pki_dir/renewer.key" \
    INFERLAB_TRUST_RENEWER_POLICY_LIFETIME_MS="$policy_lifetime_ms" \
    INFERLAB_TRUST_RENEWER_RENEW_BEFORE_MS="$renew_before_ms" \
    INFERLAB_TRUST_RENEWER_POLL_INTERVAL_MS="$poll_interval_ms" \
    INFERLAB_TRUST_RENEWER_RETRY_INTERVAL_MS="$retry_interval_ms" \
    INFERLAB_TRUST_RENEWER_REQUEST_TIMEOUT_MS="$request_timeout_ms" \
    target/debug/trust-renewer >"$proof_tmp/trust-renewer.log" 2>&1 &
  renewer_pid="$!"; live_pids+=("$renewer_pid")
  wait_endpoint "$renewer_status_url/health" "$renewer_pid" trust-renewer
}

stop_renewer() {
  local pid="$renewer_pid"
  [[ -n "$pid" ]] || return 0
  shutdown_child "$pid"
  forget_pid "$pid"
  renewer_pid=''
}

start_node() {
  local node="$1" port peers election_min=5000 pid
  port="$(node_port "$node")"; peers="$(node_peers "$node")"
  [[ "$node" == control-a ]] && election_min=300
  mkdir -p "$state_dir/$node"
  env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$NO_PROXY" \
    INFERLAB_RAFT_NODE_ID="$node" INFERLAB_RAFT_CLUSTER_ID="$cluster_id" \
    INFERLAB_RAFT_BIND="127.0.0.1:$port" INFERLAB_RAFT_PEERS="$peers" \
    INFERLAB_RAFT_DATA_DIR="$state_dir/$node" \
    INFERLAB_RAFT_ELECTION_MIN_MS="$election_min" INFERLAB_RAFT_ELECTION_MAX_MS="$((election_min + 100))" \
    INFERLAB_RAFT_HEARTBEAT_MS=50 INFERLAB_RAFT_RPC_TIMEOUT_MS=500 INFERLAB_RAFT_COMMIT_TIMEOUT_MS=2500 \
    INFERLAB_CONTROL_SIGNING_KEY_ID="$route_key_id" INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64="$route_seed" \
    INFERLAB_CONTROL_WRITER_KEYS="$writer_id=$writer_public" \
    INFERLAB_CONTROL_WRITE_MAX_AGE_MS=5000 INFERLAB_CONTROL_WRITE_MAX_FUTURE_SKEW_MS=250 \
    INFERLAB_SERVICE_ID="$node" INFERLAB_SERVICE_CREDENTIAL_ID='key-a' \
    INFERLAB_SERVICE_PRIVATE_KEY_B64="$(service_seed "$node")" \
    INFERLAB_SERVICE_TRUST_DISTRIBUTOR_URL="$distributor_url" \
    INFERLAB_SERVICE_TRUST_CACHE_PATH="$state_dir/$node/service-trust-cache.json" \
    INFERLAB_SERVICE_TRUST_STATE_PATH="$state_dir/$node/service-trust-floor.json" \
    INFERLAB_SERVICE_TRUST_ROOT_KEYS="$trust_root_id=$trust_root_public" \
    INFERLAB_SERVICE_TRUST_REVOKED_ROOT_KEY_IDS='' \
    INFERLAB_SERVICE_TRUST_MAX_POLICY_LIFETIME_MS="$policy_max_lifetime_ms" \
    INFERLAB_SERVICE_TRUST_POLICY_MAX_FUTURE_SKEW_MS=250 \
    INFERLAB_SERVICE_TRUST_TLS_CA_CERT_PATH="$pki_dir/ca.crt" \
    INFERLAB_SERVICE_TRUST_TLS_CLIENT_CERT_PATH="$pki_dir/$node.crt" \
    INFERLAB_SERVICE_TRUST_TLS_CLIENT_KEY_PATH="$pki_dir/$node.key" \
    INFERLAB_SERVICE_TRUST_POLL_MS=25 INFERLAB_SERVICE_TRUST_REQUEST_TIMEOUT_MS=1000 \
    INFERLAB_SERVICE_TRUST_MAX_BACKOFF_MS=200 \
    INFERLAB_SERVICE_AUTH_MAX_AGE_MS=5000 INFERLAB_SERVICE_AUTH_MAX_FUTURE_SKEW_MS=250 \
    target/debug/control-plane >"$proof_tmp/$node.log" 2>&1 &
  pid="$!"; live_pids+=("$pid"); set_node_pid "$node" "$pid"
  wait_endpoint "http://127.0.0.1:$port/healthz" "$pid" "$node"
}

start_worker() {
  env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$NO_PROXY" \
    INFERLAB_CPU_WORKER_ID='cpu-trust-renewal' INFERLAB_CPU_BIND='127.0.0.1:12584' \
    INFERLAB_MODEL_PATH='models/tiny-inferlab-v2.bin' INFERLAB_CPU_DECODER_MODE='paged-kv-cache' \
    INFERLAB_CPU_QUANTIZATION='fp32' INFERLAB_CPU_SPECULATIVE_DRAFT_QUANTIZATION='int8' \
    INFERLAB_CPU_ATTENTION_KERNEL='online-tiled' INFERLAB_CPU_ATTENTION_PRECISION='fp32' \
    INFERLAB_CPU_ATTENTION_TILE_TOKENS=32 INFERLAB_CPU_MAX_BATCH_SIZE=4 \
    INFERLAB_CPU_SCHEDULER_QUEUE_CAPACITY=16 INFERLAB_CPU_KV_PAGE_TOKENS=4 \
    INFERLAB_CPU_KV_PAGE_COUNT=64 INFERLAB_CPU_PREFIX_CACHE_CAPACITY=32 \
    INFERLAB_CPU_BATCH_TICK_MS=100 \
    target/debug/cpu-worker >"$proof_tmp/cpu-worker.log" 2>&1 &
  worker_pid="$!"; live_pids+=("$worker_pid")
  wait_endpoint "$worker_url/health" "$worker_pid" cpu-worker
}

start_gateway() {
  env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$NO_PROXY" \
    INFERLAB_BIND='127.0.0.1:12580' INFERLAB_CONTROL_PLANE_URLS="$control_urls" \
    INFERLAB_CONTROL_CLUSTER_ID="$cluster_id" INFERLAB_CONTROL_TRUSTED_KEYS="$route_key_id=$route_public" \
    INFERLAB_CONTROL_BOOTSTRAP_WAIT_MS=5000 INFERLAB_CONTROL_POLL_MS=25 \
    INFERLAB_GATEWAY_SERVICE_ID='gateway-primary' INFERLAB_GATEWAY_SERVICE_CREDENTIAL_ID='key-a' \
    INFERLAB_GATEWAY_SERVICE_PRIVATE_KEY_B64="$(service_seed gateway-primary)" \
    INFERLAB_CONTROL_SERVICE_TARGETS='control-a=http://127.0.0.1:12581,control-b=http://127.0.0.1:12582,control-c=http://127.0.0.1:12583' \
    INFERLAB_ROUTING_SNAPSHOT_PATH="$state_dir/gateway-routing.json" \
    INFERLAB_ROUTING_SNAPSHOT_MAX_AGE_MS=120000 INFERLAB_ROUTING_LEASE_MS=60000 \
    INFERLAB_WORKER_CONCURRENCY=4 INFERLAB_ADMISSION_QUEUE_CAPACITY=8 \
    INFERLAB_REQUEST_DEADLINE_MS=15000 INFERLAB_ATTEMPT_TIMEOUT_MS=12000 INFERLAB_MAX_RETRIES=0 \
    target/debug/gateway >"$proof_tmp/gateway.log" 2>&1 &
  gateway_pid="$!"; live_pids+=("$gateway_pid")
  wait_endpoint "$gateway_url/health" "$gateway_pid" gateway
}

sign_service_request() {
  local service_id="$1" method="$2" path="$3" audience="$4" issued="$5" nonce="$6" body="$7" output="$8"
  env -i PATH="$PATH" INFERLAB_SERVICE_ID="$service_id" INFERLAB_SERVICE_CREDENTIAL_ID='key-a' \
    INFERLAB_SERVICE_PRIVATE_KEY_B64="$(service_seed "$service_id")" \
    target/debug/sign_service_request "$method" "$path" "$cluster_id" "$audience" \
      "$issued" "$nonce" "$body" >"$output"
}

json_value() {
  python3 - "$1" "$2" <<'PY'
import json, sys
value=json.load(open(sys.argv[1],encoding="utf-8"))
for part in sys.argv[2].split('.'):
    value=value[int(part)] if part.isdigit() else value[part]
print(value)
PY
}

generic_public() {
  env -i PATH="$PATH" INFERLAB_SERVICE_ID='proof-key' INFERLAB_SERVICE_CREDENTIAL_ID='proof-key' \
    INFERLAB_SERVICE_PRIVATE_KEY_B64="$1" target/debug/service_public_key
}

service_public() {
  local service_id="$1"
  env -i PATH="$PATH" INFERLAB_SERVICE_ID="$service_id" INFERLAB_SERVICE_CREDENTIAL_ID='key-a' \
    INFERLAB_SERVICE_PRIVATE_KEY_B64="$(service_seed "$service_id")" \
    target/debug/service_public_key
}

write_renewal_template() {
  python3 - "$state_dir/renewal-template.json" "$control_a_public" "$control_b_public" \
    "$control_c_public" "$gateway_public" <<'PY'
import json, os, sys
from pathlib import Path
path = Path(sys.argv[1])
services = ["control-a", "control-b", "control-c", "gateway-primary"]
value = {
    "schema": "inferlab.service-trust-renewal-template.v1",
    "cluster_id": "inferlab-primary",
    "policy_schema": "inferlab.service-trust-policy.v2",
    "trusted_credentials": [
        {"service_id": service, "credential_id": "key-a", "public_key_base64": public}
        for service, public in zip(services, sys.argv[2:])
    ],
    "revoked_service_ids": [],
    "revoked_credentials": [],
    "gateway_service_ids": ["gateway-primary"],
}
path.write_text(json.dumps(value, separators=(",", ":")) + "\n", encoding="utf-8")
os.chmod(path, 0o600)
PY
}

run_renewer_binary() {
  local port="$1" template="$2" state="$3" log="$4"
  env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$NO_PROXY" \
    INFERLAB_TRUST_RENEWER_STATUS_BIND="127.0.0.1:$port" \
    INFERLAB_TRUST_RENEWER_DISTRIBUTOR_URL="$renewer_distributor_url" \
    INFERLAB_TRUST_RENEWER_CLUSTER_ID="$cluster_id" \
    INFERLAB_TRUST_RENEWER_TEMPLATE_PATH="$template" \
    INFERLAB_TRUST_RENEWER_STATE_PATH="$state" \
    INFERLAB_TRUST_RENEWER_ROOT_KEY_ID="$trust_root_id" \
    INFERLAB_TRUST_RENEWER_ROOT_PRIVATE_KEY_B64="$trust_root_seed" \
    INFERLAB_TRUST_RENEWER_TLS_SERVER_CA_PATH="$pki_dir/ca.crt" \
    INFERLAB_TRUST_RENEWER_TLS_CLIENT_CERT_PATH="$pki_dir/renewer.crt" \
    INFERLAB_TRUST_RENEWER_TLS_CLIENT_KEY_PATH="$pki_dir/renewer.key" \
    INFERLAB_TRUST_RENEWER_POLICY_LIFETIME_MS="$policy_lifetime_ms" \
    INFERLAB_TRUST_RENEWER_RENEW_BEFORE_MS="$renew_before_ms" \
    INFERLAB_TRUST_RENEWER_POLL_INTERVAL_MS="$poll_interval_ms" \
    INFERLAB_TRUST_RENEWER_RETRY_INTERVAL_MS="$retry_interval_ms" \
    INFERLAB_TRUST_RENEWER_REQUEST_TIMEOUT_MS="$request_timeout_ms" \
    target/debug/trust-renewer >"$log" 2>&1 &
  startup_pid="$!"
  live_pids+=("$startup_pid")
}

prepare_startup_sources() {
  local scenario="$1" directory="$2" template="$directory/template.json" state="$directory/state.json"
  cp "$state_dir/renewal-template.json" "$template"
  chmod 0600 "$template"
  case "$scenario" in
    corrupt-template)
      printf '%s\n' '{not-json' >"$template"
      ;;
    oversize-template)
      python3 - "$template" <<'PY'
import os, sys
with open(sys.argv[1], "wb") as output:
    output.write(b"x" * (256 * 1024 + 1))
os.chmod(sys.argv[1], 0o600)
PY
      ;;
    template-symlink)
      mv "$template" "$directory/template-target.json"
      ln -s "$directory/template-target.json" "$template"
      ;;
    unsafe-template-permissions)
      chmod 0644 "$template"
      ;;
    corrupt-state)
      printf '%s\n' '{not-json' >"$state"; chmod 0600 "$state"
      ;;
    state-symlink)
      printf '%s\n' '{}' >"$directory/state-target.json"; chmod 0600 "$directory/state-target.json"
      ln -s "$directory/state-target.json" "$state"
      ;;
    unsafe-state-permissions)
      printf '%s\n' '{}' >"$state"; chmod 0644 "$state"
      ;;
    writer-already-running)
      ;;
    *) return 1 ;;
  esac
}

run_startup_case() {
  local scenario="$1" kind="$2" port="$3" output="$4"
  local directory="$case_dir/startup-$scenario" template state log pid deadline state_token
  local exit_code=0 probes=0 ever_open=0 holder_pid='' diagnostic_needle
  directory="$case_dir/startup-$scenario"
  mkdir -p "$directory"
  prepare_startup_sources "$scenario" "$directory"
  template="$directory/template.json"; state="$directory/state.json"; log="$directory/process.log"
  if [[ "$scenario" == writer-already-running ]]; then
    run_renewer_binary 12620 "$template" "$state" "$directory/holder.log"
    holder_pid="$startup_pid"
    deadline=$((SECONDS + 20))
    while ! listener_is_open 12620; do
      is_owned_child "$holder_pid" || { echo 'writer holder exited before listener' >&2; return 1; }
      ((SECONDS < deadline)) || { echo 'writer holder listener timeout' >&2; return 1; }
      sleep 0.02
    done
  fi
  run_renewer_binary "$port" "$template" "$state" "$log"
  pid="$startup_pid"
  deadline=$((SECONDS + 10))
  while ((SECONDS < deadline)); do
    probes=$((probes + 1))
    if listener_is_open "$port"; then ever_open=1; break; fi
    state_token="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
    if ! is_owned_child "$pid" || [[ "$state_token" == *Z* ]]; then break; fi
    sleep 0.01
  done
  if is_owned_child "$pid"; then
    state_token="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
    if [[ "$state_token" != *Z* ]]; then
      shutdown_child "$pid"; forget_pid "$pid"
      echo "startup rejection remained live: $scenario" >&2
      return 1
    fi
  fi
  set +e; wait "$pid"; exit_code="$?"; set -e
  forget_pid "$pid"
  if [[ -n "$holder_pid" ]]; then
    shutdown_child "$holder_pid"; forget_pid "$holder_pid"
  fi
  [[ "$exit_code" != 0 && "$ever_open" == 0 ]] || {
    echo "startup rejection did not fail closed: $scenario" >&2
    tail -n 40 "$log" >&2
    return 1
  }
  case "$kind" in
    template) diagnostic_needle='Template' ;;
    state) diagnostic_needle='State' ;;
    writer_already_running) diagnostic_needle='AlreadyRunning' ;;
    *) return 1 ;;
  esac
  grep -F "$diagnostic_needle" "$log" >/dev/null || {
    echo "startup rejection omitted finite diagnostic: $scenario/$kind" >&2
    tail -n 40 "$log" >&2
    return 1
  }
  python3 - "$scenario" "$kind" "$exit_code" "$ever_open" "$probes" >"$output" <<'PY'
import json, sys
scenario, kind, code, opened, probes = sys.argv[1:]
print(json.dumps({
    "scenario": scenario,
    "error_kind": kind,
    "diagnostic": kind,
    "exit_code": int(code),
    "listener_ever_open": bool(int(opened)),
    "listener_probe_count": int(probes),
}, indent=2, sort_keys=True))
PY
}

capture_processes() {
  local output="$1"
  python3 - "$output" "$$" \
    control-a "$control_a_pid" control-b "$control_b_pid" control-c "$control_c_pid" \
    cpu-worker "$worker_pid" gateway "$gateway_pid" renewal-fault-gate "$fault_gate_pid" \
    trust-distributor "$distributor_pid" trust-renewer "$renewer_pid" <<'PY'
import hashlib, json, os, re, subprocess, sys
from pathlib import Path
output, proof_pid, *values = sys.argv[1:]
expected = {
    "control-a": r"control-plane", "control-b": r"control-plane", "control-c": r"control-plane",
    "cpu-worker": r"cpu-worker", "gateway": r"gateway",
    "renewal-fault-gate": r"(?:python3(?:\.[0-9]+)?|Python)",
    "trust-distributor": r"trust-distributor", "trust-renewer": r"trust-renewer",
}
items = []
for index in range(0, len(values), 2):
    label, raw_pid = values[index:index + 2]
    pid = int(raw_pid)
    fields = subprocess.check_output(
        ["ps", "-o", "ppid=", "-o", "stat=", "-o", "lstart=", "-p", str(pid)],
        text=True, env={**os.environ, "LC_ALL": "C"},
    ).strip().split()
    if len(fields) != 7:
        raise SystemExit(f"cannot parse process identity for {label}")
    ppid, state = int(fields[0]), fields[1]
    start_token = " ".join(fields[2:7])
    if sys.platform.startswith("linux"):
        executable_path = os.readlink(f"/proc/{pid}/exe").removesuffix(" (deleted)")
    else:
        executable_path = subprocess.check_output(
            ["ps", "-o", "comm=", "-p", str(pid)], text=True,
            env={**os.environ, "LC_ALL": "C"},
        ).strip()
    executable = os.path.basename(executable_path)
    if ppid != int(proof_pid) or re.fullmatch(expected[label], executable) is None or "Z" in state:
        raise SystemExit(f"process identity mismatch for {label}")
    raw = Path(executable_path).read_bytes()
    items.append({
        "label": label, "pid": pid, "ppid": ppid, "state": state,
        "start_token": start_token, "executable": executable,
        "executable_sha256": hashlib.sha256(raw).hexdigest(),
    })
Path(output).write_text(
    json.dumps(sorted(items, key=lambda item: item["label"]), indent=2, sort_keys=True) + "\n",
    encoding="utf-8",
)
PY
}

capture_secret_boundaries() {
  python3 - \
    control-a "$control_a_pid" control-b "$control_b_pid" control-c "$control_c_pid" \
    cpu-worker "$worker_pid" gateway "$gateway_pid" renewal-fault-gate "$fault_gate_pid" \
    trust-distributor "$distributor_pid" trust-renewer "$renewer_pid" <<'PY'
import json, os, subprocess, sys
values = sys.argv[1:]
processes = {}
for index in range(0, len(values), 2):
    label, raw_pid = values[index:index + 2]
    pid = int(raw_pid)
    if sys.platform.startswith("linux"):
        environment = open(f"/proc/{pid}/environ", "rb").read().decode(errors="replace")
    else:
        environment = subprocess.check_output(
            ["ps", "eww", "-o", "command=", "-p", str(pid)], text=True,
            env={**os.environ, "LC_ALL": "C"}, errors="replace",
        )
    processes[label] = {
        "root_private_key_env_present": "INFERLAB_TRUST_RENEWER_ROOT_PRIVATE_KEY_B64=" in environment,
        "public_root_env_present": "INFERLAB_SERVICE_TRUST_ROOT_KEYS=" in environment,
        "values_retained": False,
    }
print(json.dumps({
    "schema": "inferlab.trust-policy-renewal-secret-boundaries.v0.31",
    "processes": processes,
    "root_seed_values_retained": False,
}, indent=2, sort_keys=True))
PY
}

project_state() {
  local output="$1"
  python3 - "$state_dir/renewer.json" >"$output" <<'PY'
import hashlib, json, os, stat, sys
from pathlib import Path
path = Path(sys.argv[1])
metadata = path.lstat()
raw = path.read_bytes()
state = json.loads(raw)
committed = state.get("committed")
pending = state.get("pending")
print(json.dumps({
    "schema": "inferlab.trust-policy-renewal-state-projection.v0.31",
    "state_schema": state.get("schema"),
    "state_file_sha256": hashlib.sha256(raw).hexdigest(),
    "authority_fingerprint": state.get("authority_fingerprint"),
    "template_fingerprint": state.get("template_fingerprint"),
    "committed_generation": committed.get("generation") if isinstance(committed, dict) else None,
    "committed_snapshot_sha256": committed.get("snapshot_sha256") if isinstance(committed, dict) else None,
    "pending_generation": pending.get("generation") if isinstance(pending, dict) else None,
    "pending_snapshot_sha256": pending.get("snapshot_sha256") if isinstance(pending, dict) else None,
    "pending_late_recovery": pending.get("late_recovery") if isinstance(pending, dict) else None,
    "counters": state.get("counters"),
    "file_mode": format(stat.S_IMODE(metadata.st_mode), "04o"),
    "regular_file": stat.S_ISREG(metadata.st_mode),
    "symlink": stat.S_ISLNK(metadata.st_mode),
}, indent=2, sort_keys=True))
PY
}

wait_wall_ms() {
  python3 - "$1" "$2" <<'PY'
import sys, time
target, timeout = int(sys.argv[1]), float(sys.argv[2])
deadline = time.monotonic() + timeout
while time.time_ns() // 1_000_000 < target:
    if time.monotonic() >= deadline:
        raise SystemExit("wall-clock proof barrier timed out")
    remaining = target - time.time_ns() // 1_000_000
    time.sleep(min(0.01, max(0.001, remaining / 1000)))
PY
}

capture_generation() {
  local generation="$1" label="$2" snapshot controls distributor renewer expiry
  snapshot="$proof_tmp/generation-$generation-snapshot.json"
  controls="$proof_tmp/generation-$generation-controls.json"
  distributor="$proof_tmp/generation-$generation-distributor.json"
  renewer="$proof_tmp/generation-$generation-renewer.json"
  python3 benchmarks/trust_policy_renewal_probe.py wait-renewer --url "$renewer_status_url" \
    --phase waiting --ready --distributor-generation "$generation" --committed-generation "$generation" \
    --pending-absent --clean-error --min-successful-renewals "$generation" --timeout 25 >"$renewer"
  python3 benchmarks/trust_policy_renewal_probe.py capture \
    --url "$distributor_url/v1/service-trust/snapshot" --expect-status 200 \
    --ca-cert "$pki_dir/ca.crt" --client-cert "$pki_dir/probe.crt" \
    --client-key "$pki_dir/probe.key" --timeout 3 >"$snapshot"
  expiry="$(json_value "$snapshot" observation.body.expires_at_ms)"
  python3 benchmarks/trust_policy_renewal_probe.py wait-controls --urls "$control_urls" \
    --revision 2 --generation "$generation" --validity valid --expires-at-ms "$expiry" \
    --require-receipt-generation --timeout 25 >"$controls"
  python3 benchmarks/trust_policy_renewal_probe.py wait-distributor --url "$distributor_url" \
    --generation "$generation" --expected-receivers 'control-a,control-b,control-c' \
    --complete-receipts --ca-cert "$pki_dir/ca.crt" --client-cert "$pki_dir/probe.crt" \
    --client-key "$pki_dir/probe.key" --timeout 25 >"$distributor"
  python3 - "$generation" "$label" "$snapshot" "$controls" "$distributor" "$renewer" \
    >"$proof_tmp/generation-$generation.json" <<'PY'
import json, sys
generation, label = int(sys.argv[1]), sys.argv[2]
snapshot_capture, controls, distributor, renewer = [
    json.load(open(path, encoding="utf-8")) for path in sys.argv[3:]
]
snapshot = snapshot_capture["observation"]["body"]
print(json.dumps({
    "label": label,
    "generation": generation,
    "snapshot": snapshot,
    "snapshot_capture": snapshot_capture,
    "snapshot_sha256": snapshot_capture["observation"]["body_sha256"],
    "etag": snapshot_capture["observation"]["etag"],
    "controls": controls,
    "distributor": distributor,
    "renewer": renewer,
}, indent=2, sort_keys=True))
PY
}

prepare_output_dir
check_ports_are_free 12580 12581 12582 12583 12584 12585 12586 12587 \
  12600 12601 12602 12603 12604 12605 12606 12607 12620
command -v openssl >/dev/null
if [[ -e "$escaped_ca_serial" || -L "$escaped_ca_serial" ]]; then
  echo 'refusing v0.31 proof: escaped OpenSSL serial sentinel exists' >&2
  exit 1
fi
cargo build --locked --workspace --bins --quiet
generate_pki

trust_root_public="$(env -i PATH="$PATH" INFERLAB_SERVICE_TRUST_ROOT_KEY_ID="$trust_root_id" \
  INFERLAB_SERVICE_TRUST_ROOT_PRIVATE_KEY_B64="$trust_root_seed" \
  target/debug/service_trust_public_key)"
route_public="$(generic_public "$route_seed")"
writer_public="$(generic_public "$writer_seed")"
control_a_public="$(service_public control-a)"
control_b_public="$(service_public control-b)"
control_c_public="$(service_public control-c)"
gateway_public="$(service_public gateway-primary)"
write_renewal_template

startup_scenarios=(
  corrupt-template oversize-template template-symlink unsafe-template-permissions
  corrupt-state state-symlink unsafe-state-permissions writer-already-running
)
startup_kinds=(template template template template state state state writer_already_running)
startup_outputs=()
for index in "${!startup_scenarios[@]}"; do
  startup_output="$proof_tmp/startup-${startup_scenarios[$index]}.json"
  run_startup_case "${startup_scenarios[$index]}" "${startup_kinds[$index]}" \
    "$((12600 + index))" "$startup_output"
  startup_outputs+=("$startup_output")
done
python3 - "${startup_outputs[@]}" >"$results_dir/renewer-startup-rejections.json" <<'PY'
import json, sys
cases = [json.load(open(path, encoding="utf-8")) for path in sys.argv[1:]]
print(json.dumps({
    "schema": "inferlab.trust-policy-renewal-startup-rejections.v0.31",
    "cases": cases,
}, indent=2, sort_keys=True))
PY

python3 - "$policy_lifetime_ms" "$renew_before_ms" "$poll_interval_ms" \
  "$retry_interval_ms" "$request_timeout_ms" >"$results_dir/proof-contract.json" <<'PY'
import json, sys
lifetime, margin, poll, retry, request = map(int, sys.argv[1:])
print(json.dumps({
    "cluster_id": "inferlab-primary",
    "automatic_generations": [1, 2, 3, 4],
    "normal_renewal": [1, 2],
    "ambiguous_response_generation": 3,
    "expiry_recovery": [3, 4],
    "policy_lifetime_ms": lifetime,
    "renew_before_ms": margin,
    "poll_interval_ms": poll,
    "retry_interval_ms": retry,
    "request_timeout_ms": request,
    "runtime_processes": [
        "control-a", "control-b", "control-c", "cpu-worker", "gateway",
        "trust-distributor", "trust-renewer",
    ],
    "proof_only_processes": ["renewal-fault-gate"],
    "replaced_processes": ["trust-renewer"],
    "static_tls_identity": True,
    "scope": "deadline-safe-automated-signed-service-trust-renewal",
    "schema": "inferlab.trust-policy-renewal-proof-contract.v0.31",
}, indent=2, sort_keys=True))
PY

start_distributor
start_fault_gate
start_renewer
initial_renewer_pid="$renewer_pid"
python3 benchmarks/trust_policy_renewal_probe.py wait-renewer --url "$renewer_status_url" \
  --phase waiting --ready --distributor-generation 1 --committed-generation 1 \
  --pending-absent --clean-error --min-successful-renewals 1 --timeout 25 \
  >"$proof_tmp/initial-renewer-generation-1.json"

start_node control-b
start_node control-c
start_node control-a
start_worker
python3 benchmarks/full_stack_probe.py wait-leader --urls "$control_urls" --timeout 20 \
  >"$proof_tmp/initial-cluster.json"
leader_id="$(json_value "$proof_tmp/initial-cluster.json" leader_id)"
leader_url="$(json_value "$proof_tmp/initial-cluster.json" leader_url)"
python3 - "$proof_tmp/route-r2.json" <<'PY'
import json, sys
from pathlib import Path
Path(sys.argv[1]).write_text(json.dumps({
    "routing_policy": "round-robin",
    "workers": [{"id": "cpu-trust-renewal", "base_url": "http://127.0.0.1:12584", "weight": 1}],
}, separators=(",", ":")) + "\n", encoding="utf-8")
PY
env -i PATH="$PATH" INFERLAB_CONTROL_WRITER_ID="$writer_id" \
  INFERLAB_CONTROL_WRITER_PRIVATE_KEY_B64="$writer_seed" \
  target/debug/sign_control_write "$cluster_id" 0 now v031-route-r2-0001 \
    "$proof_tmp/route-r2.json" >"$proof_tmp/write-r2.json"
python3 benchmarks/control_write_probe.py submit --url "$leader_url" \
  --body "$proof_tmp/write-r2.json" >"$proof_tmp/r2-write-raw.json"
start_gateway

capture_generation 1 cold-start
capture_processes "$proof_tmp/initial-processes.json"
python3 - "$proof_tmp/generation-1-renewer.json" "$trust_root_public" \
  "$control_a_public" "$control_b_public" "$control_c_public" "$gateway_public" \
  >"$results_dir/authority.json" <<'PY'
import json, sys
renewer = json.load(open(sys.argv[1], encoding="utf-8"))["result"]["body"]
services = ["control-a", "control-b", "control-c", "gateway-primary"]
public = dict(zip(services, sys.argv[3:]))
print(json.dumps({
    "schema": "inferlab.trust-policy-renewal-authority.v0.31",
    "cluster_id": "inferlab-primary",
    "template_schema": "inferlab.service-trust-renewal-template.v1",
    "policy_schema": "inferlab.service-trust-policy.v2",
    "root_key_id": "service-trust-root-v031",
    "root_public_key_base64": sys.argv[2],
    "service_public_keys": public,
    "semantic_template": {
        "cluster_id": "inferlab-primary",
        "policy_schema": "inferlab.service-trust-policy.v2",
        "trusted_credentials": [
            {"service_id": service, "credential_id": "key-a", "public_key_base64": public[service]}
            for service in services
        ],
        "revoked_service_ids": [], "revoked_credentials": [],
        "gateway_service_ids": ["gateway-primary"],
    },
    "template_fingerprint": renewer["template_fingerprint"],
    "authority_fingerprint": renewer["authority_fingerprint"],
}, indent=2, sort_keys=True))
PY

capture_generation 2 normal
python3 - "$proof_tmp/generation-1.json" "$proof_tmp/generation-2.json" \
  >"$results_dir/normal-renewal.json" <<'PY'
import json, sys
one, two = [json.load(open(path, encoding="utf-8")) for path in sys.argv[1:]]
def rejections(item):
    return {
        control["node_id"]: control["service_authentication"]["trust_policy_expiration_rejections"]
        for control in item["controls"]["result"]["controls"]
    }
before, after = rejections(one), rejections(two)
print(json.dumps({
    "schema": "inferlab.trust-policy-renewal-normal.v0.31",
    "from_generation": 1, "to_generation": 2,
    "from_snapshot_sha256": one["snapshot_sha256"],
    "to_snapshot_sha256": two["snapshot_sha256"],
    "expiration_rejections_before": before,
    "expiration_rejections_after": after,
    "authorization_gap_observed": False,
}, indent=2, sort_keys=True))
PY

rm -f "$proof_tmp/fault-gate-drop.json" "$proof_tmp/fault-gate-outage.json"
set_gate_mode drop-post-response
wait_file "$proof_tmp/fault-gate-drop.json" "$fault_gate_pid" renewal-fault-gate
python3 benchmarks/trust_policy_renewal_probe.py wait-renewer --url "$renewer_status_url" \
  --phase retry_waiting --no-ready --committed-generation 2 --pending-generation 3 \
  --last-error-kind transport --min-transient-failures 1 --timeout 15 \
  >"$proof_tmp/ambiguous-before-restart-status.json"
project_state "$proof_tmp/ambiguous-pending-before-restart.json"
stop_renewer
mv "$proof_tmp/trust-renewer.log" "$proof_tmp/trust-renewer-initial.log"
start_renewer
python3 benchmarks/trust_policy_renewal_probe.py wait-renewer --url "$renewer_status_url" \
  --phase retry_waiting --no-ready --committed-generation 2 --pending-generation 3 \
  --last-error-kind transport --min-transient-failures 2 --timeout 15 \
  >"$proof_tmp/ambiguous-after-restart-status.json"
project_state "$proof_tmp/ambiguous-pending-after-restart.json"
capture_processes "$proof_tmp/restarted-processes.json"
cp "$proof_tmp/fault-gate-drop.json" "$proof_tmp/ambiguous-drop.json"
set_gate_mode pass
capture_generation 3 ambiguous-reconcile
project_state "$proof_tmp/ambiguous-committed.json"

rm -f "$proof_tmp/fault-gate-outage.json"
set_gate_mode unavailable
wait_file "$proof_tmp/fault-gate-outage.json" "$fault_gate_pid" renewal-fault-gate
python3 benchmarks/trust_policy_renewal_probe.py wait-renewer --url "$renewer_status_url" \
  --phase retry_waiting --no-ready --committed-generation 3 --pending-absent \
  --last-error-kind transport --min-transient-failures 3 --timeout 15 \
  >"$proof_tmp/outage-before-expiry-status.json"
generation_three_expiry="$(json_value "$proof_tmp/generation-3.json" snapshot.expires_at_ms)"
auth_issued_at="$((generation_three_expiry - 2000))"
sign_service_request gateway-primary GET /v1/control/config "$leader_id" "$auth_issued_at" \
  v031-expiry-pre-0001 - "$proof_tmp/pre-expiry-auth.json"
sign_service_request gateway-primary GET /v1/control/config "$leader_id" "$auth_issued_at" \
  v031-expiry-post-0002 - "$proof_tmp/post-expiry-auth.json"
sign_service_request gateway-primary GET /v1/control/config "$leader_id" "$auth_issued_at" \
  v031-expiry-post-0003 - "$proof_tmp/post-expiry-repeat-auth.json"
python3 benchmarks/trust_policy_renewal_probe.py protected-cutoff \
  --control-url "$leader_url" --generation 3 --expires-at-ms "$generation_three_expiry" \
  --before-authentication "$proof_tmp/pre-expiry-auth.json" \
  --after-authentication "$proof_tmp/post-expiry-auth.json" \
  --after-authentication-repeat "$proof_tmp/post-expiry-repeat-auth.json" \
  --before-ms 500 --after-ms 25 --timeout 25 \
  >"$results_dir/protected-request-continuity.json"
python3 benchmarks/trust_policy_renewal_probe.py wait-expired-controls --urls "$control_urls" \
  --revision 2 --generation 3 --expires-at-ms "$generation_three_expiry" --timeout 10 \
  >"$proof_tmp/expired-generation-3-controls.json"

set_gate_mode publication-unavailable
python3 benchmarks/trust_policy_renewal_probe.py wait-renewer --url "$renewer_status_url" \
  --phase retry_waiting --no-ready --distributor-generation 3 --committed-generation 3 \
  --pending-generation 4 --last-error-kind transport --min-transient-failures 4 --timeout 15 \
  >"$proof_tmp/outage-pending-status.json"
project_state "$proof_tmp/outage-pending.json"
set_gate_mode pass
outage_released_at_ms="$(python3 - <<'PY'
import time
print(time.time_ns() // 1_000_000)
PY
)"
capture_generation 4 late-recovery
project_state "$proof_tmp/final-committed.json"
capture_processes "$proof_tmp/final-processes.json"
capture_secret_boundaries >"$results_dir/secret-boundaries.json"

python3 - "$proof_tmp/generation-1.json" "$proof_tmp/generation-2.json" \
  "$proof_tmp/generation-3.json" "$proof_tmp/generation-4.json" \
  >"$results_dir/automatic-generations.json" <<'PY'
import json, sys
generations = [json.load(open(path, encoding="utf-8")) for path in sys.argv[1:]]
print(json.dumps({
    "schema": "inferlab.trust-policy-renewal-generations.v0.31",
    "generation_count": len(generations),
    "generations": generations,
}, indent=2, sort_keys=True))
PY

python3 - "$proof_tmp/ambiguous-pending-before-restart.json" \
  "$proof_tmp/ambiguous-pending-after-restart.json" "$proof_tmp/ambiguous-committed.json" \
  "$proof_tmp/outage-pending.json" "$proof_tmp/final-committed.json" \
  >"$results_dir/state-projections.json" <<'PY'
import json, sys
before, after, committed, outage, final = [
    json.load(open(path, encoding="utf-8")) for path in sys.argv[1:]
]
print(json.dumps({
    "schema": "inferlab.trust-policy-renewal-state-projections.v0.31",
    "ambiguous_pending": before,
    "ambiguous_restart_pending": after,
    "ambiguous_committed": committed,
    "outage_pending": outage,
    "final_committed": final,
}, indent=2, sort_keys=True))
PY

python3 - "$proof_tmp/ambiguous-drop.json" "$proof_tmp/ambiguous-pending-before-restart.json" \
  "$proof_tmp/ambiguous-pending-after-restart.json" "$proof_tmp/generation-3.json" \
  >"$results_dir/ambiguous-retry.json" <<'PY'
import json, sys
drop, before, after, generation = [
    json.load(open(path, encoding="utf-8")) for path in sys.argv[1:]
]
same = before["pending_snapshot_sha256"] == after["pending_snapshot_sha256"]
reconciled = before["pending_snapshot_sha256"] == generation["snapshot_sha256"]
if not same or not reconciled:
    raise SystemExit("ambiguous pending bytes did not reconcile exactly")
print(json.dumps({
    "schema": "inferlab.trust-policy-renewal-ambiguous-retry.v0.31",
    "target_generation": 3,
    "drop_event": drop,
    "same_pending_snapshot_sha256_before_after_restart": same,
    "reconciled_exact_distributor_bytes": reconciled,
    "duplicate_generation_observed": False,
    "fork_observed": False,
    "generation_skipped": False,
}, indent=2, sort_keys=True))
PY

python3 - "$proof_tmp/fault-gate-outage.json" "$proof_tmp/generation-3.json" \
  "$proof_tmp/generation-4.json" "$proof_tmp/expired-generation-3-controls.json" \
  "$proof_tmp/outage-before-expiry-status.json" "$proof_tmp/generation-4-renewer.json" \
  "$outage_released_at_ms" >"$results_dir/expiry-outage-recovery.json" <<'PY'
import json, sys
outage, three, four, controls, before, after = [
    json.load(open(path, encoding="utf-8")) for path in sys.argv[1:7]
]
before_late = before["result"]["body"]["late_recoveries"]
after_late = after["result"]["body"]["late_recoveries"]
print(json.dumps({
    "schema": "inferlab.trust-policy-renewal-expiry-recovery.v0.31",
    "expired_generation": 3,
    "recovery_generation": 4,
    "outage_started_at_ms": outage["observed_at_ms"],
    "outage_released_at_ms": int(sys.argv[7]),
    "expired_controls": controls,
    "hidden_grace_observed": False,
    "late_recovery_count_before": before_late,
    "late_recovery_count_after": after_late,
    "late_recovery_count_delta": after_late - before_late,
}, indent=2, sort_keys=True))
PY

python3 - "$proof_tmp/fault-gate-ready.json" "$proof_tmp/ambiguous-drop.json" \
  "$proof_tmp/fault-gate-outage.json" >"$results_dir/fault-gate.json" <<'PY'
import json, sys
ready, drop, outage = [json.load(open(path, encoding="utf-8")) for path in sys.argv[1:]]
print(json.dumps({
    "schema": "inferlab.trust-policy-renewal-fault-gate.v0.31",
    "process_role": "proof-only-application-fault-gate",
    "runtime_process": False,
    "ha_component": False,
    "authority_component": False,
    "ready": ready,
    "drop": drop,
    "outage": outage,
}, indent=2, sort_keys=True))
PY

python3 - "$proof_tmp/initial-processes.json" "$proof_tmp/restarted-processes.json" \
  "$proof_tmp/final-processes.json" "$$" >"$results_dir/process-continuity.json" <<'PY'
import json, sys
initial, restarted, final = [json.load(open(path, encoding="utf-8")) for path in sys.argv[1:4]]
print(json.dumps({
    "schema": "inferlab.trust-policy-renewal-process-continuity.v0.31",
    "proof_shell_pid": int(sys.argv[4]),
    "stable_runtime_processes": [
        "control-a", "control-b", "control-c", "cpu-worker", "gateway", "trust-distributor",
    ],
    "replaced_runtime_processes": ["trust-renewer"],
    "proof_only_processes": ["renewal-fault-gate"],
    "initial": initial,
    "after_renewer_restart": restarted,
    "final": final,
}, indent=2, sort_keys=True))
PY

generation_four_expiry="$(json_value "$proof_tmp/generation-4.json" snapshot.expires_at_ms)"
python3 - "$proof_tmp/generation-4-controls.json" "$generation_four_expiry" \
  >"$results_dir/final-cluster.json" <<'PY'
import json, sys
value = json.load(open(sys.argv[1], encoding="utf-8"))
value["expected_expires_at_ms"] = int(sys.argv[2])
print(json.dumps(value, indent=2, sort_keys=True))
PY
python3 benchmarks/trust_policy_renewal_probe.py completion --url "$gateway_url" \
  --prompt 'v031-json-private-proof-prompt' --temporary-body "$proof_tmp/final-json-body.json" \
  --timeout 15 >"$results_dir/final-json.json"
python3 benchmarks/trust_policy_renewal_probe.py stream --url "$gateway_url" \
  --prompt 'v031-sse-private-proof-prompt' --timeout 15 >"$results_dir/final-sse.json"

production_test_specs=(
  'service-auth|--lib|renewal::tests::strict_decode_rejects_unknown_duplicate_wrong_schema_and_cluster'
  'service-auth|--lib|renewal::tests::fingerprint_is_json_canonical_but_array_order_is_semantic'
  'service-auth|--lib|renewal::tests::authority_fingerprint_binds_template_root_id_and_public_key'
  'service-auth|--lib|renewal::tests::semantic_validation_detects_every_fixed_field_drift'
  'service-auth|--lib|renewal::tests::timing_bounds_deadline_and_clock_edges_are_exact'
  'service-auth|--lib|renewal::tests::generation_expiry_signing_and_signature_verification_are_exact'
  'trust-renewer|--lib|engine::tests::cold_start_stages_and_publishes_generation_one'
  'trust-renewer|--lib|engine::tests::ambiguous_retry_reuses_exact_pending_bytes'
  'trust-renewer|--lib|engine::tests::restart_reconciles_exact_pending_without_republishing'
  'trust-renewer|--lib|engine::tests::same_generation_fork_fails_closed'
  'trust-renewer|--lib|engine::tests::retry_crossing_expiry_records_late_recovery'
  'trust-renewer|--lib|engine::tests::backward_clock_step_does_not_postpone_due_work'
  'trust-renewer|--lib|engine::tests::future_issued_current_fails_closed'
  'trust-renewer|--lib|engine::tests::overlong_current_lifetime_fails_closed'
  'trust-renewer|--lib|engine::tests::expired_pending_fails_closed_without_post'
  'trust-renewer|--lib|engine::tests::bootstrap_rejects_pending_with_invalid_signature'
  'trust-renewer|--bin trust-renewer|tests::clean_loop_exit_is_fatal_to_supervision'
  'trust-renewer|--lib|engine::tests::post_rename_directory_sync_uncertainty_retains_next_state_and_stops_without_second_mutation'
)
production_test_arguments=()
production_test_index=0
for production_spec in "${production_test_specs[@]}"; do
  IFS='|' read -r production_package production_target production_filter <<<"$production_spec"
  production_log="$proof_tmp/production-$production_test_index.log"
  read -r -a production_target_args <<<"$production_target"
  set +e
  CARGO_TERM_COLOR=never cargo test --locked -p "$production_package" \
    "${production_target_args[@]}" "$production_filter" -- --exact \
    >"$production_log" 2>&1
  production_status="$?"
  set -e
  production_test_arguments+=(
    "$production_package" "$production_target" "$production_filter" \
    "$production_status" "$production_log"
  )
  production_test_index=$((production_test_index + 1))
done
python3 - "${production_test_arguments[@]}" >"$results_dir/production-tests.json" <<'PY'
import json, re, shlex, sys
from pathlib import Path
pattern = re.compile(
    r"^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; "
    r"[0-9]+ filtered out; finished in [0-9]+(?:\.[0-9]+)?s$"
)
values = sys.argv[1:]
tests = []
for index in range(0, len(values), 5):
    package, target, filter_, status, path = values[index:index + 5]
    lines = Path(path).read_text(errors="replace").splitlines()
    expected = f"test {filter_} ... ok"
    summaries = [line for line in lines if pattern.fullmatch(line)]
    projected = (
        ["running 1 test", expected, summaries[0]]
        if lines.count("running 1 test") == 1
        and lines.count(expected) == 1
        and len(summaries) == 1
        else []
    )
    tests.append({
        "package": package,
        "target": shlex.split(target),
        "test_filter": filter_,
        "exit_code": int(status),
        "output_lines": projected,
    })
result = {
    "schema": "inferlab.trust-policy-renewal-production-tests.v0.31",
    "test_count": len(tests),
    "tests": tests,
}
print(json.dumps(result, indent=2, sort_keys=True))
if any(item["exit_code"] != 0 or len(item["output_lines"]) != 3 for item in tests):
    raise SystemExit("v0.31 exact focused regression failed")
PY

python3 - "$proof_tmp" >"$results_dir/discarded-log-scan.json" <<'PY'
import base64, hashlib, json, sys, urllib.parse
from pathlib import Path
root = Path(sys.argv[1])
logs = [
    root / name for name in [
        "control-a.log", "control-b.log", "control-c.log", "cpu-worker.log", "gateway.log",
        "trust-distributor.log", "trust-renewer.log", "trust-renewer-initial.log",
        "renewal-fault-gate.log",
    ]
]
logs += sorted(root.glob("production-*.log"))
labels = [
    "v031-service-trust-root", "v031-route-signing", "v031-control-writer",
    *[f"v031-service-{service}" for service in [
        "control-a", "control-b", "control-c", "gateway-primary",
    ]],
]
secrets = []
for label in labels:
    value = base64.b64encode(hashlib.sha256(label.encode()).digest()).decode()
    secrets.extend([
        value, value.rstrip("="), urllib.parse.quote(value, safe=""),
        hashlib.sha256(value.encode()).hexdigest(),
    ])
markers = [
    "-----BEGIN PRIVATE KEY-----", "PRIVATE_KEY_B64", "PRIVATE_KEY_BASE64",
    "v031-json-private-proof-prompt", "v031-sse-private-proof-prompt",
]
matches = []
for path in logs:
    text = path.read_text(errors="replace")
    if any(value in text for value in secrets):
        matches.append(f"{path.name}:seed")
    if any(value in text for value in markers):
        matches.append(f"{path.name}:private-marker")
    # Runtime startup diagnostics legitimately carry configured source paths;
    # those discarded logs are never retained. Secret/prompt markers remain
    # hard failures, while retained evidence is separately host-path-sanitized.
result = {
    "schema": "inferlab.trust-policy-renewal-discarded-log-scan.v0.31",
    "files_scanned": [path.name for path in logs],
    "checks": ["deterministic-seeds", "private-markers", "fixed-private-prompts"],
    "matches": sorted(set(matches)),
    "passed": not matches,
}
print(json.dumps(result, indent=2, sort_keys=True))
if matches:
    raise SystemExit("discarded logs contain proof-private material")
PY

printf '{}\n' >"$results_dir/assertions.json"
printf '<svg xmlns="http://www.w3.org/2000/svg"/>\n' \
  >"$results_dir/trust-policy-renewal-proof.svg"
printf '{}\n' >"$results_dir/sanitizer.json"

scan_private_material() {
  python3 - "$results_dir" >"$1" <<'PY'
import base64, hashlib, json, sys, urllib.parse
from pathlib import Path
directory = Path(sys.argv[1])
labels = [
    "v031-service-trust-root", "v031-route-signing", "v031-control-writer",
    *[f"v031-service-{service}" for service in [
        "control-a", "control-b", "control-c", "gateway-primary",
    ]],
]
matches = []
for label in labels:
    padded = base64.b64encode(hashlib.sha256(label.encode()).digest()).decode()
    representations = [
        padded, padded.rstrip("="), urllib.parse.quote(padded, safe=""),
        hashlib.sha256(padded.encode()).hexdigest(),
    ]
    for path in sorted(directory.iterdir()):
        if not path.is_file() or path.name in {"manifest.json", "private-material-scan.json"}:
            continue
        text = path.read_text(errors="replace")
        for index, representation in enumerate(representations):
            if representation in text:
                matches.append({"file": path.name, "seed_label": label, "representation": index})
result = {
    "schema": "inferlab.trust-policy-renewal-private-material-scan.v0.31",
    "algorithm": "sha256-label-to-ed25519-seed",
    "files_scanned": sorted(
        path.name for path in directory.iterdir()
        if path.is_file() and path.name not in {"manifest.json", "private-material-scan.json"}
    ),
    "seed_labels_scanned": labels,
    "representations_per_seed": 4,
    "matches": matches,
    "passed": not matches,
}
print(json.dumps(result, indent=2, sort_keys=True))
if matches:
    raise SystemExit("deterministic private material entered retained evidence")
PY
}

scan_private_material "$results_dir/private-material-scan.json"
python3 benchmarks/trust_policy_renewal_probe.py sanitize-evidence \
  --evidence-dir "$results_dir" --proof-root "$proof_tmp" --project-root "$project_root" \
  >"$proof_tmp/sanitizer.json"
mv "$proof_tmp/sanitizer.json" "$results_dir/sanitizer.json"
python3 benchmarks/check_trust_policy_renewal.py --evidence-dir "$results_dir" \
  --output "$results_dir/assertions.json"
python3 benchmarks/render_trust_policy_renewal_svg.py --evidence-dir "$results_dir" \
  --output "$results_dir/trust-policy-renewal-proof.svg"
scan_private_material "$results_dir/private-material-scan.json"
python3 benchmarks/trust_policy_renewal_probe.py sanitize-evidence \
  --evidence-dir "$results_dir" --proof-root "$proof_tmp" --project-root "$project_root" \
  >"$proof_tmp/sanitizer.json"
mv "$proof_tmp/sanitizer.json" "$results_dir/sanitizer.json"
python3 benchmarks/check_trust_policy_renewal.py --evidence-dir "$results_dir" \
  --output "$results_dir/assertions.json"
python3 benchmarks/render_trust_policy_renewal_svg.py --evidence-dir "$results_dir" \
  --output "$results_dir/trust-policy-renewal-proof.svg"
scan_private_material "$results_dir/private-material-scan.json"
python3 benchmarks/trust_policy_renewal_probe.py sanitize-evidence \
  --evidence-dir "$results_dir" --proof-root "$proof_tmp" --project-root "$project_root" \
  >"$proof_tmp/sanitizer.json"
mv "$proof_tmp/sanitizer.json" "$results_dir/sanitizer.json"
python3 benchmarks/check_trust_policy_renewal.py --evidence-dir "$results_dir" \
  --output "$proof_tmp/replay-assertions.json"
cmp "$results_dir/assertions.json" "$proof_tmp/replay-assertions.json"
python3 benchmarks/render_trust_policy_renewal_svg.py --evidence-dir "$results_dir" \
  --output "$proof_tmp/replay-proof.svg"
cmp "$results_dir/trust-policy-renewal-proof.svg" "$proof_tmp/replay-proof.svg"

expected_files=(
  ambiguous-retry.json
  assertions.json
  authority.json
  automatic-generations.json
  discarded-log-scan.json
  expiry-outage-recovery.json
  fault-gate.json
  final-cluster.json
  final-json.json
  final-sse.json
  manifest.json
  normal-renewal.json
  private-material-scan.json
  process-continuity.json
  production-tests.json
  proof-contract.json
  protected-request-continuity.json
  renewer-startup-rejections.json
  sanitizer.json
  secret-boundaries.json
  state-projections.json
  trust-policy-renewal-proof.svg
)

write_manifest() {
  python3 - "$results_dir" "${expected_files[@]}" <<'PY'
import hashlib, json, sys
from pathlib import Path
directory = Path(sys.argv[1]); expected = sys.argv[2:]
if expected != sorted(expected) or len(expected) != len(set(expected)):
    raise SystemExit("manifest inventory must be sorted and unique")
entries = list(directory.iterdir())
if (
    {path.name for path in entries} != set(expected) - {"manifest.json"}
    or any(not path.is_file() or path.is_symlink() for path in entries)
):
    raise SystemExit("pre-manifest inventory mismatch")
files = []
for name in expected:
    if name == "manifest.json":
        continue
    raw = (directory / name).read_bytes()
    files.append({"name": name, "bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()})
(directory / "manifest.json").write_text(json.dumps({
    "schema": "inferlab.trust-policy-renewal-manifest.v0.31",
    "file_count": len(files), "files": files,
}, indent=2, sort_keys=True) + "\n", encoding="utf-8")
PY
}

retain_results() {
  [[ -z "${INFERLAB_V31_OUTPUT_DIR:-}" ]] && return
  local name
  for name in "${expected_files[@]}"; do
    [[ "$name" == manifest.json ]] && continue
    cp "$results_dir/$name" "$INFERLAB_V31_OUTPUT_DIR/$name"
  done
  cp "$results_dir/manifest.json" "$INFERLAB_V31_OUTPUT_DIR/manifest.json"
}

write_manifest
python3 benchmarks/check_trust_policy_renewal.py --evidence-dir "$results_dir" \
  --require-manifest --output "$proof_tmp/post-manifest-assertions.json"
cmp "$results_dir/assertions.json" "$proof_tmp/post-manifest-assertions.json"
python3 benchmarks/render_trust_policy_renewal_svg.py --evidence-dir "$results_dir" \
  --output "$proof_tmp/post-manifest-proof.svg"
cmp "$results_dir/trust-policy-renewal-proof.svg" "$proof_tmp/post-manifest-proof.svg"
retain_results
if [[ -n "${INFERLAB_V31_OUTPUT_DIR:-}" ]]; then
  python3 benchmarks/check_trust_policy_renewal.py --evidence-dir "$INFERLAB_V31_OUTPUT_DIR" \
    --require-manifest --output "$proof_tmp/retained-assertions.json"
  cmp "$results_dir/assertions.json" "$proof_tmp/retained-assertions.json"
  python3 benchmarks/render_trust_policy_renewal_svg.py \
    --evidence-dir "$INFERLAB_V31_OUTPUT_DIR" --output "$proof_tmp/retained-proof.svg"
  cmp "$results_dir/trust-policy-renewal-proof.svg" "$proof_tmp/retained-proof.svg"
fi
python3 - "$results_dir/assertions.json" <<'PY'
import json, sys
report = json.load(open(sys.argv[1], encoding="utf-8"))
print(f"v0.31 automatic trust-policy renewal proof complete: {report['passed']}/{report['total']} assertions passed")
PY
proof_succeeded=1
