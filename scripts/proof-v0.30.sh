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
  echo 'v0.30 proof requires Python with TLS 1.3 support' >&2
  exit 1
fi
python3() { "$proof_python" "$@"; }

proof_tmp_root="${TMPDIR:-/tmp}"
escaped_main_serial="${proof_tmp_root%/}/inferlab-v030-main.srl"
escaped_rogue_serial="${proof_tmp_root%/}/inferlab-v030-rogue.srl"
proof_tmp="$(mktemp -d "$proof_tmp_root/inferlab-v030.XXXXXX")"
results_dir="$proof_tmp/results"
pki_dir="$proof_tmp/pki"
bundle_dir="$proof_tmp/bundles"
policy_dir="$proof_tmp/policies"
case_dir="$proof_tmp/cases"
mkdir -p "$results_dir" "$pki_dir" "$bundle_dir" "$policy_dir" "$case_dir"

cluster_id='inferlab-primary'
gateway_url='http://127.0.0.1:12380'
control_urls='http://127.0.0.1:12381,http://127.0.0.1:12382,http://127.0.0.1:12383'
worker_url='http://127.0.0.1:12384'
distributor_url='https://localhost:12385'
route_key_id='route-v030'
writer_id='v030-deployer'
trust_root_id='service-trust-root-v030'
policy_lifetime_ms=600000

derive_seed() {
  python3 - "$1" <<'PY'
import base64, hashlib, sys
print(base64.b64encode(hashlib.sha256(sys.argv[1].encode()).digest()).decode())
PY
}
trust_root_seed="$(derive_seed v030-service-trust-root)"
route_seed="$(derive_seed v030-route-signing)"
writer_seed="$(derive_seed v030-control-writer)"
service_seed() { derive_seed "v030-service-$1"; }

live_pids=(sentinel)
control_a_pid=''
control_b_pid=''
control_c_pid=''
gateway_pid=''
worker_pid=''
distributor_pid=''
held_probe_pid=''

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
  if [[ "${INFERLAB_V30_KEEP_TMP:-0}" == 1 && "${proof_succeeded:-0}" != 1 ]]; then
    echo "retaining v0.30 proof temporary directory: $proof_tmp" >&2
  elif [[ -n "$proof_tmp" && -d "$proof_tmp" ]]; then
    case "$(basename "$proof_tmp")" in
      inferlab-v030.*) rm -rf -- "$proof_tmp" ;;
      *) echo "refusing unexpected proof cleanup path: $proof_tmp" >&2 ;;
    esac
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

prepare_output_dir() {
  [[ -z "${INFERLAB_V30_OUTPUT_DIR:-}" ]] && return
  mkdir -p "$INFERLAB_V30_OUTPUT_DIR"
  if find "$INFERLAB_V30_OUTPUT_DIR" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
    echo 'INFERLAB_V30_OUTPUT_DIR must be empty' >&2
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
            raise SystemExit(f"refusing v0.30 proof: 127.0.0.1:{port} busy: {error}")
PY
}

node_port() {
  case "$1" in control-a) echo 12381 ;; control-b) echo 12382 ;; control-c) echo 12383 ;; *) return 1 ;; esac
}
node_peers() {
  case "$1" in
    control-a) echo 'control-b=http://127.0.0.1:12382,control-c=http://127.0.0.1:12383' ;;
    control-b) echo 'control-a=http://127.0.0.1:12381,control-c=http://127.0.0.1:12383' ;;
    control-c) echo 'control-a=http://127.0.0.1:12381,control-b=http://127.0.0.1:12382' ;;
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

wait_endpoint() {
  local url="$1" pid="$2" label="$3" deadline=$((SECONDS + 60)) observed
  while true; do
    observed="$(curl --noproxy '*' --connect-timeout 0.1 --max-time 0.4 --silent \
      --output /dev/null --write-out '%{http_code}' "$url" 2>/dev/null || true)"
    [[ "$observed" == 200 ]] && return
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
  local deadline=$((SECONDS + 60)) observed
  while true; do
    observed="$(curl --noproxy '*' --connect-timeout 0.1 --max-time 0.5 --silent \
      --output /dev/null --write-out '%{http_code}' --cacert "$pki_dir/ca.crt" \
      --cert "$pki_dir/publisher-a.crt" --key "$pki_dir/publisher-a.key" \
      "$distributor_url/health" 2>/dev/null || true)"
    [[ "$observed" == 200 ]] && return
    is_owned_child "$distributor_pid" || { echo 'distributor exited before readiness' >&2; return 1; }
    ((SECONDS < deadline)) || { echo 'timeout waiting for distributor' >&2; return 1; }
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

write_bundle() {
  local path="$1" generation="$2" identity="$3" purpose="$4" leaf="$5" key="$6" ca="$7"
  local server_name="${8:-}" cluster="${9:-$cluster_id}" temporary
  temporary="$(dirname "$path")/.identity.$$.${generation}.${RANDOM}.tmp"
  python3 - "$temporary" "$generation" "$identity" "$purpose" "$leaf" "$key" "$ca" \
    "$server_name" "$cluster" <<'PY'
import json, sys
from pathlib import Path
output, generation, identity, purpose, leaf, key, ca, server_name, cluster = sys.argv[1:]
document = {
    "schema": "inferlab.tls-identity-bundle.v1",
    "cluster_id": cluster,
    "generation": int(generation),
    "identity_id": identity,
    "purpose": purpose,
    "certificate_chain_pem": Path(leaf).read_text(encoding="ascii"),
    "private_key_pem": Path(key).read_text(encoding="ascii"),
    "issuer_ca_pem": Path(ca).read_text(encoding="ascii"),
}
if server_name:
    document["server_name"] = server_name
Path(output).write_text(json.dumps(document, separators=(",", ":")) + "\n", encoding="utf-8")
PY
  chmod 0600 "$temporary"
  mv -f "$temporary" "$path"
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
CN = InferLab v0.30 disposable proof CA
[ca_ext]
basicConstraints = critical,CA:TRUE
keyUsage = critical,keyCertSign,cRLSign
subjectKeyIdentifier = hash
""", encoding="utf-8")
(d / "rogue-ca.cnf").write_text((d / "ca.cnf").read_text().replace("proof CA", "rogue proof CA"), encoding="utf-8")
def leaf(name, eku, san=None):
    san_line = f"subjectAltName = DNS:{san}\n" if san else ""
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
{san_line}""", encoding="utf-8")
for name in ["server-a", "server-b", "server-fork"]:
    leaf(name, "serverAuth", "localhost")
leaf("server-wrong-san", "serverAuth", "wrong.local")
leaf("server-wrong-eku", "clientAuth", "localhost")
for name in [
    "publisher-a", "publisher-b",
    "control-a-a", "control-a-b", "control-b-a", "control-b-b", "control-c-a", "control-c-b",
    "client-fork",
]:
    leaf(name, "clientAuth")
leaf("client-wrong-eku", "serverAuth", "localhost")
leaf("server-expired", "serverAuth", "localhost")
leaf("server-not-yet", "serverAuth", "localhost")
leaf("client-expired", "clientAuth")
leaf("client-not-yet", "clientAuth")
leaf("rogue-server", "serverAuth", "localhost")
leaf("rogue-client", "clientAuth")
(d / "ca-db.index").write_text("", encoding="ascii")
(d / "ca-db.serial").write_text("5000\n", encoding="ascii")
(d / "ca-db.cnf").write_text(f"""[ca]
default_ca = proof_ca
[proof_ca]
database = {d / 'ca-db.index'}
serial = {d / 'ca-db.serial'}
new_certs_dir = {d}
certificate = {d / 'ca.crt'}
private_key = {d / 'ca.key'}
default_md = sha256
policy = proof_policy
copy_extensions = copy
unique_subject = no
[proof_policy]
commonName = supplied
""", encoding="utf-8")
PY
  env -i PATH="$PATH" openssl req -x509 -newkey rsa:2048 -nodes -days 2 -sha256 \
    -keyout "$pki_dir/ca.key" -out "$pki_dir/ca.crt" -config "$pki_dir/ca.cnf" >/dev/null 2>&1
  env -i PATH="$PATH" openssl req -x509 -newkey rsa:2048 -nodes -days 2 -sha256 \
    -keyout "$pki_dir/rogue-ca.key" -out "$pki_dir/rogue-ca.crt" -config "$pki_dir/rogue-ca.cnf" >/dev/null 2>&1
  local leaf
  for leaf in server-a server-b server-fork server-wrong-san server-wrong-eku \
    publisher-a publisher-b control-a-a control-a-b control-b-a control-b-b control-c-a control-c-b \
    client-fork client-wrong-eku; do
    env -i PATH="$PATH" openssl req -new -newkey rsa:2048 -nodes \
      -keyout "$pki_dir/$leaf.key" -out "$pki_dir/$leaf.csr" -config "$pki_dir/$leaf.cnf" >/dev/null 2>&1
    env -i PATH="$PATH" openssl x509 -req -days 2 -sha256 -in "$pki_dir/$leaf.csr" \
      -CA "$pki_dir/ca.crt" -CAkey "$pki_dir/ca.key" -CAserial "$pki_dir/ca.srl" \
      -CAcreateserial -out "$pki_dir/$leaf.crt" -extfile "$pki_dir/$leaf.cnf" \
      -extensions leaf_ext >/dev/null 2>&1
  done
  for leaf in rogue-server rogue-client; do
    env -i PATH="$PATH" openssl req -new -newkey rsa:2048 -nodes \
      -keyout "$pki_dir/$leaf.key" -out "$pki_dir/$leaf.csr" -config "$pki_dir/$leaf.cnf" >/dev/null 2>&1
    env -i PATH="$PATH" openssl x509 -req -days 2 -sha256 -in "$pki_dir/$leaf.csr" \
      -CA "$pki_dir/rogue-ca.crt" -CAkey "$pki_dir/rogue-ca.key" \
      -CAserial "$pki_dir/rogue-ca.srl" -CAcreateserial -out "$pki_dir/$leaf.crt" \
      -extfile "$pki_dir/$leaf.cnf" -extensions leaf_ext >/dev/null 2>&1
  done
  for leaf in server-expired server-not-yet client-expired client-not-yet; do
    env -i PATH="$PATH" openssl req -new -newkey rsa:2048 -nodes \
      -keyout "$pki_dir/$leaf.key" -out "$pki_dir/$leaf.csr" -config "$pki_dir/$leaf.cnf" >/dev/null 2>&1
    if [[ "$leaf" == *expired ]]; then
      start='20000101000000Z'; end='20000102000000Z'
    else
      start='20990101000000Z'; end='20990102000000Z'
    fi
    env -i PATH="$PATH" openssl ca -batch -notext -config "$pki_dir/ca-db.cnf" \
      -startdate "$start" -enddate "$end" -in "$pki_dir/$leaf.csr" \
      -out "$pki_dir/$leaf.crt" -extfile "$pki_dir/$leaf.cnf" \
      -extensions leaf_ext >/dev/null 2>&1
  done
  python3 - "$pki_dir" "$escaped_main_serial" "$escaped_rogue_serial" <<'PY'
import os, re, stat, sys
from pathlib import Path
pki = Path(sys.argv[1]).resolve()
for name in ("ca.srl", "rogue-ca.srl", "ca-db.serial"):
    path = pki / name
    metadata = path.lstat()
    raw = path.read_bytes()
    if (
        not stat.S_ISREG(metadata.st_mode)
        or stat.S_ISLNK(metadata.st_mode)
        or stat.S_IMODE(metadata.st_mode) != 0o600
        or metadata.st_uid != os.getuid()
        or path.resolve().parent != pki
        or re.fullmatch(rb"[0-9A-Fa-f]+\n", raw) is None
    ):
        raise SystemExit(f"proof-owned OpenSSL serial failed containment: {name}")
for raw in sys.argv[2:]:
    escaped = Path(raw)
    if escaped.exists() or escaped.is_symlink():
        raise SystemExit("OpenSSL CA serial escaped the proof-owned PKI directory")
PY
}

certificate_fingerprint() {
  python3 - "$1" <<'PY'
import hashlib, ssl, sys
pem = open(sys.argv[1], encoding="ascii").read()
der = ssl.PEM_cert_to_DER_cert(pem)
print(hashlib.sha256(der).hexdigest())
PY
}

prepare_invalid_bundle() {
  local role="$1" scenario="$2" path="$3" generation="${4:-2}" candidate target
  local cluster="$cluster_id"
  candidate="$path.candidate.$$.$RANDOM"
  target="$path.target"
  rm -rf -- "$candidate" "$target"
  if [[ "$role" == server ]]; then
    identity='trust-distributor'; purpose='server'; leaf="$pki_dir/server-a.crt"; key="$pki_dir/server-a.key"; ca="$pki_dir/ca.crt"; name='localhost'
  else
    identity='control-a'; purpose='client'; leaf="$pki_dir/control-a-a.crt"; key="$pki_dir/control-a-a.key"; ca="$pki_dir/ca.crt"; name=''
  fi
  case "$scenario" in
    missing) rm -rf -- "$path"; return ;;
    malformed) printf '%s\n' '{not-json' >"$candidate"; chmod 0600 "$candidate"; mv -f "$candidate" "$path"; return ;;
    oversize) python3 - "$candidate" <<'PY'
import sys
open(sys.argv[1], "wb").write(b"x" * (512 * 1024 + 1))
PY
      chmod 0600 "$candidate"; mv -f "$candidate" "$path"; return ;;
    unsafe-permissions) write_bundle "$candidate" "$generation" "$identity" "$purpose" "$leaf" "$key" "$ca" "$name"; chmod 0644 "$candidate"; mv -f "$candidate" "$path"; return ;;
    symlink) write_bundle "$target" "$generation" "$identity" "$purpose" "$leaf" "$key" "$ca" "$name"; ln -s "$target" "$candidate"; mv -f "$candidate" "$path"; return ;;
    wrong-cluster) cluster='wrong-cluster' ;;
    wrong-identity) identity='wrong-identity' ;;
    wrong-purpose)
      if [[ "$role" == server ]]; then purpose='client'; name=''; leaf="$pki_dir/control-a-a.crt"; key="$pki_dir/control-a-a.key"
      else purpose='server'; name='localhost'; leaf="$pki_dir/server-a.crt"; key="$pki_dir/server-a.key"
      fi ;;
    wrong-server-name) name='wrong.local' ;;
    mismatched-key) key="$pki_dir/publisher-a.key" ;;
    expired) leaf="$pki_dir/$role-expired.crt"; key="$pki_dir/$role-expired.key" ;;
    not-yet-valid) leaf="$pki_dir/$role-not-yet.crt"; key="$pki_dir/$role-not-yet.key" ;;
    wrong-eku) leaf="$pki_dir/$role-wrong-eku.crt"; key="$pki_dir/$role-wrong-eku.key" ;;
    wrong-san) leaf="$pki_dir/server-wrong-san.crt"; key="$pki_dir/server-wrong-san.key" ;;
    wrong-ca)
      leaf="$pki_dir/rogue-$role.crt"; key="$pki_dir/rogue-$role.key" ;;
    issuer-ca-change)
      leaf="$pki_dir/rogue-$role.crt"; key="$pki_dir/rogue-$role.key"; ca="$pki_dir/rogue-ca.crt"; generation=$((generation + 1)) ;;
    stale) generation=1 ;;
    fork)
      if [[ "$role" == server ]]; then leaf="$pki_dir/server-fork.crt"; key="$pki_dir/server-fork.key"
      else leaf="$pki_dir/client-fork.crt"; key="$pki_dir/client-fork.key"
      fi ;;
    *) return 1 ;;
  esac
  write_bundle "$candidate" "$generation" "$identity" "$purpose" "$leaf" "$key" "$ca" "$name" "$cluster"
  mv -f "$candidate" "$path"
}

error_message() {
  case "$1" in
    source_unavailable) echo 'TLS identity bundle metadata is unavailable' ;;
    invalid_json) echo 'TLS identity bundle is not exact valid JSON' ;;
    bundle_too_large) echo 'TLS identity bundle exceeds the byte limit' ;;
    unsafe_permissions) echo 'TLS identity bundle permissions must be exactly 0600' ;;
    not_regular_file) echo 'TLS identity bundle must be a regular file and not a symbolic link' ;;
    cluster_mismatch) echo 'TLS identity bundle cluster ID does not match this process' ;;
    identity_mismatch) echo 'TLS identity bundle identity ID does not match this process' ;;
    purpose_mismatch) echo 'TLS identity bundle purpose does not match this process' ;;
    server_name_mismatch) echo 'TLS identity bundle server name does not match this process' ;;
    private_key_mismatch) echo 'TLS identity certificate chain and private key do not match' ;;
    wrong_eku) echo 'TLS identity leaf certificate omits its required extended key usage' ;;
    certificate_expired|certificate_not_yet_valid|wrong_hostname|wrong_ca) echo 'TLS identity certificate is not valid for its CA, time, purpose, or server name' ;;
    issuer_ca_mismatch) echo 'TLS identity bundle changes the process-pinned issuer CA' ;;
    stale_generation) echo 'TLS identity bundle generation is older than the active generation' ;;
    generation_fork) echo 'TLS identity bundle reuses the active generation with different contents' ;;
    *) return 1 ;;
  esac
}

run_startup_case() {
  local scenario="$1" kind="$2" port="$3" output="$4" directory source pid deadline state exit_code=0 probes=0 ever_open=0 message
  directory="$case_dir/startup-$scenario"
  source="$directory/identity.json"
  mkdir -p "$directory"
  prepare_invalid_bundle server "$scenario" "$source" 1
  env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$NO_PROXY" \
    INFERLAB_TRUST_DISTRIBUTOR_BIND="127.0.0.1:$port" \
    INFERLAB_TRUST_DISTRIBUTOR_CLUSTER_ID="$cluster_id" \
    INFERLAB_SERVICE_TRUST_ROOT_KEYS="$trust_root_id=$trust_root_public" \
    INFERLAB_SERVICE_TRUST_REVOKED_ROOT_KEY_IDS='' \
    INFERLAB_TRUST_DISTRIBUTOR_STATE_PATH="$directory/state.json" \
    INFERLAB_TRUST_DISTRIBUTOR_EXPECTED_SERVICE_IDS='control-a,control-b,control-c' \
    INFERLAB_TRUST_DISTRIBUTOR_TLS_IDENTITY_BUNDLE_PATH="$source" \
    INFERLAB_TRUST_DISTRIBUTOR_TLS_IDENTITY_BUNDLE_POLL_MS=25 \
    INFERLAB_TRUST_DISTRIBUTOR_TLS_SERVER_NAME='localhost' \
    INFERLAB_TRUST_DISTRIBUTOR_TLS_CLIENT_CA_PATH="$pki_dir/ca.crt" \
    target/debug/trust-distributor >"$directory/process.log" 2>&1 &
  pid="$!"; live_pids+=("$pid")
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
      shutdown_child "$pid"; forget_pid "$pid"
      echo "startup case remained live: $scenario" >&2; return 1
    fi
  fi
  set +e; wait "$pid"; exit_code="$?"; set -e
  forget_pid "$pid"
  message="$(error_message "$kind")"
  grep -F "$message" "$directory/process.log" >/dev/null || {
    echo "startup case missing diagnostic: $scenario/$kind" >&2
    tail -n 30 "$directory/process.log" >&2; return 1
  }
  python3 - "$scenario" "$kind" "$message" "$port" "$exit_code" "$ever_open" "$probes" >"$output" <<'PY'
import json, sys
scenario, kind, diagnostic, port, code, opened, probes = sys.argv[1:]
print(json.dumps({
    "scenario": scenario,
    "expected_error_kind": kind,
    "diagnostic": diagnostic,
    "port": int(port),
    "exit_code": int(code),
    "listener_ever_open": bool(int(opened)),
    "listener_probe_count": int(probes),
}, indent=2, sort_keys=True))
PY
}

start_distributor() {
  env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$NO_PROXY" \
    INFERLAB_TRUST_DISTRIBUTOR_BIND='127.0.0.1:12385' \
    INFERLAB_TRUST_DISTRIBUTOR_CLUSTER_ID="$cluster_id" \
    INFERLAB_SERVICE_TRUST_ROOT_KEYS="$trust_root_id=$trust_root_public" \
    INFERLAB_SERVICE_TRUST_REVOKED_ROOT_KEY_IDS='' \
    INFERLAB_TRUST_DISTRIBUTOR_STATE_PATH="$proof_tmp/distributor-state.json" \
    INFERLAB_TRUST_DISTRIBUTOR_EXPECTED_SERVICE_IDS='control-a,control-b,control-c' \
    INFERLAB_TRUST_DISTRIBUTOR_MAX_BODY_BYTES=262144 \
    INFERLAB_TRUST_DISTRIBUTOR_TLS_IDENTITY_BUNDLE_PATH="$bundle_dir/trust-distributor.json" \
    INFERLAB_TRUST_DISTRIBUTOR_TLS_IDENTITY_BUNDLE_POLL_MS=25 \
    INFERLAB_TRUST_DISTRIBUTOR_TLS_SERVER_NAME='localhost' \
    INFERLAB_TRUST_DISTRIBUTOR_TLS_CLIENT_CA_PATH="$pki_dir/ca.crt" \
    target/debug/trust-distributor >"$proof_tmp/trust-distributor.log" 2>&1 &
  distributor_pid="$!"; live_pids+=("$distributor_pid")
  wait_distributor
}

start_node() {
  local node="$1" port peers election_min=5000 pid
  port="$(node_port "$node")"; peers="$(node_peers "$node")"
  [[ "$node" == control-a ]] && election_min=300
  mkdir -p "$proof_tmp/$node"
  env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$NO_PROXY" \
    INFERLAB_RAFT_NODE_ID="$node" INFERLAB_RAFT_CLUSTER_ID="$cluster_id" \
    INFERLAB_RAFT_BIND="127.0.0.1:$port" INFERLAB_RAFT_PEERS="$peers" \
    INFERLAB_RAFT_DATA_DIR="$proof_tmp/$node" \
    INFERLAB_RAFT_ELECTION_MIN_MS="$election_min" INFERLAB_RAFT_ELECTION_MAX_MS="$((election_min + 100))" \
    INFERLAB_RAFT_HEARTBEAT_MS=50 INFERLAB_RAFT_RPC_TIMEOUT_MS=500 INFERLAB_RAFT_COMMIT_TIMEOUT_MS=2500 \
    INFERLAB_CONTROL_SIGNING_KEY_ID="$route_key_id" INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64="$route_seed" \
    INFERLAB_CONTROL_WRITER_KEYS="$writer_id=$writer_public" \
    INFERLAB_CONTROL_WRITE_MAX_AGE_MS=5000 INFERLAB_CONTROL_WRITE_MAX_FUTURE_SKEW_MS=250 \
    INFERLAB_SERVICE_ID="$node" INFERLAB_SERVICE_CREDENTIAL_ID='key-a' \
    INFERLAB_SERVICE_PRIVATE_KEY_B64="$(service_seed "$node")" \
    INFERLAB_SERVICE_TRUST_DISTRIBUTOR_URL="$distributor_url" \
    INFERLAB_SERVICE_TRUST_CACHE_PATH="$proof_tmp/$node/service-trust-cache.json" \
    INFERLAB_SERVICE_TRUST_STATE_PATH="$proof_tmp/$node/service-trust-floor.json" \
    INFERLAB_SERVICE_TRUST_ROOT_KEYS="$trust_root_id=$trust_root_public" \
    INFERLAB_SERVICE_TRUST_REVOKED_ROOT_KEY_IDS='' \
    INFERLAB_SERVICE_TRUST_MAX_POLICY_LIFETIME_MS="$policy_lifetime_ms" \
    INFERLAB_SERVICE_TRUST_POLICY_MAX_FUTURE_SKEW_MS=250 \
    INFERLAB_SERVICE_TRUST_TLS_CA_CERT_PATH="$pki_dir/ca.crt" \
    INFERLAB_SERVICE_TRUST_TLS_CLIENT_IDENTITY_BUNDLE_PATH="$bundle_dir/$node.json" \
    INFERLAB_SERVICE_TRUST_TLS_CLIENT_IDENTITY_BUNDLE_POLL_MS=25 \
    INFERLAB_SERVICE_TRUST_POLL_MS=25 INFERLAB_SERVICE_TRUST_REQUEST_TIMEOUT_MS=1000 \
    INFERLAB_SERVICE_TRUST_MAX_BACKOFF_MS=200 \
    INFERLAB_SERVICE_AUTH_MAX_AGE_MS=5000 INFERLAB_SERVICE_AUTH_MAX_FUTURE_SKEW_MS=250 \
    target/debug/control-plane >"$proof_tmp/$node.log" 2>&1 &
  pid="$!"; live_pids+=("$pid"); set_node_pid "$node" "$pid"
  wait_endpoint "http://127.0.0.1:$port/healthz" "$pid" "$node"
}

start_worker() {
  env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$NO_PROXY" \
    INFERLAB_CPU_WORKER_ID='cpu-tls-renewal' INFERLAB_CPU_BIND='127.0.0.1:12384' \
    INFERLAB_MODEL_PATH='models/tiny-inferlab-v2.bin' INFERLAB_CPU_DECODER_MODE='paged-kv-cache' \
    INFERLAB_CPU_QUANTIZATION='fp32' INFERLAB_CPU_SPECULATIVE_DRAFT_QUANTIZATION='int8' \
    INFERLAB_CPU_ATTENTION_KERNEL='online-tiled' INFERLAB_CPU_ATTENTION_PRECISION='fp32' \
    INFERLAB_CPU_ATTENTION_TILE_TOKENS=32 INFERLAB_CPU_MAX_BATCH_SIZE=4 \
    INFERLAB_CPU_SCHEDULER_QUEUE_CAPACITY=16 INFERLAB_CPU_KV_PAGE_TOKENS=4 \
    INFERLAB_CPU_KV_PAGE_COUNT=64 INFERLAB_CPU_PREFIX_CACHE_CAPACITY=32 INFERLAB_CPU_BATCH_TICK_MS=100 \
    target/debug/cpu-worker >"$proof_tmp/cpu-worker.log" 2>&1 &
  worker_pid="$!"; live_pids+=("$worker_pid")
  wait_endpoint "$worker_url/health" "$worker_pid" cpu-worker
}

start_gateway() {
  env -i PATH="$PATH" NO_PROXY="$NO_PROXY" no_proxy="$NO_PROXY" \
    INFERLAB_BIND='127.0.0.1:12380' INFERLAB_CONTROL_PLANE_URLS="$control_urls" \
    INFERLAB_CONTROL_CLUSTER_ID="$cluster_id" INFERLAB_CONTROL_TRUSTED_KEYS="$route_key_id=$route_public" \
    INFERLAB_CONTROL_BOOTSTRAP_WAIT_MS=5000 INFERLAB_CONTROL_POLL_MS=25 \
    INFERLAB_GATEWAY_SERVICE_ID='gateway-primary' INFERLAB_GATEWAY_SERVICE_CREDENTIAL_ID='key-a' \
    INFERLAB_GATEWAY_SERVICE_PRIVATE_KEY_B64="$(service_seed gateway-primary)" \
    INFERLAB_CONTROL_SERVICE_TARGETS='control-a=http://127.0.0.1:12381,control-b=http://127.0.0.1:12382,control-c=http://127.0.0.1:12383' \
    INFERLAB_ROUTING_SNAPSHOT_PATH="$proof_tmp/gateway-routing.json" \
    INFERLAB_ROUTING_SNAPSHOT_MAX_AGE_MS=120000 INFERLAB_ROUTING_LEASE_MS=60000 \
    INFERLAB_WORKER_CONCURRENCY=4 INFERLAB_ADMISSION_QUEUE_CAPACITY=8 \
    INFERLAB_REQUEST_DEADLINE_MS=15000 INFERLAB_ATTEMPT_TIMEOUT_MS=12000 INFERLAB_MAX_RETRIES=0 \
    target/debug/gateway >"$proof_tmp/gateway.log" 2>&1 &
  gateway_pid="$!"; live_pids+=("$gateway_pid")
  wait_endpoint "$gateway_url/health" "$gateway_pid" gateway
}

sign_policy() {
  env -i PATH="$PATH" INFERLAB_SERVICE_TRUST_ROOT_KEY_ID="$trust_root_id" \
    INFERLAB_SERVICE_TRUST_ROOT_PRIVATE_KEY_B64="$trust_root_seed" \
    target/debug/sign_service_trust "$1" >"$2"
}

publish_snapshot() {
  local publisher="$1" snapshot="$2" status="$3" output="$4"
  python3 benchmarks/tls_identity_handoff_probe.py capture \
    --url "$distributor_url/v1/service-trust/snapshot" --method POST --body "$snapshot" \
    --expect-status "$status" --ca-cert "$pki_dir/ca.crt" \
    --client-cert "$pki_dir/publisher-$publisher.crt" --client-key "$pki_dir/publisher-$publisher.key" \
    >"$output"
}

capture_processes() {
  local output="$1"
  python3 - "$output" "$$" \
    control-a "$control_a_pid" control-b "$control_b_pid" control-c "$control_c_pid" \
    cpu-worker "$worker_pid" gateway "$gateway_pid" trust-distributor "$distributor_pid" <<'PY'
import json, os, subprocess, sys
from pathlib import Path
output, proof_pid, *values = sys.argv[1:]
expected = {"control-a":"control-plane","control-b":"control-plane","control-c":"control-plane","cpu-worker":"cpu-worker","gateway":"gateway","trust-distributor":"trust-distributor"}
items=[]
for index in range(0,len(values),2):
    label, raw_pid = values[index:index+2]
    pid=int(raw_pid)
    fields=subprocess.check_output(["ps","-o","ppid=","-o","stat=","-o","lstart=","-p",str(pid)],text=True,env={**os.environ,"LC_ALL":"C"}).strip().split()
    if len(fields)!=7: raise SystemExit(f"cannot parse process identity for {label}")
    ppid,state=int(fields[0]),fields[1]
    start=" ".join(fields[2:7])
    if sys.platform.startswith("linux"):
        command=os.path.basename(os.readlink(f"/proc/{pid}/exe").removesuffix(" (deleted)"))
    else:
        command=os.path.basename(subprocess.check_output(["ps","-o","comm=","-p",str(pid)],text=True,env={**os.environ,"LC_ALL":"C"}).strip())
    if ppid!=int(proof_pid) or command!=expected[label] or "Z" in state: raise SystemExit(f"process identity mismatch for {label}")
    items.append({"label":label,"pid":pid,"ppid":ppid,"state":state,"start_token":start,"command":command})
Path(output).write_text(json.dumps(sorted(items,key=lambda item:item["label"]),indent=2,sort_keys=True)+"\n",encoding="utf-8")
PY
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

identity_from_distributor_capture() {
  python3 - "$1" <<'PY'
import json, sys
d=json.load(open(sys.argv[1],encoding="utf-8"))["result"]
print(json.dumps({"peer_sha256":d["tls_peer_certificate_sha256"],"identity":d["body"]["transport_security"]["identity"]},sort_keys=True))
PY
}

identity_from_control_capture() {
  python3 - "$1" <<'PY'
import json, sys
a=json.load(open(sys.argv[1],encoding="utf-8"))["result"]["body"]["service_authentication"]
print(json.dumps({"identity":a["trust_policy_tls_identity"],"last_fetch_tls_bundle_generation":a["trust_policy_last_fetch_tls_bundle_generation"],"last_fetch_outcome":a["trust_policy_last_fetch_outcome"]},sort_keys=True))
PY
}

prepare_output_dir
check_ports_are_free 12380 12381 12382 12383 12384 12385 \
  12400 12401 12402 12403 12404 12405 12406 12407 12408 12409 12410 12411 12412 12413 12414
command -v openssl >/dev/null
for escaped in "$escaped_main_serial" "$escaped_rogue_serial"; do
  if [[ -e "$escaped" || -L "$escaped" ]]; then
    echo 'refusing v0.30 proof: escaped OpenSSL serial sentinel exists' >&2; exit 1
  fi
done
cargo build --locked --workspace --bins --quiet
generate_pki

generic_public() {
  env -i PATH="$PATH" INFERLAB_SERVICE_ID='proof-key' INFERLAB_SERVICE_CREDENTIAL_ID='proof-key' \
    INFERLAB_SERVICE_PRIVATE_KEY_B64="$1" target/debug/service_public_key
}
service_public() {
  env -i PATH="$PATH" INFERLAB_SERVICE_ID="$1" INFERLAB_SERVICE_CREDENTIAL_ID='key-a' \
    INFERLAB_SERVICE_PRIVATE_KEY_B64="$(service_seed "$1")" target/debug/service_public_key
}
trust_root_public="$(env -i PATH="$PATH" INFERLAB_SERVICE_TRUST_ROOT_KEY_ID="$trust_root_id" \
  INFERLAB_SERVICE_TRUST_ROOT_PRIVATE_KEY_B64="$trust_root_seed" target/debug/service_trust_public_key)"
route_public="$(generic_public "$route_seed")"
writer_public="$(generic_public "$writer_seed")"

server_a_sha="$(certificate_fingerprint "$pki_dir/server-a.crt")"
server_b_sha="$(certificate_fingerprint "$pki_dir/server-b.crt")"
publisher_a_sha="$(certificate_fingerprint "$pki_dir/publisher-a.crt")"
publisher_b_sha="$(certificate_fingerprint "$pki_dir/publisher-b.crt")"
python3 - "$server_a_sha" "$server_b_sha" "$publisher_a_sha" "$publisher_b_sha" \
  "$pki_dir" >"$results_dir/certificate-identities.json" <<'PY'
import hashlib, json, ssl, sys
from pathlib import Path
server_a,server_b,pub_a,pub_b,raw_dir=sys.argv[1:]
d=Path(raw_dir)
def fp(name):
    return hashlib.sha256(ssl.PEM_cert_to_DER_cert((d/name).read_text())).hexdigest()
controls={}
for control in ["control-a","control-b","control-c"]:
    controls[control]={"A":fp(f"{control}-a.crt"),"B":fp(f"{control}-b.crt")}
print(json.dumps({
    "schema":"inferlab.tls-identity-handoff-certificates.v0.30",
    "digest":"sha256-der",
    "issuer_ca_unchanged":True,
    "server":{"A":server_a,"B":server_b},
    "publisher_proof_clients":{"A":pub_a,"B":pub_b,"semantics":"fresh clients; no process continuity claim"},
    "controls":controls,
},indent=2,sort_keys=True))
PY

write_bundle "$bundle_dir/trust-distributor.json" 1 trust-distributor server \
  "$pki_dir/server-a.crt" "$pki_dir/server-a.key" "$pki_dir/ca.crt" localhost
for node in control-a control-b control-c; do
  write_bundle "$bundle_dir/$node.json" 1 "$node" client \
    "$pki_dir/$node-a.crt" "$pki_dir/$node-a.key" "$pki_dir/ca.crt"
done

issued_at_ms="$(python3 - <<'PY'
import time
print(time.time_ns()//1_000_000)
PY
)"
expires_at_ms="$((issued_at_ms + policy_lifetime_ms))"
python3 - "$issued_at_ms" "$expires_at_ms" "$policy_dir" \
  "$(service_public control-a)" "$(service_public control-b)" "$(service_public control-c)" \
  "$(service_public gateway-primary)" <<'PY'
import json, sys
from pathlib import Path
issued,expires=map(int,sys.argv[1:3]); directory=Path(sys.argv[3]); keys=sys.argv[4:]
services=["control-a","control-b","control-c","gateway-primary"]
credentials=[{"service_id":service,"credential_id":"key-a","public_key_base64":key} for service,key in zip(services,keys)]
for generation in (1,2):
    policy={"schema":"inferlab.service-trust-policy.v2","cluster_id":"inferlab-primary","generation":generation,
      "issued_at_ms":issued,"expires_at_ms":expires,"trusted_credentials":credentials,
      "revoked_service_ids":[],"revoked_credentials":[],"gateway_service_ids":["gateway-primary"]}
    (directory/f"policy-g{generation}.json").write_text(json.dumps(policy,indent=2)+"\n",encoding="utf-8")
PY
sign_policy "$policy_dir/policy-g1.json" "$policy_dir/snapshot-g1.json"
sign_policy "$policy_dir/policy-g2.json" "$policy_dir/snapshot-g2.json"
python3 - "$trust_root_id" "$trust_root_public" "$policy_dir/snapshot-g1.json" "$policy_dir/snapshot-g2.json" \
  >"$results_dir/trust-generations.json" <<'PY'
import json,sys
root,public,g1,g2=sys.argv[1:]
print(json.dumps({"schema":"inferlab.tls-identity-handoff-trust-generations.v0.30","root_key_id":root,
 "root_public_key_base64":public,"generations":{"1":json.load(open(g1)),"2":json.load(open(g2))}},indent=2,sort_keys=True))
PY

cat >"$results_dir/proof-contract.json" <<'JSON'
{
  "cluster_id": "inferlab-primary",
  "connection_barriers": ["held-server-ready", "server-B-active", "held-server-release"],
  "controls": ["control-a", "control-b", "control-c"],
  "identity_generations": [1, 2],
  "policy_generations": [1, 2],
  "processes": ["control-a", "control-b", "control-c", "cpu-worker", "gateway", "trust-distributor"],
  "publisher_semantics": "A and B are separate fresh proof clients; neither is a retained runtime process",
  "schema": "inferlab.tls-identity-handoff-proof-contract.v0.30",
  "server_name": "localhost",
  "tls_protocol": "TLSv1.3"
}
JSON

startup_scenarios=(missing malformed oversize unsafe-permissions symlink wrong-cluster wrong-identity wrong-purpose wrong-server-name mismatched-key expired not-yet-valid wrong-eku wrong-san wrong-ca)
startup_kinds=(source_unavailable invalid_json bundle_too_large unsafe_permissions not_regular_file cluster_mismatch identity_mismatch purpose_mismatch server_name_mismatch private_key_mismatch certificate_expired certificate_not_yet_valid wrong_eku wrong_hostname wrong_ca)
startup_outputs=()
for index in "${!startup_scenarios[@]}"; do
  output="$proof_tmp/startup-$index.json"
  run_startup_case "${startup_scenarios[$index]}" "${startup_kinds[$index]}" "$((12400 + index))" "$output"
  startup_outputs+=("$output")
done
python3 - "${startup_outputs[@]}" >"$results_dir/startup-rejections.json" <<'PY'
import json,sys
print(json.dumps({"schema":"inferlab.tls-identity-handoff-startup-rejections.v0.30","cases":[json.load(open(p)) for p in sys.argv[1:]]},indent=2,sort_keys=True))
PY

start_distributor
publish_snapshot a "$policy_dir/snapshot-g1.json" 201 "$results_dir/publish-g1-publisher-a.json"
start_node control-b
start_node control-c
start_node control-a
start_worker
python3 benchmarks/full_stack_probe.py wait-leader --urls "$control_urls" --timeout 15 >"$proof_tmp/initial-cluster.json"
leader_id="$(json_value "$proof_tmp/initial-cluster.json" leader_id)"
leader_url="$(json_value "$proof_tmp/initial-cluster.json" leader_url)"
cat >"$proof_tmp/route-r2.json" <<'JSON'
{"routing_policy":"round-robin","workers":[{"id":"cpu-tls-renewal","base_url":"http://127.0.0.1:12384","weight":1}]}
JSON
env -i PATH="$PATH" INFERLAB_CONTROL_WRITER_ID="$writer_id" INFERLAB_CONTROL_WRITER_PRIVATE_KEY_B64="$writer_seed" \
  target/debug/sign_control_write "$cluster_id" 0 now v030-route-r2-0001 "$proof_tmp/route-r2.json" >"$proof_tmp/write-r2.json"
python3 benchmarks/control_write_probe.py submit --url "$leader_url" --body "$proof_tmp/write-r2.json" >"$proof_tmp/r2-write-raw.json"
start_gateway
python3 benchmarks/tls_identity_handoff_probe.py wait-controls --urls "$control_urls" --revision 2 \
  --policy-generation 1 --tls-generations 'control-a=1,control-b=1,control-c=1' --require-receipt-generation \
  --timeout 20 >"$results_dir/generation-1-controls.json"
python3 benchmarks/tls_identity_handoff_probe.py wait-distributor --url "$distributor_url" \
  --policy-generation 1 --tls-generation 1 --expected-services 'control-a,control-b,control-c' \
  --peer-sha256 "$server_a_sha" --ca-cert "$pki_dir/ca.crt" \
  --client-cert "$pki_dir/publisher-a.crt" --client-key "$pki_dir/publisher-a.key" --timeout 20 \
  >"$results_dir/generation-1-receipts.json"
capture_processes "$proof_tmp/initial-processes.json"

wait_distributor_identity() {
  local generation="$1" kind="$2" minimum="$3" peer="$4" output="$5"
  local args=(benchmarks/tls_identity_handoff_probe.py wait-distributor --url "$distributor_url"
    --policy-generation 1 --tls-generation "$generation" --expected-services 'control-a,control-b,control-c'
    --min-rejections "$minimum" --peer-sha256 "$peer" --ca-cert "$pki_dir/ca.crt"
    --client-cert "$pki_dir/publisher-a.crt" --client-key "$pki_dir/publisher-a.key" --timeout 15)
  [[ -z "$kind" ]] || args+=(--last-error-kind "$kind")
  python3 "${args[@]}" >"$output"
}

wait_distributor_status_identity() {
  local generation="$1" kind="$2" minimum="$3" output="$4"
  local args=(benchmarks/tls_identity_handoff_probe.py wait-distributor --url "$distributor_url"
    --policy-generation 1 --tls-generation "$generation" --expected-services 'control-a,control-b,control-c'
    --min-rejections "$minimum" --ca-cert "$pki_dir/ca.crt"
    --client-cert "$pki_dir/publisher-a.crt" --client-key "$pki_dir/publisher-a.key" --timeout 15)
  [[ -z "$kind" ]] || args+=(--last-error-kind "$kind")
  python3 "${args[@]}" >"$output"
}

run_live_server_case() {
  local scenario="$1" kind="$2" generation="$3" active_leaf="$4" active_key="$5" peer="$6" output="$7"
  local before="$proof_tmp/server-$scenario-before.json" rejected="$proof_tmp/server-$scenario-rejected.json" recovered="$proof_tmp/server-$scenario-recovered.json"
  local before_count before_status rejected_status recovered_status
  wait_distributor_identity "$generation" '' 0 "$peer" "$before"
  before_status="$(identity_from_distributor_capture "$before")"
  before_count="$(python3 - "$before" <<'PY'
import json,sys
print(json.load(open(sys.argv[1]))["result"]["body"]["transport_security"]["identity"]["rejected_reloads"])
PY
)"
  prepare_invalid_bundle server "$scenario" "$bundle_dir/trust-distributor.json" "$generation"
  wait_distributor_identity "$generation" "$kind" "$((before_count + 1))" "$peer" "$rejected"
  rejected_status="$(identity_from_distributor_capture "$rejected")"
  write_bundle "$bundle_dir/trust-distributor.json" "$generation" trust-distributor server \
    "$active_leaf" "$active_key" "$pki_dir/ca.crt" localhost
  rm -rf -- "$bundle_dir/trust-distributor.json.target"
  wait_distributor_identity "$generation" '' "$((before_count + 1))" "$peer" "$recovered"
  recovered_status="$(identity_from_distributor_capture "$recovered")"
  python3 - "$scenario" "$kind" "$distributor_pid" "$before_status" "$rejected_status" "$recovered_status" >"$output" <<'PY'
import json,sys
scenario,kind,pid,before,rejected,recovered=sys.argv[1:]
print(json.dumps({"scenario":scenario,"expected_error_kind":kind,"process":"trust-distributor","pid":int(pid),
 "before":json.loads(before),"rejected":json.loads(rejected),"recovered":json.loads(recovered),"lkg_handshakes_succeeded":True},indent=2,sort_keys=True))
PY
}

server_live_outputs=()
server_live_scenarios=(missing malformed oversize unsafe-permissions symlink wrong-cluster wrong-identity wrong-purpose wrong-server-name mismatched-key expired not-yet-valid wrong-eku wrong-san wrong-ca issuer-ca-change)
server_live_kinds=(source_unavailable invalid_json bundle_too_large unsafe_permissions not_regular_file cluster_mismatch identity_mismatch purpose_mismatch server_name_mismatch private_key_mismatch certificate_expired certificate_not_yet_valid wrong_eku wrong_hostname wrong_ca issuer_ca_mismatch)
for index in "${!server_live_scenarios[@]}"; do
  output="$proof_tmp/server-live-${server_live_scenarios[$index]}.json"
  run_live_server_case "${server_live_scenarios[$index]}" "${server_live_kinds[$index]}" 1 \
    "$pki_dir/server-a.crt" "$pki_dir/server-a.key" "$server_a_sha" "$output"
  server_live_outputs+=("$output")
done

wait_control_identity() {
  local node="$1" generation="$2" kind="$3" minimum="$4" output="$5"
  local args=(benchmarks/tls_identity_handoff_probe.py wait-control --url "$(node_url "$node")"
    --identity-id "$node" --tls-generation "$generation" --min-rejections "$minimum" --timeout 15)
  [[ -z "$kind" ]] || args+=(--last-error-kind "$kind")
  python3 "${args[@]}" >"$output"
}

run_live_client_case() {
  local scenario="$1" kind="$2" output="$3"
  local before="$proof_tmp/client-$scenario-before.json" rejected="$proof_tmp/client-$scenario-rejected.json" recovered="$proof_tmp/client-$scenario-recovered.json"
  local before_count before_status rejected_status recovered_status
  wait_control_identity control-a 1 '' 0 "$before"
  before_status="$(identity_from_control_capture "$before")"
  before_count="$(python3 - "$before" <<'PY'
import json,sys
print(json.load(open(sys.argv[1]))["result"]["body"]["service_authentication"]["trust_policy_tls_identity"]["rejected_reloads"])
PY
)"
  prepare_invalid_bundle client "$scenario" "$bundle_dir/control-a.json" 2
  wait_control_identity control-a 1 "$kind" "$((before_count + 1))" "$rejected"
  rejected_status="$(identity_from_control_capture "$rejected")"
  write_bundle "$bundle_dir/control-a.json" 1 control-a client "$pki_dir/control-a-a.crt" "$pki_dir/control-a-a.key" "$pki_dir/ca.crt"
  rm -rf -- "$bundle_dir/control-a.json.target"
  wait_control_identity control-a 1 '' "$((before_count + 1))" "$recovered"
  recovered_status="$(identity_from_control_capture "$recovered")"
  python3 - "$scenario" "$kind" "$control_a_pid" "$before_status" "$rejected_status" "$recovered_status" >"$output" <<'PY'
import json,sys
scenario,kind,pid,before,rejected,recovered=sys.argv[1:]
print(json.dumps({"scenario":scenario,"expected_error_kind":kind,"process":"control-a","pid":int(pid),
 "before":json.loads(before),"rejected":json.loads(rejected),"recovered":json.loads(recovered),"lkg_fetches_succeeded":True},indent=2,sort_keys=True))
PY
}

client_live_outputs=()
client_live_scenarios=(malformed unsafe-permissions symlink wrong-cluster wrong-identity wrong-purpose mismatched-key expired not-yet-valid wrong-eku wrong-ca issuer-ca-change)
client_live_kinds=(invalid_json unsafe_permissions not_regular_file cluster_mismatch identity_mismatch purpose_mismatch private_key_mismatch certificate_expired certificate_not_yet_valid wrong_eku wrong_ca issuer_ca_mismatch)
for index in "${!client_live_scenarios[@]}"; do
  output="$proof_tmp/client-live-${client_live_scenarios[$index]}.json"
  run_live_client_case "${client_live_scenarios[$index]}" "${client_live_kinds[$index]}" "$output"
  client_live_outputs+=("$output")
done

ready_barrier="$proof_tmp/held-server.ready.json"
release_barrier="$proof_tmp/held-server.release"
held_output="$proof_tmp/held-server.output.json"
python3 benchmarks/tls_identity_handoff_probe.py hold-connection \
  --url "$distributor_url/v1/service-trust/status" --ready "$ready_barrier" \
  --release "$release_barrier" --output "$held_output" --timeout 45 \
  --ca-cert "$pki_dir/ca.crt" --client-cert "$pki_dir/publisher-a.crt" \
  --client-key "$pki_dir/publisher-a.key" >"$proof_tmp/held-probe.stdout" 2>"$proof_tmp/held-probe.stderr" &
held_probe_pid="$!"; live_pids+=("$held_probe_pid")
deadline=$((SECONDS + 20))
while [[ ! -f "$ready_barrier" ]]; do
  is_owned_child "$held_probe_pid" || { echo 'held probe exited before ready barrier' >&2; exit 1; }
  ((SECONDS < deadline)) || { echo 'timeout waiting for held ready barrier' >&2; exit 1; }
  sleep 0.02
done
[[ "$(json_value "$ready_barrier" tls_peer_certificate_sha256)" == "$server_a_sha" ]]
write_bundle "$bundle_dir/trust-distributor.json" 2 trust-distributor server \
  "$pki_dir/server-b.crt" "$pki_dir/server-b.key" "$pki_dir/ca.crt" localhost
wait_distributor_status_identity 2 '' 0 "$proof_tmp/server-b-status-active.json"
wait_distributor_identity 2 '' 0 "$server_b_sha" "$proof_tmp/server-b-new-connection.json"
python3 - "$release_barrier" <<'PY'
import os,sys
path=sys.argv[1]; temporary=f"{path}.{os.getpid()}.tmp"
open(temporary,"w",encoding="ascii").write("release\n")
os.replace(temporary,path)
PY
set +e; wait "$held_probe_pid"; held_status="$?"; set -e
forget_pid "$held_probe_pid"; held_probe_pid=''
if [[ "$held_status" != 0 || ! -f "$held_output" ]]; then
  echo 'held connection probe failed' >&2; cat "$proof_tmp/held-probe.stderr" >&2; exit 1
fi
python3 - "$ready_barrier" "$proof_tmp/server-b-status-active.json" \
  "$proof_tmp/server-b-new-connection.json" "$held_output" "$server_a_sha" "$server_b_sha" \
  >"$results_dir/server-handoff.json" <<'PY'
import json,sys
ready,status_active,new_connection,held,a,b=sys.argv[1:]
print(json.dumps({"schema":"inferlab.tls-identity-handoff-server.v0.30","barriers":["ready-A","B-active","release"],
 "expected_fingerprints":{"A":a,"B":b},"ready":json.load(open(ready)),
 "status_activation_barrier":json.load(open(status_active)),
 "new_connection_after_activation":json.load(open(new_connection)),
 "held_connection":json.load(open(held))},indent=2,sort_keys=True))
PY

for pair in 'stale stale_generation' 'fork generation_fork' 'issuer-ca-change issuer_ca_mismatch'; do
  read -r scenario kind <<<"$pair"
  output="$proof_tmp/server-post-b-$scenario.json"
  run_live_server_case "$scenario" "$kind" 2 "$pki_dir/server-b.crt" "$pki_dir/server-b.key" "$server_b_sha" "$output"
  server_live_outputs+=("$output")
done
python3 - "${server_live_outputs[@]}" "--clients" "${client_live_outputs[@]}" \
  >"$results_dir/live-rejections.json" <<'PY'
import json,sys
split=sys.argv.index("--clients")
print(json.dumps({"schema":"inferlab.tls-identity-handoff-live-rejections.v0.30",
 "server_cases":[json.load(open(p)) for p in sys.argv[1:split]],
 "client_cases":[json.load(open(p)) for p in sys.argv[split+1:]]},indent=2,sort_keys=True))
PY

handoff_steps=()
changed=()
step=0
for node in control-b control-c control-a; do
  step=$((step + 1))
  write_bundle "$bundle_dir/$node.json" 2 "$node" client "$pki_dir/$node-b.crt" "$pki_dir/$node-b.key" "$pki_dir/ca.crt"
  changed+=("$node")
  map=''
  for candidate in control-a control-b control-c; do
    generation=1
    for switched in "${changed[@]}"; do [[ "$candidate" == "$switched" ]] && generation=2; done
    map+="${map:+,}$candidate=$generation"
  done
  capture="$proof_tmp/control-handoff-$step.json"
  processes="$proof_tmp/control-handoff-$step-processes.json"
  python3 benchmarks/tls_identity_handoff_probe.py wait-controls --urls "$control_urls" --revision 2 \
    --policy-generation 1 --tls-generations "$map" --timeout 20 >"$capture"
  capture_processes "$processes"
  combined="$proof_tmp/control-handoff-$step-combined.json"
  python3 - "$step" "$node" "$capture" "$processes" >"$combined" <<'PY'
import json,sys
step,node,capture,processes=sys.argv[1:]
print(json.dumps({"step":int(step),"identity_id":node,"controls":json.load(open(capture)),"processes":json.load(open(processes))},indent=2,sort_keys=True))
PY
  handoff_steps+=("$combined")
done
python3 - "$proof_tmp/initial-processes.json" "${handoff_steps[@]}" >"$results_dir/control-handoff.json" <<'PY'
import json,sys
print(json.dumps({"schema":"inferlab.tls-identity-handoff-controls-sequence.v0.30",
 "order":["control-b","control-c","control-a"],"initial_processes":json.load(open(sys.argv[1])),
 "steps":[json.load(open(p)) for p in sys.argv[2:]]},indent=2,sort_keys=True))
PY

publish_snapshot b "$policy_dir/snapshot-g2.json" 201 "$results_dir/publish-g2-publisher-b.json"
python3 benchmarks/tls_identity_handoff_probe.py wait-controls --urls "$control_urls" --revision 2 \
  --policy-generation 2 --tls-generations 'control-a=2,control-b=2,control-c=2' --require-receipt-generation \
  --timeout 25 >"$results_dir/generation-2-controls.json"
python3 benchmarks/tls_identity_handoff_probe.py wait-distributor --url "$distributor_url" \
  --policy-generation 2 --tls-generation 2 --expected-services 'control-a,control-b,control-c' \
  --peer-sha256 "$server_b_sha" --ca-cert "$pki_dir/ca.crt" \
  --client-cert "$pki_dir/publisher-b.crt" --client-key "$pki_dir/publisher-b.key" --timeout 25 \
  >"$results_dir/generation-2-receipts.json"

python3 benchmarks/tls_identity_handoff_probe.py wait-controls --urls "$control_urls" --revision 2 \
  --policy-generation 2 --tls-generations 'control-a=2,control-b=2,control-c=2' \
  --require-receipt-generation --timeout 20 >"$results_dir/final-cluster.json"
python3 benchmarks/tls_identity_handoff_probe.py completion --url "$gateway_url" \
  --prompt 'v030-json-private-proof-prompt' --temporary-body "$proof_tmp/final-json-body.json" --timeout 15 \
  >"$results_dir/final-json.json"
python3 benchmarks/tls_identity_handoff_probe.py stream --url "$gateway_url" \
  --prompt 'v030-sse-private-proof-prompt' --timeout 15 >"$results_dir/final-sse.json"

production_specs=(
  'transport-security|--lib|identity_bundle::tests::strict_bundle_is_bound_bounded_and_redacted'
  'transport-security|--lib|identity_bundle::tests::purpose_hostname_ca_and_private_key_fail_closed'
  'transport-security|--lib|identity_bundle::tests::current_time_and_eku_are_verified'
  'transport-security|--lib|identity_bundle::tests::file_loader_rejects_permissions_and_symlinks'
  'transport-security|--lib|identity_bundle::tests::activation_rejects_rollback_fork_ca_change_and_runtime_failure'
  'transport-security|--lib|identity_bundle::tests::concurrent_snapshots_are_entirely_old_or_new'
  'transport-security|--lib|identity_bundle::tests::watcher_loop_deduplicates_deterministic_errors_and_retries_time_dependent_sources'
  'trust-distributor|--bin trust-distributor|tests::watched_tls_identity_configuration_is_strictly_separate_and_bounded'
  'trust-distributor|--bin trust-distributor|tests::tls_identity_watcher_completion_is_process_supervised'
  'control-plane|--lib|service_trust::tests::watched_client_identity_swaps_the_whole_pool_for_new_operations'
  'control-plane|--bin control-plane|tests::supervisor_fails_when_the_tls_identity_watcher_completes'
  'control-plane|--bin control-plane|tests::malformed_unicode_tls_path_fails_closed'
)
production_arguments=()
index=0
for spec in "${production_specs[@]}"; do
  IFS='|' read -r package raw_target test_filter <<<"$spec"
  read -r -a target_arguments <<<"$raw_target"
  log="$proof_tmp/production-$index.log"
  set +e
  CARGO_TERM_COLOR=never cargo test --locked -p "$package" "${target_arguments[@]}" "$test_filter" -- --exact >"$log" 2>&1
  status="$?"
  set -e
  production_arguments+=("$package" "$raw_target" "$test_filter" "$status" "$log")
  index=$((index + 1))
done
python3 - "${production_arguments[@]}" >"$results_dir/production-tests.json" <<'PY'
import json,re,shlex,sys
from pathlib import Path
values=sys.argv[1:]; tests=[]
pattern=re.compile(r"^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; [0-9]+ filtered out; finished in [0-9]+(?:\.[0-9]+)?s$")
for i in range(0,len(values),5):
    package,target,filter_,status,path=values[i:i+5]
    lines=Path(path).read_text(errors="replace").splitlines(); expected=f"test {filter_} ... ok"; summaries=[line for line in lines if pattern.fullmatch(line)]
    projected=["running 1 test",expected,summaries[0]] if lines.count("running 1 test")==1 and lines.count(expected)==1 and len(summaries)==1 else []
    tests.append({"package":package,"target":shlex.split(target),"test_filter":filter_,"exit_code":int(status),"output_lines":projected})
result={"schema":"inferlab.tls-identity-handoff-production-tests.v0.30","test_count":len(tests),"tests":tests}
print(json.dumps(result,indent=2,sort_keys=True))
if len(tests)!=12 or any(item["exit_code"]!=0 or len(item["output_lines"])!=3 for item in tests): raise SystemExit("v0.30 focused regression failed")
PY

capture_processes "$proof_tmp/final-processes.json"
python3 - "$proof_tmp/initial-processes.json" "$proof_tmp/final-processes.json" "$$" \
  >"$results_dir/process-continuity.json" <<'PY'
import json,sys
initial=json.load(open(sys.argv[1])); final=json.load(open(sys.argv[2]))
def identity(item): return [item[k] for k in ("label","pid","ppid","start_token","command")]
print(json.dumps({"schema":"inferlab.tls-identity-handoff-process-continuity.v0.30","proof_shell_pid":int(sys.argv[3]),
 "publisher_processes_in_scope":False,"initial":initial,"final":final,
 "unchanged":[identity(x) for x in initial]==[identity(x) for x in final]},indent=2,sort_keys=True))
PY

python3 - "$proof_tmp" "$project_root" >"$results_dir/discarded-log-scan.json" <<'PY'
import base64,hashlib,json,re,sys,urllib.parse
from pathlib import Path
root=Path(sys.argv[1]); project=Path(sys.argv[2])
logs=[root/name for name in ["control-a.log","control-b.log","control-c.log","cpu-worker.log","gateway.log","trust-distributor.log"]]
logs += sorted(root.glob("production-*.log"))
labels=["v030-service-trust-root","v030-route-signing","v030-control-writer",*[f"v030-service-{s}" for s in ["control-a","control-b","control-c","gateway-primary"]]]
secrets=[]
for label in labels:
    value=base64.b64encode(hashlib.sha256(label.encode()).digest()).decode()
    secrets.extend([value,value.rstrip("="),urllib.parse.quote(value,safe=""),hashlib.sha256(value.encode()).hexdigest()])
markers=["-----BEGIN PRIVATE KEY-----","PRIVATE_KEY_B64","PRIVATE_KEY_BASE64","v030-json-private-proof-prompt","v030-sse-private-proof-prompt"]
matches=[]
for path in logs:
    text=path.read_text(errors="replace")
    if any(value in text for value in secrets): matches.append(f"{path.name}:seed")
    if any(value in text for value in markers): matches.append(f"{path.name}:private-marker")
result={"schema":"inferlab.tls-identity-handoff-discarded-log-scan.v0.30","files_scanned":[p.name for p in logs],
 "checks":["deterministic-seeds","private-markers","fixed-private-prompts"],"matches":sorted(set(matches)),"passed":not matches}
print(json.dumps(result,indent=2,sort_keys=True))
if matches: raise SystemExit("discarded logs contain proof-private material")
PY

python3 benchmarks/tls_identity_handoff_probe.py sanitize-evidence --evidence-dir "$results_dir" \
  --proof-root "$proof_tmp" --project-root "$project_root" >"$proof_tmp/sanitizer.json"
mv "$proof_tmp/sanitizer.json" "$results_dir/sanitizer.json"
printf '{}\n' >"$results_dir/assertions.json"
printf '<svg xmlns="http://www.w3.org/2000/svg"/>\n' >"$results_dir/tls-identity-handoff-proof.svg"

scan_private_material() {
  python3 - "$results_dir" >"$1" <<'PY'
import base64,hashlib,json,sys,urllib.parse
from pathlib import Path
directory=Path(sys.argv[1]); labels=["v030-service-trust-root","v030-route-signing","v030-control-writer",*[f"v030-service-{s}" for s in ["control-a","control-b","control-c","gateway-primary"]]]
matches=[]
for label in labels:
    value=base64.b64encode(hashlib.sha256(label.encode()).digest()).decode()
    representations=[value,value.rstrip("="),urllib.parse.quote(value,safe=""),hashlib.sha256(value.encode()).hexdigest()]
    for path in sorted(directory.iterdir()):
        if not path.is_file() or path.name in {"manifest.json","private-material-scan.json"}: continue
        text=path.read_text(errors="replace")
        for index,representation in enumerate(representations):
            if representation in text: matches.append({"file":path.name,"seed_label":label,"representation":index})
result={"schema":"inferlab.tls-identity-handoff-private-material-scan.v0.30","algorithm":"sha256-label-to-ed25519-seed",
 "files_scanned":sorted(p.name for p in directory.iterdir() if p.is_file() and p.name not in {"manifest.json","private-material-scan.json"}),
 "seed_labels_scanned":labels,"representations_per_seed":4,"matches":matches,"passed":not matches}
print(json.dumps(result,indent=2,sort_keys=True))
if matches: raise SystemExit("deterministic private material entered retained evidence")
PY
}

scan_private_material "$results_dir/private-material-scan.json"
python3 benchmarks/check_tls_identity_handoff.py --evidence-dir "$results_dir" --output "$results_dir/assertions.json"
python3 benchmarks/render_tls_identity_handoff_svg.py --evidence-dir "$results_dir" --output "$results_dir/tls-identity-handoff-proof.svg"
scan_private_material "$results_dir/private-material-scan.json"
python3 benchmarks/check_tls_identity_handoff.py --evidence-dir "$results_dir" --output "$results_dir/assertions.json"
python3 benchmarks/render_tls_identity_handoff_svg.py --evidence-dir "$results_dir" --output "$results_dir/tls-identity-handoff-proof.svg"
scan_private_material "$results_dir/private-material-scan.json"
python3 benchmarks/check_tls_identity_handoff.py --evidence-dir "$results_dir" --output "$proof_tmp/replay-assertions.json"
cmp "$results_dir/assertions.json" "$proof_tmp/replay-assertions.json"
python3 benchmarks/render_tls_identity_handoff_svg.py --evidence-dir "$results_dir" --output "$proof_tmp/replay-proof.svg"
cmp "$results_dir/tls-identity-handoff-proof.svg" "$proof_tmp/replay-proof.svg"

expected_files=(
  assertions.json
  certificate-identities.json
  control-handoff.json
  discarded-log-scan.json
  final-cluster.json
  final-json.json
  final-sse.json
  generation-1-controls.json
  generation-1-receipts.json
  generation-2-controls.json
  generation-2-receipts.json
  live-rejections.json
  manifest.json
  private-material-scan.json
  process-continuity.json
  production-tests.json
  proof-contract.json
  publish-g1-publisher-a.json
  publish-g2-publisher-b.json
  sanitizer.json
  server-handoff.json
  startup-rejections.json
  tls-identity-handoff-proof.svg
  trust-generations.json
)

write_manifest() {
  python3 - "$results_dir" "${expected_files[@]}" <<'PY'
import hashlib,json,sys
from pathlib import Path
directory=Path(sys.argv[1]); expected=sys.argv[2:]
if expected!=sorted(expected) or len(expected)!=len(set(expected)): raise SystemExit("manifest inventory must be sorted and unique")
entries=list(directory.iterdir())
if {p.name for p in entries}!=set(expected)-{"manifest.json"} or any(not p.is_file() or p.is_symlink() for p in entries): raise SystemExit("pre-manifest inventory mismatch")
files=[]
for name in expected:
    if name=="manifest.json": continue
    raw=(directory/name).read_bytes(); files.append({"name":name,"bytes":len(raw),"sha256":hashlib.sha256(raw).hexdigest()})
(directory/"manifest.json").write_text(json.dumps({"schema":"inferlab.tls-identity-handoff-manifest.v0.30","file_count":len(files),"files":files},indent=2,sort_keys=True)+"\n",encoding="utf-8")
PY
}

retain_results() {
  [[ -z "${INFERLAB_V30_OUTPUT_DIR:-}" ]] && return
  local name
  for name in "${expected_files[@]}"; do
    [[ "$name" == manifest.json ]] && continue
    cp "$results_dir/$name" "$INFERLAB_V30_OUTPUT_DIR/$name"
  done
  cp "$results_dir/manifest.json" "$INFERLAB_V30_OUTPUT_DIR/manifest.json"
}

write_manifest
python3 benchmarks/check_tls_identity_handoff.py --evidence-dir "$results_dir" --require-manifest \
  --output "$proof_tmp/post-manifest-assertions.json"
cmp "$results_dir/assertions.json" "$proof_tmp/post-manifest-assertions.json"
python3 benchmarks/render_tls_identity_handoff_svg.py --evidence-dir "$results_dir" --output "$proof_tmp/post-manifest-proof.svg"
cmp "$results_dir/tls-identity-handoff-proof.svg" "$proof_tmp/post-manifest-proof.svg"
retain_results
if [[ -n "${INFERLAB_V30_OUTPUT_DIR:-}" ]]; then
  python3 benchmarks/check_tls_identity_handoff.py --evidence-dir "$INFERLAB_V30_OUTPUT_DIR" --require-manifest \
    --output "$proof_tmp/retained-assertions.json"
  cmp "$results_dir/assertions.json" "$proof_tmp/retained-assertions.json"
  python3 benchmarks/render_tls_identity_handoff_svg.py --evidence-dir "$INFERLAB_V30_OUTPUT_DIR" --output "$proof_tmp/retained-proof.svg"
  cmp "$results_dir/tls-identity-handoff-proof.svg" "$proof_tmp/retained-proof.svg"
fi
python3 - "$results_dir/assertions.json" <<'PY'
import json,sys
report=json.load(open(sys.argv[1]))
print(f"v0.30 restart-free TLS identity handoff proof complete: {report['passed']}/{report['total']} assertions passed")
PY
proof_succeeded=1
