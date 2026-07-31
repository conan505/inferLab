# v0.7 proof: tiny C++ CPU decoder

This retained experiment regenerates the educational FP32 checkpoint, compares
the C++ decoder with an independent PyTorch implementation at every generation
step, and streams real model tokens through the unchanged Rust gateway.

## Hypothesis

- The committed checkpoint is deterministically reproducible.
- C++ and PyTorch tokenize the same prompts identically.
- Every C++ logit remains within `1e-4` of PyTorch.
- Greedy token IDs, final text, and stop reason match exactly.
- The real C++ runtime satisfies the same HTTP contract as fake workers.
- Each visible generated token reaches the client as a separate SSE event.

## Result chart

![C++ and PyTorch logit parity, micro-model latency, and gateway token timeline](raw/cpu-decoder-proof.svg)

## Retained result

| Observation | Result |
|---|---:|
| Checkpoint | 13,111 bytes, 3,232 FP32 parameters |
| Checkpoint SHA-256 | `654bf3f75f3f8bcdd4d2f26c62867408903184e30d492ddb863a2e388224e22c` |
| Prompts compared | 3 |
| Steps per prompt | 8, including `<eos>` |
| Logit values compared | 384 |
| Maximum absolute logit error | `4.1975708e-06` |
| Acceptance tolerance | `1e-4` |
| Greedy token mismatches | 0 |
| Expected generated text matches | 3 / 3 |
| Average C++ median generation | 50.222 µs |
| Average PyTorch median generation | 438.208 µs |
| Gateway first content token | 18.912 ms |
| Seven-token stream span | 83.462 ms |
| Machine-readable assertions | 18 / 18 passed |

The generated visible pieces are:

```text
InferLab |  turns |  prompts |  into |  real |  tokens | .
```

They reconstruct `InferLab turns prompts into real tokens.`. Step eight selects
`<eos>`, which produces the stop event instead of visible text.

The 12 ms per-token pacing is deliberate proof instrumentation. It makes
incremental SSE delivery visible and is not model latency. The tiny C++/PyTorch
latency comparison is also descriptive: a 3,232-parameter fixture is dominated
by framework and call overhead and does not predict useful-model performance.

## Reproduce

Use Python with PyTorch 2.2.2 or compatible:

```bash
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python \
  ./scripts/proof-v0.7.sh
```

To replace this retained evidence:

```bash
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python \
INFERLAB_V07_OUTPUT_DIR=docs/results/v0.7/raw \
  ./scripts/proof-v0.7.sh
```

## Raw artifacts

- [`cpu-decoder-check.json`](raw/cpu-decoder-check.json) — 18
  machine-readable release assertions
- [`cpu-decoder-proof.svg`](raw/cpu-decoder-proof.svg) — deterministic chart
  rendered from parity and SSE evidence
- [`model-metadata.json`](raw/model-metadata.json) — retained dimensions,
  tensor order, vocabulary, and checksum
- `cpp-*.json` — direct C++ token, full-logit, and warm timing traces
- `torch-*.json` — independent PyTorch traces for the same prompts
- `parity-*.json` — step-level maximum/mean error and exact token checks
- [`gateway-stream.json`](raw/gateway-stream.json) — timestamped SSE events,
  reconstructed text, TTFT, intervals, worker header, and health metadata
- [`gateway-non-stream.json`](raw/gateway-non-stream.json) — equivalent
  OpenAI-shaped final response through the gateway
- [`environment.json`](raw/environment.json) — hardware, compiler, Python,
  PyTorch, and checkpoint identity

## Limitations

This is a single-host Apple ARM64 correctness proof for an untrained
deterministic 16-token model. It does not cover a production tokenizer, broad
language behavior, randomized tensor shapes, KV cache, batching, multiple CPU
threads, SIMD/BLAS, cancellation inside a forward pass, sampling, quantization,
GPU kernels, or production performance.
