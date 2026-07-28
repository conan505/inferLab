# v0.0.4 EWMA adaptation experiment

## Hypothesis

EWMA TTFT routing should initially prefer worker A, then shift most traffic to B after A becomes slower. Deterministic probes should continue collecting A observations after the shift.

## Reproduce

```bash
INFERLAB_RESULTS_DIR=docs/results/v0.0.4/raw ./scripts/proof-v0.0.4.sh
```

## Environment and workload

- Date: 2026-07-28
- Host: Apple arm64, loopback networking
- Rust: 1.97.1
- Alpha: 0.5
- Probe interval: every 5 routing decisions
- Client concurrency: 1, keeping adaptation order observable
- Warm-up: A initial delay 5 ms, B initial delay 25 ms, 20 requests
- Slow phase: A restarts with initial delay 100 ms, 40 requests

## Recorded result

| Metric | Before slowdown | After slowdown |
|---|---:|---:|
| A EWMA TTFT | 14.485 ms | 106.562 ms |
| B EWMA TTFT | 34.191 ms | 35.749 ms |
| A request count | 17 / 20 | 5 / 40 |
| B request count | 3 / 20 | 35 / 40 |
| A observations | 17 | 22 |
| B observations | 3 | 38 |

All 60 requests succeeded.

## Conclusion

The hypothesis was supported. The gateway retained A's initially favorable history across the worker restart, observed new slow A samples, raised A's EWMA, and shifted exploitation traffic to B.

A still received five slow-phase requests. Those probes impose a short-term cost but prevent permanent ignorance: if A later recovers, fresh observations can lower its estimate again.

This closed-loop sequential experiment demonstrates learning direction, not high-concurrency stability. Pure EWMA may herd simultaneous arrivals toward the same historically fast worker.

Raw evidence:

- [`warmup-a-fast.json`](raw/warmup-a-fast.json)
- [`after-a-slow.json`](raw/after-a-slow.json)
- [`status-before.json`](raw/status-before.json)
- [`status-after.json`](raw/status-after.json)
- [`adaptation-check.json`](raw/adaptation-check.json)
