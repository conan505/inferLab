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

run_policy() {
  local policy="$1"
  local output="$2"

  INFERLAB_BIND=127.0.0.1:8180 \
  INFERLAB_ROUTING_POLICY="$policy" \
  INFERLAB_WORKERS='worker-a=http://127.0.0.1:9101,worker-b=http://127.0.0.1:9102,worker-c=http://127.0.0.1:9103' \
    target/debug/gateway >"$proof_tmp/gateway-$policy.log" 2>&1 &
  gateway_pid="$!"
  process_ids+=("$gateway_pid")
  wait_for_health http://127.0.0.1:8180/health

  python3 benchmarks/smoke.py \
    --url http://127.0.0.1:8180/v1/chat/completions \
    --requests 90 \
    --concurrency 12 \
    --label "$policy-unequal-workers" >"$output"

  kill "$gateway_pid" 2>/dev/null || true
  wait "$gateway_pid" 2>/dev/null || true
  sleep 0.05
}

cargo build --workspace --quiet

for number in 1 2 3; do
  case "$number" in
    1) worker_id="worker-a"; token_delay=5 ;;
    2) worker_id="worker-b"; token_delay=5 ;;
    3) worker_id="worker-c"; token_delay=50 ;;
  esac
  FAKE_WORKER_ID="$worker_id" \
  FAKE_WORKER_BIND="127.0.0.1:910$number" \
  FAKE_WORKER_INITIAL_DELAY_MS=5 \
  FAKE_WORKER_TOKEN_DELAY_MS="$token_delay" \
    target/debug/fake-worker >"$proof_tmp/$worker_id.log" 2>&1 &
  process_ids+=("$!")
done

wait_for_health http://127.0.0.1:9101/health
wait_for_health http://127.0.0.1:9102/health
wait_for_health http://127.0.0.1:9103/health

round_robin_result="$results_dir/round-robin.json"
least_in_flight_result="$results_dir/least-in-flight.json"
comparison_result="$results_dir/comparison.json"

echo "Running round-robin against two fast workers and one slow worker..."
run_policy round-robin "$round_robin_result"

echo "Running least-in-flight against the same workers..."
run_policy least-in-flight "$least_in_flight_result"

python3 benchmarks/compare_routing.py \
  --round-robin "$round_robin_result" \
  --least-in-flight "$least_in_flight_result" | tee "$comparison_result"

echo
echo "v0.0.2 proof passed"
echo "Raw results: $results_dir"
