# RFC 0004: EWMA TTFT routing with exploration

**Status:** Implemented | **Milestone:** v0.0.4

## Context

Weights encode an operator's static capacity belief. They do not react when a healthy worker becomes slow. Least-in-flight reacts to occupancy but treats one active short request and one active long request as equal.

The experiment asks: can the gateway learn recent time-to-first-token behavior, route toward the currently faster worker, and still collect enough data to notice recovery or degradation?

## Decision

- Add `ewma` as a routing policy.
- Measure latency from worker selection until the first successful upstream body chunk.
- Treat that sample as gateway-observed time to first streamed byte, our current TTFT proxy.
- Update one EWMA per worker using configurable `alpha`.
- Bootstrap by sampling workers whose estimates are still unknown.
- Exploit the lowest EWMA for ordinary requests.
- Use a deterministic rotating probe every configured `N` decisions.
- Record no sample for upstream HTTP failure, stream error before the first chunk, or disconnect before the first chunk.
- Expose EWMA milliseconds and observation counts through `/internal/workers`.

## EWMA formula

```text
new = alpha × latest + (1 − alpha) × previous
```

With previous=100 ms, latest=300 ms, and alpha=0.25:

```text
new = 0.25 × 300 + 0.75 × 100 = 150 ms
```

Higher alpha adapts faster but is noisier. Lower alpha is steadier but retains stale history longer.

The first valid sample becomes the initial estimate because no previous value exists.

## Why TTFT rather than total response time?

LLM responses can have different output lengths. A worker generating 500 healthy tokens should not automatically look worse than one generating 5 tokens.

TTFT focuses on how quickly generation begins. In v0.0.4 it is measured at the first body chunk seen by the gateway, which includes gateway-to-worker connection time, worker initial delay, first-event delay, and local scheduling.

This is not yet model-only prefill time, and it is not client-observed TTFT across the entire network.

## Exploration versus exploitation

Always choosing the lowest estimate creates a feedback trap:

1. C looks slow.
2. The gateway stops selecting C.
3. C recovers.
4. No new C sample exists, so C still looks slow forever.

Every `N`th decision is therefore a rotating probe. Most requests exploit the lowest EWMA; probes deliberately refresh other workers.

Deterministic probes make tests and demonstrations reproducible. A randomized exploration policy may be appropriate later, but it requires statistical rather than exact assertions.

## Invariants

1. Alpha is finite, greater than zero, and at most one.
2. Probe interval is a positive integer.
3. Every unknown worker receives an initial sample opportunity.
4. One lease records at most one TTFT observation.
5. Only a successful first response chunk records an observation.
6. Ordinary decisions choose the lowest observed EWMA.
7. Every configured probe interval triggers a rotating exploration decision.
8. Latency-state mutation is thread-safe.
9. The response-body lease continues tracking in-flight work independently of EWMA observation.

## Configuration

```bash
INFERLAB_ROUTING_POLICY=ewma \
INFERLAB_EWMA_ALPHA=0.5 \
INFERLAB_EWMA_PROBE_INTERVAL=5 \
INFERLAB_WORKERS='worker-a=http://127.0.0.1:9001,worker-b=http://127.0.0.1:9002' \
  cargo run -p gateway
```

Defaults are alpha `0.25` and probe interval `10`.

## Experiment and result

`./scripts/proof-v0.0.4.sh` performs two sequential phases:

1. A starts with 5 ms initial delay while B uses 25 ms.
2. After 20 warm-up requests, A restarts with 100 ms initial delay.
3. The gateway remains running, retaining its learned history.
4. Another 40 requests test adaptation.

Recorded result:

| Observation | Warm-up | After A slowdown |
|---|---:|---:|
| A EWMA TTFT | 14.485 ms | 106.562 ms |
| B EWMA TTFT | 34.191 ms | 35.749 ms |
| Requests routed to A | 17 of 20 | 5 of 40 |
| Requests routed to B | 3 of 20 | 35 of 40 |

Traffic moved from the initially fast A to B after fresh A observations increased its EWMA. Five post-slowdown A requests show that probes continued collecting evidence.

## Limitations

- Pure EWMA ignores current in-flight load and can herd concurrent requests toward one worker.
- Probe frequency is static rather than driven by estimate age or uncertainty.
- Samples are process-local and disappear when the gateway restarts.
- Client backpressure and gateway scheduling can influence when the first chunk is polled.
- Errors are excluded from latency estimates; health and circuit breaking remain separate future signals.
- EWMA chooses a good worker for the next request based on history, not a guaranteed completion-time prediction.
