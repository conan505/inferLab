# v0.0.3 weighted-routing experiment

## Hypothesis

With equally fast workers and configured weights A=3 and B=1, smooth weighted round-robin should route exactly 60 of 80 requests to A and 20 to B. Equal service times isolate routing arithmetic from latency or occupancy.

## Reproduce

```bash
INFERLAB_RESULTS_DIR=docs/results/v0.0.3/raw ./scripts/proof-v0.0.3.sh
```

## Environment and workload

- Date: 2026-07-28
- Host: Apple arm64, loopback networking
- Rust: 1.97.1
- Requests: 80
- Client concurrency: 8
- Worker delays: equal, with 5 ms initial and per-event delay
- Configured weights: worker A=3, worker B=1

## Recorded result

| Worker | Expected requests | Actual requests | Error |
|---|---:|---:|---:|
| A | 60 | 60 | 0 percentage points |
| B | 20 | 20 | 0 percentage points |

All 80 requests succeeded. The measured throughput was 97.537 requests/second; this is workload metadata, not the claim under test.

## Conclusion

The hypothesis was supported exactly for this complete 20-cycle workload. Smooth weighted round-robin honored the configured capacity ratio under concurrent traffic.

This result proves policy mechanics, not the correctness of the weights. If A is assigned weight 3 but is not actually capable of three times B's share, the policy will faithfully enforce a bad assumption. EWMA latency routing is the next step toward an observed rather than configured signal.

Raw evidence:

- [`worker-status.json`](raw/worker-status.json)
- [`weighted-3-to-1.json`](raw/weighted-3-to-1.json)
- [`weighted-check.json`](raw/weighted-check.json)
