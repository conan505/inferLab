# Phase 01 learning guide: the serving path

Read this after RFC 0001 and before changing the gateway.

## The story of one request

1. A client serializes a chat request as JSON and opens an HTTP connection to the gateway.
2. The gateway's async runtime wakes a small task when request bytes are available.
3. Round-robin chooses a worker and creates an in-flight lease.
4. The gateway sends the unchanged JSON over a second HTTP connection.
5. The worker returns response headers, then emits SSE records over time.
6. The gateway converts the upstream byte stream into its downstream body without collecting it.
7. The client observes a first token, later tokens, a stop chunk, and `[DONE]`.
8. When the body ends—or the client disconnects—the lease drops and decrements in-flight state.

Notice that “the response arrived” is not one instant. Headers, first byte, first semantic token, and final byte are distinct events. That is why TTFT and total latency answer different questions.

## Major concepts

### HTTP is the envelope; SSE is the sequence inside it

HTTP supplies request, status, headers, and one response body. SSE gives that body a record format: events are separated by blank lines and their payload lines begin with `data:`. `[DONE]` is an application convention, not magic built into HTTP.

Try removing `[DONE]` from the fake worker. The TCP body can still close, but an OpenAI-compatible client no longer receives the protocol's explicit terminal sentinel.

### Concurrency is not parallelism

Concurrency means multiple requests can be in progress. Parallelism means CPU instructions literally execute at the same time. Async Rust is valuable here because most gateway time is socket waiting. Later, C++ matrix multiplication needs CPU/GPU parallelism because it spends time computing.

Analogy: one cook can keep several pots in progress while each simmers—that is concurrency. Several cooks chopping simultaneously is parallelism.

### Backpressure already exists, even before admission control

If a downstream client reads slowly, the socket and body machinery eventually makes upstream polling slow too. That is transport-level backpressure. It does not replace v0.3 admission control: the system still needs an explicit bound on how many requests may enter.

Analogy: traffic slowing because a road narrows is implicit backpressure; a ramp meter limiting new cars is admission control.

### Round-robin balances request counts, not work

Round-robin is like dealing cards clockwise. Each player receives the same number of cards, but the cards may have radically different cost. Prompt length, output length, KV state, and hardware speed make inference requests unequal. v0.2 will make that mismatch measurable.

### RAII makes cleanup follow ownership

Rust's resource-acquisition-is-initialization pattern ties cleanup to a value's lifetime. The in-flight counter is incremented when a lease is created and decremented in `Drop`. Every exit path—including client disconnect—therefore shares one cleanup rule.

Analogy: a hotel keycard represents an occupied room. Returning or destroying the card ends the occupancy record; we do not rely on every checkout path remembering a separate decrement call.

## Code-reading route

1. `gateway/src/routing.rs`: selection and lease ownership.
2. `gateway/src/lib.rs`: HTTP boundary and raw stream forwarding.
3. `fake-worker/src/lib.rs`: deterministic OpenAI-shaped test responses.
4. `gateway/tests/streaming.rs`: the real-socket proof.
5. `benchmarks/smoke.py`: what TTFT and latency measurement actually observe.

## Experiments to run

### E1 — Observe incremental delivery

Set `FAKE_WORKER_TOKEN_DELAY_MS=500`, use `curl -N`, and watch events appear. Then replace the gateway stream with a fully buffered `.bytes().await` implementation on a temporary branch. Predict and compare TTFT.

### E2 — Prove routing state

Send four sequential requests. Predict A, B, C, A. Restart the gateway and observe that the sequence restarts because v0.1 routing state is in memory.

### E3 — Observe failure pass-through

Run one worker with `FAKE_WORKER_FAIL_EVERY=1`. Its assigned request should be `503`; subsequent requests still rotate because health-aware routing does not exist yet.

### E4 — Observe disconnect cleanup

Start a very slow stream, inspect `/internal/workers`, cancel the client, and inspect again. The selected worker's `in_flight` should return to zero.

## Self-check questions

- Why does releasing the worker lease after headers produce incorrect least-in-flight routing later?
- Why is a single HTTP client reused rather than constructed per request?
- Why can a gateway safely retry connection refusal but not a stream that already emitted three tokens?
- What does round-robin equalize, and what does it ignore?
- Which claim does the integration test prove that a routing unit test cannot?

## Proof checklist

- [ ] `cargo test --workspace` passes.
- [ ] Four requests route A, B, C, A.
- [ ] `curl -N` visibly receives multiple events.
- [ ] `[DONE]` is present.
- [ ] Benchmark JSON contains TTFT and latency percentiles.
- [ ] Known limitations are included with any demo or release note.

