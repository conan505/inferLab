# Phase 06 learning guide: backpressure

## The new behavior in one sentence

InferLab now has a finite waiting room and finite worker execution slots, so excess requests receive a quick, explicit rejection instead of silently increasing everyone else's wait.

## Async does not mean infinite

Rust async lets one thread manage many waiting network operations. It does not multiply:

- GPU memory;
- KV-cache capacity;
- model execution slots; or
- the rate at which tokens can be generated.

Analogy: online appointment booking lets a receptionist serve many callers efficiently. It does not create more doctors.

Without admission control, accepting work is easy and finishing it is hard. The queue records the growing difference.

## The venue analogy

Think of the gateway as a venue:

- **execution permits** are seats;
- **queue permits** are marked waiting spots;
- **the admission permit** is the fire-code counter; and
- **429** is the doorman saying the building is full.

The doorman is not a failure. Letting unlimited people enter would be the failure.

## What a semaphore does

A semaphore is a counter of permits.

```text
worker concurrency = 2

request A acquires permit → 1 remains
request B acquires permit → 0 remain
request C cannot execute yet
```

When A's response stream ends or is cancelled, its owned permit is dropped and C can acquire it. Tokio wakes semaphore waiters fairly.

Unlike an ordinary numeric counter, an owned permit makes cleanup structural: every exit path eventually drops the value.

## Why the permit lives inside the response stream

Receiving upstream HTTP headers does not mean inference work is finished. Streaming may continue for seconds.

If the permit were released at headers:

```text
worker starts A → headers → gateway releases permit
worker starts B while A is still generating
```

The configured limit would become fictional. InferLab moves the permit into the body-stream closure, alongside the worker lease. The final chunk or disconnection destroys the closure and releases both.

## Queue capacity is a latency decision

Little's Law is:

```text
L = λW
```

Rearrange it:

```text
W = L / λ
```

If useful capacity is 8 requests/second:

- queue 4 represents about 0.5 seconds of waiting work;
- queue 80 represents about 10 seconds; and
- queue 8,000 represents about 1,000 seconds.

A large queue does not increase model speed. It converts overload into latency and memory.

## Why the overload client is open-loop

A closed-loop client sends a request, waits for its response, then sends the next. When the server slows, the client automatically reduces its arrival rate. That can hide overload.

The v0.0.6 client schedules requests at 40/second regardless of earlier completion time. The worker's estimated capacity is 8/second, so the offered load remains 5× capacity.

Analogy: measuring a bridge with cars that enter only after another car exits cannot reveal rush-hour congestion.

## Reading the three counters

```text
outstanding = executing + queued + tiny transition windows
executing   = requests holding worker permits
queued      = requests waiting for worker permits
```

For the proof configuration:

```text
executing ≤ 2
queued    ≤ 4
outstanding ≤ 6
```

Peak counters remain visible after traffic drains, making the bound independently checkable.

## 429 is part of the API contract

An overload response contains:

```http
HTTP/1.1 429 Too Many Requests
Retry-After: 1
content-type: application/json
```

```json
{
  "error": {
    "type": "gateway_overloaded",
    "reason": "admission_queue_full",
    "retryable": true
  }
}
```

Machine-readable rejection lets a client distinguish capacity pressure from malformed input or permanent failure. A later milestone will add jitter and a retry budget so clients do not all retry after exactly one second.

## Read the code in this order

1. `AdmissionConfig` and `AdmissionController` in `gateway/src/admission.rs`.
2. `try_admit_request`, the non-waiting outer gate.
3. `admit_worker`, the execution-or-queue decision.
4. the three RAII guard `Drop` implementations.
5. worker semaphores in `gateway/src/routing.rs`.
6. `admission_middleware` in `gateway/src/lib.rs`.
7. the stream closure that owns every guard.
8. the overload and cancellation integration tests.
9. `benchmarks/overload.py`, its checker, and the SVG timeline renderer.

## Proof layers

- A routing unit test proves a one-permit worker cannot execute a second request until the permit is released.
- The HTTP overload test holds one stream open, fills one queue slot, verifies the third request receives 429, then proves the queued request advances.
- The cancellation test proves an abandoned waiter returns its slot.
- The open-loop experiment records every request, current/peak gateway counters, accepted/rejected latency, and a gateway RSS time series.

## What this still cannot solve

If a worker obtains a permit and then hangs forever, the permit stays occupied forever. A bounded system can still become permanently full.

The next resilience topic adds deadlines: every request gets a finite time budget. Retries then require backoff, jitter, and a budget so recovery attempts do not become a second traffic spike.

## Reproduce

```bash
cargo test --workspace
./scripts/proof-v0.0.6.sh
```

Retain a new run with:

```bash
INFERLAB_RESULTS_DIR=docs/results/v0.0.6/raw ./scripts/proof-v0.0.6.sh
```

## Check your understanding

If a worker can execute 8 requests per second, why does increasing its queue from 8 to 800 not increase throughput, and what does it increase instead?
