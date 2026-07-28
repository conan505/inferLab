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

cleanup() {
  rm -rf "$proof_tmp"
}
trap cleanup EXIT INT TERM

analysis_result="$results_dir/ring-analysis.json"
check_result="$results_dir/ring-check.json"

cargo build --quiet -p gateway --bin hash-ring-analyze
target/debug/hash-ring-analyze --keys 20000 >"$analysis_result"
python3 benchmarks/check_consistent_hash.py \
  --analysis "$analysis_result" | tee "$check_result"

echo
echo "v0.0.5 proof passed"
echo "Raw results: $results_dir"
