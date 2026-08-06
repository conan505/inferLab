# v0.20 cryptographic service-identity results

The retained exact-process proof enables required Ed25519 service
authentication on a three-node Raft cluster and on the gateway's route reads.
It layers the v0.19 writer authorization and v0.18 route signature around the
new request identity boundary.

Run:

```bash
./scripts/proof-v0.20.sh
```

Retained outcome:

- 20/20 machine-readable assertions passed;
- followers accepted signed vote/append traffic and all replicas retained r2;
- missing, unknown, stale, replayed, and tampered requests received 401;
- peer-as-gateway and gateway-as-peer requests received 403;
- high-term rejected requests left leader term 1 and route revision 2;
- the gateway exposed `gateway-primary` and exact mappings to node A/B/C;
- a separately route-signed configuration reached the real gateway;
- the real request completed in 185.707 ms; and
- the 186.723 ms SSE reached `[DONE]`.

The proof is loopback evidence. Request signatures do not encrypt HTTP, prove a
hostname, persist replay history across restart, rotate keys automatically, or
protect a compromised process.

![Cryptographic service-identity evidence](raw/service-auth-proof.svg)

Key retained files:

- `raw/assertions.json` — all checked claims and observations;
- `raw/election.json` / `raw/final-cluster.json` — authenticated consensus and
  final replicated state;
- `raw/missing-rejected.json`, `raw/unknown-rejected.json`,
  `raw/stale-rejected.json`, `raw/replay-rejected.json` — 401 classes;
- `raw/tampered-raft-rejected.json` — body-integrity failure;
- `raw/peer-read-forbidden.json`, `raw/gateway-peer-forbidden.json` — 403 role
  separation;
- `raw/gateway-read-valid.json`, `raw/gateway-ready.json` — request identity,
  route identity, and target mapping;
- `raw/request.json`, `raw/stream.json` — real worker JSON/SSE evidence; and
- `raw/service-auth-proof.svg` — data-driven summary diagram and chart.
