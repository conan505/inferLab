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

cargo build --workspace --quiet

for number in 1 2; do
  case "$number" in
    1) worker_id="worker-a" ;;
    2) worker_id="worker-b" ;;
  esac
  FAKE_WORKER_ID="$worker_id" \
  FAKE_WORKER_BIND="127.0.0.1:920$number" \
  FAKE_WORKER_INITIAL_DELAY_MS=5 \
  FAKE_WORKER_TOKEN_DELAY_MS=5 \
    target/debug/fake-worker >"$proof_tmp/$worker_id.log" 2>&1 &
  process_ids+=("$!")
done

INFERLAB_BIND=127.0.0.1:8280 \
INFERLAB_ROUTING_POLICY=weighted \
INFERLAB_WORKERS='worker-a:3=http://127.0.0.1:9201,worker-b:1=http://127.0.0.1:9202' \
  target/debug/gateway >"$proof_tmp/gateway.log" 2>&1 &
process_ids+=("$!")

wait_for_health http://127.0.0.1:9201/health
wait_for_health http://127.0.0.1:9202/health
wait_for_health http://127.0.0.1:8280/health

curl --fail --silent http://127.0.0.1:8280/internal/workers \
  >"$results_dir/worker-status.json"

benchmark_result="$results_dir/weighted-3-to-1.json"
check_result="$results_dir/weighted-check.json"

echo "Sending 80 requests with configured weights A=3 and B=1..."
python3 benchmarks/smoke.py \
  --url http://127.0.0.1:8280/v1/chat/completions \
  --requests 80 \
  --concurrency 8 \
  --label weighted-3-to-1 >"$benchmark_result"

python3 benchmarks/check_weighted.py \
  --result "$benchmark_result" \
  --expected worker-a=3 \
  --expected worker-b=1 | tee "$check_result"

echo
echo "v0.0.3 proof passed"
echo "Raw results: $results_dir"
