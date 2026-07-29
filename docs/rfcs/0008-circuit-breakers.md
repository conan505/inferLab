# RFC 0008: Per-worker sliding-window circuit breakers

**Status:** Implemented | **Milestone:** v0.0.8

## Context

Deadlines stop one request from living forever, and the retry budget bounds
extra attempts. Neither remembers that a particular worker has failed
repeatedly. Round-robin can therefore keep assigning first attempts to a known
bad worker, spending latency and retry budget to rediscover the same fact.

The experiment asks:

1. Can each worker independently open after a sustained transient-failure rate?
2. Can every routing policy skip an open worker before queueing or connecting?
3. Can cooldown permit exactly one recovery probe rather than a traffic wave?
4. Can a healed worker automatically rejoin without retry amplification?

## Decision

Each registered worker owns one circuit breaker with three states:

```text
closed --failure threshold--> open --cooldown--> half-open
   ^                                                |
   |---------------- successful probe --------------|
                         |
                         +-- failed probe --> open
```

- **Closed:** ordinary attempts are allowed and classified outcomes enter a
  bounded sliding window.
- **Open:** routing does not create a worker lease, consume an execution slot,
  queue, or make a network request.
- **Half-open:** one attempt permit is available. Concurrent routing decisions
  skip the worker until that probe resolves.
- A successful probe clears the old window and closes the circuit.
- A failed probe reopens the circuit for a full cooldown.
- A cancelled probe releases the half-open slot so another request can test it.

The default window contains 10 classified outcomes, requires at least 5
samples, opens at a 50% transient-failure rate, and remains open for 5 seconds.

## Why an attempt permit?

The breaker decision happens before worker admission:

```text
route candidate
    ↓
circuit attempt permit
    ↓
worker queue / execution permit
    ↓
network attempt
```

Analogy: an airport closes a runway before aircraft line up for takeoff.
Letting requests consume queue and execution capacity before discovering the
open circuit would retain much of the incident cost.

The permit also represents ownership of a half-open probe. Rust's `Drop`
returns an unresolved probe slot on deadline expiry, overload rejection, or
cancellation.

## Sliding-window threshold

The circuit stores only the newest `window_size` classified outcomes:

```text
failure rate = failures in window / samples in window
```

It cannot open before `minimum_requests` samples exist. Once that minimum is
met, it opens when:

```text
failures × 100 >= samples × configured percentage
```

Integer comparison avoids floating-point decisions at the state boundary.

A window distinguishes a sustained error rate from one isolated failure. A
consecutive-failure counter is simpler, but it cannot express partial
degradation such as every other request failing.

## Failure classification

The breaker uses the same narrow pre-stream classification as retry logic:

| Attempt outcome | Circuit outcome |
|---|---|
| Connection failure | Failure |
| Response-header timeout | Failure |
| HTTP 502/503/504 | Failure |
| HTTP response other than 502/503/504 | Success: the worker was reachable and answered |
| Other local request-building error | Unclassified; permit is released |
| Body error after response headers | Currently unclassified |

Retry and circuit breaking observe the same failed attempt but make different
decisions. The circuit decides whether this worker should receive future
traffic; the retry policy decides whether this request may spend another
attempt.

## Generation fencing

Several closed-state requests can be in flight concurrently. Suppose failures
from two of them open the circuit, then an older slow request succeeds. That
late success must not close the newly opened circuit.

Every attempt permit captures a circuit generation. Opening, recovery, or
reopening advances the generation. Outcomes from older generations are
ignored.

This is the same idea as an epoch or fencing token in distributed systems:
fresh state cannot be overwritten by stale work.

## Routing behavior

All five policies choose their preferred worker first and then scan viable
fallback candidates:

- round-robin preserves its rotating start;
- least-in-flight ranks current leases;
- weighted round-robin preserves accumulated entitlement;
- EWMA preserves its preferred low-latency worker and exploration cadence; and
- consistent hashing walks clockwise through distinct ring owners.

When every circuit is open or already owns a half-open probe, the gateway
returns:

```text
503 Service Unavailable
Retry-After: 1
error.type = "no_available_workers"
x-inferlab-attempts = 0
```

## Observability

Each worker object in `/internal/workers` now includes:

```text
circuit.state
circuit.samples
circuit.failures
circuit.failure_rate_percent
circuit.remaining_open_ms
circuit.probe_in_flight
circuit.opened_total
circuit.rejected_total
circuit.half_open_probes_total
circuit.recoveries_total
```

The snapshot lazily advances an expired open state to half-open. No background
timer or task is required.

## Invariants

1. An open worker receives no new attempt, queue slot, or execution slot.
2. At most one half-open probe is in flight per worker.
3. A cancelled half-open permit does not permanently block recovery.
4. A failed half-open probe receives a fresh full cooldown.
5. A successful half-open probe clears stale failure history.
6. Outcomes from an older generation cannot mutate current state.
7. The breaker cannot open before the configured minimum sample count.
8. The outcome window never exceeds its configured size.
9. Each worker's circuit changes independently.
10. An all-open pool fails before an upstream attempt.

## Configuration

```bash
INFERLAB_CIRCUIT_WINDOW_SIZE=10 \
INFERLAB_CIRCUIT_MIN_REQUESTS=5 \
INFERLAB_CIRCUIT_FAILURE_RATE_PERCENT=50 \
INFERLAB_CIRCUIT_OPEN_MS=5000 \
  cargo run -p gateway
```

## Experiment result

The retained v0.0.8 run started worker A in permanent failure mode and worker B
healthy. With a four-sample window:

- four requests to A returned 503 and opened only A's circuit;
- the next four requests all succeeded on B while A's request count stayed at
  four;
- routing recorded two skipped selections of open A;
- after A restarted healthy and the 300 ms cooldown elapsed, status reported
  half-open;
- exactly one request probed A and closed its circuit;
- the following four requests split two to A and two to B; and
- with one retry enabled per request and a 10% global budget, the four early
  failures earned no retry credit: 17 originals produced exactly 17 upstream
  attempts and zero retries.

All 15 machine-readable proof assertions passed. See
[`docs/results/v0.0.8`](../results/v0.0.8/README.md).

## Alternatives considered

### Consecutive-failure threshold

This misses a worker that alternates failure and success forever. A sliding
window expresses partial failure directly.

### Active background health checks

They add traffic and can disagree with the real request path. Half-open probes
reuse genuine traffic and test the exact endpoint. Active checks may be added
later for idle pools.

### Send all traffic after cooldown

If the worker is still broken, this recreates the incident in one burst. One
probe buys evidence before restoring traffic.

### Global circuit breaker

One bad worker would remove the entire pool. The failure memory belongs to the
worker that produced it.

### Break only retry selection

First attempts would continue paying the known failure cost. Every routing
decision must respect the circuit.

## Limitations

- State is process-local and resets when the gateway restarts.
- The window is count-based, not time-based.
- A quiet open worker becomes half-open lazily on status inspection or traffic.
- Success is recorded at response headers; a later streaming-body failure does
  not currently affect the circuit.
- Static topology means removed or newly discovered workers are not yet
  supported.
- Weighted and latency policy scores are retained while a circuit is open, so a
  recovered worker may initially receive its accumulated policy entitlement.
- `Retry-After` is a simple one-second hint rather than the exact earliest
  worker cooldown.
- The next milestone combines kill, slowdown, disconnect, and recovery events
  under continuous offered load.
