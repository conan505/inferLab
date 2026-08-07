#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

if [[ -x "$project_root/.tools/cargo/bin/cargo" ]]; then
  export RUSTUP_HOME="$project_root/.tools/rustup"
  export CARGO_HOME="$project_root/.tools/cargo"
  export PATH="$CARGO_HOME/bin:$PATH"
fi

unset INFERLAB_PUBLIC_API_KEYS || true
umask 077

# Apple's system Python may be linked to an old LibreSSL without TLS 1.3.
# Select a TLS-1.3-capable interpreter explicitly while keeping CI portable.
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
  echo 'v0.24 proof requires Python linked to a TLS 1.3-capable OpenSSL' >&2
  exit 1
fi
python3() {
  "$proof_python" "$@"
}

proof_tmp_root="${TMPDIR:-/tmp}"
proof_tmp="$(mktemp -d "$proof_tmp_root/inferlab-v024.XXXXXX")"
results_dir="$proof_tmp/results"
policy_dir="$proof_tmp/policies"
pki_dir="$proof_tmp/pki"
mkdir -p "$results_dir" "$policy_dir" "$pki_dir"

live_pids=()
node_a_pid=''
node_b_pid=''
node_c_pid=''
gateway_pid=''
distributor_pid=''

cluster_id='inferlab-primary'
urls='http://127.0.0.1:9951,http://127.0.0.1:9952,http://127.0.0.1:9953'
gateway_url='http://127.0.0.1:9950'
distributor_url='https://localhost:9955'
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
gateway_seed='AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA='

forget_pid() {
  local forgotten="$1"
  local pid
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
  local attempt state
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

cleanup() {
  local owned_pids=("${live_pids[@]}")
  local pid
  for pid in "${owned_pids[@]}"; do
    shutdown_child "$pid"
    forget_pid "$pid"
  done
  if [[ -n "$proof_tmp" && -d "$proof_tmp" ]]; then
    case "$(basename "$proof_tmp")" in
      inferlab-v024.*) rm -rf -- "$proof_tmp" ;;
      *) echo "refusing to remove unexpected proof path: $proof_tmp" >&2 ;;
    esac
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

prepare_output_dir() {
  if [[ -z "${INFERLAB_V24_OUTPUT_DIR:-}" ]]; then
    return
  fi
  mkdir -p "$INFERLAB_V24_OUTPUT_DIR"
  if find "$INFERLAB_V24_OUTPUT_DIR" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
    echo 'INFERLAB_V24_OUTPUT_DIR must be empty so stale evidence cannot survive' >&2
    exit 1
  fi
}

prepare_output_dir

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
            raise SystemExit(f"refusing v0.24 proof: 127.0.0.1:{port} is busy: {error}")
PY
}

wait_for_health() {
  local url="$1"
  local deadline=$((SECONDS + 60))
  until curl --connect-timeout 0.05 --max-time 0.10 --fail --silent "$url" >/dev/null; do
    if ((SECONDS >= deadline)); then
      echo "timed out waiting for $url" >&2
      return 1
    fi
    sleep 0.05
  done
}

wait_for_mtls_health() {
  local deadline=$((SECONDS + 60))
  until curl --connect-timeout 0.05 --max-time 0.15 --fail --silent \
    --cacert "$pki_dir/ca.crt" \
    --cert "$pki_dir/publisher.crt" \
    --key "$pki_dir/publisher.key" \
    "$distributor_url/health" >/dev/null; do
    if [[ -n "$distributor_pid" ]] && ! kill -0 "$distributor_pid" 2>/dev/null; then
      echo 'mTLS trust distributor exited before becoming healthy' >&2
      return 1
    fi
    if ((SECONDS >= deadline)); then
      echo 'timed out waiting for the mTLS trust distributor' >&2
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
print(int(time.time() * 1000))
PY
}

service_seed() {
  case "$1/$2" in
    node-a/key-a) printf '%s' "$node_a_seed" ;;
    node-b/key-a) printf '%s' "$node_b_seed" ;;
    node-c/key-a) printf '%s' "$node_c_seed" ;;
    gateway-primary/key-a) printf '%s' "$gateway_seed" ;;
    *) return 1 ;;
  esac
}

public_key() {
  local service_id="$1" credential_id="$2" seed="$3"
  INFERLAB_SERVICE_ID="$service_id" \
  INFERLAB_SERVICE_CREDENTIAL_ID="$credential_id" \
  INFERLAB_SERVICE_PRIVATE_KEY_B64="$seed" \
    target/debug/service_public_key
}

node_port() {
  case "$1" in
    node-a) printf '9951' ;;
    node-b) printf '9952' ;;
    node-c) printf '9953' ;;
    *) return 1 ;;
  esac
}

node_peers() {
  case "$1" in
    node-a) printf 'node-b=http://127.0.0.1:9952,node-c=http://127.0.0.1:9953' ;;
    node-b) printf 'node-a=http://127.0.0.1:9951,node-c=http://127.0.0.1:9953' ;;
    node-c) printf 'node-a=http://127.0.0.1:9951,node-b=http://127.0.0.1:9952' ;;
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
CN = InferLab v0.24 private proof CA
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
for name in ["publisher", "node-a", "node-b", "node-c", "rogue-client"]:
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
  openssl req -x509 -newkey rsa:2048 -nodes -days 1 -sha256 \
    -keyout "$pki_dir/rogue-ca.key" -out "$pki_dir/rogue-ca.crt" \
    -config "$pki_dir/ca.cnf" >/dev/null 2>&1

  local leaf
  for leaf in server publisher node-a node-b node-c; do
    openssl req -new -newkey rsa:2048 -nodes \
      -keyout "$pki_dir/$leaf.key" -out "$pki_dir/$leaf.csr" \
      -config "$pki_dir/$leaf.cnf" >/dev/null 2>&1
    openssl x509 -req -days 1 -sha256 \
      -in "$pki_dir/$leaf.csr" -CA "$pki_dir/ca.crt" -CAkey "$pki_dir/ca.key" \
      -CAcreateserial -out "$pki_dir/$leaf.crt" \
      -extfile "$pki_dir/$leaf.cnf" -extensions leaf_ext >/dev/null 2>&1
  done
  openssl req -new -newkey rsa:2048 -nodes \
    -keyout "$pki_dir/rogue-client.key" -out "$pki_dir/rogue-client.csr" \
    -config "$pki_dir/rogue-client.cnf" >/dev/null 2>&1
  openssl x509 -req -days 1 -sha256 \
    -in "$pki_dir/rogue-client.csr" -CA "$pki_dir/rogue-ca.crt" -CAkey "$pki_dir/rogue-ca.key" \
    -CAcreateserial -out "$pki_dir/rogue-client.crt" \
    -extfile "$pki_dir/rogue-client.cnf" -extensions leaf_ext >/dev/null 2>&1
}

start_distributor() {
  INFERLAB_TRUST_DISTRIBUTOR_BIND='127.0.0.1:9955' \
  INFERLAB_TRUST_DISTRIBUTOR_CLUSTER_ID="$cluster_id" \
  INFERLAB_SERVICE_TRUST_ROOT_KEYS="$trust_root_id=$trust_root_public" \
  INFERLAB_SERVICE_TRUST_REVOKED_ROOT_KEY_IDS='' \
  INFERLAB_TRUST_DISTRIBUTOR_STATE_PATH="$proof_tmp/distributor-state.json" \
  INFERLAB_TRUST_DISTRIBUTOR_EXPECTED_RECEIVERS='node-a/key-a,node-b/key-a,node-c/key-a' \
  INFERLAB_TRUST_DISTRIBUTOR_MAX_BODY_BYTES=262144 \
  INFERLAB_TRUST_DISTRIBUTOR_TLS_CERT_PATH="$pki_dir/server.crt" \
  INFERLAB_TRUST_DISTRIBUTOR_TLS_KEY_PATH="$pki_dir/server.key" \
  INFERLAB_TRUST_DISTRIBUTOR_TLS_CLIENT_CA_PATH="$pki_dir/ca.crt" \
    target/debug/trust-distributor >"$proof_tmp/distributor.log" 2>&1 &
  distributor_pid="$!"
  live_pids+=("$distributor_pid")
  wait_for_mtls_health
}

stop_distributor() {
  if [[ -n "$distributor_pid" ]]; then
    local stopped_pid="$distributor_pid"
    shutdown_child "$stopped_pid"
    forget_pid "$stopped_pid"
    distributor_pid=''
  fi
}

start_node() {
  local node_id="$1"
  local port peers election_min election_max seed
  port="$(node_port "$node_id")"
  peers="$(node_peers "$node_id")"
  election_min="$(node_election_min "$node_id")"
  election_max="$((election_min + 60))"
  seed="$(service_seed "$node_id" key-a)"
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
  INFERLAB_SERVICE_TRUST_DISTRIBUTOR_URL="$distributor_url" \
  INFERLAB_SERVICE_TRUST_CACHE_PATH="$proof_tmp/$node_id/service-trust-cache.json" \
  INFERLAB_SERVICE_TRUST_STATE_PATH="$proof_tmp/$node_id/service-trust-floor.json" \
  INFERLAB_SERVICE_TRUST_ROOT_KEYS="$trust_root_id=$trust_root_public" \
  INFERLAB_SERVICE_TRUST_REVOKED_ROOT_KEY_IDS='' \
  INFERLAB_SERVICE_TRUST_TLS_CA_CERT_PATH="$pki_dir/ca.crt" \
  INFERLAB_SERVICE_TRUST_TLS_CLIENT_CERT_PATH="$pki_dir/$node_id.crt" \
  INFERLAB_SERVICE_TRUST_TLS_CLIENT_KEY_PATH="$pki_dir/$node_id.key" \
  INFERLAB_SERVICE_TRUST_POLL_MS=25 \
  INFERLAB_SERVICE_TRUST_REQUEST_TIMEOUT_MS=1000 \
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
  local node_id="$1" pid
  pid="$(get_node_pid "$node_id")"
  if [[ -n "$pid" ]]; then
    shutdown_child "$pid"
    forget_pid "$pid"
    set_node_pid "$node_id" ''
  fi
}

sign_policy() {
  local policy="$1" output="$2"
  INFERLAB_SERVICE_TRUST_ROOT_KEY_ID="$trust_root_id" \
  INFERLAB_SERVICE_TRUST_ROOT_PRIVATE_KEY_B64="$trust_root_seed" \
    target/debug/sign_service_trust "$policy" >"$output"
}

post_document() {
  local endpoint="$1" body="$2" status="$3" output="$4"
  python3 benchmarks/mtls_trust_probe.py post \
    --url "$distributor_url$endpoint" --body "$body" --status "$status" \
    --ca-cert "$pki_dir/ca.crt" \
    --client-cert "$pki_dir/publisher.crt" \
    --client-key "$pki_dir/publisher.key" >"$output"
}

capture_distributor() {
  local output="$1"
  python3 benchmarks/mtls_trust_probe.py capture \
    --url "$distributor_url/v1/service-trust/status" \
    --ca-cert "$pki_dir/ca.crt" \
    --client-cert "$pki_dir/publisher.crt" \
    --client-key "$pki_dir/publisher.key" >"$output"
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
for node in ["node-a", "node-b", "node-c"]:
    nodes[node] = {}
    for role, name in [("cache", "service-trust-cache.json"), ("floor", "service-trust-floor.json")]:
        content = (root / node / name).read_bytes()
        nodes[node][f"{role}_sha256"] = hashlib.sha256(content).hexdigest()
print(json.dumps({"schema": "inferlab.mtls-durable-state.v0.24", "nodes": nodes}, indent=2, sort_keys=True))
PY
}

scan_private_material() {
  local output="$1"
  python3 - "$results_dir" "$pki_dir" \
    "$route_seed" "$writer_seed" "$trust_root_seed" \
    "$node_a_seed" "$node_b_seed" "$node_c_seed" "$gateway_seed" \
    >"$output" <<'PY'
import json
import sys
from pathlib import Path

evidence = Path(sys.argv[1])
pki = Path(sys.argv[2])
seed_labels = [
    "route_seed",
    "writer_seed",
    "trust_root_seed",
    "node_a_seed",
    "node_b_seed",
    "node_c_seed",
    "gateway_seed",
]
seed_values = sys.argv[3:]

def compact(value: str) -> str:
    return "".join(value.replace("\\n", "").replace("\\r", "").split())

candidates = []
for label, value in zip(seed_labels, seed_values):
    normalized = compact(value)
    candidates.append((label, normalized))
    candidates.append((f"{label}_unpadded", normalized.rstrip("=")))

key_files = sorted(pki.glob("*.key"))
for path in key_files:
    lines = [
        line.strip()
        for line in path.read_text(encoding="utf-8").splitlines()
        if not line.startswith("-----")
    ]
    payload = compact("".join(lines))
    if not payload:
        raise SystemExit(f"generated PKI key payload is empty: {path.name}")
    candidates.append((f"pki:{path.name}", payload))
    candidates.append((f"pki:{path.name}:unpadded", payload.rstrip("=")))

files = sorted(
    path for path in evidence.iterdir() if path.suffix in {".json", ".svg"}
)
matches = []
for path in files:
    retained = compact(path.read_text(encoding="utf-8"))
    for label, candidate in candidates:
        if len(candidate) >= 32 and candidate in retained:
            matches.append({"file": path.name, "candidate_label": label})
if matches:
    raise SystemExit(
        "private material leaked into retained evidence: "
        + ", ".join(f"{item['candidate_label']} in {item['file']}" for item in matches)
    )

print(json.dumps({
    "schema": "inferlab.private-material-scan.v0.24",
    "files_scanned": [path.name for path in files],
    "known_ed25519_seed_labels_scanned": seed_labels,
    "known_ed25519_seed_count": len(seed_labels),
    "generated_pki_private_key_files_scanned": [path.name for path in key_files],
    "generated_pki_private_key_count": len(key_files),
    "normalized_base64_and_escaped_newlines": True,
    "matches": 0,
}, indent=2, sort_keys=True))
PY
}

start_worker() {
  INFERLAB_CPU_WORKER_ID=cpu-mtls-trust \
  INFERLAB_CPU_BIND=127.0.0.1:9954 \
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
  wait_for_health 'http://127.0.0.1:9954/health'
}

start_gateway() {
  INFERLAB_BIND=127.0.0.1:9950 \
  INFERLAB_CONTROL_PLANE_URLS="$urls" \
  INFERLAB_CONTROL_CLUSTER_ID="$cluster_id" \
  INFERLAB_CONTROL_TRUSTED_KEYS="$route_key_id=$route_public" \
  INFERLAB_CONTROL_BOOTSTRAP_WAIT_MS=1500 \
  INFERLAB_CONTROL_POLL_MS=50 \
  INFERLAB_GATEWAY_SERVICE_ID='gateway-primary' \
  INFERLAB_GATEWAY_SERVICE_CREDENTIAL_ID='key-a' \
  INFERLAB_GATEWAY_SERVICE_PRIVATE_KEY_B64="$gateway_seed" \
  INFERLAB_CONTROL_SERVICE_TARGETS='node-a=http://127.0.0.1:9951,node-b=http://127.0.0.1:9952,node-c=http://127.0.0.1:9953' \
  INFERLAB_ROUTING_SNAPSHOT_PATH="$proof_tmp/gateway-routing.json" \
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

check_ports_are_free 9950 9951 9952 9953 9954 9955
command -v openssl >/dev/null
cargo build --workspace --bins --quiet
generate_pki

trust_root_public="$(
  INFERLAB_SERVICE_TRUST_ROOT_KEY_ID="$trust_root_id" \
  INFERLAB_SERVICE_TRUST_ROOT_PRIVATE_KEY_B64="$trust_root_seed" \
    target/debug/service_trust_public_key
)"
node_a_public="$(public_key node-a key-a "$node_a_seed")"
node_b_public="$(public_key node-b key-a "$node_b_seed")"
node_c_public="$(public_key node-c key-a "$node_c_seed")"
gateway_public="$(public_key gateway-primary key-a "$gateway_seed")"

issued_at="$(now_ms)"
python3 - "$issued_at" "$node_a_public" "$node_b_public" "$node_c_public" \
  "$gateway_public" "$policy_dir" <<'PY'
import json
import sys
from pathlib import Path

issued = int(sys.argv[1])
node_a, node_b, node_c, gateway = sys.argv[2:6]
directory = Path(sys.argv[6])
credentials = [
    {"service_id": "node-a", "credential_id": "key-a", "public_key_base64": node_a},
    {"service_id": "node-b", "credential_id": "key-a", "public_key_base64": node_b},
    {"service_id": "node-c", "credential_id": "key-a", "public_key_base64": node_c},
    {"service_id": "gateway-primary", "credential_id": "key-a", "public_key_base64": gateway},
]
for name, generation in [("g1", 1), ("g2", 2)]:
    payload = {
        "schema": "inferlab.service-trust-policy.v1",
        "cluster_id": "inferlab-primary",
        "generation": generation,
        "issued_at_ms": issued + generation,
        "trusted_credentials": credentials,
        "revoked_service_ids": [],
        "revoked_credentials": [],
        "gateway_service_ids": ["gateway-primary"],
    }
    (directory / f"policy-{name}.json").write_text(
        json.dumps(payload, indent=2) + "\n", encoding="utf-8"
    )
PY
sign_policy "$policy_dir/policy-g1.json" "$policy_dir/snapshot-g1.json"
sign_policy "$policy_dir/policy-g2.json" "$policy_dir/snapshot-g2.json"
python3 - "$policy_dir/snapshot-g2.json" "$policy_dir/snapshot-tampered.json" <<'PY'
import json
import sys
from pathlib import Path

snapshot = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
snapshot["generation"] = 9
Path(sys.argv[2]).write_text(json.dumps(snapshot, indent=2) + "\n", encoding="utf-8")
PY

start_distributor
python3 benchmarks/mtls_trust_probe.py handshake \
  --host 127.0.0.1 --port 9955 --server-hostname localhost \
  --ca-cert "$pki_dir/ca.crt" \
  --client-cert "$pki_dir/publisher.crt" \
  --client-key "$pki_dir/publisher.key" >"$results_dir/tls-handshake.json"
post_document '/v1/service-trust/snapshot' "$policy_dir/snapshot-g1.json" 201 \
  "$results_dir/publish-g1.json"

start_node node-a
start_node node-b
start_node node-c
initial_node_a_pid="$node_a_pid"
initial_node_b_pid="$node_b_pid"
initial_node_c_pid="$node_c_pid"

python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" --timeout 5 >"$proof_tmp/initial-leader.json"
python3 benchmarks/mtls_trust_probe.py wait-controls \
  --urls "$urls" --generation 1 --bootstrap-source remote --timeout 5 \
  >"$results_dir/initial-controls.json"
python3 benchmarks/mtls_trust_probe.py wait-distributor \
  --url "$distributor_url" --generation 1 \
  --acked-receivers 'node-a/key-a,node-b/key-a,node-c/key-a' --timeout 5 \
  --ca-cert "$pki_dir/ca.crt" \
  --client-cert "$pki_dir/publisher.crt" \
  --client-key "$pki_dir/publisher.key" \
  >"$results_dir/generation-1-receipts.json"

python3 - "$proof_tmp/config.json" <<'PY'
import json
import sys
from pathlib import Path

Path(sys.argv[1]).write_text(json.dumps({
    "routing_policy": "round-robin",
    "workers": [{"id": "cpu-mtls-trust", "base_url": "http://127.0.0.1:9954", "weight": 1}],
}, indent=2) + "\n", encoding="utf-8")
PY
leader_url="$(json_field "$proof_tmp/initial-leader.json" leader_url)"
INFERLAB_CONTROL_WRITER_ID="$writer_id" \
INFERLAB_CONTROL_WRITER_PRIVATE_KEY_B64="$writer_seed" \
  target/debug/sign_control_write \
    "$cluster_id" 0 now mtls-trust-route-1 "$proof_tmp/config.json" \
    >"$proof_tmp/write.json"
python3 benchmarks/control_write_probe.py submit \
  --url "$leader_url" --body "$proof_tmp/write.json" \
  >"$results_dir/write-committed.json"
start_worker
start_gateway
python3 benchmarks/full_stack_probe.py wait-gateway \
  --gateway-url "$gateway_url" --policy round-robin --revision 2 \
  --worker-ids cpu-mtls-trust --timeout 5 \
  >"$results_dir/gateway-ready.json"

record_durable_state "$results_dir/state-before-transport-attacks.json"

# Each invalid transport is observed as a handshake/protocol failure before an
# HTTP status exists. No invalid peer can reach snapshot or receipt handlers.
python3 benchmarks/mtls_trust_probe.py expect-transport-failure \
  --scenario plaintext-downgrade --url 'http://127.0.0.1:9955/health' \
  --ca-cert "$pki_dir/ca.crt" >"$results_dir/plaintext-downgrade.json"
python3 benchmarks/mtls_trust_probe.py expect-transport-failure \
  --scenario no-client-certificate --url "$distributor_url/health" \
  --omit-client-certificate --ca-cert "$pki_dir/ca.crt" \
  >"$results_dir/no-client-certificate.json"
python3 benchmarks/mtls_trust_probe.py expect-transport-failure \
  --scenario rogue-client-ca --url "$distributor_url/health" \
  --ca-cert "$pki_dir/ca.crt" \
  --client-cert "$pki_dir/rogue-client.crt" \
  --client-key "$pki_dir/rogue-client.key" >"$results_dir/rogue-client-ca.json"
python3 benchmarks/mtls_trust_probe.py expect-transport-failure \
  --scenario wrong-server-ca --url "$distributor_url/health" \
  --ca-cert "$pki_dir/rogue-ca.crt" \
  --client-cert "$pki_dir/publisher.crt" \
  --client-key "$pki_dir/publisher.key" >"$results_dir/wrong-server-ca.json"
python3 benchmarks/mtls_trust_probe.py expect-transport-failure \
  --scenario wrong-server-hostname --url 'https://127.0.0.1:9955/health' \
  --ca-cert "$pki_dir/ca.crt" \
  --client-cert "$pki_dir/publisher.crt" \
  --client-key "$pki_dir/publisher.key" >"$results_dir/wrong-server-hostname.json"

record_durable_state "$results_dir/state-after-transport-attacks.json"
capture_distributor "$results_dir/post-transport-status.json"
python3 benchmarks/mtls_trust_probe.py wait-controls \
  --urls "$urls" --generation 1 --timeout 5 \
  >"$results_dir/after-transport-controls.json"

# Channel identity is necessary but does not replace root/service signatures.
post_document '/v1/service-trust/snapshot' "$policy_dir/snapshot-tampered.json" 400 \
  "$results_dir/tampered-snapshot.json"
python3 - "$results_dir/generation-1-receipts.json" "$proof_tmp/forged-receipt.json" <<'PY'
import json
import sys
from pathlib import Path

status = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))["status"]["body"]
receipt = status["receipts"][0]
receipt["applied_at_ms"] += 1
Path(sys.argv[2]).write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
PY
post_document '/v1/service-trust/receipts' "$proof_tmp/forged-receipt.json" 400 \
  "$results_dir/forged-receipt.json"
capture_distributor "$results_dir/post-application-attacks.json"

post_document '/v1/service-trust/snapshot' "$policy_dir/snapshot-g2.json" 201 \
  "$results_dir/publish-g2.json"
python3 benchmarks/mtls_trust_probe.py wait-controls \
  --urls "$urls" --generation 2 --timeout 5 \
  >"$results_dir/generation-2-convergence.json"
python3 benchmarks/mtls_trust_probe.py wait-distributor \
  --url "$distributor_url" --generation 2 \
  --acked-receivers 'node-a/key-a,node-b/key-a,node-c/key-a' --timeout 5 \
  --ca-cert "$pki_dir/ca.crt" \
  --client-cert "$pki_dir/publisher.crt" \
  --client-key "$pki_dir/publisher.key" \
  >"$results_dir/generation-2-receipts.json"
record_durable_state "$results_dir/state-after-generation-2.json"

python3 - "$initial_node_a_pid" "$initial_node_b_pid" "$initial_node_c_pid" \
  "$node_a_pid" "$node_b_pid" "$node_c_pid" \
  >"$results_dir/online-process-continuity.json" <<'PY'
import json
import sys

names = ["node-a", "node-b", "node-c"]
before = dict(zip(names, map(int, sys.argv[1:4])))
after = dict(zip(names, map(int, sys.argv[4:7])))
print(json.dumps({
    "schema": "inferlab.mtls-service-trust-process-continuity.v0.24",
    "before": before,
    "after_generation_2": after,
    "unchanged_before_cache_restart": before == after,
}, indent=2, sort_keys=True))
PY

restart_node="$(python3 - "$results_dir/generation-2-convergence.json" <<'PY'
import json
import sys

sample = json.load(open(sys.argv[1], encoding="utf-8"))
print(next(status["body"]["node_id"] for status in sample["statuses"] if status["body"]["role"] == "follower"))
PY
)"
old_restart_pid="$(get_node_pid "$restart_node")"
stop_node "$restart_node"
stopped_distributor_pid="$distributor_pid"
stop_distributor
if kill -0 "$stopped_distributor_pid" 2>/dev/null; then
  echo 'stopped distributor PID unexpectedly remains alive' >&2
  exit 1
fi
python3 benchmarks/mtls_trust_probe.py expect-transport-failure \
  --scenario distributor-stopped --url "$distributor_url/health" \
  --ca-cert "$pki_dir/ca.crt" \
  --client-cert "$pki_dir/publisher.crt" \
  --client-key "$pki_dir/publisher.key" >"$proof_tmp/distributor-outage-observation.json"
python3 - "$stopped_distributor_pid" "$proof_tmp/distributor-outage-observation.json" \
  >"$results_dir/distributor-outage.json" <<'PY'
import json
import sys

observation = json.load(open(sys.argv[2], encoding="utf-8"))
print(json.dumps({
    "schema": "inferlab.mtls-distributor-outage.v0.24",
    "stopped_distributor_pid": int(sys.argv[1]),
    "stopped_pid_alive": False,
    "connection_observation": observation,
}, indent=2, sort_keys=True))
PY
start_node "$restart_node"
new_restart_pid="$(get_node_pid "$restart_node")"
restart_url="http://127.0.0.1:$(node_port "$restart_node")"
python3 benchmarks/mtls_trust_probe.py wait-controls \
  --urls "$restart_url" --generation 2 --bootstrap-source cache \
  --minimum-receipt-failures 1 --timeout 5 \
  >"$results_dir/cache-restart-wait.json"
python3 - "$old_restart_pid" "$new_restart_pid" "$restart_node" \
  "$results_dir/cache-restart-wait.json" >"$results_dir/cache-restart.json" <<'PY'
import json
import sys

sample = json.load(open(sys.argv[4], encoding="utf-8"))
print(json.dumps({
    "schema": "inferlab.mtls-service-trust-cache-restart.v0.24",
    "node_id": sys.argv[3],
    "old_pid": int(sys.argv[1]),
    "new_pid": int(sys.argv[2]),
    "status": sample["statuses"][0],
}, indent=2, sort_keys=True))
PY

python3 benchmarks/full_stack_probe.py wait-leader \
  --urls "$urls" --timeout 5 >"$results_dir/final-cluster.json"
python3 benchmarks/full_stack_probe.py requests \
  --gateway-url "$gateway_url" --requests 1 \
  --prompt 'mutual TLS trust cache route' --speculative-tokens 2 \
  >"$results_dir/request.json"
python3 benchmarks/full_stack_probe.py stream \
  --gateway-url "$gateway_url" --prompt 'authenticated channel trust stream' \
  --speculative-tokens 2 >"$results_dir/stream.json"
python3 benchmarks/full_stack_probe.py wait-gateway \
  --gateway-url "$gateway_url" --policy round-robin --revision 2 \
  --worker-ids cpu-mtls-trust --timeout 5 \
  >"$results_dir/final-gateway.json"

python3 benchmarks/mtls_trust_probe.py sanitize-evidence \
  --evidence-dir "$results_dir" --proof-root "$proof_tmp" \
  >"$proof_tmp/evidence-sanitization.json"
mv "$proof_tmp/evidence-sanitization.json" "$results_dir/evidence-sanitization.json"
scan_private_material "$results_dir/private-material-scan.json"
python3 benchmarks/check_mtls_service_trust.py \
  --evidence-dir "$results_dir" >"$results_dir/assertions.json"
python3 benchmarks/render_mtls_service_trust_svg.py \
  --evidence-dir "$results_dir" \
  --output "$results_dir/mtls-service-trust-proof.svg"

# Repeat after assertions and SVG exist so the retained scan report covers the
# complete evidence bundle, not only the checker inputs.
scan_private_material "$proof_tmp/private-material-scan-final.json"
mv "$proof_tmp/private-material-scan-final.json" "$results_dir/private-material-scan.json"
python3 benchmarks/check_mtls_service_trust.py \
  --evidence-dir "$results_dir" >"$results_dir/assertions.json"
python3 benchmarks/render_mtls_service_trust_svg.py \
  --evidence-dir "$results_dir" \
  --output "$results_dir/mtls-service-trust-proof.svg"
# The retained report is now exactly the report embedded by assertions.json.
# One discarded final pass covers those rewritten assertions and SVG without
# creating a self-referential retained artifact.
scan_private_material "$proof_tmp/private-material-scan-complete.json"

python3 - "$results_dir" "$proof_tmp" <<'PY'
import re
import sys
from pathlib import Path

evidence = Path(sys.argv[1])
proof_root = sys.argv[2]
for path in sorted(evidence.iterdir()):
    if path.suffix not in {".json", ".svg"}:
        raise SystemExit(f"unexpected retained evidence type: {path.name}")
    text = path.read_text(encoding="utf-8")
    if proof_root in text or "/Users/" in text or "/var/folders/" in text or "/private/" in text:
        raise SystemExit(f"host path leaked into {path.name}")
    if re.search(r"-----BEGIN [^-]*(?:PRIVATE KEY|CERTIFICATE)-----", text):
        raise SystemExit(f"certificate or private-key PEM leaked into {path.name}")
PY

evidence_manifest=(
  after-transport-controls.json
  assertions.json
  cache-restart-wait.json
  cache-restart.json
  evidence-sanitization.json
  distributor-outage.json
  final-cluster.json
  final-gateway.json
  forged-receipt.json
  gateway-ready.json
  generation-1-receipts.json
  generation-2-convergence.json
  generation-2-receipts.json
  initial-controls.json
  mtls-service-trust-proof.svg
  no-client-certificate.json
  online-process-continuity.json
  plaintext-downgrade.json
  post-application-attacks.json
  post-transport-status.json
  private-material-scan.json
  publish-g1.json
  publish-g2.json
  request.json
  rogue-client-ca.json
  state-after-generation-2.json
  state-after-transport-attacks.json
  state-before-transport-attacks.json
  stream.json
  tampered-snapshot.json
  tls-handshake.json
  write-committed.json
  wrong-server-ca.json
  wrong-server-hostname.json
)
python3 - "$results_dir" "${evidence_manifest[@]}" <<'PY'
import sys
from pathlib import Path

directory = Path(sys.argv[1])
expected = set(sys.argv[2:])
actual = {path.name for path in directory.iterdir()}
if actual != expected:
    raise SystemExit(
        f"evidence manifest mismatch; missing={sorted(expected - actual)}, "
        f"unexpected={sorted(actual - expected)}"
    )
PY

if [[ -n "${INFERLAB_V24_OUTPUT_DIR:-}" ]]; then
  for evidence_name in "${evidence_manifest[@]}"; do
    cp "$results_dir/$evidence_name" "$INFERLAB_V24_OUTPUT_DIR/$evidence_name"
  done
fi
cat "$results_dir/assertions.json"
