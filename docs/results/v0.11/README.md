# v0.11 proof: quantization and speculative decoding

This retained experiment proves exact active-storage accounting for FP32,
per-row INT8, and groupwise INT4; quantized-logit and greedy correctness;
greedy and rejection-sampling speculative behavior; and real worker/gateway
JSON and SSE integration. It also preserves the negative result that fewer
target calls do not make this scalar same-architecture draft faster.

## Hypothesis

- INT8 and INT4 reduce active linear and total tensor payload after including
  scales, zero points, and FP32 islands.
- Across three prompts and 24 steps, INT8 remains within `2e-4` and INT4 within
  `4e-3` maximum absolute FP32 logit error with no greedy-token mismatch.
- The FP32 target remains within `1e-4` of the independent PyTorch oracle.
- Accepted greedy draft windows preserve target output and reduce target calls.
- Sampled speculation remains within one percentage point of the FP32 target
  distribution and replays for a fixed seed.
- Rejection correction preserves the target distribution even when a synthetic
  draft is poor enough to force thousands of rejections.
- Gateway non-streaming and SSE responses preserve the verified token stream;
  an INT4 worker exposes its active representation; structured speculation
  fails before streaming.
- Wall time is measured independently from call-count reduction.

## Result chart

![Active payload, quantized correctness, target calls, measured latency, and rejection correction](raw/optimization-proof.svg)

## Retained result

| Observation | Result |
|---|---:|
| Machine-readable release assertions | 33 / 33 passed |
| Quantization prompts / greedy steps | 3 / 24 |
| FP32 active tensor / linear bytes | 13,720 / 9,600 |
| INT8 active tensor / linear bytes | 7,056 / 2,936 |
| INT8 tensor / linear compression | 1.944× / 3.270× |
| INT8 scales / zero points | 134 / 0 |
| INT4 active tensor / linear bytes | 6,820 / 2,700 |
| INT4 tensor / linear compression | 2.012× / 3.556× |
| INT4 group size / scales / zero points | 8 / 300 / 300 |
| INT8 / INT4 maximum absolute logit error | 0.000182867 / 0.003354073 |
| INT8 / INT4 greedy mismatches | 0 / 24 · 0 / 24 |
| FP32 / PyTorch maximum absolute logit error | `4.1975708e-06` |
| Baseline target calls for 8 tokens | 8 |
| Window 1 / 2 / 3 target calls | 4 / 3 / 2 |
| Window-3 target-call reduction | 75% |
| Greedy INT8 / INT4 proposal acceptance | 100% / 100% |
| Baseline median wall time | 24.625 µs |
| INT8 window-3 median / speed ratio | 110.541 µs / `0.223x` |
| Best speculative retained speed ratio | `0.261x` baseline |
| Real-draft samples per INT8 / INT4 mode | 10,000 / 10,000 |
| Real target-vs-speculative maximum error | 0.800 / 0.780 percentage points |
| Real sampled replay checks | 4 / 4 each |
| Synthetic identical-draft acceptance | 100.00% |
| Synthetic softened-draft acceptance / rejections | 83.87% / 1,613 |
| Synthetic reversed-draft acceptance / rejections | 42.05% / 5,795 |
| Largest synthetic corrected probability error | 0.846 percentage points |
| Gateway baseline / speculative / SSE status | 200 / 200 / 200 |
| Gateway greedy output equality | exact |
| Unsupported structured speculation status | 400 pre-stream |
| Direct INT4 worker dtype / output | `uint4-groupwise` / exact greedy text |

The tensor figures measure active model tensor payload. They intentionally do
not claim process RSS: vocabulary strings, container bookkeeping, allocator
capacity, KV pages, and a separately loaded draft are outside that counter.

The greedy-path perplexities—1.001519718 FP32, 1.001519785 INT8, and
1.001519129 INT4—are sensitivity measurements along the retained FP32 greedy
trace. They are not corpus perplexity or model-quality evaluation.

## The negative performance result

The algorithmic result and system result disagree in an instructive way.
Window-three speculation reduces authoritative target invocations from eight
to two and accepts every retained real proposal. Nevertheless, every retained
speculative profile is slower than baseline; the best is only `0.261x` baseline
speed.

The draft uses the same transformer architecture and checkpoint, changing only
weight precision. Quantized values are dequantized through scalar inner-loop
access, and the target `forward_all` verification recomputes the combined
sequence instead of reusing paged KV state. On a tiny model, this extra work and
orchestration dominate. v0.11 therefore claims fewer target calls and correct
output—not a speculative latency or throughput improvement.

## Why there are two sampling experiments

The integrated INT8 and INT4 draft experiment checks the real session, model,
HTTP, replay, and metrics path. Both quantized drafts are so close to the FP32
target that 20,000/20,000 first proposals are accepted. That does not exercise
the correction branch.

The one-step synthetic experiment fixes target logits at `[0, 1, 2]` and uses
identical, softened, and reversed draft logits. Acceptance falls from 100.00%
to 83.87% to 42.05%, forcing 7,408 total rejections in the latter two cases,
while every corrected output distribution stays within one percentage point of
the target. The two experiments prove integration and rejection behavior
separately.

## Reproduce

Use Python with PyTorch 2.2.2 or compatible:

```bash
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python \
  ./scripts/proof-v0.11.sh
```

To replace this retained evidence:

```bash
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python \
INFERLAB_V11_OUTPUT_DIR=docs/results/v0.11/raw \
  ./scripts/proof-v0.11.sh
```

The script regenerates both deterministic checkpoints in a temporary
directory, byte-compares them with the committed artifacts, builds the
workspace, runs the native/PyTorch and optimization probes, starts two workers
and one gateway, evaluates 33 assertions, and renders the SVG from raw JSON.

## Raw artifacts

- [`optimization-check.json`](raw/optimization-check.json) — 33
  machine-readable assertions and release summary
- [`optimization-proof.svg`](raw/optimization-proof.svg) — chart rendered
  from retained JSON rather than hand-entered values
- [`optimization-probe.json`](raw/optimization-probe.json) — memory, full-logit
  error, greedy-path perplexity, timings, greedy speculation, real sampled
  speculation, and synthetic draft-quality sweep
- [`gateway-optimization.json`](raw/gateway-optimization.json) — baseline and
  speculative JSON, same-seed sample replay, SSE reconstruction, structured
  error, INT4 response, model health, and generation metrics
- [`fp32-target.json`](raw/fp32-target.json) — complete native FP32 trace
- [`torch-v2.json`](raw/torch-v2.json) — independent PyTorch trace
- [`torch-parity-v2.json`](raw/torch-parity-v2.json) — step-level native/PyTorch
  comparison
- [`environment.json`](raw/environment.json) — host, compiler, PyTorch, and
  checkpoint identity

## Environment and limitations

The retained run used macOS 26.5.2 on ARM64, Apple clang 21.0.0, Python 3.9.6,
PyTorch 2.2.2 with 14 threads, and the 13,969-byte v2 checkpoint with SHA-256
`36c76ff3b2dcdedd3589a0b03350f5b2851c7ff2979640311c0559d8da5f3f9a`.

This is a one-layer, 16-dimensional synthetic model. The benchmark uses 101
warm single-process repetitions and tiny microsecond timings dominated by
reference overhead. There are no SIMD integer kernels, separately trained
small draft, calibration dataset, production tokenizer, corpus perplexity,
task-quality evaluation, paged-KV multi-token verification, structured
speculation, or cross-engine seed guarantee.
