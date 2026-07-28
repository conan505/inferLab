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
  local fail_every="${1:-}"
  if [[ -n "$fail_every" ]]; then
    FAKE_WORKER_ID=worker-a \
    FAKE_WORKER_BIND=127.0.0.1:9501 \
    FAKE_WORKER_INITIAL_DELAY_MS=5 \
    FAKE_WORKER_TOKEN_DELAY_MS=0 \
    FAKE_WORKER_FAIL_EVERY="$fail_every" \
      target/debug/fake-worker >"$proof_tmp/worker-a-$fail_every.log" 2>&1 &
  else
    FAKE_WORKER_ID=worker-a \
    FAKE_WORKER_BIND=127.0.0.1:9501 \
    FAKE_WORKER_INITIAL_DELAY_MS=5 \
    FAKE_WORKER_TOKEN_DELAY_MS=0 \
      target/debug/fake-worker >"$proof_tmp/worker-a-healthy.log" 2>&1 &
  fi
  worker_a_pid="$!"
  process_ids+=("$worker_a_pid")
  wait_for_health http://127.0.0.1:9501/health
}

cargo build --workspace --quiet

start_worker_a
FAKE_WORKER_ID=worker-b \
FAKE_WORKER_BIND=127.0.0.1:9502 \
FAKE_WORKER_INITIAL_DELAY_MS=5 \
FAKE_WORKER_TOKEN_DELAY_MS=0 \
  target/debug/fake-worker >"$proof_tmp/worker-b.log" 2>&1 &
process_ids+=("$!")

INFERLAB_BIND=127.0.0.1:8580 \
INFERLAB_ROUTING_POLICY=round-robin \
INFERLAB_REQUEST_DEADLINE_MS=1000 \
INFERLAB_ATTEMPT_TIMEOUT_MS=300 \
INFERLAB_MAX_RETRIES=2 \
INFERLAB_RETRY_BUDGET_PERCENT=10 \
INFERLAB_RETRY_BASE_DELAY_MS=10 \
INFERLAB_RETRY_MAX_DELAY_MS=40 \
INFERLAB_JITTER_SEED=42 \
INFERLAB_WORKERS='worker-a=http://127.0.0.1:9501,worker-b=http://127.0.0.1:9502' \
  target/debug/gateway >"$proof_tmp/retry-gateway.log" 2>&1 &
process_ids+=("$!")

wait_for_health http://127.0.0.1:9502/health
wait_for_health http://127.0.0.1:8580/health

for request_number in {1..10}; do
  curl --fail --silent http://127.0.0.1:8580/v1/chat/completions \
    -H 'content-type: application/json' \
    -d "{\"stream\":false,\"messages\":[{\"role\":\"user\",\"content\":\"warmup $request_number\"}]}" \
    >/dev/null
done

kill "$worker_a_pid"
wait "$worker_a_pid" 2>/dev/null || true
start_worker_a 1

retry_result="$results_dir/retry-budget.json"
deadline_result="$results_dir/deadline.json"
jitter_result="$results_dir/jitter-simulation.json"
check_result="$results_dir/resilience-check.json"
graph_result="$results_dir/retry-jitter.svg"

python3 benchmarks/resilience_probe.py \
  --url http://127.0.0.1:8580/v1/chat/completions \
  --status-url http://127.0.0.1:8580/internal/workers \
  --worker-health http://127.0.0.1:9501/health \
  --worker-health http://127.0.0.1:9502/health \
  --requests 10 \
  --label retry-budget >"$retry_result"

FAKE_WORKER_ID=worker-deadline \
FAKE_WORKER_BIND=127.0.0.1:9510 \
FAKE_WORKER_INITIAL_DELAY_MS=500 \
FAKE_WORKER_TOKEN_DELAY_MS=0 \
  target/debug/fake-worker >"$proof_tmp/worker-deadline.log" 2>&1 &
process_ids+=("$!")

INFERLAB_BIND=127.0.0.1:8581 \
INFERLAB_REQUEST_DEADLINE_MS=180 \
INFERLAB_ATTEMPT_TIMEOUT_MS=1000 \
INFERLAB_MAX_RETRIES=0 \
INFERLAB_RETRY_BUDGET_PERCENT=10 \
INFERLAB_WORKERS='worker-deadline=http://127.0.0.1:9510' \
  target/debug/gateway >"$proof_tmp/deadline-gateway.log" 2>&1 &
process_ids+=("$!")

wait_for_health http://127.0.0.1:9510/health
wait_for_health http://127.0.0.1:8581/health

python3 benchmarks/resilience_probe.py \
  --url http://127.0.0.1:8581/v1/chat/completions \
  --status-url http://127.0.0.1:8581/internal/workers \
  --worker-health http://127.0.0.1:9510/health \
  --requests 1 \
  --label request-deadline >"$deadline_result"

target/debug/retry-simulate >"$jitter_result"
python3 benchmarks/check_resilience.py \
  --retry "$retry_result" \
  --deadline "$deadline_result" \
  --jitter "$jitter_result" | tee "$check_result"
python3 benchmarks/render_retry_jitter_svg.py \
  --analysis "$jitter_result" \
  --output "$graph_result"

echo
echo "v0.0.7 proof passed"
echo "Raw results: $results_dir"
