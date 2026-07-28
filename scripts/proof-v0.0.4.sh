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
  local initial_delay="$1"
  FAKE_WORKER_ID=worker-a \
  FAKE_WORKER_BIND=127.0.0.1:9301 \
  FAKE_WORKER_INITIAL_DELAY_MS="$initial_delay" \
  FAKE_WORKER_TOKEN_DELAY_MS=5 \
    target/debug/fake-worker >"$proof_tmp/worker-a-$initial_delay.log" 2>&1 &
  worker_a_pid="$!"
  process_ids+=("$worker_a_pid")
  wait_for_health http://127.0.0.1:9301/health
}

cargo build --workspace --quiet

FAKE_WORKER_ID=worker-b \
FAKE_WORKER_BIND=127.0.0.1:9302 \
FAKE_WORKER_INITIAL_DELAY_MS=25 \
FAKE_WORKER_TOKEN_DELAY_MS=5 \
  target/debug/fake-worker >"$proof_tmp/worker-b.log" 2>&1 &
process_ids+=("$!")

start_worker_a 5

INFERLAB_BIND=127.0.0.1:8380 \
INFERLAB_ROUTING_POLICY=ewma \
INFERLAB_EWMA_ALPHA=0.5 \
INFERLAB_EWMA_PROBE_INTERVAL=5 \
INFERLAB_WORKERS='worker-a=http://127.0.0.1:9301,worker-b=http://127.0.0.1:9302' \
  target/debug/gateway >"$proof_tmp/gateway.log" 2>&1 &
process_ids+=("$!")

wait_for_health http://127.0.0.1:9302/health
wait_for_health http://127.0.0.1:8380/health

warmup_result="$results_dir/warmup-a-fast.json"
after_result="$results_dir/after-a-slow.json"
status_before="$results_dir/status-before.json"
status_after="$results_dir/status-after.json"
check_result="$results_dir/adaptation-check.json"

echo "Warm-up: A starts faster than B..."
python3 benchmarks/smoke.py \
  --url http://127.0.0.1:8380/v1/chat/completions \
  --requests 20 \
  --concurrency 1 \
  --label ewma-warmup-a-fast >"$warmup_result"
curl --fail --silent http://127.0.0.1:8380/internal/workers >"$status_before"

echo "Slowdown: restarting A with 100 ms initial latency..."
kill "$worker_a_pid"
wait "$worker_a_pid" 2>/dev/null || true
start_worker_a 100

python3 benchmarks/smoke.py \
  --url http://127.0.0.1:8380/v1/chat/completions \
  --requests 40 \
  --concurrency 1 \
  --label ewma-after-a-slowdown >"$after_result"
curl --fail --silent http://127.0.0.1:8380/internal/workers >"$status_after"

python3 benchmarks/check_ewma.py \
  --warmup "$warmup_result" \
  --after-slowdown "$after_result" \
  --status-before "$status_before" \
  --status-after "$status_after" | tee "$check_result"

echo
echo "v0.0.4 proof passed"
echo "Raw results: $results_dir"
