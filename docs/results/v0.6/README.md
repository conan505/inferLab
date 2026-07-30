# v0.6 proof: three-node Raft control plane

This retained experiment kills two successive leaders, commits configuration
with one node down, restarts stale nodes from their original disks, proves final
log identity, and keeps real gateway requests flowing during both elections.

## Hypothesis

With three control-plane nodes:

- exactly one leader is observed per term;
- losing one leader causes a bounded re-election;
- the remaining majority can commit a new routing configuration;
- a restarted stale node repairs its log and applies the committed state;
- no committed configuration is lost;
- gateway request serving continues from its last committed snapshot; and
- later committed revisions change real gateway routing.

## Timeline

![Raft leadership, commits, failures, restarts, repair, and gateway snapshots](raw/raft-timeline.svg)

## Result

| Observation | Retained result |
|---|---:|
| Leadership sequence | node A term 1 → node B term 2 → node A term 3 |
| Re-election after first kill | 364.540 ms |
| Re-election after second kill | 243.314 ms |
| Configuration log revisions | 2, 4, 6 |
| Final committed policy | weighted round robin, weights 3:1:1 |
| Final identical persistent logs | 3 / 3 |
| Final log / commit index | 6 / 6 |
| Requests during first election | 6 / 6 succeeded |
| Requests during second election | 6 / 6 succeeded |
| Final weighted request distribution | 6 / 2 / 2 |
| Machine-readable assertions | 17 / 17 passed |

Each leader appends a no-op in its own term, so configuration commands occupy
even revisions:

```text
term 1: [1 no-op] [2 round-robin]
term 2: [3 no-op] [4 least-in-flight]
term 3: [5 no-op] [6 weighted-round-robin]
```

After each kill, the harness retains the exact positive child PID and verifies
ownership before signaling it. Restarted nodes reuse the same data directory;
their final logs are byte-equivalent as decoded JSON state.

## Reproduce

```bash
./scripts/proof-v0.6.sh
```

To replace the retained evidence:

```bash
INFERLAB_RESULTS_DIR=docs/results/v0.6/raw \
  ./scripts/proof-v0.6.sh
```

## Raw artifacts

- [`raft-analysis.json`](raw/raft-analysis.json) — merged node events, failure
  events, elections, writes, convergence snapshots, gateway observations, and
  final persistent states
- [`raft-check.json`](raw/raft-check.json) — 17 machine-readable assertions
- [`raft-timeline.svg`](raw/raft-timeline.svg) — deterministic rendering of the
  merged timeline
- [`fault-events.jsonl`](raw/fault-events.jsonl) — exact killed leader PIDs,
  terms, timestamps, scope, and bind
- `node-a-events.jsonl`, `node-b-events.jsonl`, and `node-c-events.jsonl` —
  synced local state-transition traces
- `node-a-state.json`, `node-b-state.json`, and `node-c-state.json` — final
  durable Raft term, vote, log, and commit state
- `initial-election.json`, `re-election-1.json`, and `re-election-2.json` —
  observed roles, leaders, terms, and latencies
- `config-*-write.json` and convergence snapshots — commit responses and
  per-node applied state
- `gateway-*.json` — real request outcomes and control snapshot revisions
  before, during, and after elections

## Limitations

This is a single-host loopback proof with fixed three-node membership and
bounded deterministic timing ranges. It does not inject partitions, delayed or
reordered RPCs, disk/power failure, two-node loss, membership changes, snapshots,
large logs, multiple gateways, authentication, or linearizable follower reads.
