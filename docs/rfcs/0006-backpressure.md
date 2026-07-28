# RFC 0006: Bounded admission and worker concurrency

**Status:** Implemented | **Milestone:** v0.0.6

## Context

An async server can accept connections far faster than an inference worker can generate completions. Async concurrency prevents threads from blocking, but it does not create CPU, GPU memory, or generation capacity.

If offered load `λ` remains above service capacity `μ`, an unbounded queue grows for as long as overload lasts. Memory and waiting time grow with it. Eventually the system spends resources holding work it cannot finish promptly.

The experiment asks: can InferLab preserve useful completions during 5× overload while keeping active work, waiting work, latency, and gateway memory bounded?

## Decision

- Apply a non-waiting admission gate before the request body is extracted.
- Limit the total admitted requests to total worker execution capacity plus queue capacity.
- Give every worker its own Tokio semaphore with a configurable concurrency limit.
- If a selected worker has no permit, wait only after acquiring one slot from a bounded global admission queue.
- Reject immediately when no admission or queue slot remains.
- Return `429 Too Many Requests`, `Retry-After: 1`, and structured `gateway_overloaded` JSON.
- Keep the request permit, worker execution permit, routing lease, and response stream under the same lifetime.
- Release queue and execution counts through RAII when a request completes, fails, or is cancelled.
- Expose current and peak counts from `/internal/workers`.

## Why two kinds of permits?

The admission permit answers:

> May this HTTP request occupy gateway resources at all?

The worker semaphore answers:

> May this request execute on this particular worker now?

With three workers and concurrency 2, theoretical execution capacity is 6. A global queue of 4 therefore permits at most 10 outstanding completion requests. The per-worker semaphore still prevents a routing policy or hot consistent-hash key from sending more than 2 active generations to one worker.

The queue semaphore separately guarantees that no more than 4 requests wait for worker permits, even if other workers are idle.

## Lifecycle

```text
request arrives
  → try total-admission permit
      full → 429
  → select worker and create routing lease
  → try worker execution permit
      available → execute
      unavailable → try bounded queue slot
          full → 429
          available → wait fairly for that worker
  → proxy response stream
  → final chunk, failure, or cancellation drops every permit
```

The outer admission middleware runs before Axum's `Bytes` extractor. Rejected requests therefore do not first join an unbounded collection of buffered request bodies.

## Little's Law

```text
L = λW
```

- `L`: average work present in the system
- `λ`: average completed arrival rate
- `W`: average time each admitted request spends there

A queue size is therefore a latency promise. If a worker completes 8 requests/second and 4 requests wait, the queue alone represents roughly 0.5 seconds of work. A queue of 4,000 would not create throughput; it would create a much larger waiting-time promise.

## Goodput versus throughput

Throughput often counts all attempted operations. Goodput counts useful work completed within the intended service behavior.

During overload, accepting every request can reduce goodput through memory pressure, timeouts, and cascading retries. Intentional rejection preserves capacity for the work already admitted. A fast `429` is more honest than a completion that waits indefinitely.

## Invariants

1. Executing requests on one worker never exceed its configured limit.
2. Globally queued requests never exceed queue capacity.
3. Outstanding requests never exceed total execution capacity plus queue capacity.
4. Capacity checks never wait before deciding whether a request may enter.
5. An execution permit spans the entire downstream body stream.
6. Cancellation while queued releases both the queue slot and routing lease.
7. Every overload rejection is machine-readable and includes retry guidance.
8. Health and internal status endpoints remain available during completion overload.
9. Existing routing policies retain their selection semantics.

## Configuration

```bash
INFERLAB_WORKER_CONCURRENCY=2 \
INFERLAB_ADMISSION_QUEUE_CAPACITY=4 \
INFERLAB_ROUTING_POLICY=least-in-flight \
INFERLAB_WORKERS='worker-a=http://127.0.0.1:9001,worker-b=http://127.0.0.1:9002' \
  cargo run -p gateway
```

Defaults are 8 executing requests per worker and 64 queued requests globally. Queue capacity may be zero for immediate load shedding. Worker concurrency must be positive.

## Why 429 rather than 503?

`429` means this request exceeded the gateway's temporary admission capacity and the caller may retry later. `Retry-After` communicates that intent.

`503` remains appropriate when no healthy upstream service is available or a connection cannot be established. Health classification and circuit breaking arrive in a later resilience milestone.

## Experiment

`./scripts/proof-v0.0.6.sh` runs one fake worker with:

- two execution permits;
- four queue slots;
- an estimated service capacity of 8 requests/second;
- 160 open-loop requests offered at 40 requests/second; and
- gateway RSS sampling every 50 milliseconds.

The checker requires:

- execution peak ≤2 and queue peak ≤4;
- outstanding peak ≤6;
- both successful completions and intentional rejections;
- only HTTP 200 or 429 outcomes;
- correct JSON and `Retry-After` on every rejection;
- accepted p99 below one second;
- gateway RSS growth below 32 MiB; and
- all counts returned to zero after the run.

Recorded result:

| Observation | Result |
|---|---:|
| Offered load | 40 req/s, estimated 5× capacity |
| Completed | 34 |
| Intentionally rejected | 126 |
| Accepted p99 | 797.827 ms |
| Rejected p99 | 5.697 ms |
| Peak executing / configured | 2 / 2 |
| Peak queued / configured | 4 / 4 |
| Peak outstanding / configured | 6 / 6 |
| Gateway RSS increase | 976 KiB |
| Transport or unexpected HTTP failures | 0 |
| Counts after drain | 0 executing, 0 queued, 0 outstanding |

## Alternatives considered

### An unbounded Tokio channel

It decouples producers and consumers but merely relocates overload into memory. The absence of a bound is the bug.

### A global concurrency semaphore only

It bounds execution but cannot express a small, observable waiting room. Every request either executes or is rejected, leaving no controlled smoothing for short bursts.

### A queue only

A bounded queue limits waiters but does not stop a worker from executing too many memory-heavy generations concurrently.

### Blocking the caller until capacity exists

Waiting applies backpressure only when the caller shares the same bounded process chain. An internet client can open more connections. Interactive HTTP needs an explicit rejection boundary.

## Limitations

- No queue-wait or end-to-end deadline exists yet; an admitted request can wait forever if its selected worker hangs.
- Limits are static and uniform across workers.
- The queue is global, but a waiter remains bound to its selected worker.
- No priority, tenant fairness, or per-model capacity partition exists.
- `Retry-After: 1` is static rather than estimated from live drain time.
- RSS is process-level evidence from one host, not a heap attribution or universal memory guarantee.
- Clients that retry every 429 simultaneously can create another overload wave; jitter and retry budgets are the next topic.
