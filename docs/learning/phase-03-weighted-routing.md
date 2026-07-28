# Phase 03 learning guide: weighted routing

## The new behavior in one sentence

The gateway can now give a known high-capacity worker a larger, configured share of requests.

## Weight means proportion, not speed

With:

```text
A weight = 3
B weight = 1
```

the total weight is 4:

```text
A share = 3 / 4 = 75%
B share = 1 / 4 = 25%
```

A weight of 3 does not prove that A is three times faster. It records an operator's decision that A should receive three times B's request share.

Analogy: a raffle drum contains three tickets labeled A and one labeled B. Weighted routing produces the same long-run share without actually storing repeated tickets.

## Why “smooth” matters

The clumped schedule below has the correct final count:

```text
A, A, A, B
```

But this schedule spreads B's work more evenly:

```text
A, B, A, A
```

Both are 3:1. Smoothness is about spacing, not the final ratio.

The algorithm maintains a running credit:

1. Add each worker's weight to its credit.
2. Select the highest credit.
3. Subtract the total weight from the winner.
4. Repeat.

A worker that does not win keeps accumulating credit, so every positive-weight worker eventually gets a turn.

## Rust ideas introduced

### `WorkerRegistration`

Previously the gateway stored worker configuration as a pair:

```text
(ID, URL)
```

Weights introduce a third related field, so the code now uses a named structure:

```text
WorkerRegistration {
    id,
    base_url,
    weight
}
```

Named fields make invalid or reversed arguments easier to notice than a growing tuple.

### `AtomicI64`

Smooth scores sometimes become negative after the winner pays the total weight. That is why the running score uses a signed integer.

The values are atomic so they remain safe to inspect in shared worker records. The selection mutex is still required because updating every score and choosing one winner is a multi-step decision.

### Parsing at the boundary

Humans configure:

```text
worker-a:3=http://127.0.0.1:9001
```

Startup parsing converts that text into a checked `WorkerRegistration`. The rest of the gateway deals with a positive integer weight rather than repeatedly interpreting strings.

This follows a useful systems rule: validate untrusted text once at the boundary, then use typed values internally.

## Read the code in this order

1. `WorkerRegistration` in `gateway/src/routing.rs`.
2. `RoutingPolicy::WeightedRoundRobin`.
3. `choose_weighted_round_robin`.
4. `parse_workers` in `gateway/src/main.rs`.
5. The 3:1 unit test.
6. `scripts/proof-v0.0.3.sh`.
7. `benchmarks/check_weighted.py`.

## What the proof establishes

The 60/20 result proves:

- the gateway read weights correctly;
- concurrent requests used the weighted policy;
- all requests reached configured workers; and
- the observed distribution matched 3:1.

It does not establish:

- that A truly has three times B's capacity;
- that weighted routing improves latency; or
- that the policy reacts when a worker slows down.

Those require different experiments.

## Reproduce

```bash
cargo test --workspace
./scripts/proof-v0.0.3.sh
```

Retain a new raw run with:

```bash
INFERLAB_RESULTS_DIR=docs/results/v0.0.3/raw ./scripts/proof-v0.0.3.sh
```

## Check your understanding

If A, B, and C have weights 4, 2, and 2, how many of eight requests should each receive over a complete weighted cycle? Why are weights 4:2:2 equivalent to 2:1:1?
