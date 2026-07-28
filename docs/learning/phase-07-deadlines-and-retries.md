# Phase 07 learning guide: deadlines and bounded retries

## The new behavior in one sentence

InferLab can try another worker after a narrow transient failure, but only before streaming, only within one end-to-end time budget, and only while the global retry allowance remains below 10%.

## Deadline: an expiry time for the whole journey

A timeout often describes one operation:

> Wait at most 5 seconds for this worker's response headers.

A deadline describes the entire user-visible journey:

> This request must be over by 30 seconds from admission.

Analogy: an airline connection has several legs, but your visa expires at one fixed time. Delaying one leg does not move the expiry time.

```text
30-second deadline
├── queue wait
├── attempt A
├── jittered backoff
├── attempt B
└── streamed body
```

Every stage asks how much time remains. No stage receives a fresh 30 seconds.

## Retry safety: has the client heard anything?

Suppose worker A returns 503 before the gateway responds to the client. The gateway can discard A's private response and try B.

Suppose A has already streamed `"The answer is "`. Retrying on B might produce `"It depends..."`. Combining them would corrupt the response; restarting would duplicate client-visible output.

The safety boundary is not “GET versus POST” by itself. It is whether an externally visible effect has begun. In this gateway, no retry occurs after downstream response delivery.

## Why errors need classification

Retries help only failures that another attempt may change.

- Connection refused: B may still be reachable.
- 503: a different worker may be ready.
- 400 malformed JSON: every worker will reject it.
- 401 unauthorized: waiting does not create credentials.
- Local 429 overload: an internal retry would consume more of the capacity already missing.

“Retry all errors” is not resilience; it is load amplification.

## Exponential backoff is not enough

For base 100 ms:

```text
retry 1 cap = 100 ms
retry 2 cap = 200 ms
retry 3 cap = 400 ms
```

If every failed client waits exactly those values, they retry together at 100, 300, and 700 ms.

Full jitter chooses a different value between zero and each cap. The cap still grows, but arrivals spread across time.

Analogy: after a fire alarm, telling everyone to re-enter in exactly five minutes creates another crowd at the door. Giving each person a random return window spreads demand.

## A retry budget is an error-amplification fuse

Two retries per request sounds bounded, but at scale:

```text
10,000 originals × 2 retries = 20,000 extra attempts
```

InferLab's default cumulative budget allows:

```text
10,000 originals × 10% = at most 1,000 retries
```

The per-request maximum and global budget solve different problems:

- max retries prevents one request from looping;
- global budget prevents all requests together from multiplying an incident.

## Why reservation uses RAII

A retry token is reserved before backoff so concurrent requests cannot claim the same allowance.

If the deadline expires while sleeping, the reservation object is dropped without `commit()`. Its `Drop` implementation refunds the token.

This is the same ownership idea used for worker leases and admission slots: make cleanup a property of value lifetime, not a collection of manually remembered branches.

## Why release execution capacity before sleeping?

Backoff deliberately does no worker work.

Holding a worker permit during a 400 ms sleep would reduce useful capacity during failure—the opposite of resilience. The failed response, execution guard, and routing lease are dropped before the timer begins.

## Reading the counters

`/internal/workers` now reports:

```text
original_requests
attempts
transient_failures
retries_granted
retries_denied_budget
retry_limit_exhausted
deadline_exceeded
```

The key accounting identity in a fully drained run is:

```text
attempts = original requests + committed retries
```

Requests rejected by the outer admission gate are not counted as admitted originals.

## Read the code in this order

1. `ResilienceConfig` in `gateway/src/resilience.rs`.
2. `RequestContext` and its one monotonic deadline.
3. `reserve_retry` and the 10% compare-and-exchange loop.
4. `RetryReservation::commit` and `Drop`.
5. `FullJitter::delay` and `exponential_cap`.
6. the attempt loop in `gateway/src/lib.rs`.
7. transient status/error classification.
8. the downstream `take_until` deadline.
9. retry/deadline integration tests in `gateway/tests/streaming.rs`.
10. `retry-simulate`, the resilience probe, checker, and SVG renderer.

## Proof layers

- Unit tests prove exponential caps, configuration rejection, strict 10% accounting, reservation refund, and alternate-worker selection.
- Integration tests prove 503 failover, timeout-header propagation, admission wait under the same deadline, and no retry after stream headers.
- The live proof sends 10 healthy warmup requests and then 10 requests while A fails, earning exactly two cumulative retries—not one retry per failure.
- A slow worker demonstrates machine-readable 504 near the configured 180 ms deadline.
- A 1,000-client simulation compares retry waves with and without full jitter.

## What this still cannot solve

The gateway knows only that an individual attempt failed. It does not remember that worker A has failed repeatedly across requests.

The next topic is the circuit breaker:

```text
closed → open → half-open → closed
```

It will stop sending ordinary attempts to a repeatedly failing worker, wait for a cooldown, and use a controlled probe to detect recovery.

## Reproduce

```bash
cargo test --workspace
./scripts/proof-v0.0.7.sh
```

Retain a new run with:

```bash
INFERLAB_RESULTS_DIR=docs/results/v0.0.7/raw ./scripts/proof-v0.0.7.sh
```

## Check your understanding

Why is it safe to retry a 503 that the gateway has not returned to the client, but unsafe to retry after the client has received even one streamed token?
