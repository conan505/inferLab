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
  echo "v0.11 proof requires PyTorch 2.2.2 or compatible." >&2
  echo "Create .tools/v0.7-python and install torch, or set INFERLAB_ORACLE_PYTHON." >&2
  exit 1
fi

echo "Regenerating both deterministic checkpoints..."
python3 oracle/generate_tiny_model.py \
  --model "$proof_tmp/tiny-inferlab-v1.bin" \
  --metadata "$proof_tmp/tiny-inferlab-v1.json"
python3 oracle/generate_tiny_model_v2.py \
  --model "$proof_tmp/tiny-inferlab-v2.bin" \
  --metadata "$proof_tmp/tiny-inferlab-v2.json"
cmp models/tiny-inferlab-v1.bin "$proof_tmp/tiny-inferlab-v1.bin"
cmp models/tiny-inferlab-v1.json "$proof_tmp/tiny-inferlab-v1.json"
cmp models/tiny-inferlab-v2.bin "$proof_tmp/tiny-inferlab-v2.bin"
cmp models/tiny-inferlab-v2.json "$proof_tmp/tiny-inferlab-v2.json"

echo "Building the v0.11 workspace..."
cargo build --workspace --quiet

target/debug/inferlab-cpu-cli \
  --model models/tiny-inferlab-v2.bin \
  --prompt "teach me streaming" \
  --max-tokens 8 \
  --mode paged-kv-cache \
  --quantization fp32 \
  --repetitions 31 \
  --output "$proof_tmp/fp32-target.json"
"$oracle_python" oracle/torch_reference.py \
  --model models/tiny-inferlab-v2.bin \
  --prompt "teach me streaming" \
  --max-tokens 8 \
  --repetitions 31 \
  --output "$proof_tmp/torch-v2.json"
python3 benchmarks/compare_cpu_decoder.py \
  --cpp "$proof_tmp/fp32-target.json" \
  --torch "$proof_tmp/torch-v2.json" \
  --output "$proof_tmp/torch-parity-v2.json"

target/debug/inferlab-optimization-probe \
  --model models/tiny-inferlab-v2.bin \
  --repetitions 101 \
  --samples 10000 \
  --output "$proof_tmp/optimization-probe.json"

echo "Starting FP32 target, INT4 target, and gateway for HTTP/SSE proof..."
INFERLAB_CPU_BIND=127.0.0.1:9941 \
INFERLAB_CPU_WORKER_ID=cpu-optimization-fp32 \
INFERLAB_MODEL_PATH=models/tiny-inferlab-v2.bin \
INFERLAB_CPU_DECODER_MODE=paged-kv-cache \
INFERLAB_CPU_QUANTIZATION=fp32 \
INFERLAB_CPU_SPECULATIVE_DRAFT_QUANTIZATION=int8 \
INFERLAB_CPU_MAX_BATCH_SIZE=4 \
INFERLAB_CPU_KV_PAGE_TOKENS=4 \
INFERLAB_CPU_KV_PAGE_COUNT=64 \
INFERLAB_CPU_PREFIX_CACHE_CAPACITY=32 \
  target/debug/cpu-worker >"$proof_tmp/fp32-worker.log" 2>&1 &
process_ids+=("$!")

INFERLAB_CPU_BIND=127.0.0.1:9942 \
INFERLAB_CPU_WORKER_ID=cpu-optimization-int4 \
INFERLAB_MODEL_PATH=models/tiny-inferlab-v2.bin \
INFERLAB_CPU_DECODER_MODE=paged-kv-cache \
INFERLAB_CPU_QUANTIZATION=int4 \
INFERLAB_CPU_SPECULATIVE_DRAFT_QUANTIZATION=off \
INFERLAB_CPU_MAX_BATCH_SIZE=4 \
INFERLAB_CPU_KV_PAGE_TOKENS=4 \
INFERLAB_CPU_KV_PAGE_COUNT=64 \
INFERLAB_CPU_PREFIX_CACHE_CAPACITY=32 \
  target/debug/cpu-worker >"$proof_tmp/int4-worker.log" 2>&1 &
process_ids+=("$!")

INFERLAB_BIND=127.0.0.1:9940 \
INFERLAB_ROUTING_POLICY=least-in-flight \
INFERLAB_WORKERS='cpu-optimization-fp32=http://127.0.0.1:9941' \
INFERLAB_MAX_RETRIES=0 \
INFERLAB_ATTEMPT_TIMEOUT_MS=5000 \
INFERLAB_REQUEST_DEADLINE_MS=10000 \
  target/debug/gateway >"$proof_tmp/gateway.log" 2>&1 &
process_ids+=("$!")

wait_for_health http://127.0.0.1:9941/health
wait_for_health http://127.0.0.1:9942/health
wait_for_health http://127.0.0.1:9940/health

python3 benchmarks/optimization_gateway_probe.py \
  --gateway-url http://127.0.0.1:9940/v1/chat/completions \
  --int4-worker-url http://127.0.0.1:9942/v1/chat/completions \
  --output "$proof_tmp/gateway-optimization.json"

"$oracle_python" benchmarks/cpu_environment.py \
  --model models/tiny-inferlab-v2.bin \
  --output "$proof_tmp/environment.json"

python3 benchmarks/check_optimizations.py \
  --optimization-probe "$proof_tmp/optimization-probe.json" \
  --gateway-probe "$proof_tmp/gateway-optimization.json" \
  --torch-parity "$proof_tmp/torch-parity-v2.json" \
  --output "$proof_tmp/optimization-check.json"

python3 benchmarks/render_optimization_svg.py \
  --check "$proof_tmp/optimization-check.json" \
  --probe "$proof_tmp/optimization-probe.json" \
  --output "$proof_tmp/optimization-proof.svg"

if [[ -n "${INFERLAB_V11_OUTPUT_DIR:-}" ]]; then
  output_dir="$INFERLAB_V11_OUTPUT_DIR"
  mkdir -p "$output_dir"
  cp "$proof_tmp/fp32-target.json" "$output_dir/"
  cp "$proof_tmp/torch-v2.json" "$output_dir/"
  cp "$proof_tmp/torch-parity-v2.json" "$output_dir/"
  cp "$proof_tmp/optimization-probe.json" "$output_dir/"
  cp "$proof_tmp/gateway-optimization.json" "$output_dir/"
  cp "$proof_tmp/environment.json" "$output_dir/"
  cp "$proof_tmp/optimization-check.json" "$output_dir/"
  cp "$proof_tmp/optimization-proof.svg" "$output_dir/"
  echo "Retained evidence in $output_dir"
fi

echo "v0.11 proof passed"
