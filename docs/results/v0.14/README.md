# v0.14 proof: restart-safe gateway routing snapshots

This retained experiment proves that a control-configured gateway can persist
its last committed route map, restart from that map while all three Raft nodes
are unavailable, reconcile to a newer committed revision when control returns,
and reject a later stale-control rollback.

The disk file is a read-only restart cache of Raft-committed state. It is not a
new consensus authority.

## Result chart

![Gateway restart, reconciliation, and rollback-guard evidence](raw/gateway-restart-proof.svg)

## Retained result

| Observation | Result |
|---|---:|
| Machine-readable release assertions | 19 / 19 passed |
| Raft nodes / real CPU workers | 3 / 2 |
| Initial configuration | revision 2, term 1, round robin |
| Initial snapshot schema | `inferlab.gateway-routing-snapshot.v1` |
| Live boot source / observed latency | live control plane / 786.002 ms |
| Live-start requests | 2 / 2 success on revision 2 |
| First fault | exact gateway child + all 3 exact control children stopped |
| Offline restart source / observed latency | disk snapshot / 221.203 ms |
| Requests with every control node offline | 4 / 4 success on revision 2 |
| Recovered control plane | term 2, weighted revision 4 |
| Persisted/applied update | revision 4 before new requests |
| Eight-request 3:1 distribution | 6 to `cpu-restart-a`, 2 to `cpu-restart-b` |
| Stale live control / durable gateway | revision 2 / revision 4 |
| Stale-guard observed boot latency | 46.320 ms |
| Final speculative SSE | revision 4, seven pieces, `[DONE]`, two target calls |
| Corrupt disk + unavailable control | startup exits 1 |
| Temporary snapshot files after replacement | 0 |
| Non-stream requests | 14 / 14 success |

Boot timings are observations from one loopback run, not service-level
objectives. Offline startup intentionally includes a configured 150 ms window
for live control to respond.

## Recovery phases

| Phase | Control state | Disk state | Gateway state | Traffic result |
|---|---|---|---|---|
| Live bootstrap | revision 2 reachable | saves revision 2 | live bootstrap r2/t1 | 2/2 success |
| Full outage | all nodes stopped | loads revision 2 | disk bootstrap r2/t1 | 4/4 success |
| Reconciliation | term 2 commits revision 4 | replaces with revision 4 | applies r4/t2 | 8/8 success, 6:2 routing |
| Rollback guard | stale revision 2 reachable | retains revision 4 | ignores r2, serves r4 | speculative SSE reaches `[DONE]` |

## Reproduce

```bash
./scripts/proof-v0.14.sh
```

To replace this retained evidence:

```bash
INFERLAB_V14_OUTPUT_DIR=docs/results/v0.14/raw \
  ./scripts/proof-v0.14.sh
```

The script checks eight loopback ports, builds the workspace, starts three Raft
nodes and two real CPU workers, commits revision 2, starts and stops exact
gateway/control children, preserves old Raft directories as a deliberate stale
cluster, commits revision 4 after control recovery, verifies disk replacement,
tests divergent and corrupt bootstrap failure, evaluates 19 assertions, renders
the SVG from retained JSON, and cleans up its exact child processes.

## Raw artifacts

- [`gateway-restart-check.json`](raw/gateway-restart-check.json) — 19
  machine-readable assertions and release summary
- [`gateway-restart-proof.svg`](raw/gateway-restart-proof.svg) — chart generated
  from retained JSON
- [`snapshot-initial.json`](raw/snapshot-initial.json) and
  [`snapshot-updated.json`](raw/snapshot-updated.json) — exact versioned disk
  documents before and after reconciliation
- [`gateway-live.json`](raw/gateway-live.json),
  [`gateway-offline.json`](raw/gateway-offline.json),
  [`gateway-reconciled.json`](raw/gateway-reconciled.json), and
  [`gateway-stale-control.json`](raw/gateway-stale-control.json) — startup source,
  persisted revision, live freshness, routing state, and observed latency
- [`config-initial.json`](raw/config-initial.json) and
  [`config-updated.json`](raw/config-updated.json) — committed revision 2 and 4
- [`initial-election.json`](raw/initial-election.json),
  [`recovered-election.json`](raw/recovered-election.json), and
  [`stale-election.json`](raw/stale-election.json) — control leadership phases
- [`stale-control-config.json`](raw/stale-control-config.json) — reachable but
  older revision used to test the rollback guard
- [`requests-live.json`](raw/requests-live.json),
  [`requests-offline.json`](raw/requests-offline.json), and
  [`requests-weighted.json`](raw/requests-weighted.json) — real-model outcomes
  and response revision headers
- [`stream-final.json`](raw/stream-final.json) — final SSE pieces and speculative
  generation metrics
- [`gateway-first-stop.json`](raw/gateway-first-stop.json),
  [`gateway-second-stop.json`](raw/gateway-second-stop.json), and the three
  `control-*-outage.json` files — exact process fault scope
- [`divergent-bootstrap.json`](raw/divergent-bootstrap.json) and
  [`corrupt-bootstrap.json`](raw/corrupt-bootstrap.json) — fail-closed startup
  evidence for equal-revision disagreement and an unreadable snapshot
- [`snapshot-directory.json`](raw/snapshot-directory.json) — destination and
  temporary-file observation

## Limits

This is one macOS local-filesystem and loopback-process experiment. It does not
prove power-loss durability, network-filesystem rename semantics, disk-full or
permission handling, concurrent writers, authenticated cluster identity,
snapshot age/revocation, multi-host partitions, sustained load, public-model
quality, or CUDA execution. The format has structural and semantic validation
but no checksum, signature, encryption, or cluster ID.
