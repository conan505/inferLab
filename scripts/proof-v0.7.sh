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
  echo "v0.7 proof requires PyTorch 2.2.2 or compatible." >&2
  echo "Create .tools/v0.7-python and install torch, or set INFERLAB_ORACLE_PYTHON." >&2
  exit 1
fi

echo "Building the Rust gateway and C++ runtime..."
cargo build --workspace --quiet

echo "Regenerating the checkpoint byte-for-byte..."
python3 oracle/generate_tiny_model.py \
  --model "$proof_tmp/regenerated-model.bin" \
  --metadata "$proof_tmp/regenerated-model.json"
cmp models/tiny-inferlab-v1.bin "$proof_tmp/regenerated-model.bin"

prompt_names=("teach-streaming" "unknown-words" "known-words")
prompts=("teach me streaming" "why does inference matter" "hello systems")
parity_paths=()
for index in "${!prompts[@]}"; do
  name="${prompt_names[$index]}"
  prompt="${prompts[$index]}"
  cpp_output="$proof_tmp/cpp-$name.json"
  torch_output="$proof_tmp/torch-$name.json"
  parity_output="$proof_tmp/parity-$name.json"
  target/debug/inferlab-cpu-cli \
    --model models/tiny-inferlab-v1.bin \
    --prompt "$prompt" \
    --max-tokens 8 \
    --repetitions 31 \
    --output "$cpp_output"
  "$oracle_python" oracle/torch_reference.py \
    --model models/tiny-inferlab-v1.bin \
    --prompt "$prompt" \
    --max-tokens 8 \
    --repetitions 31 \
    --output "$torch_output"
  python3 benchmarks/compare_cpu_decoder.py \
    --cpp "$cpp_output" \
    --torch "$torch_output" \
    --output "$parity_output"
  parity_paths+=("$parity_output")
done

echo "Starting the real C++ decoder behind the existing gateway..."
INFERLAB_CPU_BIND=127.0.0.1:9931 \
INFERLAB_CPU_WORKER_ID=cpu-worker-a \
INFERLAB_MODEL_PATH=models/tiny-inferlab-v1.bin \
INFERLAB_CPU_TOKEN_DELAY_MS=12 \
  target/debug/cpu-worker >"$proof_tmp/cpu-worker.log" 2>&1 &
process_ids+=("$!")

INFERLAB_BIND=127.0.0.1:9930 \
INFERLAB_WORKERS='cpu-worker-a=http://127.0.0.1:9931' \
INFERLAB_MAX_RETRIES=0 \
INFERLAB_ATTEMPT_TIMEOUT_MS=5000 \
INFERLAB_REQUEST_DEADLINE_MS=10000 \
  target/debug/gateway >"$proof_tmp/gateway.log" 2>&1 &
process_ids+=("$!")

wait_for_health http://127.0.0.1:9931/health
wait_for_health http://127.0.0.1:9930/health

python3 benchmarks/cpu_stream_probe.py \
  --url http://127.0.0.1:9930/v1/chat/completions \
  --worker-health-url http://127.0.0.1:9931/health \
  --prompt "teach me streaming" \
  --stream-output "$proof_tmp/gateway-stream.json" \
  --non-stream-output "$proof_tmp/gateway-non-stream.json"

"$oracle_python" benchmarks/cpu_environment.py \
  --model models/tiny-inferlab-v1.bin \
  --output "$proof_tmp/environment.json"

python3 benchmarks/check_cpu_decoder.py \
  --model models/tiny-inferlab-v1.bin \
  --metadata models/tiny-inferlab-v1.json \
  --parity "${parity_paths[@]}" \
  --stream "$proof_tmp/gateway-stream.json" \
  --non-stream "$proof_tmp/gateway-non-stream.json" \
  --environment "$proof_tmp/environment.json" \
  --output "$proof_tmp/cpu-decoder-check.json"

python3 benchmarks/render_cpu_decoder_svg.py \
  --check "$proof_tmp/cpu-decoder-check.json" \
  --parity "${parity_paths[@]}" \
  --stream "$proof_tmp/gateway-stream.json" \
  --output "$proof_tmp/cpu-decoder-proof.svg"

if [[ -n "${INFERLAB_V07_OUTPUT_DIR:-}" ]]; then
  output_dir="$INFERLAB_V07_OUTPUT_DIR"
  mkdir -p "$output_dir"
  cp models/tiny-inferlab-v1.json "$output_dir/model-metadata.json"
  cp "$proof_tmp"/cpp-*.json "$output_dir/"
  cp "$proof_tmp"/torch-*.json "$output_dir/"
  cp "$proof_tmp"/parity-*.json "$output_dir/"
  cp "$proof_tmp/gateway-stream.json" "$output_dir/"
  cp "$proof_tmp/gateway-non-stream.json" "$output_dir/"
  cp "$proof_tmp/environment.json" "$output_dir/"
  cp "$proof_tmp/cpu-decoder-check.json" "$output_dir/"
  cp "$proof_tmp/cpu-decoder-proof.svg" "$output_dir/"
  echo "Retained evidence in $output_dir"
fi

echo "v0.7 proof passed"
