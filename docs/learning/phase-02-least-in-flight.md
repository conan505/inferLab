# Phase 02 learning guide: least-in-flight routing

## The new behavior in one sentence

Instead of blindly taking turns, the gateway can now send a request to the worker with the fewest unfinished responses.

## Follow one example

Suppose the counts are:

```text
A = 3 active requests
B = 1 active request
C = 2 active requests
```

The algorithm:

1. reads all three counters;
2. identifies `1` as the minimum;
3. selects B;
4. increments B to `2`; and
5. returns a lease representing that active request.

The counter is decremented only after the full streamed response ends or the client disconnects.

## Rust ideas introduced

### `enum`: a choice from a closed set

`RoutingPolicy` can currently be only:

```text
RoundRobin
LeastInFlight
```

An enum prevents invalid internal states such as a misspelled policy string circulating through the gateway. The environment string is parsed once during startup and converted into this checked form.

Analogy: after checking a paper ticket at the entrance, the system exchanges it for one of two official wristband colors. The rest of the venue reasons about wristbands, not arbitrary handwriting.

### Atomic counter: safe change to one number

`AtomicUsize` lets concurrent tasks increment, decrement, or read one integer without corrupting it.

It does not automatically make a multi-step algorithm atomic. “Read three numbers, choose one, then increment it” contains several operations.

### Mutex: one person edits the decision board at a time

The selection mutex protects the multi-step decision. A task locks it, checks the counts, reserves a worker, and immediately unlocks it.

Analogy: atomic counters are three safe digital score displays. The mutex is the rule that only one dispatcher may inspect the displays and assign the next taxi at a time.

The mutex does not remain locked while inference runs. That would turn the entire gateway into a one-request-at-a-time system.

### Tie-breaking

When all counts are equal, “choose the minimum” gives no answer. InferLab rotates where it begins scanning so ties produce A, B, C, A instead of always A.

## Read the code in this order

1. `RoutingPolicy` in `gateway/src/routing.rs` — the two allowed policies.
2. `WorkerPool::choose` — dispatch to the selected algorithm.
3. `choose_least_in_flight` — lock, rotating scan, and reservation.
4. `WorkerLease::drop` — completion decreases the count.
5. `gateway/src/main.rs` — environment text becomes a `RoutingPolicy`.
6. `scripts/proof-v0.0.2.sh` — the falsifiable experiment.

## Understand the result

Round-robin sent exactly one-third of requests to the worker that was ten times slower. Least-in-flight noticed that C's requests remained active longer and increasingly selected A and B.

That improved p90 dramatically, but not p95. Percentiles are positions in an ordered sample:

- p90 asks, roughly, “how slow was the 90th request out of 100?”
- p95 asks, roughly, “how slow was the 95th request out of 100?”

Because 8.89% of least-in-flight requests still went to slow worker C, p90 landed among fast requests while p95 landed among slow requests.

This is why reporting only an average—or only one percentile—can hide the shape of a system.

## What least-in-flight still cannot know

If A has two tiny requests and B has one enormous request, least-in-flight chooses B even though A may finish first. It counts requests; it does not estimate their remaining cost.

Later policies will add two different signals:

- **weights:** configured knowledge that one worker has more capacity;
- **EWMA latency:** learned knowledge from recent response times.

## Reproduce

```bash
cargo test --workspace
./scripts/proof-v0.0.2.sh
```

To retain new raw output:

```bash
INFERLAB_RESULTS_DIR=docs/results/v0.0.2/raw ./scripts/proof-v0.0.2.sh
```

## Check your understanding

If A has two short active requests and B has one very long active request, least-in-flight selects B. Explain why that decision follows the policy and why it still might be slower.
