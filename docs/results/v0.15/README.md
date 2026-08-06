# v0.15 bounded-age routing fallback evidence

This release adds an optional cold-start age limit and future-clock-skew guard
to the restart-safe route file introduced in v0.14.

The experiment uses three persistent Raft processes, two real online-attention
CPU workers, a restartable gateway, and exact child-process shutdown. It keeps
routing content constant while deliberately changing only `saved_at_ms`, which
isolates the temporal eligibility decision from schema and revision validity.

## Retained result

| Observation | Retained value |
|---|---:|
| Machine-readable assertions | 15 / 15 passed |
| Committed routing identity | revision 2, term 1 |
| Configured maximum disk age | 5,000 ms |
| Configured maximum future skew | 100 ms |
| Fresh disk age at bootstrap | 433 ms |
| Fresh disk boot latency | 230.748 ms |
| Fresh-disk requests with every control node down | 3 / 3 succeeded |
| Synthetic expired age | 6,000 ms; startup exit 1 |
| Synthetic future delta | 5,100 ms; startup exit 1 |
| Live-repair boot latency | 100.618 ms |
| Permitted non-stream traffic | 7 / 7 succeeded |
| Final speculative SSE | revision 2, `[DONE]`, two target calls |
| Temporary routing files after proof | 0 |

![Fresh, expired, future-dated, and live-repair outcomes](raw/snapshot-freshness-proof.svg)

The chart's green window is the exact cold-start disk eligibility interval:
from `now − 5,000 ms` through `now + 100 ms`, inclusive. It does not represent
worker health or a runtime shutdown deadline.

## Recovery phases

```mermaid
flowchart LR
    Live["live control<br/>persist r2"] --> Stop["gateway + all Raft<br/>children stop"]
    Stop --> Fresh["fresh disk<br/>serve 3/3"]
    Fresh --> Expired["age 6000 ms<br/>fail closed"]
    Expired --> Future["ahead 5100 ms<br/>fail closed"]
    Future --> Recover["Raft recovers<br/>live overwrites file"]
    Recover --> Serve["serve 2/2<br/>SSE DONE"]
```

## Reproduce

Prerequisites are the same as v0.14: Rust, a C++20 compiler, Python 3, `curl`,
and the checked-in tiny v2 model.

```bash
./scripts/proof-v0.15.sh
```

To replace the retained raw evidence intentionally:

```bash
INFERLAB_V15_OUTPUT_DIR=docs/results/v0.15/raw \
  ./scripts/proof-v0.15.sh
```

The script checks seven loopback ports, builds the workspace, starts three Raft
nodes and two real CPU workers, commits revision 2, persists its route under the
bounded-age policy, stops exact gateway/control children, serves from fresh disk, injects
expired and future timestamps, recovers live control, repairs the file, checks
15 assertions, renders the SVG from retained JSON, and cleans up owned child
processes.

## Raw artifacts

- [`snapshot-freshness-check.json`](raw/snapshot-freshness-check.json) — 15
  machine-readable assertions and release summary
- [`snapshot-freshness-proof.svg`](raw/snapshot-freshness-proof.svg) — chart
  generated from the retained JSON
- [`gateway-live.json`](raw/gateway-live.json),
  [`gateway-fresh-disk.json`](raw/gateway-fresh-disk.json), and
  [`gateway-live-repair.json`](raw/gateway-live-repair.json) — startup source,
  age policy, observed age, persistence window, and latency
- [`snapshot-initial.json`](raw/snapshot-initial.json) and
  [`snapshot-repaired.json`](raw/snapshot-repaired.json) — durable document
  before and after live repair
- [`expired-fixture.json`](raw/expired-fixture.json) and
  [`future-fixture.json`](raw/future-fixture.json) — controlled time mutations
- [`expired-bootstrap.json`](raw/expired-bootstrap.json) and
  [`future-bootstrap.json`](raw/future-bootstrap.json) — fail-closed process
  exits and exact reasons
- [`initial-election.json`](raw/initial-election.json) and
  [`recovered-election.json`](raw/recovered-election.json) — control leadership
  before and after the total outage
- [`requests-live.json`](raw/requests-live.json),
  [`requests-fresh-disk.json`](raw/requests-fresh-disk.json), and
  [`requests-live-repair.json`](raw/requests-live-repair.json) — real-model
  response outcomes and revision headers
- [`stream-final.json`](raw/stream-final.json) — final speculative SSE tokens,
  completion, and generation metrics
- gateway/control event JSON — exact owned-child fault scope
- [`snapshot-directory.json`](raw/snapshot-directory.json) — final atomic
  temporary-file observation

## Honest limits

This is one macOS loopback experiment. It mutates the document timestamp rather
than the host clock and does not test NTP jumps, suspend/resume, power loss,
network filesystems, multi-host partitions, an attacker with file/clock access,
or production-model load.

The maximum age gates only a new process's disk fallback. It does not revoke a
running gateway, refresh on every equal-revision poll, or prove that named
workers are healthy. Those limits define the runtime routing-lease boundary.
