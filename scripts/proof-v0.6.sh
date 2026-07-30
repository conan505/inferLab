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
results_dir="${INFERLAB_RESULTS_DIR:-$proof_tmp/results}"
mkdir -p "$results_dir"
live_pids=(0)
started_pid=0
node_a_pid=0
node_b_pid=0
node_c_pid=0

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
                f"refusing to start Raft proof: 127.0.0.1:{port} is busy: {error}"
            )
PY
}

wait_for_health() {
  local url="$1"
  local attempts=0
  until curl --fail --silent "$url" >/dev/null; do
    attempts=$((attempts + 1))
    if ((attempts >= 120)); then
      echo "timed out waiting for $url" >&2
      return 1
    fi
    sleep 0.025
  done
}

start_node() {
  local node_id="$1"
  local port="$2"
  local peers="$3"
  local election_min_ms="$4"
  local election_max_ms="$5"
  local log_label="$6"
  mkdir -p "$proof_tmp/$node_id"
  INFERLAB_RAFT_NODE_ID="$node_id" \
  INFERLAB_RAFT_BIND="127.0.0.1:$port" \
  INFERLAB_RAFT_PEERS="$peers" \
  INFERLAB_RAFT_DATA_DIR="$proof_tmp/$node_id" \
  INFERLAB_RAFT_ELECTION_MIN_MS="$election_min_ms" \
  INFERLAB_RAFT_ELECTION_MAX_MS="$election_max_ms" \
  INFERLAB_RAFT_HEARTBEAT_MS=50 \
  INFERLAB_RAFT_RPC_TIMEOUT_MS=100 \
  INFERLAB_RAFT_COMMIT_TIMEOUT_MS=1500 \
  RUST_LOG=info \
    target/debug/control-plane >"$proof_tmp/$log_label.log" 2>&1 &
  started_pid="$!"
  register_pid "$started_pid"
  wait_for_health "http://127.0.0.1:$port/healthz"
}

start_node_by_id() {
  local node_id="$1"
  local log_suffix="$2"
  case "$node_id" in
    node-a)
      start_node node-a 9811 \
        'node-b=http://127.0.0.1:9812,node-c=http://127.0.0.1:9813' \
        180 240 "node-a-$log_suffix"
      node_a_pid="$started_pid"
      ;;
    node-b)
      start_node node-b 9812 \
        'node-a=http://127.0.0.1:9811,node-c=http://127.0.0.1:9813' \
        300 360 "node-b-$log_suffix"
      node_b_pid="$started_pid"
      ;;
    node-c)
      start_node node-c 9813 \
        'node-a=http://127.0.0.1:9811,node-b=http://127.0.0.1:9812' \
        420 480 "node-c-$log_suffix"
      node_c_pid="$started_pid"
      ;;
    *)
      echo "unknown node $node_id" >&2
      return 1
      ;;
  esac
}

pid_for_node() {
  case "$1" in
    node-a) echo "$node_a_pid" ;;
    node-b) echo "$node_b_pid" ;;
    node-c) echo "$node_c_pid" ;;
    *) return 1 ;;
  esac
}

record_fault() {
  local event="$1"
  local node_id="$2"
  local target_pid="$3"
  local term="$4"
  python3 - "$fault_events" "$event" "$node_id" "$target_pid" "$term" <<'PY'
import json
import sys
import time

path, event, node_id, target_pid, term = sys.argv[1:]
record = {
    "at_ms": round(time.time() * 1000, 3),
    "event": event,
    "node_id": node_id,
    "target_pid": int(target_pid),
    "term": int(term),
    "scope": "owned-child-process",
    "bind": "127.0.0.1",
}
with open(path, "a", encoding="utf-8") as destination:
    destination.write(json.dumps(record, sort_keys=True) + "\n")
print(record["at_ms"])
PY
}

json_field() {
  python3 - "$1" "$2" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    value = json.load(source)
for component in sys.argv[2].split("."):
    value = value[component]
print(value)
PY
}

start_worker() {
  local worker_id="$1"
  local port="$2"
  FAKE_WORKER_ID="$worker_id" \
  FAKE_WORKER_BIND="127.0.0.1:$port" \
  FAKE_WORKER_INITIAL_DELAY_MS=5 \
  FAKE_WORKER_TOKEN_DELAY_MS=0 \
    target/debug/fake-worker >"$proof_tmp/$worker_id.log" 2>&1 &
  started_pid="$!"
  register_pid "$started_pid"
  wait_for_health "http://127.0.0.1:$port/health"
}

urls='http://127.0.0.1:9811,http://127.0.0.1:9812,http://127.0.0.1:9813'
fault_events="$results_dir/fault-events.jsonl"
: >"$fault_events"

check_ports_are_free 9811 9812 9813 9820 9821 9822 9823
cargo build --workspace --quiet

start_node_by_id node-a initial
start_node_by_id node-b initial
start_node_by_id node-c initial

python3 benchmarks/raft_probe.py wait-leader \
  --urls "$urls" >"$results_dir/initial-election.json"
initial_leader="$(json_field "$results_dir/initial-election.json" leader_id)"

python3 benchmarks/raft_probe.py write-config \
  --urls "$urls" \
  --policy round-robin \
  --weights 1,1,1 >"$results_dir/config-1-write.json"
revision_1="$(json_field "$results_dir/config-1-write.json" committed.revision)"
python3 benchmarks/raft_probe.py wait-config \
  --urls "$urls" \
  --policy round-robin \
  --expected-nodes 3 \
  --minimum-revision "$revision_1" >"$results_dir/config-1-convergence.json"

start_worker worker-a 9821
start_worker worker-b 9822
start_worker worker-c 9823
INFERLAB_BIND=127.0.0.1:9820 \
INFERLAB_CONTROL_PLANE_URLS="$urls" \
INFERLAB_CONTROL_POLL_MS=50 \
INFERLAB_REQUEST_DEADLINE_MS=1000 \
INFERLAB_ATTEMPT_TIMEOUT_MS=300 \
INFERLAB_MAX_RETRIES=0 \
  target/debug/gateway >"$proof_tmp/gateway.log" 2>&1 &
gateway_pid="$!"
register_pid "$gateway_pid"
wait_for_health http://127.0.0.1:9820/health
python3 benchmarks/raft_probe.py gateway-probe \
  --gateway-url http://127.0.0.1:9820 \
  --expected-policy round-robin \
  --minimum-revision "$revision_1" >"$results_dir/gateway-before.json"

first_leader_pid="$(pid_for_node "$initial_leader")"
first_term="$(json_field "$results_dir/initial-election.json" term)"
first_fault_ms="$(record_fault leader_killed "$initial_leader" "$first_leader_pid" "$first_term")"
stop_owned_process "$first_leader_pid" "$initial_leader"

# The gateway must keep serving from its last committed snapshot while the
# control plane has no leader.
python3 benchmarks/raft_probe.py gateway-probe \
  --gateway-url http://127.0.0.1:9820 \
  --expected-policy round-robin \
  --minimum-revision "$revision_1" >"$results_dir/gateway-election-1.json"
python3 benchmarks/raft_probe.py wait-leader \
  --urls "$urls" \
  --since-ms "$first_fault_ms" >"$results_dir/re-election-1.json"
second_leader="$(json_field "$results_dir/re-election-1.json" leader_id)"

python3 benchmarks/raft_probe.py write-config \
  --urls "$urls" \
  --policy least-in-flight \
  --weights 1,1,1 >"$results_dir/config-2-write.json"
revision_2="$(json_field "$results_dir/config-2-write.json" committed.revision)"
python3 benchmarks/raft_probe.py wait-config \
  --urls "$urls" \
  --policy least-in-flight \
  --expected-nodes 2 \
  --minimum-revision "$revision_2" >"$results_dir/config-2-majority.json"

start_node_by_id "$initial_leader" restarted-after-first-kill
python3 benchmarks/raft_probe.py wait-config \
  --urls "$urls" \
  --policy least-in-flight \
  --expected-nodes 3 \
  --minimum-revision "$revision_2" >"$results_dir/restarted-node-1-caught-up.json"
python3 benchmarks/raft_probe.py gateway-probe \
  --gateway-url http://127.0.0.1:9820 \
  --expected-policy least-in-flight \
  --minimum-revision "$revision_2" >"$results_dir/gateway-after-config-2.json"

second_leader_pid="$(pid_for_node "$second_leader")"
second_term="$(json_field "$results_dir/re-election-1.json" term)"
second_fault_ms="$(record_fault leader_killed "$second_leader" "$second_leader_pid" "$second_term")"
stop_owned_process "$second_leader_pid" "$second_leader"
python3 benchmarks/raft_probe.py gateway-probe \
  --gateway-url http://127.0.0.1:9820 \
  --expected-policy least-in-flight \
  --minimum-revision "$revision_2" >"$results_dir/gateway-election-2.json"
python3 benchmarks/raft_probe.py wait-leader \
  --urls "$urls" \
  --since-ms "$second_fault_ms" >"$results_dir/re-election-2.json"
third_leader="$(json_field "$results_dir/re-election-2.json" leader_id)"

python3 benchmarks/raft_probe.py write-config \
  --urls "$urls" \
  --policy weighted-round-robin \
  --weights 3,1,1 >"$results_dir/config-3-write.json"
revision_3="$(json_field "$results_dir/config-3-write.json" committed.revision)"
python3 benchmarks/raft_probe.py wait-config \
  --urls "$urls" \
  --policy weighted-round-robin \
  --expected-nodes 2 \
  --minimum-revision "$revision_3" >"$results_dir/config-3-majority.json"

start_node_by_id "$second_leader" restarted-after-second-kill
python3 benchmarks/raft_probe.py wait-config \
  --urls "$urls" \
  --policy weighted-round-robin \
  --expected-nodes 3 \
  --minimum-revision "$revision_3" >"$results_dir/final-convergence.json"
python3 benchmarks/raft_probe.py gateway-probe \
  --gateway-url http://127.0.0.1:9820 \
  --expected-policy weighted-round-robin \
  --minimum-revision "$revision_3" \
  --requests 10 >"$results_dir/gateway-final.json"

for node_id in node-a node-b node-c; do
  cp "$proof_tmp/$node_id/state.json" "$results_dir/$node_id-state.json"
  cp "$proof_tmp/$node_id/events.jsonl" "$results_dir/$node_id-events.jsonl"
done

python3 benchmarks/analyze_raft.py \
  --results "$results_dir" >"$results_dir/raft-analysis.json"
python3 benchmarks/check_raft.py \
  --analysis "$results_dir/raft-analysis.json" >"$results_dir/raft-check.json"
python3 benchmarks/render_raft_svg.py \
  --analysis "$results_dir/raft-analysis.json" \
  --output "$results_dir/raft-timeline.svg"

python3 - "$results_dir/raft-check.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    report = json.load(source)
print(
    "v0.6 Raft proof passed: "
    f"{report['terms_observed']} leadership terms, "
    f"re-elections {report['reelection_latencies_ms']} ms, "
    f"final revision {report['final_revision']} on 3/3 nodes"
)
PY
