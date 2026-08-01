# v0.9 proof: paged KV cache and prefix ownership

This retained experiment proves that fixed-size page placement preserves the
v0.8 cache results, bounds physical capacity, exposes page-size fragmentation,
shares exact prompt prefixes with copy-on-write isolation, safely evicts LRU
directory ownership, and keeps repeat keys on stable gateway owners.

## Hypothesis

- Paged and contiguous KV layouts produce identical greedy tokens, text, finish
  reasons, and logits within `1e-6`.
- The paged path remains within `1e-4` of the independent PyTorch oracle.
- Allocated pages never exceed the configured pool and all pages return after
  their final references release.
- Smaller pages reduce tail fragmentation for the retained length mix.
- Two sessions can retain one exact prompt page, then diverge without mutating
  each other's state.
- Longest-prefix reuse projects only missing token positions.
- LRU eviction removes inactive cache ownership without invalidating live
  session block tables.
- Six repeat requests routed through the gateway return to their cold owner and
  reduce K/V projection work.
- A stable consistent-hash topology preserves all ownership; adding one worker
  remaps keys only to that worker.

## Result chart

![Paged-cache capacity, fragmentation, shared-prefix lifecycle, work reduction, and ownership](raw/paged-cache-proof.svg)

## Retained result

| Observation | Result |
|---|---:|
| Prompts compared | 3 |
| Paged/contiguous maximum absolute logit error | `0` |
| Paged/PyTorch maximum absolute logit error | `4.1975708e-06` |
| Greedy token, text, or finish-reason mismatches | 0 |
| Fixed physical capacity | 16 pages × 4 slots = 64 token slots |
| Live eight-token sessions at capacity | 8 |
| Declared 32-token max-reservation baseline | 2 sessions |
| Capacity ratio for that comparison | 4× |
| Ninth-session result | `paged KV cache capacity exhausted` |
| Pages after final release | 0 allocated, 16 free |
| Fragmentation for page sizes 1 / 2 / 4 / 8 | 0.0% / 9.1% / 23.1% / 37.5% |
| Shared prompt physical used bytes | 384 |
| Logical bytes across directory + two sessions | 1,152 |
| Bytes avoided while shared | 768 |
| Warm session forks | 2, one COW copy each |
| Longest-prefix result | 3 tokens reused, 1 projected |
| Gateway cold/warm prompt pairs | 6 / 6 warm hits |
| K/V projections across six pairs | 24 cold → 6 warm |
| Stable two-worker ownership | 256 / 256 keys |
| Ownership after adding worker C | A: 67, B: 82, C: 107 |
| Keys remapped after adding C | 107 / 256, 41.8% |
| Invalid A↔B remaps | 0 |
| Machine-readable assertions | 22 / 22 passed |

The capacity ratio compares actual four-token page allocation with a declared
baseline that reserves all 32 context positions per session. The v0.8 vectors
did not reserve the maximum up front; they grew per session but had no global
pool, sharing, or fixed reclamation unit.

The 41.8% remap fraction is specific to the retained worker IDs, 128 virtual
nodes per worker, and 256 keys. The consistent-hash invariant is that every
moved key goes only to newly added C; no A-owned key changes to B or vice versa.

## Reproduce

Use Python with PyTorch 2.2.2 or compatible:

```bash
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python \
  ./scripts/proof-v0.9.sh
```

To replace this retained evidence:

```bash
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python \
INFERLAB_V09_OUTPUT_DIR=docs/results/v0.9/raw \
  ./scripts/proof-v0.9.sh
```

## Raw artifacts

- [`paged-cache-check.json`](raw/paged-cache-check.json) — 22
  machine-readable release assertions and headline observations
- [`paged-cache-proof.svg`](raw/paged-cache-proof.svg) — deterministic chart
  rendered from retained JSON rather than hand-entered values
- [`page-cache-probe.json`](raw/page-cache-probe.json) — capacity exhaustion,
  release, page-size fragmentation, shared-prefix/COW, longest-prefix, and LRU
  scenarios
- [`prefix-ownership.json`](raw/prefix-ownership.json) — six cold/warm gateway
  pairs, worker cache snapshots, and 256-key two/three-worker ownership maps
- `contiguous-*.json` — complete v0.8-layout token/logit traces and metrics
- `paged-*.json` — complete paged-layout token/logit traces, page metrics, and
  pool statistics
- `paged-parity-*.json` — step-level contiguous/paged comparisons
- `torch-*.json` — independent PyTorch traces for the same prompts
- `torch-parity-*.json` — paged C++ versus PyTorch comparisons
- [`gateway-stream.json`](raw/gateway-stream.json) — timestamped paged-decoder
  SSE events, reconstruction, `[DONE]`, headers, and worker health
- [`gateway-non-stream.json`](raw/gateway-non-stream.json) — equivalent final
  response with generation and page metrics
- [`environment.json`](raw/environment.json) — host, toolchain, PyTorch, and
  checkpoint identity

## Limitations

This is a single-host Apple ARM64 experiment using an untrained
3,232-parameter model. The page manager gathers rows into temporary contiguous
vectors before the unchanged v0.8 attention kernel, so no page-aware-kernel or
speedup claim is made. Pages are process-local CPU heap storage; prefix lookup
linearly scans a bounded map; LRU is synchronous; one mutex serializes pool
operations; admission does not reserve future page growth; worker restart loses
cache contents; and consistent hashing provides affinity, not distributed cache
coherence. Sharing bytes count every session and directory ownership edge as a
logical reference.
