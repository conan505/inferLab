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
  echo "v0.9 proof requires PyTorch 2.2.2 or compatible." >&2
  echo "Create .tools/v0.7-python and install torch, or set INFERLAB_ORACLE_PYTHON." >&2
  exit 1
fi

echo "Building the v0.9 workspace..."
cargo build --workspace --quiet

prompt_names=("teach-streaming" "unknown-words" "known-words")
prompts=("teach me streaming" "why does inference matter" "hello systems")
paged_parity_paths=()
torch_parity_paths=()
for index in "${!prompts[@]}"; do
  name="${prompt_names[$index]}"
  prompt="${prompts[$index]}"
  contiguous_output="$proof_tmp/contiguous-$name.json"
  paged_output="$proof_tmp/paged-$name.json"
  paged_parity_output="$proof_tmp/paged-parity-$name.json"
  torch_output="$proof_tmp/torch-$name.json"
  torch_parity_output="$proof_tmp/torch-parity-$name.json"

  target/debug/inferlab-cpu-cli \
    --model models/tiny-inferlab-v1.bin \
    --prompt "$prompt" \
    --max-tokens 8 \
    --mode kv-cache \
    --prefix-capacity 0 \
    --repetitions 101 \
    --output "$contiguous_output"
  target/debug/inferlab-cpu-cli \
    --model models/tiny-inferlab-v1.bin \
    --prompt "$prompt" \
    --max-tokens 8 \
    --mode paged-kv-cache \
    --page-tokens 4 \
    --page-count 64 \
    --prefix-capacity 0 \
    --repetitions 101 \
    --output "$paged_output"
  python3 benchmarks/compare_paged_cache.py \
    --contiguous "$contiguous_output" \
    --paged "$paged_output" \
    --output "$paged_parity_output"

  "$oracle_python" oracle/torch_reference.py \
    --model models/tiny-inferlab-v1.bin \
    --prompt "$prompt" \
    --max-tokens 8 \
    --repetitions 31 \
    --output "$torch_output"
  python3 benchmarks/compare_cpu_decoder.py \
    --cpp "$paged_output" \
    --torch "$torch_output" \
    --output "$torch_parity_output"
  paged_parity_paths+=("$paged_parity_output")
  torch_parity_paths+=("$torch_parity_output")
done

target/debug/inferlab-page-probe \
  --model models/tiny-inferlab-v1.bin \
  --output "$proof_tmp/page-cache-probe.json"

echo "Starting three paged-cache workers and two consistent-hash topologies..."
for suffix in a b c; do
  case "$suffix" in
    a) port=9951 ;;
    b) port=9952 ;;
    c) port=9953 ;;
  esac
  INFERLAB_CPU_BIND="127.0.0.1:$port" \
  INFERLAB_CPU_WORKER_ID="cpu-page-$suffix" \
  INFERLAB_MODEL_PATH=models/tiny-inferlab-v1.bin \
  INFERLAB_CPU_DECODER_MODE=paged-kv-cache \
  INFERLAB_CPU_MAX_BATCH_SIZE=4 \
  INFERLAB_CPU_KV_PAGE_TOKENS=4 \
  INFERLAB_CPU_KV_PAGE_COUNT=64 \
  INFERLAB_CPU_PREFIX_CACHE_CAPACITY=32 \
    target/debug/cpu-worker >"$proof_tmp/worker-$suffix.log" 2>&1 &
  process_ids+=("$!")
done

INFERLAB_BIND=127.0.0.1:9950 \
INFERLAB_ROUTING_POLICY=consistent-hash \
INFERLAB_CONSISTENT_HASH_VNODES=128 \
INFERLAB_WORKERS='cpu-page-a=http://127.0.0.1:9951,cpu-page-b=http://127.0.0.1:9952' \
INFERLAB_MAX_RETRIES=0 \
INFERLAB_ATTEMPT_TIMEOUT_MS=5000 \
INFERLAB_REQUEST_DEADLINE_MS=10000 \
  target/debug/gateway >"$proof_tmp/gateway-two.log" 2>&1 &
process_ids+=("$!")

INFERLAB_BIND=127.0.0.1:9954 \
INFERLAB_ROUTING_POLICY=consistent-hash \
INFERLAB_CONSISTENT_HASH_VNODES=128 \
INFERLAB_WORKERS='cpu-page-a=http://127.0.0.1:9951,cpu-page-b=http://127.0.0.1:9952,cpu-page-c=http://127.0.0.1:9953' \
INFERLAB_MAX_RETRIES=0 \
INFERLAB_ATTEMPT_TIMEOUT_MS=5000 \
INFERLAB_REQUEST_DEADLINE_MS=10000 \
  target/debug/gateway >"$proof_tmp/gateway-three.log" 2>&1 &
process_ids+=("$!")

wait_for_health http://127.0.0.1:9951/health
wait_for_health http://127.0.0.1:9952/health
wait_for_health http://127.0.0.1:9953/health
wait_for_health http://127.0.0.1:9950/health
wait_for_health http://127.0.0.1:9954/health

python3 benchmarks/prefix_cache_probe.py \
  --two-worker-url http://127.0.0.1:9950/v1/chat/completions \
  --three-worker-url http://127.0.0.1:9954/v1/chat/completions \
  --worker-cache cpu-page-a=http://127.0.0.1:9951/internal/cache \
  --worker-cache cpu-page-b=http://127.0.0.1:9952/internal/cache \
  --worker-cache cpu-page-c=http://127.0.0.1:9953/internal/cache \
  --keys 256 \
  --output "$proof_tmp/prefix-ownership.json"

python3 benchmarks/cpu_stream_probe.py \
  --url http://127.0.0.1:9950/v1/chat/completions \
  --worker-health-url http://127.0.0.1:9951/health \
  --prompt "teach me streaming" \
  --stream-output "$proof_tmp/gateway-stream.json" \
  --non-stream-output "$proof_tmp/gateway-non-stream.json"

"$oracle_python" benchmarks/cpu_environment.py \
  --model models/tiny-inferlab-v1.bin \
  --output "$proof_tmp/environment.json"

python3 benchmarks/check_paged_cache.py \
  --paged-parity "${paged_parity_paths[@]}" \
  --torch-parity "${torch_parity_paths[@]}" \
  --page-probe "$proof_tmp/page-cache-probe.json" \
  --prefix-probe "$proof_tmp/prefix-ownership.json" \
  --gateway-stream "$proof_tmp/gateway-stream.json" \
  --output "$proof_tmp/paged-cache-check.json"

python3 benchmarks/render_paged_cache_svg.py \
  --check "$proof_tmp/paged-cache-check.json" \
  --page-probe "$proof_tmp/page-cache-probe.json" \
  --prefix-probe "$proof_tmp/prefix-ownership.json" \
  --output "$proof_tmp/paged-cache-proof.svg"

if [[ -n "${INFERLAB_V09_OUTPUT_DIR:-}" ]]; then
  output_dir="$INFERLAB_V09_OUTPUT_DIR"
  mkdir -p "$output_dir"
  cp "$proof_tmp"/contiguous-*.json "$output_dir/"
  cp "$proof_tmp"/paged-*.json "$output_dir/"
  cp "$proof_tmp"/paged-parity-*.json "$output_dir/"
  cp "$proof_tmp"/torch-*.json "$output_dir/"
  cp "$proof_tmp"/torch-parity-*.json "$output_dir/"
  cp "$proof_tmp/page-cache-probe.json" "$output_dir/"
  cp "$proof_tmp/prefix-ownership.json" "$output_dir/"
  cp "$proof_tmp/gateway-stream.json" "$output_dir/"
  cp "$proof_tmp/gateway-non-stream.json" "$output_dir/"
  cp "$proof_tmp/environment.json" "$output_dir/"
  cp "$proof_tmp/paged-cache-check.json" "$output_dir/"
  cp "$proof_tmp/paged-cache-proof.svg" "$output_dir/"
  echo "Retained evidence in $output_dir"
fi

echo "v0.9 proof passed"
