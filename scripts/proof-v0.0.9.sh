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
# Bash 3.2 with `set -u` treats an empty array expansion as unbound. PID zero is
# an inert sentinel that keeps cleanup safe before the first child is started.
live_pids=(0)
started_pid=""

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
        # Match ordinary server restart behavior: stale TIME_WAIT sockets are
        # harmless, while an active listener still makes this bind fail.
        probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            probe.bind(("127.0.0.1", port))
        except OSError as error:
            raise SystemExit(
                f"refusing to start chaos proof: 127.0.0.1:{port} is busy: {error}"
            )
PY
}

wait_for_health() {
  local url="$1"
  local attempts=0
  until curl --fail --silent "$url" >/dev/null; do
    attempts=$((attempts + 1))
    if ((attempts >= 100)); then
      echo "timed out waiting for $url" >&2
      return 1
    fi
    sleep 0.05
  done
}

start_worker() {
  local worker_id="$1"
  local port="$2"
  local initial_delay_ms="$3"
  local log_label="$4"
  FAKE_WORKER_ID="$worker_id" \
  FAKE_WORKER_BIND="127.0.0.1:$port" \
  FAKE_WORKER_INITIAL_DELAY_MS="$initial_delay_ms" \
  FAKE_WORKER_TOKEN_DELAY_MS=0 \
    target/debug/fake-worker >"$proof_tmp/$log_label.log" 2>&1 &
  started_pid="$!"
  register_pid "$started_pid"
  wait_for_health "http://127.0.0.1:$port/health"
}

wait_until_elapsed() {
  local target_seconds="$1"
  python3 - "$experiment_started_epoch_ms" "$target_seconds" <<'PY'
import sys
import time

started = float(sys.argv[1]) / 1000
target = started + float(sys.argv[2])
time.sleep(max(0.0, target - time.time()))
PY
}

record_event() {
  local event="$1"
  local worker="$2"
  local action="$3"
  local mode="$4"
  local target_pid="$5"
  python3 - \
    "$events_result" \
    "$experiment_started_epoch_ms" \
    "$event" \
    "$worker" \
    "$action" \
    "$mode" \
    "$target_pid" <<'PY'
import json
import sys
import time

path, started, event, worker, action, mode, raw_pid = sys.argv[1:]
target_pid = int(raw_pid)
record = {
    "elapsed_ms": (
        0.0
        if event == "traffic_started"
        else round(time.time() * 1000 - float(started), 3)
    ),
    "event": event,
    "worker": worker or None,
    "action": action,
    "mode": mode,
    "target_pid": target_pid,
    "scope": "owned-child-process" if target_pid > 0 else "harness",
    "bind": "127.0.0.1",
}
with open(path, "a", encoding="utf-8") as destination:
    destination.write(json.dumps(record, sort_keys=True) + "\n")
PY
}

check_ports_are_free 9780 9701 9702 9703
cargo build --workspace --quiet

start_worker worker-a 9701 12 worker-a-initial
worker_a_pid="$started_pid"
start_worker worker-b 9702 12 worker-b-initial
worker_b_pid="$started_pid"
start_worker worker-c 9703 12 worker-c-initial
worker_c_pid="$started_pid"

INFERLAB_BIND=127.0.0.1:9780 \
INFERLAB_ROUTING_POLICY=round-robin \
INFERLAB_WORKER_CONCURRENCY=2 \
INFERLAB_ADMISSION_QUEUE_CAPACITY=6 \
INFERLAB_REQUEST_DEADLINE_MS=700 \
INFERLAB_ATTEMPT_TIMEOUT_MS=150 \
INFERLAB_MAX_RETRIES=1 \
INFERLAB_RETRY_BUDGET_PERCENT=10 \
INFERLAB_RETRY_BASE_DELAY_MS=10 \
INFERLAB_RETRY_MAX_DELAY_MS=40 \
INFERLAB_JITTER_SEED=9009 \
INFERLAB_CIRCUIT_WINDOW_SIZE=4 \
INFERLAB_CIRCUIT_MIN_REQUESTS=4 \
INFERLAB_CIRCUIT_FAILURE_RATE_PERCENT=50 \
INFERLAB_CIRCUIT_OPEN_MS=700 \
INFERLAB_WORKERS='worker-a=http://127.0.0.1:9701,worker-b=http://127.0.0.1:9702,worker-c=http://127.0.0.1:9703' \
  target/debug/gateway >"$proof_tmp/gateway.log" 2>&1 &
gateway_pid="$!"
register_pid "$gateway_pid"
wait_for_health http://127.0.0.1:9780/health

run_result="$results_dir/chaos-run.json"
events_result="$results_dir/events.jsonl"
analysis_result="$results_dir/chaos-analysis.json"
check_result="$results_dir/chaos-check.json"
graph_result="$results_dir/chaos-recovery.svg"
ready_file="$proof_tmp/chaos-ready.json"
: >"$events_result"

python3 benchmarks/chaos_probe.py \
  --url http://127.0.0.1:9780/v1/chat/completions \
  --status-url http://127.0.0.1:9780/internal/workers \
  --duration-seconds 18 \
  --offered-rate-rps 18 \
  --request-timeout 2 \
  --status-interval 0.1 \
  --gateway-pid "$gateway_pid" \
  --ready-file "$ready_file" >"$run_result" &
probe_pid="$!"
register_pid "$probe_pid"

ready_attempts=0
until [[ -s "$ready_file" ]]; do
  ready_attempts=$((ready_attempts + 1))
  if ((ready_attempts >= 100)); then
    echo "timed out waiting for chaos probe readiness" >&2
    exit 1
  fi
  sleep 0.02
done
experiment_started_epoch_ms="$(
  python3 - "$ready_file" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    print(json.load(source)["started_epoch_ms"])
PY
)"
record_event traffic_started "" start continuous-open-loop 0

wait_until_elapsed 2.0
record_event worker_a_killed worker-a terminate unavailable "$worker_a_pid"
stop_owned_process "$worker_a_pid" worker-a

wait_until_elapsed 4.5
start_worker worker-a 9701 12 worker-a-restarted
worker_a_pid="$started_pid"
record_event worker_a_restarted worker-a start healthy "$worker_a_pid"

wait_until_elapsed 7.0
record_event worker_b_slowed worker-b restart slow-response "$worker_b_pid"
stop_owned_process "$worker_b_pid" worker-b
start_worker worker-b 9702 350 worker-b-slow
worker_b_pid="$started_pid"

wait_until_elapsed 9.5
stop_owned_process "$worker_b_pid" worker-b-slow
start_worker worker-b 9702 12 worker-b-restored
worker_b_pid="$started_pid"
record_event worker_b_restored worker-b restart healthy "$worker_b_pid"

wait_until_elapsed 12.0
record_event worker_c_disconnected worker-c terminate connection-refused "$worker_c_pid"
stop_owned_process "$worker_c_pid" worker-c

wait_until_elapsed 14.5
start_worker worker-c 9703 12 worker-c-reconnected
worker_c_pid="$started_pid"
record_event worker_c_reconnected worker-c start healthy "$worker_c_pid"

wait "$probe_pid"
unregister_pid "$probe_pid"
record_event traffic_completed "" stop continuous-open-loop 0

python3 benchmarks/analyze_chaos.py \
  --run "$run_result" \
  --events "$events_result" >"$analysis_result"
python3 benchmarks/check_chaos.py \
  --analysis "$analysis_result" | tee "$check_result"
python3 benchmarks/render_chaos_svg.py \
  --analysis "$analysis_result" \
  --output "$graph_result"

echo
echo "v0.0.9 proof passed"
echo "Raw results: $results_dir"
