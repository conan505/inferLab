# Phase 08 learning guide: circuit breakers

## The new behavior in one sentence

InferLab remembers when one worker is repeatedly unhealthy, temporarily routes
around it, and uses one controlled request to prove recovery before restoring
ordinary traffic.

## Why retries were not enough

In v0.0.7, worker A could fail and the request could retry on B. But the next
request could choose A again and repeat the same expensive discovery.

Retries answer:

> May this request make another attempt?

The circuit breaker answers:

> Should any new request try this worker right now?

The retry budget is a global load-amplification fuse. The circuit breaker is
worker-local failure memory. We need both.

## The three states

### Closed

The electrical circuit is connected, so traffic flows. “Closed” does not mean
broken.

The breaker records a bounded window of classified successes and failures. It
opens only after enough evidence exists and the failure-rate threshold is met.

### Open

The circuit is disconnected. Routing skips that worker before queueing or
connecting.

Analogy: after several ovens trip the same kitchen breaker, the restaurant host
stops assigning new meals to that kitchen. Continuing to send orders merely to
confirm it is still broken wastes customer time.

### Half-open

After cooldown, the worker receives one test request.

- success → close and restore normal routing;
- failure → reopen for a new full cooldown; or
- cancellation → release the probe slot for another request.

Half-open is cautious optimism: “we have waited, but we still need evidence.”

## Why only one probe?

Cooldown is not proof of recovery. If 1,000 waiting clients all test the worker
simultaneously, half-open becomes another retry storm.

A single probe is a small experiment:

```text
unknown recovery state + one request → evidence
```

All other routing decisions continue using healthy workers while the experiment
is in flight.

## Sliding window instead of consecutive failures

Consider outcomes:

```text
failure, success, failure, success, failure, success
```

There are never two consecutive failures, yet the worker loses half its
requests. A consecutive counter calls this acceptable. A sliding window reports
a 50% error rate.

InferLab retains only the newest configured outcomes, requires a minimum sample
count, and uses integer threshold arithmetic:

```text
failures × 100 >= samples × threshold_percent
```

The bounded window lets old history age out and prevents memory growth.

## The attempt permit

Routing does not merely ask “is the circuit open?” and then act later. That
check would race under concurrency.

It acquires a circuit attempt permit:

- closed permits are ordinary observations;
- one half-open permit owns the recovery probe; and
- open circuits return no permit.

The permit exists before the worker queue and network call. Its lifetime also
handles cancellation through RAII.

## Generation fencing

Imagine three requests started while closed:

```text
request 1: slow success
request 2: failure ┐
request 3: failure ┴─ opens circuit
request 1: late success
```

Request 1 belongs to the old world. Its late success must not close the circuit
opened by newer evidence.

Each permit carries a generation number. State-changing transitions advance the
generation, so stale outcomes are ignored.

Analogy: a hotel keycard from yesterday should not unlock a room after a new
guest checks in.

## What counts as failure?

The breaker and retry classifier intentionally agree:

- connection failure;
- response-header timeout; and
- HTTP 502, 503, or 504.

A 400 or 401 is not worker-health evidence: the worker was reachable and
answered. A streaming-body failure currently occurs after the breaker records
header success and is a documented limitation.

## Reading the live proof

The retained experiment has four visible phases:

```text
closed --4 failures--> open --300 ms--> half-open --1 success--> closed
```

While A was open:

- four new requests all went to B;
- A's process request counter did not change; and
- two round-robin choices that would have selected A were rejected locally.

After A restarted:

- status inspection showed half-open;
- one real request reached A;
- the probe succeeded and recorded one recovery; and
- subsequent requests used both A and B again.

The final accounting was:

```text
17 upstream attempts = 17 original requests + 0 retries
```

One retry per request was enabled, but the 10% global budget denied all four
early failure-phase retries. The breaker removed repeated failure cost without
creating extra load.

## Read the code in this order

1. `CircuitBreakerConfig` in `gateway/src/circuit_breaker.rs`.
2. `CircuitState` and the outcome window.
3. `try_acquire_at` and the single half-open permit.
4. `resolve`, generation checks, and threshold arithmetic.
5. `Drop for CircuitAttempt`.
6. `try_lease_indices` in `gateway/src/routing.rs`.
7. gateway success/failure recording in `gateway/src/lib.rs`.
8. state-machine unit tests.
9. recovery integration tests in `gateway/tests/streaming.rs`.
10. `proof-v0.0.8.sh` and its machine-readable checker.

## Reproduce

```bash
cargo test --workspace
./scripts/proof-v0.0.8.sh
```

Retain a new run with:

```bash
INFERLAB_RESULTS_DIR=docs/results/v0.0.8/raw ./scripts/proof-v0.0.8.sh
```

## What comes next

The next milestone is a continuous chaos experiment. Instead of testing one
controlled failure phase at a time, it will inject worker death, slowdown, and
disconnects while requests keep arriving, then plot detection, rerouting, and
recovery.

## Check your understanding

Why does half-open allow one probe instead of immediately restoring all traffic
after the cooldown expires?
