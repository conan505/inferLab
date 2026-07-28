# v0.0.2 unequal-worker routing experiment

## Hypothesis

When one worker is ten times slower per emitted event, least-in-flight should send it fewer requests than round-robin and improve cluster throughput. It may not eliminate slow requests because it observes active counts rather than worker speed or remaining work.

## Reproduce

```bash
INFERLAB_RESULTS_DIR=docs/results/v0.0.2/raw ./scripts/proof-v0.0.2.sh
```

## Environment and workload

- Date: 2026-07-28
- Host: Apple arm64, loopback networking
- Rust: 1.97.1
- Requests: 90 per policy
- Client concurrency: 12
- Workers A/B: 5 ms initial delay, 5 ms per SSE event
- Worker C: 5 ms initial delay, 50 ms per SSE event

## Recorded result

| Metric | Round-robin | Least-in-flight | Change |
|---|---:|---:|---:|
| Worker A assignments | 30 | 42 | +12 |
| Worker B assignments | 30 | 40 | +10 |
| Slow worker C assignments | 30 | 8 | -22 |
| Requests/sec | 41.583 | 77.083 | +85.371% |
| E2E p90 | 580.683 ms | 90.189 ms | -84.468% |
| E2E p95 | 582.344 ms | 578.471 ms | -0.665% |

All 180 requests across both runs succeeded.

## Conclusion

The hypothesis was partly supported. Least-in-flight reacted to longer-lived requests and used the slow worker much less, producing a large throughput and p90 improvement.

The near-unchanged p95 matters: 8 of 90 least-in-flight requests still went to C. That 8.89% slow cohort includes the 95th-percentile position. Least-in-flight balances active counts, not predicted completion time, so it cannot fully account for heterogeneous speed.

Raw evidence:

- [`round-robin.json`](raw/round-robin.json)
- [`least-in-flight.json`](raw/least-in-flight.json)
- [`comparison.json`](raw/comparison.json)
