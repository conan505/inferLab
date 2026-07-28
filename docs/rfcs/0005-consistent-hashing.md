# RFC 0005: Consistent hashing for prompt-prefix ownership

**Status:** Implemented | **Milestone:** v0.0.5

## Context

A worker can eventually cache the key/value tensors produced by a shared prompt prefix. Reusing that memory requires later requests for the same prefix to reach the same worker.

`hash(key) % worker_count` gives stable ownership only while the worker count is unchanged. Changing from three workers to four changes the divisor, so most keys move and most warm cache entries become useless at once.

The experiment asks: can equal workers receive roughly balanced ownership while a worker join or leave moves only that worker's share?

## Decision

- Add `consistent-hash` as a selectable routing policy.
- Hash worker virtual-node labels onto a sorted 64-bit ring.
- Give each physical worker 128 virtual nodes by default.
- Hash a prompt-affinity key and walk clockwise to its first ring point, wrapping at the end.
- Use a specified, deterministic FNV-1a-64 hash followed by an avalanche mix; never use Rust's process-seeded default hasher for persisted ownership.
- Prefer the `x-inferlab-cache-key` request header when a caller knows the reusable prefix identity.
- Otherwise derive the key from canonical JSON containing `model` and `messages`; ignore delivery and sampling fields.
- Reject empty/duplicate worker IDs, zero virtual nodes, excessive ring size, and virtual-point collisions.

## Mental model

Imagine workers as library shelves placed around a circular room. Each prompt key is also given a position. Walk clockwise from the prompt until reaching a shelf: that shelf owns it.

Adding one shelf captures only the section immediately before it. It does not renumber every existing shelf. Virtual nodes are multiple small shelf markers for one worker, spreading ownership around the room instead of giving each worker one accidentally huge or tiny arc.

## Lookup

```text
ring points = sort(hash(worker ID + virtual-node number))
key point   = hash(prompt-affinity key)
owner       = first ring point >= key point, otherwise ring[0]
```

Sorted lookup is `O(log(V × W))`, where `V` is virtual nodes per worker and `W` is physical workers. Ring construction is outside the request hot path.

## Prompt key contract

The gateway cannot infer arbitrary prefix equivalence. Two full conversations may share a large system prompt while having different final user messages.

Therefore:

1. a cache-aware caller supplies the same `x-inferlab-cache-key` for requests whose reusable prefix is identical;
2. an ordinary caller gets full-prompt affinity from canonical `model + messages`; and
3. malformed JSON falls back to hashing the raw body.

The header is a routing hint, not authentication. A multi-tenant deployment must namespace it by trusted tenant identity before real cache sharing is enabled.

## Invariants

1. The same worker set, virtual-node count, hash function, and key always select the same worker.
2. Every ring point belongs to exactly one configured worker.
3. Lookup wraps from the largest hash value to the first ring point.
4. Removing worker D can move only keys previously owned by D.
5. Adding worker D can move only keys that become owned by D.
6. A request lease still spans the complete upstream response body.
7. Non-consistent policies retain their existing selection behavior.

## Alternatives considered

### Modulo hashing

Simple and fast, but changing `N` changes almost every `hash % N` result. That is precisely the cache-churn failure this milestone studies.

### Rendezvous hashing

For each key, score every worker and choose the maximum. It also provides minimal remapping and handles weights elegantly, but lookup is `O(W)`. It remains a good alternative if ring management becomes more complex than its benefit.

### One point per physical worker

Correct minimal remapping, poor balance. Random arc sizes vary widely. The recorded one-vnode distribution gave workers 53.52%, 6.72%, and 39.76% of 20,000 keys.

### Randomized or process-default hashing

It would make ownership change across restarts or language implementations. Cache affinity needs a named, versioned hash contract.

## Configuration

```bash
INFERLAB_ROUTING_POLICY=consistent-hash \
INFERLAB_CONSISTENT_HASH_VNODES=128 \
INFERLAB_WORKERS='worker-a=http://127.0.0.1:9001,worker-b=http://127.0.0.1:9002' \
  cargo run -p gateway
```

## Experiment and result

`./scripts/proof-v0.0.5.sh` maps 20,000 deterministic prompt keys:

| Evidence | Result |
|---|---:|
| Maximum deviation from equal share, 1 virtual node | 79.84% |
| Maximum deviation from equal share, 128 virtual nodes | 12.335% |
| Keys remapped when D joins A/B/C | 4,892 / 20,000 (24.46%) |
| Unexpected join remaps | 0 |
| Keys remapped when D leaves A/B/C/D | 4,892 / 20,000 (24.46%) |
| Unexpected leave remaps | 0 |
| Deterministic replay | true |

Virtual nodes materially improved balance. The remapped fraction is near D's one-quarter share, not the majority of keys.

## Limitations

- The gateway owns only routing affinity; a real KV-prefix cache is not implemented yet.
- The ring is rebuilt from static startup configuration and cannot change live.
- Equal virtual-node counts model equal cache capacity; heterogeneous capacity needs weighted vnode allocation.
- Hash ownership ignores current load, latency, and worker health.
- A hot prefix can overload its single owner.
- Adding/removing a worker still invalidates the moved quarter of cached prefixes; replication could reduce that cost.
- The 20,000-key corpus demonstrates these inputs and this topology, not a universal statistical guarantee.
