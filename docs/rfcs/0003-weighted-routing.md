# RFC 0003: Smooth weighted round-robin

**Status:** Implemented | **Milestone:** v0.0.3

## Context

Round-robin assumes equal capacity. Least-in-flight reacts to active request counts, but it still has no configured knowledge that one worker may have more GPUs, faster hardware, or a larger safe concurrency limit.

The experiment asks: can an operator declare a capacity ratio and have the gateway produce that request-share ratio without clumping requests or duplicating workers in memory?

## Decision

- Add `weighted` as a routing policy.
- Extend worker configuration from `id=url` to `id[:weight]=url`.
- Default an omitted weight to 1 for backward compatibility.
- Reject zero and malformed weights at startup.
- Implement smooth weighted round-robin using a running score per worker.
- Serialize score updates with the existing short selection mutex.
- Expose configured weights in `/internal/workers`.

## Mental model: accumulating credits

A weight is the number of credits a worker earns every routing round.

For A=3 and B=1:

| Decision | Add weights | Winner | Subtract total weight from winner | Scores after |
|---|---|---|---|---|
| 1 | A=3, B=1 | A | A−4 | A=−1, B=1 |
| 2 | A=2, B=2 | B (rotating tie) | B−4 | A=2, B=−2 |
| 3 | A=5, B=−1 | A | A−4 | A=1, B=−1 |
| 4 | A=4, B=0 | A | A−4 | A=0, B=0 |

The four-request sequence is A, B, A, A. The scores return to zero, so the cycle repeats. A receives three requests and B receives one, while B's request is spread through the cycle.

## Why not expand a list?

A naive implementation could create `[A, A, A, B]` and run ordinary round-robin. That works for small integer weights, but:

- large weights waste memory;
- common factors must be manually reduced;
- the repeated entries can create clumps; and
- changing weights requires rebuilding the expanded list.

Smooth weighted round-robin stores one worker record and one running score per worker.

## Invariants

1. Every worker weight is a positive integer.
2. Omitted weights equal 1.
3. Each decision adds every worker's configured weight exactly once.
4. The selected worker's score decreases by the total cluster weight exactly once.
5. Score update and selection occur inside one short critical section.
6. Network and streaming work never holds the selection mutex.
7. Every positive-weight worker eventually receives requests.
8. Over complete cycles, request shares match normalized weights.

## Configuration

```bash
INFERLAB_ROUTING_POLICY=weighted \
INFERLAB_WORKERS='worker-a:3=http://127.0.0.1:9001,worker-b:1=http://127.0.0.1:9002' \
  cargo run -p gateway
```

This means A should receive 75% of routing decisions and B should receive 25%. A weight is relative: 3:1 and 6:2 describe the same intended proportions.

## Experiment and result

`./scripts/proof-v0.0.3.sh` starts two equally fast fake workers, configures weights A=3 and B=1, and sends 80 requests at concurrency 8.

The recorded run produced:

| Worker | Weight | Expected | Actual | Percentage-point error |
|---|---:|---:|---:|---:|
| A | 3 | 60 | 60 | 0.0 |
| B | 1 | 20 | 20 | 0.0 |

All 80 requests succeeded. Equal worker speeds deliberately isolate the routing arithmetic from latency effects.

## Limitations

- Weights are operator-supplied assumptions, not measured capacity.
- Weighted routing does not inspect in-flight count or current latency.
- A worker with weight 3 can still become slow while continuing to receive 75% of requests.
- Integer request counts approximate proportions over short, incomplete cycles.
- Dynamic membership and live weight changes are not implemented.

The next experiment uses EWMA latency so the gateway can learn from recent response times instead of relying only on static configuration.
