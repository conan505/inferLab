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
    if ((attempts >= 120)); then
      echo "timed out waiting for $url" >&2
      return 1
    fi
    sleep 0.05
  done
}

oracle_python="${INFERLAB_ORACLE_PYTHON:-}"
if [[ -z "$oracle_python" && -x "$project_root/.tools/v0.7-python/bin/python" ]]; then
  oracle_python="$project_root/.tools/v0.7-python/bin/python"
fi
if [[ -z "$oracle_python" ]]; then
  oracle_python="$(command -v python3)"
fi
if ! "$oracle_python" -c 'import torch' >/dev/null 2>&1; then
  echo "v0.8 proof requires PyTorch 2.2.2 or compatible." >&2
  echo "Create .tools/v0.7-python and install torch, or set INFERLAB_ORACLE_PYTHON." >&2
  exit 1
fi

echo "Building the v0.8 workspace..."
cargo build --workspace --quiet

prompt_names=("teach-streaming" "unknown-words" "known-words")
prompts=("teach me streaming" "why does inference matter" "hello systems")
kv_parity_paths=()
torch_parity_paths=()
for index in "${!prompts[@]}"; do
  name="${prompt_names[$index]}"
  prompt="${prompts[$index]}"
  recompute_output="$proof_tmp/recompute-$name.json"
  cached_output="$proof_tmp/cached-$name.json"
  kv_parity_output="$proof_tmp/kv-parity-$name.json"
  torch_output="$proof_tmp/torch-$name.json"
  torch_parity_output="$proof_tmp/torch-parity-$name.json"

  target/debug/inferlab-cpu-cli \
    --model models/tiny-inferlab-v1.bin \
    --prompt "$prompt" \
    --max-tokens 8 \
    --mode recompute \
    --repetitions 101 \
    --output "$recompute_output"
  target/debug/inferlab-cpu-cli \
    --model models/tiny-inferlab-v1.bin \
    --prompt "$prompt" \
    --max-tokens 8 \
    --mode kv-cache \
    --repetitions 101 \
    --output "$cached_output"
  python3 benchmarks/compare_kv_cache.py \
    --recompute "$recompute_output" \
    --cached "$cached_output" \
    --output "$kv_parity_output"

  "$oracle_python" oracle/torch_reference.py \
    --model models/tiny-inferlab-v1.bin \
    --prompt "$prompt" \
    --max-tokens 8 \
    --repetitions 31 \
    --output "$torch_output"
  python3 benchmarks/compare_cpu_decoder.py \
    --cpp "$cached_output" \
    --torch "$torch_output" \
    --output "$torch_parity_output"
  kv_parity_paths+=("$kv_parity_output")
  torch_parity_paths+=("$torch_parity_output")
done

echo "Starting one-slot and four-slot KV-cached workers..."
INFERLAB_CPU_BIND=127.0.0.1:9941 \
INFERLAB_CPU_WORKER_ID=cpu-one-slot \
INFERLAB_MODEL_PATH=models/tiny-inferlab-v1.bin \
INFERLAB_CPU_DECODER_MODE=kv-cache \
INFERLAB_CPU_MAX_BATCH_SIZE=1 \
INFERLAB_CPU_BATCH_TICK_MS=3 \
  target/debug/cpu-worker >"$proof_tmp/one-slot-worker.log" 2>&1 &
process_ids+=("$!")

INFERLAB_CPU_BIND=127.0.0.1:9942 \
INFERLAB_CPU_WORKER_ID=cpu-four-slot \
INFERLAB_MODEL_PATH=models/tiny-inferlab-v1.bin \
INFERLAB_CPU_DECODER_MODE=kv-cache \
INFERLAB_CPU_MAX_BATCH_SIZE=4 \
INFERLAB_CPU_BATCH_TICK_MS=3 \
  target/debug/cpu-worker >"$proof_tmp/four-slot-worker.log" 2>&1 &
process_ids+=("$!")

INFERLAB_BIND=127.0.0.1:9940 \
INFERLAB_WORKERS='cpu-four-slot=http://127.0.0.1:9942' \
INFERLAB_MAX_RETRIES=0 \
INFERLAB_ATTEMPT_TIMEOUT_MS=5000 \
INFERLAB_REQUEST_DEADLINE_MS=10000 \
  target/debug/gateway >"$proof_tmp/gateway.log" 2>&1 &
process_ids+=("$!")

wait_for_health http://127.0.0.1:9941/health
wait_for_health http://127.0.0.1:9942/health
wait_for_health http://127.0.0.1:9940/health

python3 benchmarks/continuous_batch_probe.py \
  --serial-url http://127.0.0.1:9941/v1/chat/completions \
  --serial-scheduler-url http://127.0.0.1:9941/internal/scheduler \
  --continuous-url http://127.0.0.1:9942/v1/chat/completions \
  --continuous-scheduler-url http://127.0.0.1:9942/internal/scheduler \
  --concurrency 1,2,4,8 \
  --requests-per-level 24 \
  --output "$proof_tmp/continuous-batch-load.json"

python3 benchmarks/cpu_stream_probe.py \
  --url http://127.0.0.1:9940/v1/chat/completions \
  --worker-health-url http://127.0.0.1:9942/health \
  --prompt "teach me streaming" \
  --stream-output "$proof_tmp/gateway-stream.json" \
  --non-stream-output "$proof_tmp/gateway-non-stream.json"

"$oracle_python" benchmarks/cpu_environment.py \
  --model models/tiny-inferlab-v1.bin \
  --output "$proof_tmp/environment.json"

python3 benchmarks/check_kv_batch.py \
  --kv-parity "${kv_parity_paths[@]}" \
  --torch-parity "${torch_parity_paths[@]}" \
  --load "$proof_tmp/continuous-batch-load.json" \
  --gateway-stream "$proof_tmp/gateway-stream.json" \
  --output "$proof_tmp/kv-batch-check.json"

python3 benchmarks/render_kv_batch_svg.py \
  --check "$proof_tmp/kv-batch-check.json" \
  --kv-parity "$proof_tmp/kv-parity-teach-streaming.json" \
  --load "$proof_tmp/continuous-batch-load.json" \
  --output "$proof_tmp/kv-batch-proof.svg"

if [[ -n "${INFERLAB_V08_OUTPUT_DIR:-}" ]]; then
  output_dir="$INFERLAB_V08_OUTPUT_DIR"
  mkdir -p "$output_dir"
  cp "$proof_tmp"/recompute-*.json "$output_dir/"
  cp "$proof_tmp"/cached-*.json "$output_dir/"
  cp "$proof_tmp"/kv-parity-*.json "$output_dir/"
  cp "$proof_tmp"/torch-*.json "$output_dir/"
  cp "$proof_tmp"/torch-parity-*.json "$output_dir/"
  cp "$proof_tmp/continuous-batch-load.json" "$output_dir/"
  cp "$proof_tmp/gateway-stream.json" "$output_dir/"
  cp "$proof_tmp/gateway-non-stream.json" "$output_dir/"
  cp "$proof_tmp/environment.json" "$output_dir/"
  cp "$proof_tmp/kv-batch-check.json" "$output_dir/"
  cp "$proof_tmp/kv-batch-proof.svg" "$output_dir/"
  echo "Retained evidence in $output_dir"
fi

echo "v0.8 proof passed"
