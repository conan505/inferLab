# v0.22 signed online service-trust results

The retained exact-process proof starts three persistent controls from a
root-signed generation-1 policy, publishes generations 2 and 3 without control
restarts, attacks rollback and signature integrity, and proves a durable
generation floor still rejects rollback after a follower restarts.

Run:

```bash
./scripts/proof-v0.22.sh
```

Retained outcome:

- 20/20 machine-readable assertions passed;
- all three unchanged controls loaded generation 2 in 5.001 ms observed proof
  time and generation 3 in 4.856 ms;
- key B worked during the A+B overlap;
- generation 3 revoked key A while key B continued to read route revision 2;
- a valid root-signed generation-2 rollback and a tampered higher generation
  both left generation 3 active on every control;
- a restarted follower refused generation 2 because its durable floor was 3;
- restoring generation 3 let the follower rejoin the three-node cluster;
- the real request completed in 189.236 ms; and
- the 187.796 ms SSE reached `[DONE]`.

The proof is single-host loopback evidence. Local-file distribution is external,
swaps are atomic only per process, last known good can delay a failed revocation,
the floor assumes filesystem integrity, roots/private seeds remain static, and
HTTP still provides neither encryption nor hostname authentication.

![Signed online service-trust evidence](raw/online-service-trust-proof.svg)

Key retained files:

- `raw/assertions.json` — all checked claims and observations;
- `raw/initial-cluster.json` — three controls bootstrapped from signed g1;
- `raw/generation-2-convergence.json` — per-node online A+B convergence;
- `raw/generation-2-key-b-valid.json` — new key B works during overlap;
- `raw/generation-3-convergence.json` — per-node online g3 convergence;
- `raw/generation-3-key-a-revoked.json` — old A rejection;
- `raw/generation-3-key-b-valid.json` — current B success;
- `raw/rollback-rejected.json` — valid older snapshot retained g3;
- `raw/tamper-rejected.json` — invalid signature retained g3;
- `raw/restart-floor-rejection.json` — durable floor blocked rollback startup;
- `raw/final-cluster.json`, `raw/final-trust.json`, and
  `raw/final-gateway.json` — recovered end state;
- `raw/request.json` and `raw/stream.json` — real worker JSON/SSE evidence; and
- `raw/online-service-trust-proof.svg` — data-driven evidence chart.
