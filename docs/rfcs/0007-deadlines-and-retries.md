# RFC 0007: Deadline-aware retries with full jitter and a budget

**Status:** Implemented | **Milestone:** v0.0.7

## Context

Backpressure bounds how much work enters InferLab, but an admitted request can still occupy capacity forever if a worker hangs. Retrying every failure is not a solution: retries are additional load precisely when the system may already be unhealthy.

The experiment asks:

1. Can one end-to-end deadline cover admission wait, upstream attempts, backoff, and streaming?
2. Can a transient failure move to an untried worker before any downstream output?
3. Can cumulative retries remain at or below 10% of original admitted requests?
4. Does full jitter reduce synchronized retry spikes compared with identical exponential delays?

## Decision

- Start a monotonic request deadline in middleware before request-body extraction.
- Apply that same deadline to admission waiting, attempt timeouts, backoff sleeps, and downstream streaming.
- Give each attempt its own smaller response-header timeout.
- Propagate the current attempt timeout to workers as `x-inferlab-timeout-ms` and the one-based attempt as `x-inferlab-attempt`.
- Classify connection errors, response-header timeouts, and HTTP 502/503/504 as transient.
- Retry only before a response is returned downstream.
- Prefer a worker not already tried by this request.
- Use exponential backoff caps and sample each delay uniformly from `[0, cap]` (full jitter).
- Enforce a process-wide cumulative retry budget:

```text
allowed retries = floor(original admitted requests × budget percent / 100)
```

- Default to a 10% budget, two retries, 25 ms base delay, and 500 ms maximum delay.
- Expose resilience configuration and counters from `/internal/workers`.

## Deadline versus timeout

A deadline is an absolute end of the entire request budget:

```text
admission + queue + attempt 1 + backoff + attempt 2 + streaming ≤ deadline
```

An attempt timeout is a smaller local bound on waiting for one worker's response headers:

```text
attempt timeout = min(configured attempt timeout, remaining request budget)
```

The worker receives that remaining attempt budget as a relative duration. A monotonic `Instant` cannot be serialized across machines, and absolute wall clocks can disagree.

## Retry safety boundary

Analogy: redial only before the other person answers.

Before downstream headers or body:

- InferLab privately owns the failed attempt.
- A 503 response can be discarded.
- Another worker can attempt the same request.

After downstream streaming starts:

- the client may already have observed tokens;
- another worker may generate different tokens; and
- restarting would duplicate or splice output.

InferLab therefore never retries a stream after returning its response. If the deadline expires mid-stream, the body ends, permits are released, and `[DONE]` is absent.

## Backoff and full jitter

The retry cap grows exponentially:

```text
cap(0) = min(max, base × 1)
cap(1) = min(max, base × 2)
cap(2) = min(max, base × 4)
```

Without jitter, 1,000 clients that fail together retry at the same boundaries. The outage receives concentrated waves.

Full jitter chooses:

```text
delay = uniform random duration from 0 through cap
```

The implementation uses a process-seeded SplitMix64 sequence. It is not cryptographic; its job is to decorrelate timing. Tests and the proof provide an explicit seed for reproducibility.

## Retry budget

Retries consume the same worker, network, queue, and memory resources as original requests. The global cumulative budget makes that cost explicit.

At 10%:

- requests 1–9 earn no retry;
- request 10 makes one cumulative retry available;
- request 20 makes a second available; and
- concurrent reservations use atomic compare-and-exchange so they cannot oversubscribe the allowance.

A reservation is refunded if the request deadline expires during backoff before the retry begins.

This strict bootstrap behavior favors overload safety over helping the first few requests in a new process.

## Resource ownership

A failed attempt releases its worker execution permit and routing lease before backoff. The outer request remains admitted, but sleeping must not occupy generation capacity.

A successful final attempt moves these values into the downstream body stream:

- request admission permit;
- worker execution permit;
- worker routing lease; and
- request deadline timer.

Completion, stream error, deadline, or client cancellation drops them together.

## Invariants

1. No attempt timeout exceeds the remaining request deadline.
2. Queue wait, attempts, backoff, and streaming share one end-to-end deadline.
3. No retry occurs after downstream response delivery begins.
4. Only classified transient failures are retried.
5. Each retry prefers an untried worker when one exists.
6. Cumulative committed retries never exceed the configured percentage of original admitted requests.
7. A retry reservation that never starts is refunded.
8. Full-jitter delay never exceeds its exponential cap.
9. Backoff owns no worker execution permit.
10. Every attempt carries a relative timeout and attempt number to the worker.

## Configuration

```bash
INFERLAB_REQUEST_DEADLINE_MS=30000 \
INFERLAB_ATTEMPT_TIMEOUT_MS=5000 \
INFERLAB_MAX_RETRIES=2 \
INFERLAB_RETRY_BUDGET_PERCENT=10 \
INFERLAB_RETRY_BASE_DELAY_MS=25 \
INFERLAB_RETRY_MAX_DELAY_MS=500 \
  cargo run -p gateway
```

`INFERLAB_JITTER_SEED` is optional and intended primarily for reproducible experiments.

## Failure classification

| Outcome before downstream delivery | Retry? | Reason |
|---|---|---|
| Connection failure | Yes | Another worker may be reachable |
| Response-header timeout | Yes | No client-visible response exists yet |
| HTTP 502/503/504 | Yes | Narrow transient upstream class |
| HTTP 400/401/403/404 | No | Retrying does not repair the request |
| HTTP 429 from gateway admission | No internal retry | Retrying would amplify local overload |
| Body error or deadline after streaming begins | No | Client may already have observed output |

## Experiment result

The retained v0.0.7 proof sent 10 healthy warmup requests followed by 10
requests while worker A returned 503:

- 20 original requests produced 22 total attempts;
- the 10% cumulative budget granted exactly 2 retries;
- both retries recovered on the untried worker B;
- 4 more transient failures were denied a retry by the budget; and
- worker B observed `x-inferlab-attempt: 2` and the propagated 300 ms
  attempt timeout.

A separate slow-worker probe used a 180 ms request deadline. It returned a
machine-readable 504 after 187.447 ms, made one attempt, performed no retry,
and the worker observed 179 ms of remaining attempt budget.

The deterministic 1,000-client simulation scheduled three retries per client.
Identical exponential backoff produced a peak of 1,000 retries in one 25 ms
bucket. Full jitter spread events across 27 buckets and reduced the peak to
362, a 63.8% reduction.

The checker passed all 16 assertions. The report and raw artifacts live in
[`docs/results/v0.0.7`](../results/v0.0.7/README.md).

## Alternatives considered

### Retry immediately

Immediate retries preserve synchronization and add load without giving a dependency time to recover.

### Exponential backoff without jitter

Every caller computes the same delay and produces periodic retry waves.

### Unlimited retries per request

One request can consume arbitrary capacity and outlive the user's patience.

### A per-request retry count without a global budget

Ten thousand failing original requests with two retries can create twenty thousand extra attempts. A local limit does not bound fleet-wide amplification.

### Retry after streaming starts

This can duplicate tokens or splice outputs from different workers and is rejected categorically.

## Limitations

- The cumulative budget does not use a rolling time window and resets on process restart.
- A strict 10% budget permits no retry during the first nine admitted requests.
- Streaming deadline expiry ends the body without a final `[DONE]`; HTTP status cannot change after headers.
- The propagated timeout is a custom educational header rather than a standardized RPC deadline.
- Retry selection avoids workers only within one request; it does not remember cluster health.
- A repeatedly failing worker continues receiving first attempts. Per-worker circuit breakers are the next milestone.
- Client-side retries are outside the gateway budget and can still amplify traffic.
