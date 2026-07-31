# v0.8 proof: KV cache and continuous batching

This retained experiment proves the optimized decoder preserves v0.7 results,
counts the computation it avoids and memory it adds, compares one-slot with
four-slot HTTP scheduling at four concurrency levels, retains an exact backfill
trace, and streams the cached path through the existing gateway.

## Hypothesis

- KV-cache and recompute modes produce identical greedy tokens, text, finish
  reasons, and logits within `1e-6`.
- The cached path still matches the independent PyTorch oracle within `1e-4`.
- KV caching reduces query projection, K/V projection, and attention-score work
  by more than 75% for the retained eight-step request.
- A four-slot scheduler never exceeds four active sessions and admits waiting
  work after an early completion but before the longest request finishes.
- Under the declared shared 3 ms batch-tick workload, continuous scheduling
  improves concurrency-8 throughput by at least 2× and lowers p95 latency.
- Gateway JSON and SSE behavior remains unchanged.

## Result chart

![KV work reduction, concurrency throughput and latency, and continuous backfill lanes](raw/kv-batch-proof.svg)

## Retained result

| Observation | Result |
|---|---:|
| Prompts compared | 3 |
| Decode steps per prompt | 8, including `<eos>` |
| Recompute/cache maximum absolute logit error | `0` |
| Cache/PyTorch maximum absolute logit error | `4.1975708e-06` |
| Greedy token mismatches | 0 |
| Query token projections | 60 → 8, down 86.7% |
| K/V token projections | 60 → 11, down 81.7% |
| Attention score elements | 1,104 → 240, down 78.3% |
| Peak logical KV-cache bytes | 1,408 |
| HTTP requests in concurrency matrix | 192 |
| Additional mixed-length backfill requests | 8 |
| Maximum active sessions, one-slot worker | 1 |
| Maximum active sessions, continuous worker | 4 |
| Concurrency-8 request throughput, one slot | 37.843 requests/s |
| Concurrency-8 request throughput, four slots | 135.318 requests/s |
| Concurrency-8 throughput ratio | 3.576× |
| Concurrency-8 p95 latency, one slot | 212.439 ms |
| Concurrency-8 p95 latency, four slots | 69.003 ms |
| Machine-readable assertions | 16 / 16 passed |

The mixed load repeats maximum token limits `2, 4, 6, 8` for 24 requests at
each concurrency level `1, 2, 4, 8`. Both workers use the same checkpoint,
cached decoder, loopback HTTP path, and 3 ms delay paid once per scheduler
batch. The one-slot worker caps active sequences at one; the comparison worker
caps them at four.

The delay is explicit proof instrumentation. It makes a shared scheduler
iteration cost reproducible, but it is not C++ model latency or a GPU-kernel
claim. The v0.8 scheduler still calls the C++ decoder sequentially once per
active session.

## Reproduce

Use Python with PyTorch 2.2.2 or compatible:

```bash
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python \
  ./scripts/proof-v0.8.sh
```

To replace this retained evidence:

```bash
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python \
INFERLAB_V08_OUTPUT_DIR=docs/results/v0.8/raw \
  ./scripts/proof-v0.8.sh
```

## Raw artifacts

- [`kv-batch-check.json`](raw/kv-batch-check.json) — 16 machine-readable
  release assertions and headline results
- [`kv-batch-proof.svg`](raw/kv-batch-proof.svg) — deterministic chart rendered
  from the retained JSON rather than hand-entered values
- [`continuous-batch-load.json`](raw/continuous-batch-load.json) — every HTTP
  latency, completion, generation metric, scheduler snapshot, and backfill event
- `recompute-*.json` — full C++ baseline token/logit traces and warm timings
- `cached-*.json` — full C++ cached token/logit traces, warm timings, and work
  counters
- `kv-parity-*.json` — step-level recompute/cache comparisons and reductions
- `torch-*.json` — independent PyTorch traces for the same prompts
- `torch-parity-*.json` — cached C++ versus PyTorch comparisons
- [`gateway-stream.json`](raw/gateway-stream.json) — timestamped cached-decoder
  SSE events, reconstruction, finish reason, `[DONE]`, headers, and worker health
- [`gateway-non-stream.json`](raw/gateway-non-stream.json) — equivalent final
  response with InferLab generation metadata
- [`environment.json`](raw/environment.json) — host, toolchain, PyTorch, and
  checkpoint identity

## Limitations

This is a single-host Apple ARM64 experiment using an untrained 3,232-parameter
model. KV vectors are contiguous and private to one session; there is no page
allocator, prefix sharing, eviction, reference counting, or copy-on-write. The
scheduler groups active requests but does not form a vectorized tensor batch.
It has no priorities, cost-aware admission, or mid-token cancellation. The load
distribution and injected batch tick are deterministic educational
instrumentation, not a production workload or useful-model benchmark.
