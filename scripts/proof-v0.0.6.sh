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

FAKE_WORKER_ID=worker-a \
FAKE_WORKER_BIND=127.0.0.1:9401 \
FAKE_WORKER_INITIAL_DELAY_MS=250 \
FAKE_WORKER_TOKEN_DELAY_MS=0 \
  target/debug/fake-worker >"$proof_tmp/worker-a.log" 2>&1 &
process_ids+=("$!")

INFERLAB_BIND=127.0.0.1:8480 \
INFERLAB_ROUTING_POLICY=round-robin \
INFERLAB_WORKER_CONCURRENCY=2 \
INFERLAB_ADMISSION_QUEUE_CAPACITY=4 \
INFERLAB_WORKERS='worker-a=http://127.0.0.1:9401' \
  target/debug/gateway >"$proof_tmp/gateway.log" 2>&1 &
gateway_pid="$!"
process_ids+=("$gateway_pid")

wait_for_health http://127.0.0.1:9401/health
wait_for_health http://127.0.0.1:8480/health

# Warm connection pools and allocators before treating the first RSS sample as a baseline.
curl --fail --silent http://127.0.0.1:8480/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"stream":false,"messages":[{"role":"user","content":"warm up"}]}' >/dev/null

analysis_result="$results_dir/overload-analysis.json"
check_result="$results_dir/overload-check.json"
graph_result="$results_dir/backpressure-timeseries.svg"

python3 benchmarks/overload.py \
  --url http://127.0.0.1:8480/v1/chat/completions \
  --status-url http://127.0.0.1:8480/internal/workers \
  --requests 160 \
  --offered-rate-rps 40 \
  --estimated-capacity-rps 8 \
  --gateway-pid "$gateway_pid" >"$analysis_result"

python3 benchmarks/check_backpressure.py \
  --analysis "$analysis_result" | tee "$check_result"
python3 benchmarks/render_backpressure_svg.py \
  --analysis "$analysis_result" \
  --output "$graph_result"

echo
echo "v0.0.6 proof passed"
echo "Raw results: $results_dir"
