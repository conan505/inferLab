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
queue_pid=0

is_owned_child() {
  local pid="$1"
  local parent
  [[ "$pid" =~ ^[1-9][0-9]*$ ]] || return 1
  parent="$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d '[:space:]')"
  [[ "$parent" == "$$" ]]
}

stop_queue() {
  if ! is_owned_child "$queue_pid"; then
    echo "refusing to stop queue: PID $queue_pid is not a live child of this harness" >&2
    return 1
  fi
  kill "$queue_pid"
  wait "$queue_pid" 2>/dev/null || true
  queue_pid=0
}

cleanup() {
  if is_owned_child "$queue_pid"; then
    kill "$queue_pid" 2>/dev/null || true
    wait "$queue_pid" 2>/dev/null || true
  fi
  rm -rf "$proof_tmp"
}
trap cleanup EXIT INT TERM

check_port_is_free() {
  python3 - <<'PY'
import socket

with socket.socket() as probe:
    probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        probe.bind(("127.0.0.1", 9790))
    except OSError as error:
        raise SystemExit(
            f"refusing to start batch proof: 127.0.0.1:9790 is busy: {error}"
        )
PY
}

wait_for_health() {
  local attempts=0
  until curl --fail --silent http://127.0.0.1:9790/healthz >/dev/null; do
    attempts=$((attempts + 1))
    if ((attempts >= 100)); then
      echo "timed out waiting for durable batch queue" >&2
      return 1
    fi
    sleep 0.05
  done
}

start_queue() {
  local log_path="$1"
  INFERLAB_BATCH_BIND=127.0.0.1:9790 \
  INFERLAB_BATCH_WAL="$proof_tmp/queue-events.wal.jsonl" \
  RUST_LOG=info \
    target/debug/batch-queue >"$log_path" 2>&1 &
  queue_pid="$!"
  wait_for_health
}

check_port_is_free
cargo build --workspace --quiet

before_result="$results_dir/before-crash.json"
after_result="$results_dir/after-restart.json"
wal_result="$results_dir/queue-events.wal.jsonl"
check_result="$results_dir/batch-check.json"
chart_result="$results_dir/batch-state.svg"

start_queue "$proof_tmp/queue-before-crash.log"
python3 benchmarks/batch_queue_probe.py \
  --base-url http://127.0.0.1:9790 \
  --phase prepare \
  --effect-ledger "$proof_tmp/effect-ledger.sqlite" >"$before_result"

# This is a real process boundary: memory disappears, while the synced WAL is
# reused by a fresh queue process.
stop_queue
start_queue "$proof_tmp/queue-after-restart.log"
sleep 0.75

python3 benchmarks/batch_queue_probe.py \
  --base-url http://127.0.0.1:9790 \
  --phase recover \
  --before "$before_result" \
  --effect-ledger "$proof_tmp/effect-ledger.sqlite" >"$after_result"
stop_queue

cp "$proof_tmp/queue-events.wal.jsonl" "$wal_result"
python3 benchmarks/check_batch_queue.py \
  --before "$before_result" \
  --after "$after_result" \
  --wal "$wal_result" >"$check_result"
python3 benchmarks/render_batch_queue_svg.py \
  --check "$check_result" \
  --wal "$wal_result" \
  --output "$chart_result"

python3 - "$check_result" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    report = json.load(source)
print(
    "v0.5 durable batch proof passed: "
    f"{report['final_status']['wal_events']} durable transitions, "
    f"{report['final_status']['redeliveries_total']} redeliveries, "
    f"{report['final_status']['dead_letter']} dead-lettered job"
)
PY
