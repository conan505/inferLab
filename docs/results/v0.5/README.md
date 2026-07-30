# v0.5 proof: durable batch queue

This retained run proves recovery from an actual queue process restart, stale
claim fencing, idempotent external effects, bounded attempts, and dead-letter
handling from one inspectable WAL.

## Hypothesis

After the queue and first consumer disappear after an effect but before
acknowledgement:

- synced jobs and claims survive restart;
- the expired job is delivered again with the same identity and a new token;
- reapplying its external effect creates no duplicate row;
- the old token cannot complete the newer lease;
- untouched pending work is preserved; and
- a poison job stops after its configured attempt bound.

## Lifecycle

![Durable lifecycle reconstructed from all 13 WAL transitions](raw/batch-state.svg)

## Result

| Claim | Retained evidence |
|---|---:|
| Jobs | 3 |
| WAL transitions | 13 |
| WAL bytes | 2,152 |
| Claims | 5 |
| Redeliveries | 2 |
| Acknowledgements | 2 |
| Completed / dead letter | 2 / 1 |
| External effects after two deliveries | 1 |
| Torn tail records in this run | 0 |
| Machine-readable assertions | 15 of 15 passed |

The crash job is claimed as attempt 1, performs one SQLite effect, and receives
no acknowledgement. After a fresh queue process replays the WAL and the lease
expires, attempt 2 receives a different token. The second `INSERT OR IGNORE`
creates no row. The attempt-1 acknowledgement returns
`409 stale_claim`; attempt 2 completes.

The poison job explicitly fails twice with `max_attempts=2`. Its first failure
returns it to pending; its second moves it to the DLQ.

## Reproduce

```bash
./scripts/proof-v0.5.sh
```

To replace these artifacts:

```bash
INFERLAB_RESULTS_DIR=docs/results/v0.5/raw \
  ./scripts/proof-v0.5.sh
```

## Raw artifacts

- [`before-crash.json`](raw/before-crash.json) — enqueues, deduplication,
  conflict, attempt-1 claim, and first external effect
- [`after-restart.json`](raw/after-restart.json) — redelivery, duplicate-effect
  suppression, fencing, acknowledgements, failures, DLQ, and final status
- [`queue-events.wal.jsonl`](raw/queue-events.wal.jsonl) — every durable state
  transition in replay order
- [`batch-check.json`](raw/batch-check.json) — 15 machine-readable assertions
- [`batch-state.svg`](raw/batch-state.svg) — deterministic rendering of the WAL

## Limitations

This is a single-host, single-writer, sequential proof. It does not simulate
power loss, disk failure, replicated queue ownership, concurrent service
processes, lease renewal, WAL compaction, or production throughput. The SQLite
effect ledger demonstrates the required consumer pattern; it is not owned by
the queue service.
