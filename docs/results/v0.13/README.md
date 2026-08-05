# v0.13 proof: real-worker full-stack integration

This retained experiment proves that the Raft-configured gateway can route,
retry, reconfigure, and stream through real CPU inference workers while each
request remains fenced to one immutable control-plane revision and term.

It combines mechanisms proved separately in earlier milestones. It is a
correctness and fault-continuity experiment, not a throughput benchmark or a
CUDA claim.

## Result chart

![Full-stack revision, failure, continuity, and routing evidence](raw/full-stack-proof.svg)

## Retained result

| Observation | Result |
|---|---:|
| Machine-readable release assertions | 23 / 23 passed |
| Raft nodes / real CPU workers | 3 / 3 |
| Worker runtime | online-tiled FP32 attention, paged KV cache, INT8 draft |
| Initial configuration | revision 2, term 1, consistent hash, 3 workers |
| Repeated affinity requests | same worker, miss then prefix hit |
| Killed affinity owner | `cpu-real-b`, exact owned child PID |
| Failover | success on attempt 2, still revision 2 / term 1 |
| Live configuration | revision 3, term 1, only 2 surviving workers |
| Post-reconfiguration requests | 4 / 4 success, one attempt each |
| Killed control-plane leader | `node-a`, exact owned child PID |
| Requests during election | 6 / 6 success on revision 3 |
| Replacement leader | `node-b`, term 2, 374.214 ms |
| Final configuration | revision 5, term 2, weighted round robin 3:1 |
| Eight-request weighted distribution | 6 to `cpu-real-a`, 2 to `cpu-real-c` |
| Non-stream requests | 21 / 21 success |
| Final SSE | status 200, seven pieces, `[DONE]`, one attempt |
| Final speculative generation | 6 proposed / 6 accepted, 2 target calls |
| CUDA compiler / runtime | unavailable / unavailable |

Revision 4 is a Raft log position used while establishing the new leader's
current term; revisions identify committed log state and need not be consecutive
user-visible configuration edits.

The retained 374.214 ms re-election is one local observation. The checker
asserts a broader bound below 1,500 ms. The exact 6:2 distribution is expected
for this deterministic eight-request smooth-weighted-round-robin schedule; it is
not a statistical fairness or throughput claim.

## Request/revision phases

| Phase | Requests | Attempts | Revision / term | Important observation |
|---|---:|---:|---:|---|
| Affinity | 2 | 1 each | 2 / 1 | Same worker; second request hits paged prefix |
| Worker failover | 1 | 2 | 2 / 1 | Retry succeeds on survivor without adopting a newer snapshot |
| After membership update | 4 | 1 each | 3 / 1 | Failed worker is no longer selected |
| During leader election | 6 | 1 each | 3 / 1 | Data plane continues from installed committed state |
| Weighted routing | 8 | 1 each | 5 / 2 | 3:1 weights yield exact 6:2 schedule |
| Speculative SSE | 1 stream | 1 | 5 / 2 | Real token pieces reconstruct completion and end in `[DONE]` |

## Reproduce

Use Python with PyTorch 2.2.2 or compatible for environment capture:

```bash
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python \
  ./scripts/proof-v0.13.sh
```

To replace the retained evidence:

```bash
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python \
INFERLAB_V13_OUTPUT_DIR=docs/results/v0.13/raw \
  ./scripts/proof-v0.13.sh
```

The script verifies that seven loopback ports are free, regenerates and
byte-compares the deterministic v2 checkpoint, builds the workspace, starts
three Raft nodes and three real workers, runs each phase, kills only exact owned
child PIDs, evaluates 23 assertions, renders the SVG from retained JSON, and
cleans up its child processes.

## Raw artifacts

- [`full-stack-check.json`](raw/full-stack-check.json) — 23 machine-readable
  assertions and release summary
- [`full-stack-proof.svg`](raw/full-stack-proof.svg) — chart generated from the
  retained JSON
- [`initial-election.json`](raw/initial-election.json) and
  [`re-election.json`](raw/re-election.json) — leader observations and timing
- [`config-initial.json`](raw/config-initial.json),
  [`config-live.json`](raw/config-live.json), and
  [`config-weighted.json`](raw/config-weighted.json) — committed configurations
- [`gateway-initial.json`](raw/gateway-initial.json),
  [`gateway-live.json`](raw/gateway-live.json), and
  [`gateway-weighted.json`](raw/gateway-weighted.json) — applied atomic snapshots
- [`affinity.json`](raw/affinity.json) — real prefix miss/hit pair
- [`worker-fault.json`](raw/worker-fault.json) and
  [`control-fault.json`](raw/control-fault.json) — exact child-process fault scope
- [`failover.json`](raw/failover.json),
  [`post-reconfigure.json`](raw/post-reconfigure.json),
  [`election-continuity.json`](raw/election-continuity.json), and
  [`weighted.json`](raw/weighted.json) — request outcomes and response headers
- [`stream.json`](raw/stream.json) — SSE pieces, completion, prefix, attention,
  and speculative-decoding metrics
- [`worker-health.json`](raw/worker-health.json) — runtime configuration of all
  three real workers
- [`environment.json`](raw/environment.json) — host, compiler, model identity,
  and accelerator boundary

## Environment and limits

The retained run used macOS 26.5.2 on ARM64, an Apple M4 Pro, Apple clang 21,
Python 3.9.6, and PyTorch 2.2.2. The 13,969-byte checkpoint SHA-256 is
`36c76ff3b2dcdedd3589a0b03350f5b2851c7ff2979640311c0559d8da5f3f9a`.

Everything runs on loopback with one tiny one-layer teaching model. There is no
multi-host partition, gateway restart persistence, automatic worker-health
membership update, linearizable configuration read, Raft compaction, sustained
load, production tokenizer/model quality, NVIDIA GPU execution, HBM profiling,
or CUDA performance result.
