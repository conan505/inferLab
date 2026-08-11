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
  echo 'v0.29 proof requires Python with TLS 1.3 support' >&2
  exit 1
fi
python3() { "$proof_python" "$@"; }

proof_tmp_root="${TMPDIR:-/tmp}"
escaped_ca_serial="${proof_tmp_root%/}/inferlab-v029.srl"
proof_tmp="$(mktemp -d "$proof_tmp_root/inferlab-v029.XXXXXX")"
results_dir="$proof_tmp/results"
bundle_dir="$proof_tmp/bundles"
policy_dir="$proof_tmp/policies"
pki_dir="$proof_tmp/pki"
case_dir="$proof_tmp/cases"
mkdir -p "$results_dir" "$bundle_dir" "$policy_dir" "$pki_dir" "$case_dir"

cluster_id='inferlab-primary'
gateway_url='http://127.0.0.1:12080'
control_urls='http://127.0.0.1:12081,http://127.0.0.1:12082,http://127.0.0.1:12083'
worker_url='http://127.0.0.1:12084'
distributor_url='https://localhost:12085'
route_key_id='route-v029'
writer_id='v029-deployer'
trust_root_id='service-trust-root-v029'
policy_lifetime_ms=600000

derive_seed() {
  python3 - "$1" <<'PY'
import base64, hashlib, sys
print(base64.b64encode(hashlib.sha256(sys.argv[1].encode()).digest()).decode())
PY
}

trust_root_seed="$(derive_seed v029-service-trust-root)"
route_seed="$(derive_seed v029-route-signing)"
writer_seed="$(derive_seed v029-control-writer)"

service_seed() {
  derive_seed "v029-$1-$2"
}

live_pids=(sentinel)
control_a_pid=''
control_b_pid=''
control_c_pid=''
gateway_pid=''
worker_pid=''
distributor_pid=''

forget_pid() {
  local forgotten="$1" pid
  local retained=(sentinel)
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
  if [[ -n "$proof_tmp" && -d "$proof_tmp" ]]; then
    case "$(basename "$proof_tmp")" in
      inferlab-v029.*) rm -rf -- "$proof_tmp" ;;
      *) echo "refusing unexpected proof cleanup path: $proof_tmp" >&2 ;;
    esac
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

prepare_output_dir() {
  [[ -z "${INFERLAB_V29_OUTPUT_DIR:-}" ]] && return
  mkdir -p "$INFERLAB_V29_OUTPUT_DIR"
  if find "$INFERLAB_V29_OUTPUT_DIR" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
    echo 'INFERLAB_V29_OUTPUT_DIR must be empty' >&2
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
            raise SystemExit(f"refusing v0.29 proof: 127.0.0.1:{port} busy: {error}")
PY
}

wait_endpoint() {
  local url="$1" pid="$2" label="$3" expected="${4:-200}" deadline observed
  deadline=$((SECONDS + 60))
  while true; do
    observed="$(curl --noproxy '*' --connect-timeout 0.1 --max-time 0.4 \
      --silent --output /dev/null --write-out '%{http_code}' "$url" 2>/dev/null || true)"
    [[ "$observed" == "$expected" ]] && return
    if ! is_owned_child "$pid"; then
      echo "$label exited before readiness" >&2
      tail -n 40 "$proof_tmp/$label.log" 2>/dev/null || true
      return 1
    fi
    ((SECONDS < deadline)) || { echo "timeout waiting for $label" >&2; return 1; }
    sleep 0.05
  done
}

wait_distributor() {
  local deadline=$((SECONDS + 60)) observed curl_exit=0 exit_bucket child_state state
  while true; do
    curl_exit=0
    observed="$(curl --noproxy '*' --connect-timeout 0.1 --max-time 0.5 --silent \
      --output /dev/null --write-out '%{http_code}' --cacert "$pki_dir/ca.crt" \
      --cert "$pki_dir/publisher.crt" --key "$pki_dir/publisher.key" \
      "$distributor_url/health" 2>/dev/null)" || curl_exit="$?"
    [[ "$observed" == 200 ]] && return
    is_owned_child "$distributor_pid" || { echo 'distributor exited before readiness' >&2; return 1; }
    if ((SECONDS >= deadline)); then
      case "$curl_exit" in
        0|22) exit_bucket='http' ;;
        6|7) exit_bucket='connect' ;;
        28) exit_bucket='timeout' ;;
        51|60) exit_bucket='verify' ;;
        35|58|59|64|66|77|80|82|83|90|91) exit_bucket='tls' ;;
        *) exit_bucket='other' ;;
      esac
      [[ "$observed" =~ ^[0-9]{3}$ ]] || observed='000'
      state="$(ps -o stat= -p "$distributor_pid" 2>/dev/null || true)"
      if [[ "$state" == *Z* ]]; then
        child_state='zombie'
      elif is_owned_child "$distributor_pid"; then
        child_state='live'
      else
        child_state='exited'
      fi
      echo "distributor-readiness-diagnostic exit_bucket=$exit_bucket http_status=$observed child_state=$child_state" >&2
      echo 'timeout waiting for distributor' >&2
      return 1
    fi
    sleep 0.05
  done
}

node_port() {
  case "$1" in control-a) echo 12081 ;; control-b) echo 12082 ;; control-c) echo 12083 ;; *) return 1 ;; esac
}

node_peers() {
  case "$1" in
    control-a) echo 'control-b=http://127.0.0.1:12082,control-c=http://127.0.0.1:12083' ;;
    control-b) echo 'control-a=http://127.0.0.1:12081,control-c=http://127.0.0.1:12083' ;;
    control-c) echo 'control-a=http://127.0.0.1:12081,control-b=http://127.0.0.1:12082' ;;
    *) return 1 ;;
  esac
}

node_pid() {
  case "$1" in control-a) echo "$control_a_pid" ;; control-b) echo "$control_b_pid" ;; control-c) echo "$control_c_pid" ;; *) return 1 ;; esac
}

set_node_pid() {
  case "$1" in control-a) control_a_pid="$2" ;; control-b) control_b_pid="$2" ;; control-c) control_c_pid="$2" ;; *) return 1 ;; esac
}

node_url() { echo "http://127.0.0.1:$(node_port "$1")"; }

write_bundle() {
  local service="$1" generation="$2" active="$3" path="$4"
  local cluster="${5:-$cluster_id}" encoded_service="${6:-$service}" active_value="${7:-$active}"
  local seed_a seed_b temporary
  seed_a="$(service_seed "$service" key-a)"
  seed_b="$(service_seed "$service" key-b)"
  temporary="$(dirname "$path")/.bundle.$$.${generation}.${RANDOM}.tmp"
  python3 - "$cluster" "$generation" "$encoded_service" "$active_value" "$seed_a" "$seed_b" "$temporary" <<'PY'
import json, sys
from pathlib import Path
cluster, generation, service, active, seed_a, seed_b, output = sys.argv[1:]
document = {
    "schema": "inferlab.service-signing-bundle.v1",
    "cluster_id": cluster,
    "generation": int(generation),
    "service_id": service,
    "active_credential_id": active,
    "credentials": [
        {"credential_id": "key-a", "private_key_base64": seed_a},
        {"credential_id": "key-b", "private_key_base64": seed_b},
    ],
}
Path(output).write_text(json.dumps(document, separators=(",", ":")) + "\n", encoding="utf-8")
PY
  chmod 0600 "$temporary"
  mv -f "$temporary" "$path"
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
CN = InferLab v0.29 disposable proof CA
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
  env -i PATH="$PATH" openssl req -x509 -newkey rsa:2048 -nodes -days 1 -sha256 \
    -keyout "$pki_dir/ca.key" -out "$pki_dir/ca.crt" -config "$pki_dir/ca.cnf" >/dev/null 2>&1
  local leaf
  for leaf in server publisher control-a control-b control-c; do
    env -i PATH="$PATH" openssl req -new -newkey rsa:2048 -nodes -keyout "$pki_dir/$leaf.key" \
      -out "$pki_dir/$leaf.csr" -config "$pki_dir/$leaf.cnf" >/dev/null 2>&1
    env -i PATH="$PATH" openssl x509 -req -days 1 -sha256 -in "$pki_dir/$leaf.csr" \
      -CA "$pki_dir/ca.crt" -CAkey "$pki_dir/ca.key" \
      -CAserial "$pki_dir/ca.srl" -CAcreateserial \
      -out "$pki_dir/$leaf.crt" -extfile "$pki_dir/$leaf.cnf" \
      -extensions leaf_ext >/dev/null 2>&1
  done
  python3 - "$pki_dir" "$escaped_ca_serial" <<'PY'
import os, re, stat, sys
from pathlib import Path
pki = Path(sys.argv[1])
serial = pki / "ca.srl"
escaped = Path(sys.argv[2])
if escaped.exists() or escaped.is_symlink():
    raise SystemExit("OpenSSL CA serial escaped the proof-owned PKI directory")
try:
    metadata = serial.lstat()
    raw = serial.read_bytes()
except OSError as error:
    raise SystemExit("proof-owned OpenSSL CA serial is unavailable") from error
if (
    not stat.S_ISREG(metadata.st_mode)
    or stat.S_ISLNK(metadata.st_mode)
    or stat.S_IMODE(metadata.st_mode) != 0o600
    or metadata.st_uid != os.getuid()
    or serial.resolve().parent != pki.resolve()
    or re.fullmatch(rb"[0-9A-Fa-f]+\n", raw) is None
    or not 2 <= len(raw) <= 129
):
    raise SystemExit("proof-owned OpenSSL CA serial failed its containment contract")
PY
}

start_distributor() {
  env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$NO_PROXY" \
    INFERLAB_TRUST_DISTRIBUTOR_BIND='127.0.0.1:12085' \
    INFERLAB_TRUST_DISTRIBUTOR_CLUSTER_ID="$cluster_id" \
    INFERLAB_SERVICE_TRUST_ROOT_KEYS="$trust_root_id=$trust_root_public" \
    INFERLAB_SERVICE_TRUST_REVOKED_ROOT_KEY_IDS='' \
    INFERLAB_TRUST_DISTRIBUTOR_STATE_PATH="$proof_tmp/distributor-state.json" \
    INFERLAB_TRUST_DISTRIBUTOR_EXPECTED_SERVICE_IDS='control-a,control-b,control-c' \
    INFERLAB_TRUST_DISTRIBUTOR_MAX_BODY_BYTES=262144 \
    INFERLAB_TRUST_DISTRIBUTOR_TLS_CERT_PATH="$pki_dir/server.crt" \
    INFERLAB_TRUST_DISTRIBUTOR_TLS_KEY_PATH="$pki_dir/server.key" \
    INFERLAB_TRUST_DISTRIBUTOR_TLS_CLIENT_CA_PATH="$pki_dir/ca.crt" \
    target/debug/trust-distributor >"$proof_tmp/trust-distributor.log" 2>&1 &
  distributor_pid="$!"
  live_pids+=("$distributor_pid")
  wait_distributor
}

start_node() {
  local node="$1" port peers election_min pid
  port="$(node_port "$node")"
  peers="$(node_peers "$node")"
  election_min=5000
  [[ "$node" == control-a ]] && election_min=300
  mkdir -p "$proof_tmp/$node"
  env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$NO_PROXY" \
    INFERLAB_RAFT_NODE_ID="$node" \
    INFERLAB_RAFT_CLUSTER_ID="$cluster_id" \
    INFERLAB_RAFT_BIND="127.0.0.1:$port" \
    INFERLAB_RAFT_PEERS="$peers" \
    INFERLAB_RAFT_DATA_DIR="$proof_tmp/$node" \
    INFERLAB_RAFT_ELECTION_MIN_MS="$election_min" \
    INFERLAB_RAFT_ELECTION_MAX_MS="$((election_min + 100))" \
    INFERLAB_RAFT_HEARTBEAT_MS=50 \
    INFERLAB_RAFT_RPC_TIMEOUT_MS=500 \
    INFERLAB_RAFT_COMMIT_TIMEOUT_MS=2500 \
    INFERLAB_CONTROL_SIGNING_KEY_ID="$route_key_id" \
    INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64="$route_seed" \
    INFERLAB_CONTROL_WRITER_KEYS="$writer_id=$writer_public" \
    INFERLAB_CONTROL_WRITE_MAX_AGE_MS=5000 \
    INFERLAB_CONTROL_WRITE_MAX_FUTURE_SKEW_MS=250 \
    INFERLAB_SERVICE_ID="$node" \
    INFERLAB_SERVICE_SIGNING_BUNDLE_PATH="$bundle_dir/$node.json" \
    INFERLAB_SERVICE_SIGNING_BUNDLE_POLL_MS=25 \
    INFERLAB_SERVICE_TRUST_DISTRIBUTOR_URL="$distributor_url" \
    INFERLAB_SERVICE_TRUST_CACHE_PATH="$proof_tmp/$node/service-trust-cache.json" \
    INFERLAB_SERVICE_TRUST_STATE_PATH="$proof_tmp/$node/service-trust-floor.json" \
    INFERLAB_SERVICE_TRUST_ROOT_KEYS="$trust_root_id=$trust_root_public" \
    INFERLAB_SERVICE_TRUST_REVOKED_ROOT_KEY_IDS='' \
    INFERLAB_SERVICE_TRUST_MAX_POLICY_LIFETIME_MS="$policy_lifetime_ms" \
    INFERLAB_SERVICE_TRUST_POLICY_MAX_FUTURE_SKEW_MS=250 \
    INFERLAB_SERVICE_TRUST_TLS_CA_CERT_PATH="$pki_dir/ca.crt" \
    INFERLAB_SERVICE_TRUST_TLS_CLIENT_CERT_PATH="$pki_dir/$node.crt" \
    INFERLAB_SERVICE_TRUST_TLS_CLIENT_KEY_PATH="$pki_dir/$node.key" \
    INFERLAB_SERVICE_TRUST_POLL_MS=25 \
    INFERLAB_SERVICE_TRUST_REQUEST_TIMEOUT_MS=1000 \
    INFERLAB_SERVICE_TRUST_MAX_BACKOFF_MS=200 \
    INFERLAB_SERVICE_AUTH_MAX_AGE_MS=5000 \
    INFERLAB_SERVICE_AUTH_MAX_FUTURE_SKEW_MS=250 \
    target/debug/control-plane >"$proof_tmp/$node.log" 2>&1 &
  pid="$!"
  live_pids+=("$pid")
  set_node_pid "$node" "$pid"
  wait_endpoint "http://127.0.0.1:$port/healthz" "$pid" "$node"
}

start_worker() {
  env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$NO_PROXY" \
    INFERLAB_CPU_WORKER_ID='cpu-signer-handoff' \
    INFERLAB_CPU_BIND='127.0.0.1:12084' \
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
    INFERLAB_CPU_BATCH_TICK_MS=100 \
    target/debug/cpu-worker >"$proof_tmp/cpu-worker.log" 2>&1 &
  worker_pid="$!"
  live_pids+=("$worker_pid")
  wait_endpoint "$worker_url/health" "$worker_pid" cpu-worker
}

start_gateway() {
  env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$NO_PROXY" \
    INFERLAB_BIND='127.0.0.1:12080' \
    INFERLAB_CONTROL_PLANE_URLS="$control_urls" \
    INFERLAB_CONTROL_CLUSTER_ID="$cluster_id" \
    INFERLAB_CONTROL_TRUSTED_KEYS="$route_key_id=$route_public" \
    INFERLAB_CONTROL_BOOTSTRAP_WAIT_MS=5000 \
    INFERLAB_CONTROL_POLL_MS=25 \
    INFERLAB_GATEWAY_SERVICE_ID='gateway-primary' \
    INFERLAB_GATEWAY_SERVICE_SIGNING_BUNDLE_PATH="$bundle_dir/gateway-primary.json" \
    INFERLAB_GATEWAY_SERVICE_SIGNING_BUNDLE_POLL_MS=25 \
    INFERLAB_CONTROL_SERVICE_TARGETS='control-a=http://127.0.0.1:12081,control-b=http://127.0.0.1:12082,control-c=http://127.0.0.1:12083' \
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
  wait_endpoint "$gateway_url/health" "$gateway_pid" gateway
}

sign_policy() {
  env -i PATH="$PATH" \
    INFERLAB_SERVICE_TRUST_ROOT_KEY_ID="$trust_root_id" \
    INFERLAB_SERVICE_TRUST_ROOT_PRIVATE_KEY_B64="$trust_root_seed" \
    target/debug/sign_service_trust "$1" >"$2"
}

sign_request() {
  local service="$1" credential="$2" method="$3" path="$4" audience="$5" nonce="$6" body="$7" output="$8"
  env -i PATH="$PATH" \
    INFERLAB_SERVICE_ID="$service" \
    INFERLAB_SERVICE_CREDENTIAL_ID="$credential" \
    INFERLAB_SERVICE_PRIVATE_KEY_B64="$(service_seed "$service" "$credential")" \
    target/debug/sign_service_request "$method" "$path" "$cluster_id" "$audience" \
      "$(python3 - <<'PY'
import time
print(time.time_ns() // 1_000_000)
PY
)" "$nonce" "$body" >"$output"
}

publish_snapshot() {
  python3 benchmarks/signer_handoff_probe.py capture \
    --url "$distributor_url/v1/service-trust/snapshot" --method POST --body "$1" \
    --expect-status "$2" --ca-cert "$pki_dir/ca.crt" \
    --client-cert "$pki_dir/publisher.crt" --client-key "$pki_dir/publisher.key" >"$3"
}

capture_processes() {
  local output="$1"
  python3 - "$output" "$$" \
    control-a "$control_a_pid" control-b "$control_b_pid" control-c "$control_c_pid" \
    cpu-worker "$worker_pid" gateway "$gateway_pid" trust-distributor "$distributor_pid" <<'PY'
import json, os, subprocess, sys
from pathlib import Path
output, proof_pid, *values = sys.argv[1:]
expected = {
    "control-a": "control-plane", "control-b": "control-plane", "control-c": "control-plane",
    "cpu-worker": "cpu-worker", "gateway": "gateway", "trust-distributor": "trust-distributor",
}

def linux_executable_name(target):
    deleted_suffix = " (deleted)"
    if target.endswith(deleted_suffix):
        target = target[:-len(deleted_suffix)]
    if not target or deleted_suffix in target:
        raise ValueError("invalid Linux executable target")
    return os.path.basename(target)

items = []
for index in range(0, len(values), 2):
    label, raw_pid = values[index:index + 2]
    pid = int(raw_pid)
    fields = subprocess.check_output(
        ["ps", "-o", "ppid=", "-o", "stat=", "-o", "lstart=", "-p", str(pid)],
        text=True,
        env={**os.environ, "LC_ALL": "C"},
    ).strip().split()
    if len(fields) != 7:
        raise SystemExit(f"cannot parse process identity for {label}")
    ppid, state = int(fields[0]), fields[1]
    start_token = " ".join(fields[2:7])
    if sys.platform.startswith("linux"):
        try:
            command = linux_executable_name(os.readlink(f"/proc/{pid}/exe"))
        except (OSError, ValueError):
            raise SystemExit(f"cannot resolve process executable for {label}") from None
    else:
        command = os.path.basename(subprocess.check_output(
            ["ps", "-o", "comm=", "-p", str(pid)],
            text=True,
            env={**os.environ, "LC_ALL": "C"},
        ).strip())
    if ppid != int(proof_pid) or command != expected[label] or "Z" in state:
        raise SystemExit(f"process identity mismatch for {label}")
    items.append({
        "label": label, "pid": pid, "ppid": ppid, "state": state,
        "start_token": start_token, "command": command,
    })
Path(output).write_text(json.dumps(sorted(items, key=lambda item: item["label"]), indent=2, sort_keys=True) + "\n", encoding="utf-8")
PY
}

json_value() {
  python3 - "$1" "$2" <<'PY'
import json, sys
value = json.load(open(sys.argv[1], encoding="utf-8"))
for part in sys.argv[2].split('.'):
    value = value[part]
print(value)
PY
}

signing_status_from_capture() {
  python3 - "$1" <<'PY'
import json, sys
document = json.load(open(sys.argv[1], encoding="utf-8"))
print(json.dumps(document["result"]["body"]["service_signing"], sort_keys=True))
PY
}

auth_guard_from_capture() {
  python3 - "$1" <<'PY'
import json, sys
document = json.load(open(sys.argv[1], encoding="utf-8"))
body = document["observation"]["body"]
auth = body["service_authentication"]
committed = body["committed_configuration"]
print(json.dumps({
    "term": body["term"], "revision": committed["revision"],
    "commit_index": body["commit_index"], "last_applied": body["last_applied"],
    "last_log_index": body["last_log_index"],
    "authentication_rejections": auth["authentication_rejections"],
    "credential_revocation_rejections": auth["credential_revocation_rejections"],
}, sort_keys=True))
PY
}

project_write() {
  python3 - "$1" "$2" >"$3" <<'PY'
import json, sys
source = json.load(open(sys.argv[1], encoding="utf-8"))
label, expected_revision = sys.argv[2], int(sys.argv[2][-1])
response = source["response"]
body = response.get("body")
configuration = body.get("configuration") if isinstance(body, dict) else None
workers = configuration.get("workers") if isinstance(configuration, dict) else None
projected = {
    "schema": "inferlab.signer-handoff-write.v0.29",
    "label": label,
    "started_at_ms": int(source["started_at_ms"]),
    "observed_at_ms": int(source["observed_at_ms"]),
    "status": response["status"],
    "committed": {
        "cluster_id": body.get("cluster_id") if isinstance(body, dict) else None,
        "revision": body.get("revision") if isinstance(body, dict) else None,
        "term": body.get("term") if isinstance(body, dict) else None,
        "routing_policy": configuration.get("routing_policy") if isinstance(configuration, dict) else None,
        "worker_ids": sorted(item.get("id") for item in workers if isinstance(item, dict)) if isinstance(workers, list) else [],
        "writer_id": body.get("writer", {}).get("writer_id") if isinstance(body, dict) and isinstance(body.get("writer"), dict) else None,
        "authentication_key_id": body.get("authentication", {}).get("key_id") if isinstance(body, dict) and isinstance(body.get("authentication"), dict) else None,
    },
}
if projected["committed"]["revision"] != expected_revision:
    raise SystemExit("write projection revision mismatch")
print(json.dumps(projected, indent=2, sort_keys=True))
PY
}

error_message() {
  case "$1" in
    source_unavailable) echo 'service signing bundle metadata is unavailable' ;;
    invalid_json) echo 'service signing bundle is not exact valid JSON' ;;
    bundle_too_large) echo 'service signing bundle exceeds the byte limit' ;;
    unsafe_permissions) echo 'service signing bundle permissions must be exactly 0600' ;;
    not_regular_file) echo 'service signing bundle must be a regular file and not a symbolic link' ;;
    cluster_mismatch) echo 'service signing bundle cluster ID does not match this process' ;;
    service_mismatch) echo 'service signing bundle service ID does not match this process' ;;
    unknown_active_credential) echo 'service signing bundle active credential is not configured' ;;
    stale_generation) echo 'service signing bundle generation is older than the active generation' ;;
    generation_fork) echo 'service signing bundle reuses the active generation with different contents' ;;
    candidate_rejected) echo 'service signing bundle candidate was rejected by local policy' ;;
    *) return 1 ;;
  esac
}

prepare_invalid_source() {
  local scenario="$1" service="$2" path="$3"
  local target candidate
  target="$path.target"
  candidate="$path.invalid.$$.${RANDOM}"
  rm -rf -- "$candidate" "$target"
  case "$scenario" in
    missing)
      rm -rf -- "$path"
      ;;
    malformed)
      printf '%s\n' '{not-json' >"$candidate"
      chmod 0600 "$candidate"
      mv -f "$candidate" "$path"
      ;;
    oversize)
      python3 - "$candidate" <<'PY'
import sys
open(sys.argv[1], "wb").write(b"x" * 16385)
PY
      chmod 0600 "$candidate"
      mv -f "$candidate" "$path"
      ;;
    unsafe-permissions)
      write_bundle "$service" 1 key-a "$candidate"
      chmod 0644 "$candidate"
      mv -f "$candidate" "$path"
      ;;
    non-regular)
      mkfifo "$candidate"
      mv -f "$candidate" "$path"
      ;;
    symlink)
      write_bundle "$service" 1 key-a "$target"
      ln -s "$target" "$candidate"
      mv -f "$candidate" "$path"
      ;;
    wrong-cluster)
      write_bundle "$service" 1 key-a "$candidate" 'wrong-cluster'
      mv -f "$candidate" "$path"
      ;;
    wrong-service)
      write_bundle "$service" 1 key-a "$candidate" "$cluster_id" 'wrong-service'
      mv -f "$candidate" "$path"
      ;;
    unknown-active)
      write_bundle "$service" 1 key-a "$candidate" "$cluster_id" "$service" 'key-unknown'
      mv -f "$candidate" "$path"
      ;;
    stale)
      write_bundle "$service" 1 key-a "$candidate"
      mv -f "$candidate" "$path"
      ;;
    fork)
      write_bundle "$service" 2 key-a "$candidate"
      mv -f "$candidate" "$path"
      ;;
    *) return 1 ;;
  esac
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

run_startup_case() {
  local scenario="$1" kind="$2" port="$3" output="$4"
  local directory="$case_dir/startup-$scenario" source pid deadline state exit_code probes=0 ever_open=0 message
  directory="$case_dir/startup-$scenario"
  source="$directory/bundle.json"
  mkdir -p "$directory/state"
  prepare_invalid_source "$scenario" control-a "$source"
  env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$NO_PROXY" \
    INFERLAB_RAFT_NODE_ID='control-a' \
    INFERLAB_RAFT_CLUSTER_ID="$cluster_id" \
    INFERLAB_RAFT_BIND="127.0.0.1:$port" \
    INFERLAB_RAFT_PEERS='control-b=http://127.0.0.1:12082,control-c=http://127.0.0.1:12083' \
    INFERLAB_RAFT_DATA_DIR="$directory/state" \
    INFERLAB_CONTROL_SIGNING_KEY_ID="$route_key_id" \
    INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64="$route_seed" \
    INFERLAB_CONTROL_WRITER_KEYS="$writer_id=$writer_public" \
    INFERLAB_SERVICE_ID='control-a' \
    INFERLAB_SERVICE_SIGNING_BUNDLE_PATH="$source" \
    INFERLAB_SERVICE_SIGNING_BUNDLE_POLL_MS=25 \
    target/debug/control-plane >"$directory/process.log" 2>&1 &
  pid="$!"
  live_pids+=("$pid")
  deadline=$((SECONDS + 10))
  while ((SECONDS < deadline)); do
    probes=$((probes + 1))
    if listener_is_open "$port"; then ever_open=1; break; fi
    state="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
    if ! is_owned_child "$pid" || [[ "$state" == *Z* ]]; then break; fi
    sleep 0.01
  done
  if is_owned_child "$pid"; then
    state="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
    if [[ "$state" != *Z* ]]; then
      shutdown_child "$pid"
      forget_pid "$pid"
      echo "startup case remained live: $scenario" >&2
      return 1
    fi
  fi
  set +e
  wait "$pid"
  exit_code="$?"
  set -e
  forget_pid "$pid"
  message="$(error_message "$kind")"
  grep -F "$message" "$directory/process.log" >/dev/null || {
    echo "startup case missing exact diagnostic: $scenario" >&2
    tail -n 20 "$directory/process.log" >&2
    return 1
  }
  python3 - "$scenario" "$kind" "$message" "$port" "$exit_code" "$pid" "$ever_open" "$probes" \
    "$directory/state" >"$output" <<'PY'
import json, sys
from pathlib import Path
scenario, kind, diagnostic, port, exit_code, pid, ever_open, probes, state_dir = sys.argv[1:]
files = sorted(path.name for path in Path(state_dir).rglob("*") if path.is_file())
print(json.dumps({
    "scenario": scenario,
    "expected_error_kind": kind,
    "port": int(port),
    "exit_code": int(exit_code),
    "pid": int(pid),
    "listener_ever_open": bool(int(ever_open)),
    "listener_probe_count": int(probes),
    "state_files_created": files,
    "diagnostic": diagnostic,
}, indent=2, sort_keys=True))
PY
}

prepare_output_dir
check_ports_are_free 12080 12081 12082 12083 12084 12085 12180 12181 12182 12183 12184 12185 12186 12187 12188
command -v openssl >/dev/null
if [[ -e "$escaped_ca_serial" || -L "$escaped_ca_serial" ]]; then
  echo 'refusing v0.29 proof: escaped OpenSSL CA serial sentinel already exists' >&2
  exit 1
fi
cargo build --locked --workspace --bins --quiet
generate_pki

service_public() {
  env -i PATH="$PATH" INFERLAB_SERVICE_ID="$1" \
    INFERLAB_SERVICE_CREDENTIAL_ID="$2" \
    INFERLAB_SERVICE_PRIVATE_KEY_B64="$(service_seed "$1" "$2")" \
    target/debug/service_public_key
}

generic_public() {
  env -i PATH="$PATH" INFERLAB_SERVICE_ID='proof-key' \
    INFERLAB_SERVICE_CREDENTIAL_ID='proof-key' \
    INFERLAB_SERVICE_PRIVATE_KEY_B64="$1" target/debug/service_public_key
}

trust_root_public="$(env -i PATH="$PATH" \
  INFERLAB_SERVICE_TRUST_ROOT_KEY_ID="$trust_root_id" \
  INFERLAB_SERVICE_TRUST_ROOT_PRIVATE_KEY_B64="$trust_root_seed" \
  target/debug/service_trust_public_key)"
route_public="$(generic_public "$route_seed")"
writer_public="$(generic_public "$writer_seed")"

for service in control-a control-b control-c gateway-primary; do
  write_bundle "$service" 1 key-a "$bundle_dir/$service.json"
done

issued_at_ms="$(python3 - <<'PY'
import time
print(time.time_ns() // 1_000_000)
PY
)"
expires_at_ms="$((issued_at_ms + policy_lifetime_ms))"
python3 - "$issued_at_ms" "$expires_at_ms" "$policy_dir" \
  "$(service_public control-a key-a)" "$(service_public control-a key-b)" \
  "$(service_public control-b key-a)" "$(service_public control-b key-b)" \
  "$(service_public control-c key-a)" "$(service_public control-c key-b)" \
  "$(service_public gateway-primary key-a)" "$(service_public gateway-primary key-b)" <<'PY'
import json, sys
from pathlib import Path
issued, expires = map(int, sys.argv[1:3])
directory = Path(sys.argv[3])
keys = sys.argv[4:]
services = ["control-a", "control-b", "control-c", "gateway-primary"]
credentials = []
for index, service in enumerate(services):
    credentials.extend([
        {"service_id": service, "credential_id": "key-a", "public_key_base64": keys[index * 2]},
        {"service_id": service, "credential_id": "key-b", "public_key_base64": keys[index * 2 + 1]},
    ])
for generation in (1, 2):
    policy = {
        "schema": "inferlab.service-trust-policy.v2",
        "cluster_id": "inferlab-primary",
        "generation": generation,
        "issued_at_ms": issued,
        "expires_at_ms": expires,
        "trusted_credentials": credentials,
        "revoked_service_ids": [],
        "revoked_credentials": [] if generation == 1 else [
            {"service_id": service, "credential_id": "key-a"} for service in services
        ],
        "gateway_service_ids": ["gateway-primary"],
    }
    (directory / f"policy-g{generation}.json").write_text(
        json.dumps(policy, indent=2) + "\n", encoding="utf-8"
    )
PY
sign_policy "$policy_dir/policy-g1.json" "$policy_dir/snapshot-g1.json"
sign_policy "$policy_dir/policy-g2.json" "$policy_dir/snapshot-g2.json"
python3 - "$trust_root_id" "$trust_root_public" "$policy_dir/snapshot-g1.json" \
  "$policy_dir/snapshot-g2.json" >"$results_dir/trust-generations.json" <<'PY'
import json, sys
root_id, root_public, g1, g2 = sys.argv[1:]
print(json.dumps({
    "schema": "inferlab.signer-handoff-trust-generations.v0.29",
    "root_key_id": root_id,
    "root_public_key_base64": root_public,
    "generations": {
        "1": json.load(open(g1, encoding="utf-8")),
        "2": json.load(open(g2, encoding="utf-8")),
    },
}, indent=2, sort_keys=True))
PY

cat >"$results_dir/proof-contract.json" <<'JSON'
{
  "bundle_generations": [1, 2],
  "cluster_id": "inferlab-primary",
  "controls": ["control-a", "control-b", "control-c"],
  "credentials": ["key-a", "key-b"],
  "expected_receiver_mode": "service-id",
  "handoff_order": "follower,follower,leader,gateway",
  "ports": {
    "control-a": 12081,
    "control-b": 12082,
    "control-c": 12083,
    "cpu-worker": 12084,
    "gateway": 12080,
    "trust-distributor": 12085
  },
  "startup_ports": {
    "malformed": 12181,
    "missing": 12180,
    "non-regular": 12184,
    "oversize": 12182,
    "symlink": 12185,
    "unknown-active": 12188,
    "unsafe-permissions": 12183,
    "wrong-cluster": 12186,
    "wrong-service": 12187
  },
  "processes": ["control-a", "control-b", "control-c", "cpu-worker", "gateway", "trust-distributor"],
  "schema": "inferlab.signer-handoff-proof-contract.v0.29",
  "services": ["control-a", "control-b", "control-c", "gateway-primary"],
  "trust_generations": [1, 2]
}
JSON

startup_scenarios=(missing malformed oversize unsafe-permissions non-regular symlink wrong-cluster wrong-service unknown-active)
startup_kinds=(source_unavailable invalid_json bundle_too_large unsafe_permissions not_regular_file not_regular_file cluster_mismatch service_mismatch unknown_active_credential)
startup_outputs=()
for index in "${!startup_scenarios[@]}"; do
  output="$proof_tmp/startup-$index.json"
  run_startup_case "${startup_scenarios[$index]}" "${startup_kinds[$index]}" \
    "$((12180 + index))" "$output"
  startup_outputs+=("$output")
done
python3 - "${startup_outputs[@]}" >"$results_dir/startup-rejections.json" <<'PY'
import json, sys
print(json.dumps({
    "schema": "inferlab.signer-handoff-startup-rejections.v0.29",
    "cases": [json.load(open(path, encoding="utf-8")) for path in sys.argv[1:]],
}, indent=2, sort_keys=True))
PY

start_distributor
publish_snapshot "$policy_dir/snapshot-g1.json" 201 "$results_dir/publish-g1.json"
start_node control-b
start_node control-c
start_node control-a
start_worker

python3 benchmarks/full_stack_probe.py wait-leader --urls "$control_urls" --timeout 15 \
  >"$proof_tmp/initial-cluster.json"
leader_id="$(json_value "$proof_tmp/initial-cluster.json" leader_id)"
leader_url="$(json_value "$proof_tmp/initial-cluster.json" leader_url)"

cat >"$proof_tmp/route-r2.json" <<'JSON'
{
  "routing_policy": "round-robin",
  "workers": [
    {"id": "cpu-signer-handoff", "base_url": "http://127.0.0.1:12084", "weight": 1}
  ]
}
JSON
env -i PATH="$PATH" INFERLAB_CONTROL_WRITER_ID="$writer_id" \
  INFERLAB_CONTROL_WRITER_PRIVATE_KEY_B64="$writer_seed" \
  target/debug/sign_control_write "$cluster_id" 0 now v029-route-write-r2-0001 "$proof_tmp/route-r2.json" \
  >"$proof_tmp/write-r2.json"
python3 benchmarks/control_write_probe.py submit --url "$leader_url" --body "$proof_tmp/write-r2.json" \
  >"$proof_tmp/r2-write-raw.json"
project_write "$proof_tmp/r2-write-raw.json" r2 "$results_dir/r2-write.json"

start_gateway
python3 benchmarks/signer_handoff_probe.py wait-controls --urls "$control_urls" --revision 2 \
  --bundle-generation 1 --expected-signers 'control-a=key-a,control-b=key-a,control-c=key-a' \
  --timeout 15 >"$results_dir/generation-1-controls.json"
python3 benchmarks/signer_handoff_probe.py wait-distributor --url "$distributor_url" \
  --generation 1 --credential key-a --expected-services 'control-a,control-b,control-c' \
  --ca-cert "$pki_dir/ca.crt" --client-cert "$pki_dir/publisher.crt" \
  --client-key "$pki_dir/publisher.key" --timeout 15 >"$results_dir/generation-1-receipts.json"
python3 benchmarks/signer_handoff_probe.py wait-gateway --url "$gateway_url" --revision 2 \
  --bundle-generation 1 --credential key-a --worker-id cpu-signer-handoff --timeout 15 \
  >"$results_dir/gateway-r2.json"
capture_processes "$proof_tmp/initial-processes.json"

wait_control_state() {
  local service="$1" credential="$2" generation="$3" kind="$4" min_rejections="$5" output="$6"
  local arguments=(
    benchmarks/signer_handoff_probe.py wait-control-signer
    --url "$(node_url "$service")" --service-id "$service"
    --credential "$credential" --bundle-generation "$generation"
    --min-rejections "$min_rejections" --timeout 12
  )
  [[ -z "$kind" ]] || arguments+=(--last-error-kind "$kind")
  python3 "${arguments[@]}" >"$output"
}

run_live_case() {
  local scenario="$1" kind="$2" service="$3" generation="$4" credential="$5" output="$6"
  local path="$bundle_dir/$service.json" before_capture rejected_capture recovered_capture recovery_candidate
  local before_status rejected_status recovered_status before_count pid start_token
  before_capture="$proof_tmp/live-$scenario-before.json"
  rejected_capture="$proof_tmp/live-$scenario-rejected.json"
  recovered_capture="$proof_tmp/live-$scenario-recovered.json"
  wait_control_state "$service" "$credential" "$generation" '' 0 "$before_capture"
  before_status="$(signing_status_from_capture "$before_capture")"
  before_count="$(python3 - "$before_capture" <<'PY'
import json, sys
print(json.load(open(sys.argv[1], encoding="utf-8"))["result"]["body"]["service_signing"]["rejected_reloads"])
PY
)"
  prepare_invalid_source "$scenario" "$service" "$path"
  wait_control_state "$service" "$credential" "$generation" "$kind" "$((before_count + 1))" "$rejected_capture"
  rejected_status="$(signing_status_from_capture "$rejected_capture")"
  recovery_candidate="$path.recovery.$$.$RANDOM"
  rm -rf -- "$recovery_candidate"
  write_bundle "$service" "$generation" "$credential" "$recovery_candidate"
  mv -f "$recovery_candidate" "$path"
  rm -rf -- "$path.target"
  wait_control_state "$service" "$credential" "$generation" '' "$((before_count + 1))" "$recovered_capture"
  recovered_status="$(signing_status_from_capture "$recovered_capture")"
  pid="$(node_pid "$service")"
  start_token="$(python3 - "$proof_tmp/initial-processes.json" "$service" <<'PY'
import json, sys
items = json.load(open(sys.argv[1], encoding="utf-8"))
print(next(item["start_token"] for item in items if item["label"] == sys.argv[2]))
PY
)"
  python3 - "$scenario" "$kind" "$service" "$pid" "$start_token" \
    "$before_status" "$rejected_status" "$recovered_status" >"$output" <<'PY'
import json, sys
scenario, kind, service, pid, start, before, rejected, recovered = sys.argv[1:]
print(json.dumps({
    "scenario": scenario,
    "expected_error_kind": kind,
    "service_id": service,
    "pid": int(pid),
    "start_token": start,
    "before": json.loads(before),
    "rejected": json.loads(rejected),
    "recovered": json.loads(recovered),
}, indent=2, sort_keys=True))
PY
}

followers="$(python3 - "$proof_tmp/initial-cluster.json" <<'PY'
import json, sys
document = json.load(open(sys.argv[1], encoding="utf-8"))
leader = document["leader_id"]
print(" ".join(sorted(item["body"]["node_id"] for item in document["statuses"] if item["body"]["node_id"] != leader)))
PY
)"
read -r follower_one follower_two <<<"$followers"

live_outputs=()
live_scenarios=(missing malformed oversize unsafe-permissions non-regular symlink wrong-cluster wrong-service unknown-active)
live_kinds=(source_unavailable invalid_json bundle_too_large unsafe_permissions not_regular_file not_regular_file cluster_mismatch service_mismatch unknown_active_credential)
for index in "${!live_scenarios[@]}"; do
  output="$proof_tmp/live-${live_scenarios[$index]}.json"
  run_live_case "${live_scenarios[$index]}" "${live_kinds[$index]}" \
    "$follower_one" 1 key-a "$output"
  live_outputs+=("$output")
done

handoff_steps=()
switched=()
step_index=0
for service in "$follower_one" "$follower_two" "$leader_id"; do
  step_index=$((step_index + 1))
  write_bundle "$service" 2 key-b "$bundle_dir/$service.json"
  switched+=("$service")
  signer_map=''
  generation_map=''
  for candidate in control-a control-b control-c; do
    credential=key-a
    generation=1
    for changed in "${switched[@]}"; do
      if [[ "$candidate" == "$changed" ]]; then credential=key-b; generation=2; fi
    done
    signer_map+="${signer_map:+,}$candidate=$credential"
    generation_map+="${generation_map:+,}$candidate=$generation"
  done
  cluster_capture="$proof_tmp/handoff-$step_index-cluster.json"
  gateway_capture="$proof_tmp/handoff-$step_index-gateway.json"
  process_capture="$proof_tmp/handoff-$step_index-processes.json"
  python3 benchmarks/signer_handoff_probe.py wait-controls --urls "$control_urls" --revision 2 \
    --expected-signers "$signer_map" --expected-generations "$generation_map" --timeout 15 \
    >"$cluster_capture"
  python3 benchmarks/signer_handoff_probe.py wait-gateway --url "$gateway_url" --revision 2 \
    --bundle-generation 1 --credential key-a --worker-id cpu-signer-handoff --timeout 15 \
    >"$gateway_capture"
  capture_processes "$process_capture"
  step_output="$proof_tmp/handoff-step-$step_index.json"
  python3 - "$step_index" "$service" "$cluster_capture" "$gateway_capture" "$process_capture" \
    >"$step_output" <<'PY'
import json, sys
index, service, cluster, gateway, processes = sys.argv[1:]
print(json.dumps({
    "index": int(index),
    "role_at_handoff": "leader" if service == json.load(open(cluster, encoding="utf-8"))["result"]["leader_id"] else "follower",
    "service_id": service,
    "cluster": json.load(open(cluster, encoding="utf-8")),
    "gateway": json.load(open(gateway, encoding="utf-8")),
    "processes": json.load(open(processes, encoding="utf-8")),
}, indent=2, sort_keys=True))
PY
  handoff_steps+=("$step_output")
done

write_bundle gateway-primary 2 key-b "$bundle_dir/gateway-primary.json"
cluster_capture="$proof_tmp/handoff-4-cluster.json"
gateway_capture="$proof_tmp/handoff-4-gateway.json"
process_capture="$proof_tmp/handoff-4-processes.json"
python3 benchmarks/signer_handoff_probe.py wait-controls --urls "$control_urls" --revision 2 \
  --bundle-generation 2 --expected-signers 'control-a=key-b,control-b=key-b,control-c=key-b' \
  --timeout 15 >"$cluster_capture"
python3 benchmarks/signer_handoff_probe.py wait-gateway --url "$gateway_url" --revision 2 \
  --bundle-generation 2 --credential key-b --worker-id cpu-signer-handoff --timeout 15 \
  >"$gateway_capture"
capture_processes "$process_capture"
step_output="$proof_tmp/handoff-step-4.json"
python3 - "$cluster_capture" "$gateway_capture" "$process_capture" >"$step_output" <<'PY'
import json, sys
print(json.dumps({
    "index": 4,
    "role_at_handoff": "gateway",
    "service_id": "gateway-primary",
    "cluster": json.load(open(sys.argv[1], encoding="utf-8")),
    "gateway": json.load(open(sys.argv[2], encoding="utf-8")),
    "processes": json.load(open(sys.argv[3], encoding="utf-8")),
}, indent=2, sort_keys=True))
PY
handoff_steps+=("$step_output")
python3 - "$proof_tmp/initial-processes.json" "${handoff_steps[@]}" \
  >"$results_dir/handoff-sequence.json" <<'PY'
import json, sys
print(json.dumps({
    "schema": "inferlab.signer-handoff-sequence.v0.29",
    "initial_processes": json.load(open(sys.argv[1], encoding="utf-8")),
    "steps": [json.load(open(path, encoding="utf-8")) for path in sys.argv[2:]],
}, indent=2, sort_keys=True))
PY

for pair in 'stale stale_generation' 'fork generation_fork'; do
  read -r scenario kind <<<"$pair"
  output="$proof_tmp/live-$scenario.json"
  run_live_case "$scenario" "$kind" "$follower_one" 2 key-b "$output"
  live_outputs+=("$output")
done
python3 - "${live_outputs[@]}" >"$results_dir/live-source-rejections.json" <<'PY'
import json, sys
print(json.dumps({
    "schema": "inferlab.signer-handoff-live-rejections.v0.29",
    "cases": [json.load(open(path, encoding="utf-8")) for path in sys.argv[1:]],
}, indent=2, sort_keys=True))
PY

python3 benchmarks/signer_handoff_probe.py wait-distributor --url "$distributor_url" \
  --generation 1 --credential key-a --expected-services 'control-a,control-b,control-c' \
  --ca-cert "$pki_dir/ca.crt" --client-cert "$pki_dir/publisher.crt" \
  --client-key "$pki_dir/publisher.key" --timeout 15 \
  >"$results_dir/generation-1-after-handoff.json"

publish_snapshot "$policy_dir/snapshot-g2.json" 201 "$results_dir/publish-g2.json"
python3 benchmarks/signer_handoff_probe.py wait-controls --urls "$control_urls" --revision 2 \
  --bundle-generation 2 --expected-signers 'control-a=key-b,control-b=key-b,control-c=key-b' \
  --timeout 20 >"$results_dir/generation-2-controls.json"
python3 benchmarks/signer_handoff_probe.py wait-distributor --url "$distributor_url" \
  --generation 2 --credential key-b --expected-services 'control-a,control-b,control-c' \
  --ca-cert "$pki_dir/ca.crt" --client-cert "$pki_dir/publisher.crt" \
  --client-key "$pki_dir/publisher.key" --timeout 20 \
  >"$results_dir/generation-2-receipts.json"

capture_control_status() {
  python3 benchmarks/signer_handoff_probe.py capture --url "$(node_url "$1")/v1/control/status" \
    --projection control-status --expect-status 200 >"$2"
}

capture_control_status "$leader_id" "$proof_tmp/gateway-attack-before.json"
gateway_before_guard="$(auth_guard_from_capture "$proof_tmp/gateway-attack-before.json")"
sign_request gateway-primary key-a GET /v1/control/config "$leader_id" \
  v029-old-a-gateway-0001 - "$proof_tmp/old-a-gateway-auth.json"
python3 benchmarks/signer_handoff_probe.py capture --url "$leader_url/v1/control/config" \
  --authentication "$proof_tmp/old-a-gateway-auth.json" --expect-status 401 \
  >"$proof_tmp/old-a-gateway-response.json"
capture_control_status "$leader_id" "$proof_tmp/gateway-attack-after.json"
gateway_after_guard="$(auth_guard_from_capture "$proof_tmp/gateway-attack-after.json")"

capture_control_status "$leader_id" "$proof_tmp/peer-attack-before.json"
peer_before_guard="$(auth_guard_from_capture "$proof_tmp/peer-attack-before.json")"
leader_term="$(python3 - "$proof_tmp/peer-attack-before.json" <<'PY'
import json, sys
print(json.load(open(sys.argv[1], encoding="utf-8"))["observation"]["body"]["term"])
PY
)"
high_term="$((leader_term + 1000))"
python3 - "$high_term" "$follower_one" >"$proof_tmp/high-term-vote.json" <<'PY'
import json, sys
print(json.dumps({
    "cluster_id": "inferlab-primary",
    "term": int(sys.argv[1]),
    "candidate_id": sys.argv[2],
    "last_log_index": 0,
    "last_log_term": 0,
}, separators=(",", ":")))
PY
sign_request "$follower_one" key-a POST /raft/request-vote "$leader_id" \
  v029-old-a-peer-vote-0001 "$proof_tmp/high-term-vote.json" "$proof_tmp/old-a-peer-auth.json"
python3 benchmarks/signer_handoff_probe.py capture --url "$leader_url/raft/request-vote" --method POST \
  --body "$proof_tmp/high-term-vote.json" --authentication "$proof_tmp/old-a-peer-auth.json" \
  --expect-status 401 >"$proof_tmp/old-a-peer-response.json"
capture_control_status "$leader_id" "$proof_tmp/peer-attack-after.json"
peer_after_guard="$(auth_guard_from_capture "$proof_tmp/peer-attack-after.json")"

sign_request gateway-primary key-b GET /v1/control/config "$leader_id" \
  v029-valid-b-gateway-0001 - "$proof_tmp/valid-b-auth.json"
python3 benchmarks/signer_handoff_probe.py capture --url "$leader_url/v1/control/config" \
  --authentication "$proof_tmp/valid-b-auth.json" --projection control-config --expect-status 200 \
  >"$proof_tmp/valid-b-read.json"

revoked_service="$follower_two"
revoked_before_capture="$proof_tmp/revoked-bundle-before.json"
revoked_after_capture="$proof_tmp/revoked-bundle-after.json"
revoked_recovered_capture="$proof_tmp/revoked-bundle-recovered.json"
wait_control_state "$revoked_service" key-b 2 '' 0 "$revoked_before_capture"
revoked_before_status="$(signing_status_from_capture "$revoked_before_capture")"
revoked_before_count="$(python3 - "$revoked_before_capture" <<'PY'
import json, sys
print(json.load(open(sys.argv[1], encoding="utf-8"))["result"]["body"]["service_signing"]["rejected_reloads"])
PY
)"
write_bundle "$revoked_service" 3 key-a "$bundle_dir/$revoked_service.json"
python3 - "$bundle_dir/$revoked_service.json" >"$proof_tmp/revoked-bundle-candidate.json" <<'PY'
import json, sys
source = json.load(open(sys.argv[1], encoding="utf-8"))
print(json.dumps({
    "schema": source.get("schema"),
    "cluster_id": source.get("cluster_id"),
    "service_id": source.get("service_id"),
    "bundle_generation": source.get("generation"),
    "active_credential_id": source.get("active_credential_id"),
    "configured_credential_ids": sorted(
        item.get("credential_id") for item in source.get("credentials", []) if isinstance(item, dict)
    ),
    "configured_credential_count": len(source.get("credentials", [])),
}, indent=2, sort_keys=True))
PY
wait_control_state "$revoked_service" key-b 2 candidate_rejected "$((revoked_before_count + 1))" "$revoked_after_capture"
revoked_after_status="$(signing_status_from_capture "$revoked_after_capture")"
write_bundle "$revoked_service" 2 key-b "$bundle_dir/$revoked_service.json"
wait_control_state "$revoked_service" key-b 2 '' "$((revoked_before_count + 1))" "$revoked_recovered_capture"
revoked_recovered_status="$(signing_status_from_capture "$revoked_recovered_capture")"
revoked_pid="$(node_pid "$revoked_service")"
revoked_start="$(python3 - "$proof_tmp/initial-processes.json" "$revoked_service" <<'PY'
import json, sys
print(next(item["start_token"] for item in json.load(open(sys.argv[1], encoding="utf-8")) if item["label"] == sys.argv[2]))
PY
)"

python3 - "$gateway_before_guard" "$gateway_after_guard" \
  "$proof_tmp/old-a-gateway-response.json" "$follower_one" "$high_term" \
  "$proof_tmp/high-term-vote.json" "$peer_before_guard" "$peer_after_guard" \
  "$proof_tmp/old-a-peer-response.json" \
  "$revoked_service" "$revoked_pid" "$revoked_start" "$revoked_before_status" \
  "$revoked_after_status" "$revoked_recovered_status" "$proof_tmp/revoked-bundle-candidate.json" \
  "$proof_tmp/valid-b-read.json" \
  >"$results_dir/revoked-a-attacks.json" <<'PY'
import json, sys
(
    gateway_before, gateway_after, gateway_response, peer_service, high_term, peer_request,
    peer_before, peer_after, peer_response, revoked_service, revoked_pid,
    revoked_start, revoked_before, revoked_after, revoked_recovered, revoked_candidate, valid_b,
) = sys.argv[1:]
print(json.dumps({
    "schema": "inferlab.signer-handoff-revoked-a.v0.29",
    "gateway_old_a": {
        "service_id": "gateway-primary", "credential_id": "key-a",
        "before": json.loads(gateway_before),
        "response": json.load(open(gateway_response, encoding="utf-8"))["observation"],
        "after": json.loads(gateway_after),
    },
    "peer_old_a": {
        "service_id": peer_service, "credential_id": "key-a", "candidate_id": peer_service,
        "high_term": int(high_term), "request": json.load(open(peer_request, encoding="utf-8")),
        "before_mutation": json.loads(peer_before),
        "response": json.load(open(peer_response, encoding="utf-8"))["observation"],
        "after_mutation": json.loads(peer_after),
    },
    "revoked_bundle": {
        "service_id": revoked_service, "pid": int(revoked_pid), "start_token": revoked_start,
        "expected_error_kind": "candidate_rejected", "before": json.loads(revoked_before),
        "after": json.loads(revoked_after), "recovered": json.loads(revoked_recovered),
        "candidate": json.load(open(revoked_candidate, encoding="utf-8")),
    },
    "valid_b_read": json.load(open(valid_b, encoding="utf-8"))["observation"],
}, indent=2, sort_keys=True))
PY

cat >"$proof_tmp/route-r3.json" <<'JSON'
{
  "routing_policy": "least-in-flight",
  "workers": [
    {"id": "cpu-signer-handoff", "base_url": "http://127.0.0.1:12084", "weight": 1}
  ]
}
JSON
env -i PATH="$PATH" INFERLAB_CONTROL_WRITER_ID="$writer_id" \
  INFERLAB_CONTROL_WRITER_PRIVATE_KEY_B64="$writer_seed" \
  target/debug/sign_control_write "$cluster_id" 2 now v029-route-write-r3-0001 "$proof_tmp/route-r3.json" \
  >"$proof_tmp/write-r3.json"
python3 benchmarks/control_write_probe.py submit --url "$leader_url" --body "$proof_tmp/write-r3.json" \
  >"$proof_tmp/r3-write-raw.json"
project_write "$proof_tmp/r3-write-raw.json" r3 "$results_dir/r3-write.json"

python3 benchmarks/signer_handoff_probe.py wait-controls --urls "$control_urls" --revision 3 \
  --bundle-generation 2 --expected-signers 'control-a=key-b,control-b=key-b,control-c=key-b' \
  --timeout 20 >"$results_dir/final-cluster.json"
python3 benchmarks/signer_handoff_probe.py wait-gateway --url "$gateway_url" --revision 3 \
  --bundle-generation 2 --credential key-b --worker-id cpu-signer-handoff --timeout 20 \
  >"$results_dir/final-gateway.json"
python3 benchmarks/signer_handoff_probe.py completion --url "$gateway_url" \
  --prompt 'v029-final-json-private-proof-prompt' --temporary-body "$proof_tmp/final-json-body.json" \
  --timeout 15 >"$results_dir/final-json.json"
python3 benchmarks/signer_handoff_probe.py stream --url "$gateway_url" \
  --prompt 'v029-final-sse-private-proof-prompt' --timeout 15 >"$results_dir/final-sse.json"

# Prebuild each exact harness outside retained output so compiler paths can
# never enter the bounded three-line production-test projection.
cargo test --locked -p service-auth --lib --no-run --quiet
cargo test --locked -p control-plane --lib --no-run --quiet
cargo test --locked -p control-plane --bin control-plane --no-run --quiet
cargo test --locked -p gateway --lib --no-run --quiet
cargo test --locked -p gateway --bin gateway --no-run --quiet
cargo test --locked -p trust-distributor --test distributor --no-run --quiet

production_specs=(
  'service-auth|--lib|signing_bundle::tests::same_millisecond_concurrent_handoff_never_reuses_nonce'
  'service-auth|--lib|signing_bundle::tests::rollback_fork_and_policy_rejection_retain_last_known_good'
  'service-auth|--lib|signing_bundle::tests::load_requires_exact_0600_regular_file'
  'service-auth|--lib|signing_bundle::tests::watched_receipts_are_cluster_bound_while_static_receipts_remain_compatible'
  'control-plane|--lib|raft::tests::raft_requests_use_the_current_bundle_signer_without_reopening_the_node'
  'control-plane|--lib|service_trust::tests::remote_policy_receipts_follow_the_current_signer_without_false_handoff_receipts'
  'control-plane|--bin control-plane|tests::watched_service_signer_activates_only_the_exact_policy_key'
  'control-plane|--bin control-plane|tests::supervisor_fails_when_the_service_signing_watcher_completes_or_panics'
  'gateway|--lib|service_client::tests::in_flight_request_keeps_its_snapshot_and_the_next_request_uses_the_handoff'
  'gateway|--bin gateway|tests::signing_watch_loop_retries_transient_source_race_but_dedupes_deterministic_input'
  'trust-distributor|--test distributor|service_receiver_mode_survives_credential_handoff_and_revocation'
)
production_arguments=()
production_index=0
for spec in "${production_specs[@]}"; do
  IFS='|' read -r package raw_target test_filter <<<"$spec"
  read -r -a target_arguments <<<"$raw_target"
  test_log="$proof_tmp/production-$production_index.log"
  set +e
  CARGO_TERM_COLOR=never cargo test --locked -p "$package" "${target_arguments[@]}" \
    "$test_filter" -- --exact >"$test_log" 2>&1
  test_status="$?"
  set -e
  production_arguments+=("$package" "$raw_target" "$test_filter" "$test_status" "$test_log")
  production_index=$((production_index + 1))
done
python3 - "${production_arguments[@]}" >"$results_dir/production-tests.json" <<'PY'
import json, re, shlex, sys
from pathlib import Path
values = sys.argv[1:]
if len(values) % 5:
    raise SystemExit("production evidence requires package/target/filter/status/log quintuples")
summary_pattern = re.compile(
    r"^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; "
    r"[0-9]+ filtered out; finished in [0-9]+(?:\.[0-9]+)?s$"
)
tests = []
for index in range(0, len(values), 5):
    package, raw_target, test_filter, raw_status, path = values[index:index + 5]
    lines = Path(path).read_text(encoding="utf-8", errors="replace").splitlines()
    test_line = f"test {test_filter} ... ok"
    summaries = [line for line in lines if summary_pattern.fullmatch(line)]
    projected = ["running 1 test", test_line, summaries[0]] if (
        lines.count("running 1 test") == 1
        and lines.count(test_line) == 1
        and len(summaries) == 1
    ) else []
    target = shlex.split(raw_target)
    tests.append({
        "package": package,
        "test_filter": test_filter,
        "command": ["cargo", "test", "--locked", "-p", package, *target, test_filter, "--", "--exact"],
        "environment": {"CARGO_TERM_COLOR": "never"},
        "exit_code": int(raw_status),
        "summary_line": summaries[0] if len(summaries) == 1 else None,
        "output_lines": projected,
    })
result = {
    "schema": "inferlab.signer-handoff-production-tests.v0.29",
    "test_count": len(tests),
    "tests": tests,
}
print(json.dumps(result, indent=2, sort_keys=True))
if len(tests) != 11 or any(item["exit_code"] != 0 or len(item["output_lines"]) != 3 for item in tests):
    raise SystemExit("an exact v0.29 production regression did not run once and pass")
PY

# Take the final identity snapshot after every live capture and exact production
# regression, immediately before offline evidence derivation.
capture_processes "$proof_tmp/final-processes.json"
python3 - "$proof_tmp/initial-processes.json" "$proof_tmp/final-processes.json" "$$" \
  >"$results_dir/process-continuity.json" <<'PY'
import json, sys
initial = json.load(open(sys.argv[1], encoding="utf-8"))
final = json.load(open(sys.argv[2], encoding="utf-8"))
def identity(item):
    return [item[key] for key in ("label", "pid", "ppid", "start_token", "command")]
print(json.dumps({
    "schema": "inferlab.signer-handoff-process-continuity.v0.29",
    "proof_shell_pid": int(sys.argv[3]),
    "initial": initial,
    "final": final,
    "unchanged": [identity(item) for item in initial] == [identity(item) for item in final],
}, indent=2, sort_keys=True))
PY

python3 - "$proof_tmp" "$case_dir" "$bundle_dir" "$pki_dir" "$policy_dir" "$project_root" \
  >"$results_dir/discarded-log-scan.json" <<'PY'
import base64, hashlib, json, os, re, sys, urllib.parse
from pathlib import Path
raw_root, raw_cases, raw_bundles, raw_pki, raw_policies, raw_project = sys.argv[1:]
root, cases, bundles, pki, policies, project = map(
    Path, (raw_root, raw_cases, raw_bundles, raw_pki, raw_policies, raw_project)
)
runtime = ["control-a.log", "control-b.log", "control-c.log", "cpu-worker.log", "gateway.log", "trust-distributor.log"]
startup_scenarios = [
    "missing", "malformed", "oversize", "unsafe-permissions", "non-regular",
    "symlink", "wrong-cluster", "wrong-service", "unknown-active",
]
entries = [(name, root / name) for name in runtime]
entries += [(f"startup-{name}.log", cases / f"startup-{name}" / "process.log") for name in startup_scenarios]
entries += [(f"production-{index}.log", root / f"production-{index}.log") for index in range(11)]
startup_sources = [
    path
    for name in startup_scenarios
    for path in (
        cases / f"startup-{name}" / "bundle.json",
        cases / f"startup-{name}" / "bundle.json.target",
    )
]
seed_labels = [
    "v029-service-trust-root", "v029-route-signing", "v029-control-writer",
    *[f"v029-{service}-{credential}" for service in ["control-a", "control-b", "control-c", "gateway-primary"] for credential in ["key-a", "key-b"]],
]
seed_values = []
for label in seed_labels:
    padded = base64.b64encode(hashlib.sha256(label.encode()).digest()).decode()
    seed_values.extend([
        padded, padded.rstrip("="), urllib.parse.quote(padded, safe=""),
        hashlib.sha256(padded.encode()).hexdigest(),
    ])
prompts = ["v029-final-json-private-proof-prompt", "v029-final-sse-private-proof-prompt"]
private = [
    "-----BEGIN PRIVATE KEY-----", "-----BEGIN RSA PRIVATE KEY-----",
    "-----BEGIN EC PRIVATE KEY-----", "PRIVATE_KEY_B64", "PRIVATE_KEY_BASE64",
]
host_path = re.compile(r"(?:/Users/|/home/|/tmp/|/private/var/|/var/folders/|/workspace/|/github/workspace)")
host_tags = [
    ("/Users/", "users"),
    ("/home/", "home"),
    ("/tmp/", "tmp"),
    ("/private/var/", "private-var"),
    ("/var/folders/", "var-folders"),
    ("/github/workspace", "github"),
    ("/workspace/", "workspace"),
]
field_tags = [
    "data_directory", "cache_path", "floor_path", "routing_snapshot_path", "state_path",
]
ansi_sgr = re.compile(r"\x1b\[[0-9;:]*m")
def decolor(text):
    return ansi_sgr.sub("", text)
def path_variants(path, raw=None):
    path_text = str(path)
    raw_text = path_text if raw is None else raw
    return {
        raw_text,
        path_text,
        os.path.normpath(raw_text),
        os.path.normpath(path_text),
        str(path.resolve(strict=False)),
    }
def replace_path_token(text, value, replacement):
    pattern = re.compile(
        r"(^|[\s=\"'`,(\[{])((?:\x1b\[[0-9;:]*m)*)" + re.escape(value)
        + r"(?=$|/|[\s\"'`,)\]}]|\x1b)",
        re.MULTILINE,
    )
    return pattern.sub(
        lambda match: match.group(1) + match.group(2) + replacement,
        text,
    )
proof_root_variants = path_variants(root, raw_root)
raw_root_parent = raw_root.rsplit("/", 1)[0]
proof_parent_variants = path_variants(root.parent, raw_root_parent)
project_variants = path_variants(project, raw_project)
sensitive_path_variants = set()
for value in startup_sources:
    sensitive_path_variants.update(path_variants(value))
for value, raw_value in (
    (cases, raw_cases),
    (bundles, raw_bundles),
    (pki, raw_pki),
    (policies, raw_policies),
):
    sensitive_path_variants.update(path_variants(value, raw_value))
for owned in proof_root_variants:
    sample = f"state_path={owned}/state.json"
    for value in sorted(proof_root_variants, key=len, reverse=True):
        sample = replace_path_token(sample, value, "<proof-root>")
    if host_path.search(sample):
        raise SystemExit("discarded log owned-path normalization self-test failed")
resolved_root = str(root.resolve(strict=False))
normalized_root = os.path.normpath(str(root))
ansi_owned = (
    f"field\x1b[0m\x1b[2m=\x1b[38:5:1m{resolved_root}/state.json\x1b[0m"
)
ansi_owned = decolor(ansi_owned)
for value in sorted(proof_root_variants, key=len, reverse=True):
    ansi_owned = replace_path_token(ansi_owned, value, "<proof-root>")
if host_path.search(ansi_owned):
    raise SystemExit("discarded log ANSI path-normalization self-test failed")
for hostile in (
    f"state_path={resolved_root}-outside/secret.txt",
    f"state_path=/prefix{resolved_root}/secret.txt",
    f"state_path={normalized_root}-outside/secret.txt",
    f"state_path=/prefix{normalized_root}/secret.txt",
    f"field\x1b[0m\x1b[2m=\x1b[0m{resolved_root}-outside/secret.txt",
):
    sample = decolor(hostile)
    for value in sorted(proof_root_variants, key=len, reverse=True):
        sample = replace_path_token(sample, value, "<proof-root>")
    if not host_path.search(sample):
        raise SystemExit("discarded log path-boundary self-test failed")
if not any(value in decolor("PRIVATE_\x1b[0mKEY_B64") for value in private):
    raise SystemExit("discarded log ANSI secret self-test failed")
if not any(value in decolor("PRIVATE_\x1b[38:5:1mKEY_B64") for value in private):
    raise SystemExit("discarded log colon-SGR secret self-test failed")
matches = []
for name, path in entries:
    text = decolor(path.read_text(encoding="utf-8", errors="replace"))
    if any(value in text for value in seed_values): matches.append(f"{name}:deterministic-seed")
    if any(value in text for value in prompts): matches.append(f"{name}:fixed-prompt")
    if any(value in text for value in private): matches.append(f"{name}:private-marker")
    if any(value in text for value in sensitive_path_variants):
        matches.append(f"{name}:sensitive-source-path")
    if any(value in text for value in project_variants):
        matches.append(f"{name}:project-path")
    normalized = text
    for value in sorted(proof_root_variants, key=len, reverse=True):
        normalized = replace_path_token(normalized, value, "<proof-root>")
    for value in sorted(project_variants, key=len, reverse=True):
        normalized = replace_path_token(normalized, value, "<project-root>")
    for line in normalized.splitlines():
        if not host_path.search(line):
            continue
        host_tag = next(tag for literal, tag in host_tags if literal in line)
        field_tag = next((field for field in field_tags if field in line), "unknown")
        ownership_tag = (
            "owned-substring"
            if any(value in line for value in proof_root_variants)
            else "not-owned-substring"
        )
        basename_tag = (
            "proof-basename-present" if root.name in line else "proof-basename-absent"
        )
        parent_tag = (
            "proof-parent-present"
            if any(value in line for value in proof_parent_variants)
            else "proof-parent-absent"
        )
        matches.append(
            f"{name}:{host_tag}:{field_tag}:{ownership_tag}:{basename_tag}:{parent_tag}"
        )
matches = sorted(set(matches))
result = {
    "schema": "inferlab.signer-handoff-discarded-log-scan.v0.29",
    "files_scanned": sorted(name for name, _ in entries),
    "checks": [
        "deterministic-seeds", "fixed-prompts", "sensitive-source-paths", "project-paths",
        "unexpected-host-paths", "private-markers",
    ],
    "matches": matches,
    "passed": not matches,
}
print(json.dumps(result, indent=2, sort_keys=True))
if matches:
    raise SystemExit(
        "discarded log scan found private proof material: " + ",".join(matches)
    )
PY

python3 benchmarks/signer_handoff_probe.py sanitize-evidence --evidence-dir "$results_dir" \
  --proof-root "$proof_tmp" --project-root "$project_root" >"$proof_tmp/sanitizer.json"
mv "$proof_tmp/sanitizer.json" "$results_dir/sanitizer.json"

# Deterministic placeholders make the private-scan inventory complete on its
# first pass. They are replaced before any renderer/replay/manifest gate.
printf '{}\n' >"$results_dir/assertions.json"
printf '<svg xmlns="http://www.w3.org/2000/svg"/>\n' >"$results_dir/signer-handoff-proof.svg"

scan_private_material() {
  python3 - "$results_dir" >"$1" <<'PY'
import base64, hashlib, json, sys, urllib.parse
from pathlib import Path
directory = Path(sys.argv[1])
labels = [
    "v029-service-trust-root", "v029-route-signing", "v029-control-writer",
    *[f"v029-{service}-{credential}" for service in ["control-a", "control-b", "control-c", "gateway-primary"] for credential in ["key-a", "key-b"]],
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
        text = path.read_text(encoding="utf-8", errors="replace")
        for index, representation in enumerate(representations):
            if representation in text:
                matches.append({"file": path.name, "seed_label": label, "representation": index})
result = {
    "schema": "inferlab.signer-handoff-private-material-scan.v0.29",
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
if matches: raise SystemExit("deterministic private material entered retained evidence")
PY
}

scan_private_material "$results_dir/private-material-scan.json"
python3 benchmarks/check_signer_handoff.py --evidence-dir "$results_dir" \
  --output "$results_dir/assertions.json"
python3 benchmarks/render_signer_handoff_svg.py --evidence-dir "$results_dir" \
  --output "$results_dir/signer-handoff-proof.svg"

# Stabilize the derived-file inventory before byte replay. The second and third
# passes are exact fixed points: private scan, checker and renderer bytes must
# stop changing once assertions/SVG themselves are included.
scan_private_material "$results_dir/private-material-scan.json"
python3 benchmarks/check_signer_handoff.py --evidence-dir "$results_dir" \
  --output "$results_dir/assertions.json"
python3 benchmarks/render_signer_handoff_svg.py --evidence-dir "$results_dir" \
  --output "$results_dir/signer-handoff-proof.svg"
scan_private_material "$results_dir/private-material-scan.json"
python3 benchmarks/check_signer_handoff.py --evidence-dir "$results_dir" \
  --output "$proof_tmp/replay-assertions.json"
cmp "$results_dir/assertions.json" "$proof_tmp/replay-assertions.json"
python3 benchmarks/render_signer_handoff_svg.py --evidence-dir "$results_dir" \
  --output "$proof_tmp/replay-signer-handoff-proof.svg"
cmp "$results_dir/signer-handoff-proof.svg" "$proof_tmp/replay-signer-handoff-proof.svg"

expected_files=(
  assertions.json
  discarded-log-scan.json
  final-cluster.json
  final-gateway.json
  final-json.json
  final-sse.json
  gateway-r2.json
  generation-1-after-handoff.json
  generation-1-controls.json
  generation-1-receipts.json
  generation-2-controls.json
  generation-2-receipts.json
  handoff-sequence.json
  live-source-rejections.json
  manifest.json
  private-material-scan.json
  process-continuity.json
  production-tests.json
  proof-contract.json
  publish-g1.json
  publish-g2.json
  r2-write.json
  r3-write.json
  revoked-a-attacks.json
  sanitizer.json
  signer-handoff-proof.svg
  startup-rejections.json
  trust-generations.json
)

write_manifest() {
  python3 - "$results_dir" "${expected_files[@]}" <<'PY'
import hashlib, json, sys
from pathlib import Path
directory = Path(sys.argv[1])
expected = sys.argv[2:]
if expected != sorted(expected) or len(expected) != len(set(expected)):
    raise SystemExit("manifest input inventory must be sorted and unique")
entries = list(directory.iterdir())
if (
    {path.name for path in entries} != set(expected) - {"manifest.json"}
    or any(not path.is_file() or path.is_symlink() for path in entries)
):
    raise SystemExit("pre-manifest result inventory mismatch")
entries = []
for name in expected:
    if name == "manifest.json":
        continue
    raw = (directory / name).read_bytes()
    entries.append({"name": name, "bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()})
manifest = {
    "schema": "inferlab.signer-handoff-manifest.v0.29",
    "file_count": len(entries),
    "files": entries,
}
(directory / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
PY
}

retain_results() {
  [[ -z "${INFERLAB_V29_OUTPUT_DIR:-}" ]] && return
  local name
  for name in "${expected_files[@]}"; do
    [[ "$name" == manifest.json ]] && continue
    cp "$results_dir/$name" "$INFERLAB_V29_OUTPUT_DIR/$name"
  done
  cp "$results_dir/manifest.json" "$INFERLAB_V29_OUTPUT_DIR/manifest.json"
}

write_manifest
python3 benchmarks/check_signer_handoff.py --evidence-dir "$results_dir" --require-manifest \
  --output "$proof_tmp/post-manifest-assertions.json"
cmp "$results_dir/assertions.json" "$proof_tmp/post-manifest-assertions.json"
python3 benchmarks/render_signer_handoff_svg.py --evidence-dir "$results_dir" \
  --output "$proof_tmp/post-manifest-signer-handoff-proof.svg"
cmp "$results_dir/signer-handoff-proof.svg" "$proof_tmp/post-manifest-signer-handoff-proof.svg"
retain_results
if [[ -n "${INFERLAB_V29_OUTPUT_DIR:-}" ]]; then
  python3 benchmarks/check_signer_handoff.py --evidence-dir "$INFERLAB_V29_OUTPUT_DIR" \
    --require-manifest --output "$proof_tmp/retained-assertions.json"
  cmp "$results_dir/assertions.json" "$proof_tmp/retained-assertions.json"
  python3 benchmarks/render_signer_handoff_svg.py --evidence-dir "$INFERLAB_V29_OUTPUT_DIR" \
    --output "$proof_tmp/retained-signer-handoff-proof.svg"
  cmp "$results_dir/signer-handoff-proof.svg" "$proof_tmp/retained-signer-handoff-proof.svg"
fi

python3 - "$results_dir/assertions.json" <<'PY'
import json, sys
report = json.load(open(sys.argv[1], encoding="utf-8"))
print(f"v0.29 restart-free signer handoff proof complete: {report['passed']}/{report['total']} assertions passed")
PY
