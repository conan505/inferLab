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

for number in 1 2 3; do
  case "$number" in
    1) worker_id="worker-a" ;;
    2) worker_id="worker-b" ;;
    3) worker_id="worker-c" ;;
  esac
  FAKE_WORKER_ID="$worker_id" \
  FAKE_WORKER_BIND="127.0.0.1:900$number" \
  FAKE_WORKER_INITIAL_DELAY_MS=10 \
  FAKE_WORKER_TOKEN_DELAY_MS=10 \
    target/debug/fake-worker >"$proof_tmp/$worker_id.log" 2>&1 &
  process_ids+=("$!")
done

INFERLAB_WORKERS='worker-a=http://127.0.0.1:9001,worker-b=http://127.0.0.1:9002,worker-c=http://127.0.0.1:9003' \
  target/debug/gateway >"$proof_tmp/gateway.log" 2>&1 &
process_ids+=("$!")

wait_for_health http://127.0.0.1:9001/health
wait_for_health http://127.0.0.1:9002/health
wait_for_health http://127.0.0.1:9003/health
wait_for_health http://127.0.0.1:8080/health

expected=(worker-a worker-b worker-c worker-a)
echo "Round-robin proof:"
for index in 0 1 2 3; do
  headers="$proof_tmp/headers-$index"
  body="$proof_tmp/body-$index"
  curl --fail --silent --show-error --no-buffer \
    --dump-header "$headers" \
    --output "$body" \
    http://127.0.0.1:8080/v1/chat/completions \
    -H 'content-type: application/json' \
    -d '{"model":"inferlab-fake","stream":true,"messages":[{"role":"user","content":"prove the serving path"}]}'

  selected="$(awk 'tolower($1) == "x-inferlab-worker:" {gsub("\r", "", $2); print $2}' "$headers")"
  if [[ "$selected" != "${expected[$index]}" ]]; then
    echo "request $((index + 1)): expected ${expected[$index]}, got $selected" >&2
    exit 1
  fi
  if ! grep -q 'data: \[DONE\]' "$body"; then
    echo "request $((index + 1)): missing [DONE] sentinel" >&2
    exit 1
  fi
  echo "  request $((index + 1)) -> $selected (stream ended with [DONE])"
done

echo
echo "Worker state after completed streams:"
curl --fail --silent http://127.0.0.1:8080/internal/workers
echo

echo
echo "Benchmark proof:"
if [[ -n "${INFERLAB_BENCHMARK_OUTPUT:-}" ]]; then
  mkdir -p "$(dirname "$INFERLAB_BENCHMARK_OUTPUT")"
  python3 benchmarks/smoke.py --requests 12 --concurrency 3 | tee "$INFERLAB_BENCHMARK_OUTPUT"
else
  python3 benchmarks/smoke.py --requests 12 --concurrency 3
fi

echo
echo "v0.1 proof passed"
