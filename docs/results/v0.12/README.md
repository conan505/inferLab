# v0.12 proof: tiled online-softmax causal attention

This retained experiment proves that materialized and tiled online-softmax CPU
attention implement the same causal function across FP32, simulated FP16, and
simulated BF16 storage; that the online algorithm removes the quadratic score
intermediate; and that the selected path runs through the real decoder, worker,
gateway JSON response, and gateway SSE stream.

It does not claim CUDA or FlashAttention execution. The retained host has an
Apple M4 Pro and no CUDA compiler or NVIDIA runtime.

## Result chart

![Score scratch, modeled traffic, scalar CPU time, and storage-precision drift](raw/attention-proof.svg)

## Retained result

| Observation | Result |
|---|---:|
| Machine-readable release assertions | 21 / 21 passed |
| Algorithms × storage precisions | 2 × 3 = 6 |
| Maximum C++ / PyTorch attention error | `1.1553e-7` |
| Maximum online / materialized fixture error | `1.0e-7` |
| Full-model maximum online / materialized logit error | `1.0e-7` |
| Full-model greedy token/text equality | exact |
| FP16 storage drift from FP32 | `0.000199139` |
| BF16 storage drift from FP32 | `0.001946092` |
| Future-key/value isolation for query zero | exact zero change |
| Large-score outputs | finite in all six variants |
| 256-token materialized score scratch | 1,048,576 B |
| 256-token online score scratch | 128 B |
| Score-scratch reduction | 8,192× |
| Counted online working set | 256 B |
| Materialized / online modeled traffic | 4.50 / 2.25 MiB |
| Modeled-traffic reduction | 2.0× |
| 256-token materialized / online median | 2,774.958 / 1,240.542 µs |
| Observed scalar CPU speedup at 256 tokens | 2.237× |
| Gateway selected worker | `cpu-attention-online` |
| Direct worker / gateway / SSE text | exact equality |
| Historical native / PyTorch full-model error | `4.1975708e-6` |
| CUDA compiler / runtime available | false / false |

Timings are observations from one retained run. The score-scratch bytes are
real kernel buffer sizes excluding allocator metadata. External-traffic bytes
are a declared analytical schedule model, not CPU-cache, DRAM, Metal, CUDA, or
HBM performance-counter readings.

FP16 and BF16 are storage simulations: inputs are rounded through the selected
16-bit representation, then held in host FP32 vectors and accumulated in FP32.
The numerical error is measured, but actual prepared-vector allocation and
16-bit hardware throughput are not claimed.

## Scaling sweep

| Tokens | Algorithm | Median µs | P95 µs | Score scratch | Modeled traffic |
|---:|---|---:|---:|---:|---:|
| 32 | materialized | 137.416 | 148.333 | 16,384 B | 128 KiB |
| 32 | online tiled | 75.084 | 88.959 | 128 B | 64 KiB |
| 64 | materialized | 328.458 | 381.791 | 65,536 B | 384 KiB |
| 64 | online tiled | 165.958 | 177.041 | 128 B | 192 KiB |
| 128 | materialized | 869.875 | 1,016.083 | 262,144 B | 1.25 MiB |
| 128 | online tiled | 385.500 | 403.541 | 128 B | 0.625 MiB |
| 256 | materialized | 2,774.958 | 2,995.208 | 1,048,576 B | 4.50 MiB |
| 256 | online tiled | 1,240.542 | 1,296.625 | 128 B | 2.25 MiB |

The sweep uses four heads, head dimension 32, 32-token tiles, causal FP32
attention, three warm-ups, and 31 measured repetitions per profile.

## Reproduce

Use Python with PyTorch 2.2.2 or compatible:

```bash
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python \
  ./scripts/proof-v0.12.sh
```

To replace this retained evidence:

```bash
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python \
INFERLAB_V12_OUTPUT_DIR=docs/results/v0.12/raw \
  ./scripts/proof-v0.12.sh
```

The script regenerates and byte-compares both deterministic checkpoints, builds
the workspace, runs complete-model native/PyTorch parity, runs six standalone
attention variants plus scaling and failure fixtures, starts materialized and
online workers behind a gateway, evaluates 21 assertions, and renders the SVG
from retained JSON.

## Raw artifacts

- [`attention-check.json`](raw/attention-check.json) — 21 machine-readable
  assertions and release summary
- [`attention-proof.svg`](raw/attention-proof.svg) — chart generated from the
  retained JSON
- [`attention-probe.json`](raw/attention-probe.json) — fixture inputs and all
  outputs, stability/isolation tests, scratch/traffic stats, and timings
- [`attention-torch.json`](raw/attention-torch.json) — independent
  precision-matched PyTorch attention outputs
- [`materialized-model.json`](raw/materialized-model.json) — complete native
  trace using the compatibility baseline
- [`online-model.json`](raw/online-model.json) — complete native trace using
  tiled online softmax
- [`gateway-attention.json`](raw/gateway-attention.json) — direct workers,
  health configurations, gateway JSON, and SSE reconstruction
- [`torch-v2.json`](raw/torch-v2.json) — independent full-model PyTorch trace
- [`torch-parity-v2.json`](raw/torch-parity-v2.json) — native/PyTorch step-level
  comparison
- [`environment.json`](raw/environment.json) — host, accelerator, CUDA/Metal,
  compiler, PyTorch, and checkpoint identity

## Environment and limits

The retained run used macOS 26.5.2 on ARM64, Apple M4 Pro with 20 GPU cores and
Metal 4, Apple clang 21.0.0, Python 3.9.6, and PyTorch 2.2.2. CUDA compiler and
runtime availability were both false. The 13,969-byte checkpoint SHA-256 is
`36c76ff3b2dcdedd3589a0b03350f5b2851c7ff2979640311c0559d8da5f3f9a`.

The model, fixture, schedule, compiler, caches, and host are intentionally small
and specific. There are no hardware traffic counters, packed 16-bit inputs,
SIMD intrinsics, multicore kernel, GPU execution, backward pass, dropout,
cross-attention, or production-model evaluation.
