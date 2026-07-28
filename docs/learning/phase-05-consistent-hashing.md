# Phase 05 learning guide: consistent hashing

## The new behavior in one sentence

When consistent-hash routing is selected, the same prompt-prefix key repeatedly goes to the same worker, and changing the worker set moves only the affected worker's share.

## Why should a gateway care where a prompt goes?

Suppose 1,000 requests start with the same long system prompt. A future inference worker can calculate that prefix once and retain its KV-cache blocks.

The next request benefits only if it reaches the worker holding those blocks. Round-robin deliberately sends repetitions elsewhere. Least-in-flight and EWMA may change their choices from moment to moment. They optimize immediate load or latency, not memory locality.

Analogy: a supermarket loyalty card should lead you back to the locker containing your pre-packed recurring order. Sending you to the shortest random queue loses the prepared work.

## Why ordinary modulo hashing fails

With four workers:

```text
owner = hash(key) % 4
```

After one worker joins:

```text
owner = hash(key) % 5
```

The same integer is now divided by a different number. Most remainders change even though only one worker was added. This creates a cache cold-start wave.

Consistent hashing changes the question from “what is the remainder?” to “what is the next worker clockwise on a stable ring?”

## The ring, one step at a time

```text
0 ---------------------------------------------------------- 2^64 - 1
          A₁          C₁       B₁          A₂       C₂
                      ^
                  prompt key
```

The prompt belongs to `C₁`, the first point clockwise. If the key lands after the final point, lookup wraps to the first.

The implementation stores sorted points and uses `partition_point`, a binary search. It does not walk all points.

## Why one worker appears many times

One random point gives a worker one random-sized arc. Those arcs can be wildly unequal.

Virtual nodes give A labels such as `A#vnode-0` through `A#vnode-127`. Their positions interleave with B and C, so each worker receives many small arcs. Large and small accidents average out.

The measured maximum deviation from equal share fell:

```text
1 vnode   → 79.840%
16 vnodes → 49.630%
128 vnodes → 12.335%
```

More points consume memory and make ring rebuilds slower, so 128 is a deliberate default rather than “infinity.”

## What exactly is the key?

There are two levels:

- **Explicit prefix affinity:** `x-inferlab-cache-key` identifies a reusable shared prefix. The caller must namespace and compute it correctly.
- **Automatic full-prompt affinity:** without the header, the gateway serializes `model + messages` canonically.

The fallback ignores `stream`, temperature, and other sampling options because they do not change the input tokens whose prefix could be cached. A different final user message does change the fallback key, even if most earlier messages match. Discovering longest shared prefixes belongs to a later cache implementation.

## What “minimal remapping” actually promises

Before removing D:

```text
A owns some arcs
B owns some arcs
C owns some arcs
D owns some arcs
```

After removing D, its clockwise successors inherit only D's arcs. A key formerly owned by A, B, or C must not move.

The proof checks this per key; it does not merely check that “about 25% moved.” A plausible percentage with even one unrelated move would fail the invariant.

## Read the code in this order

1. `stable_hash` in `gateway/src/routing.rs`.
2. `ConsistentHashRing::new` and its virtual-node loop.
3. `ConsistentHashRing::owner_index` and its binary search.
4. `WorkerPool::choose_for_key`.
5. `prompt_affinity_key` in `gateway/src/lib.rs`.
6. The HTTP affinity integration test in `gateway/tests/streaming.rs`.
7. `gateway/src/bin/hash-ring-analyze.rs`.
8. `benchmarks/check_consistent_hash.py`.

## Proof layers

- Unit tests prove deterministic hash output, repeated-key affinity, invalid configuration rejection, and the removal property across 10,000 keys.
- The HTTP integration test proves the header actually crosses the gateway routing boundary.
- The analyzer measures distribution at 1, 16, and 128 virtual nodes and checks both join and leave remapping across 20,000 keys.
- The checker converts measured claims into pass/fail conditions.

## What this still cannot solve

Affinity and load balancing pull in different directions. If one prompt becomes extremely popular, consistent hashing keeps sending it to one worker even when that worker is overloaded.

Production systems combine techniques: replicate hot prefixes, use multiple candidate owners, bound admission, or fall back when an owner is unhealthy. This milestone isolates locality first so its guarantee is visible.

## Reproduce

```bash
cargo test --workspace
./scripts/proof-v0.0.5.sh
```

Retain a new run with:

```bash
INFERLAB_RESULTS_DIR=docs/results/v0.0.5/raw ./scripts/proof-v0.0.5.sh
```

## Check your understanding

If worker D is removed from an A/B/C/D ring, which keys are allowed to change owners, and why would seeing an A-owned key move indicate a bug?
