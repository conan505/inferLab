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
process_ids=()

cleanup() {
  if ((${#process_ids[@]})); then
    kill "${process_ids[@]}" 2>/dev/null || true
    wait "${process_ids[@]}" 2>/dev/null || true
  fi
  rm -rf "$proof_tmp"
}
trap cleanup EXIT INT TERM

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

start_worker_a() {
  local failure_mode="$1"
  if [[ "$failure_mode" == "failing" ]]; then
    FAKE_WORKER_ID=worker-a \
    FAKE_WORKER_BIND=127.0.0.1:9601 \
    FAKE_WORKER_INITIAL_DELAY_MS=3 \
    FAKE_WORKER_TOKEN_DELAY_MS=0 \
    FAKE_WORKER_FAIL_EVERY=1 \
      target/debug/fake-worker >"$proof_tmp/worker-a-failing.log" 2>&1 &
  else
    FAKE_WORKER_ID=worker-a \
    FAKE_WORKER_BIND=127.0.0.1:9601 \
    FAKE_WORKER_INITIAL_DELAY_MS=3 \
    FAKE_WORKER_TOKEN_DELAY_MS=0 \
      target/debug/fake-worker >"$proof_tmp/worker-a-healthy.log" 2>&1 &
  fi
  worker_a_pid="$!"
  process_ids+=("$worker_a_pid")
  wait_for_health http://127.0.0.1:9601/health
}

run_phase() {
  local label="$1"
  local requests="$2"
  local output="$3"
  python3 benchmarks/circuit_breaker_probe.py \
    --url http://127.0.0.1:8680/v1/chat/completions \
    --status-url http://127.0.0.1:8680/internal/workers \
    --worker-health http://127.0.0.1:9601/health \
    --worker-health http://127.0.0.1:9602/health \
    --requests "$requests" \
    --label "$label" >"$output"
}

cargo build --workspace --quiet

start_worker_a failing
FAKE_WORKER_ID=worker-b \
FAKE_WORKER_BIND=127.0.0.1:9602 \
FAKE_WORKER_INITIAL_DELAY_MS=3 \
FAKE_WORKER_TOKEN_DELAY_MS=0 \
  target/debug/fake-worker >"$proof_tmp/worker-b.log" 2>&1 &
process_ids+=("$!")

INFERLAB_BIND=127.0.0.1:8680 \
INFERLAB_ROUTING_POLICY=round-robin \
INFERLAB_REQUEST_DEADLINE_MS=1000 \
INFERLAB_ATTEMPT_TIMEOUT_MS=300 \
INFERLAB_MAX_RETRIES=1 \
INFERLAB_RETRY_BUDGET_PERCENT=10 \
INFERLAB_CIRCUIT_WINDOW_SIZE=4 \
INFERLAB_CIRCUIT_MIN_REQUESTS=4 \
INFERLAB_CIRCUIT_FAILURE_RATE_PERCENT=50 \
INFERLAB_CIRCUIT_OPEN_MS=300 \
INFERLAB_WORKERS='worker-a=http://127.0.0.1:9601,worker-b=http://127.0.0.1:9602' \
  target/debug/gateway >"$proof_tmp/gateway.log" 2>&1 &
process_ids+=("$!")

wait_for_health http://127.0.0.1:9602/health
wait_for_health http://127.0.0.1:8680/health

trip_result="$results_dir/trip.json"
open_result="$results_dir/open.json"
half_open_result="$results_dir/half-open-status.json"
probe_result="$results_dir/probe.json"
recovered_result="$results_dir/recovered.json"
check_result="$results_dir/circuit-check.json"
graph_result="$results_dir/circuit-recovery.svg"

run_phase trip 8 "$trip_result"
run_phase open 4 "$open_result"

kill "$worker_a_pid"
wait "$worker_a_pid" 2>/dev/null || true
start_worker_a healthy
sleep 0.35
curl --fail --silent http://127.0.0.1:8680/internal/workers >"$half_open_result"

run_phase probe 1 "$probe_result"
run_phase recovered 4 "$recovered_result"

python3 benchmarks/check_circuit_breaker.py \
  --trip "$trip_result" \
  --open "$open_result" \
  --half-open "$half_open_result" \
  --probe "$probe_result" \
  --recovered "$recovered_result" | tee "$check_result"
python3 benchmarks/render_circuit_breaker_svg.py \
  --analysis "$check_result" \
  --output "$graph_result"

echo
echo "v0.0.8 proof passed"
echo "Raw results: $results_dir"
