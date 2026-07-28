# RFC 0002: Least-in-flight routing

**Status:** Implemented
**Milestone:** v0.0.2

## Context

Round-robin equalizes request counts. It cannot see that one worker is slow or that one worker already has several unfinished streams. Inference requests also have unequal prompt and output lengths, so equal request counts rarely mean equal work.

The experiment asks: when workers have unequal service times, does routing to the smallest active-request count improve how the cluster is used?

## Decision

- Make the gateway routing policy configurable with `INFERLAB_ROUTING_POLICY`.
- Support `round-robin` and `least-in-flight`.
- Define in-flight as “selected but response body not yet completed or dropped.”
- Read all current counters and increment the chosen counter inside one short mutex-protected selection step.
- Rotate the scan's starting index so equal-count ties are distributed fairly.
- Keep network calls and response streaming outside the mutex.

## Why selection needs a mutex

Consider two requests arriving together while every worker has a count of zero:

1. Request X reads A=0.
2. Request Y reads A=0.
3. Both select A.
4. Both increment A.

The individual reads and increments can each be thread-safe while the combined decision is still wrong. The mutex makes “inspect counts and reserve one slot” a single logical operation among competing selectors.

The protected section contains only a few counter reads and one increment. The lock is released before connecting to a worker, so slow inference never holds the routing lock.

## Tie-breaking

Always scanning from worker A would silently bias equal-count decisions toward A. A rotating start index produces A, B, C, A when all counts are tied.

Tie-breaking is not the routing policy itself. It is the smaller rule used when the main policy cannot distinguish candidates.

## Invariants

1. Every accepted v0.0.2 request reserves exactly one worker.
2. Selection plus reservation is serialized for least-in-flight callers.
3. The response body owns the lease until completion or disconnect.
4. The in-flight counter increments and decrements exactly once.
5. A uniquely smallest observed count is selected.
6. Equal-count selections rotate rather than permanently favoring worker zero.
7. No network operation occurs while the selection mutex is held.

## Configuration

```bash
INFERLAB_ROUTING_POLICY=least-in-flight cargo run -p gateway
```

Accepted names include `round-robin`, `rr`, `least-in-flight`, and `lif`. An unknown policy prevents startup instead of silently choosing a default.

## Experiment and result

`./scripts/proof-v0.0.2.sh` runs 90 requests at concurrency 12 against:

- worker A: 5 ms per SSE event;
- worker B: 5 ms per SSE event; and
- worker C: 50 ms per SSE event.

The recorded local run found:

| Metric | Round-robin | Least-in-flight |
|---|---:|---:|
| Assignments to A/B/C | 30 / 30 / 30 | 42 / 40 / 8 |
| Requests/sec | 41.583 | 77.083 |
| End-to-end p90 | 580.683 ms | 90.189 ms |
| End-to-end p95 | 582.344 ms | 578.471 ms |

Least-in-flight reduced slow-worker assignments by 22 and improved throughput by 85.371% in this synthetic run.

The p95 improvement was only 0.665%. Eight of 90 requests still used worker C, which is 8.89% of the sample; therefore the 95th percentile remained inside the slow group. This is evidence of the policy's limitation, not a failed measurement.

## Limitations

- A request count is not a work estimate. One 2,000-token generation can cost more than several short requests.
- The policy reacts to current occupancy but does not directly learn worker speed.
- A newly joined or recently idle slow worker initially looks attractive.
- There is no health filtering, capacity weight, latency history, or queue-depth signal yet.
- The mutex serializes routing decisions. Its tiny critical section is appropriate here, but contention must eventually be measured rather than assumed harmless.

Weighted routing and EWMA-latency routing are the next experiments for incorporating capacity and observed service time.
