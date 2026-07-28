# RFC 0001: Minimal streaming serving path

**Status:** Accepted  
**Milestone:** v0.1

## Context

Before implementing model math, InferLab needs a real boundary between clients, a gateway, and workers. This boundary should remain useful when fake workers are replaced by the C++ runtime.

The first experiment asks: can the gateway select a worker and relay its output incrementally without accidentally buffering the completion?

## Decision

- Use an OpenAI-shaped `POST /v1/chat/completions` endpoint.
- Use SSE for streamed events and terminate with `data: [DONE]`.
- Run the gateway and each fake worker as separate processes.
- Configure workers as stable `id=url` pairs.
- Begin with round-robin because its expected sequence is deterministic and easy to falsify.
- Proxy raw upstream bytes instead of parsing and reconstructing chunks in the gateway.
- Hold an in-flight lease until the downstream response body finishes or is dropped.

## Mental models

### Streaming is a conveyor belt

Buffering constructs a warehouse: every item waits until the full order is present. Streaming is a conveyor belt: each item moves downstream as soon as it arrives. TTFT measures the wait for the first item; total latency measures the last.

### Async concurrency is waiting without occupying the cashier

An OS thread blocked on worker I/O is like a cashier staring at one customer while their payment processes. An async task records what it is waiting for, lets the thread serve other tasks, and resumes when the socket is ready.

### A worker lease is a library checkout

Selecting a worker increments its in-flight count. The checkout is not over when response headers arrive; the “book” returns only when the streaming body ends or the client disconnects. Releasing at headers would make a busy streaming worker look idle.

### A fake worker is a wind tunnel

A wind tunnel is not an airplane, but it makes one property controllable. The fake worker is not inference; it makes latency, token pacing, and failure deterministic so gateway behavior can be tested before model compute is introduced.

## Invariants

1. Every accepted request selects exactly one worker in v0.1.
2. The selected worker's in-flight count increments once and decrements once.
3. The lease outlives the entire downstream body.
4. Upstream status codes and content type reach the client.
5. Upstream chunks are forwarded without whole-body buffering.
6. Round-robin over `N > 0` workers selects index `request_number mod N`.
7. All default listeners bind only to loopback.

## Alternatives considered

### WebSockets

Rejected for the first slice. Generation is primarily server-to-client after one request, so SSE expresses the required direction with standard HTTP semantics and matches common OpenAI-compatible clients.

### One process containing all fake workers

Rejected. Separate processes preserve real sockets, independent failure, and the language-neutral boundary that the C++ runtime will later implement.

### Parse and rebuild every SSE event in the gateway

Rejected for v0.1. It adds allocation and protocol coupling without a current transformation requirement. Later observability can inspect events through a deliberate bounded parser if necessary.

### Least-in-flight first

Deferred to v0.2. It is useful under heterogeneous service time, but round-robin gives the cleanest baseline and exposes why workload cost breaks equal request counting.

## Failure semantics

- Connection failure before upstream headers: gateway returns `503` JSON.
- Upstream HTTP failure: gateway passes through the status and body.
- Failure after streaming starts: the downstream stream terminates; v0.1 does not retry.
- Empty worker configuration: gateway refuses to start.

The “never retry after streaming begins” boundary prevents duplicated or contradictory tokens. Retry budgets and circuit breakers arrive in v0.4.

## Experiment

Run `./scripts/proof-v0.1.sh`.

Expected observations:

- health endpoints become ready;
- four requests expose `x-inferlab-worker` as A, B, C, A;
- the streaming body contains multiple `data:` events and `[DONE]`;
- workspace tests pass;
- the benchmark emits TTFT, total-latency percentiles, throughput, and worker counts.

## Known limitations

There is no health-aware selection, request admission, retry, timeout, circuit breaker, metrics exporter, durable state, or model. Those omissions are phase boundaries, not hidden production claims.

