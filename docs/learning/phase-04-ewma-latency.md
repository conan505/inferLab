# Phase 04 learning guide: EWMA latency routing

## The new behavior in one sentence

The gateway now remembers recent time to first streamed byte and usually selects the worker with the lowest learned value.

## Static knowledge versus observed knowledge

Weighted routing says:

> “The operator believes A should receive three times B's share.”

EWMA routing says:

> “Recent successful requests suggest A starts responding faster than B.”

Weights are configuration. EWMA is a measurement.

Analogy: weights are road speed-limit signs; EWMA is a navigation app's estimate from recent drivers.

## What does “exponentially weighted” mean?

Every update mixes the latest sample with the previous summary:

```text
new = alpha × latest + (1 − alpha) × previous
```

We do not store the entire history. The previous EWMA already summarizes it.

With alpha 0.25:

- the new sample contributes 25%;
- accumulated history contributes 75%; and
- older samples remain indirectly, with exponentially shrinking influence.

For previous=100 ms and latest=300 ms, the answer is 150 ms.

## What exactly do we measure?

The timer starts when the gateway selects a worker. The first successful body chunk stops it.

That approximates TTFT:

```text
selection
  → connection/request forwarding
  → worker initial processing
  → first SSE event
```

Total response time is deliberately not used as the routing signal because output lengths differ.

## Why the stream closure owns the observation

The gateway cannot record first-byte latency when it merely receives HTTP headers—the first SSE event has not arrived yet.

The response stream therefore carries:

- the worker lease;
- the start time; and
- a boolean saying whether the first chunk was already observed.

When the first successful chunk passes through, it records one sample. Later chunks do nothing. If the stream fails before that point, it records nothing.

## Exploration is intentionally doing something that looks worse

Once A looks fastest, exploitation keeps choosing A.

But without occasional B requests, the gateway cannot learn whether B improved. Every fifth request in the proof is a rotating probe.

This is the same tension seen in recommendation systems, clinical trials, and choosing a commute:

- use what currently looks best;
- occasionally test alternatives so knowledge does not become stale.

## Read the code in this order

1. `RoutingPolicy::EwmaLatency` in `gateway/src/routing.rs`.
2. `RoutingConfig` and its validation.
3. `LatencyEstimate` and `Worker::observe_latency`.
4. `choose_ewma_latency`.
5. `WorkerLease::observe_latency`.
6. The first-chunk closure in `gateway/src/lib.rs`.
7. `scripts/proof-v0.0.4.sh`.
8. `benchmarks/check_ewma.py`.

## Understand the recorded result

During warm-up:

```text
A EWMA ≈ 14.5 ms → 17 requests
B EWMA ≈ 34.2 ms →  3 requests
```

After A slowed:

```text
A EWMA ≈ 106.6 ms →  5 requests
B EWMA ≈  35.7 ms → 35 requests
```

The five A requests after slowdown are important. They are the cost of exploration and the evidence that A remained observable.

## What this still cannot solve

If 100 concurrent requests arrive while A has the lowest historical EWMA, many may select A before any new latency sample returns. Historical speed alone does not represent current load.

A production policy often combines latency with occupancy, outstanding requests, health, or capacity. We keep signals separate first so their behavior is understandable.

## Reproduce

```bash
cargo test --workspace
./scripts/proof-v0.0.4.sh
```

Retain a new run with:

```bash
INFERLAB_RESULTS_DIR=docs/results/v0.0.4/raw ./scripts/proof-v0.0.4.sh
```

## Check your understanding

Why must the gateway occasionally send a probe to a worker that currently has a worse EWMA? What failure of learning occurs if it never does?
