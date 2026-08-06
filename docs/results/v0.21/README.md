# v0.21 overlap-safe service credential rotation results

The retained exact-process proof starts a three-node Raft cluster and gateway
on credential A while every receiver trusts credentials A+B. It then rolls
control signers and the gateway to B, rolls explicit A revocation, attacks the
old credential boundary, and serves real inference through the surviving B
path.

Run:

```bash
./scripts/proof-v0.21.sh
```

Retained outcome:

- 18/18 machine-readable assertions passed;
- all six rolling restart checkpoints retained three statuses and exactly one
  leader;
- every control signer and the gateway moved from A to B without losing route
  revision 2;
- receivers recorded accepted A and B traffic during the overlap window;
- an old gateway A request worked before revocation;
- old gateway A and peer A requests received explicit 401 credential-revoked
  errors afterward;
- a rejected high-term peer request left term and revision unchanged;
- a current gateway B request continued to read revision 2;
- the real request completed in 182.663 ms; and
- the 182.597 ms SSE reached `[DONE]`.

The proof is loopback evidence. Trust and revocation still require rolling
restarts, verification is bounded-linear, diagnostic counters reset on restart,
and signed HTTP still provides neither encryption nor hostname authentication.

![Overlap-safe credential rotation evidence](raw/service-credential-rotation-proof.svg)

Key retained files:

- `raw/assertions.json` — all checked claims and observations;
- `raw/initial-cluster.json` — three key-A signers with A+B trust;
- `raw/control-key-b-step-*.json` — quorum checkpoints while rotating control
  signers;
- `raw/after-control-key-b.json` — final control signer state and mixed A/B
  verification counts;
- `raw/overlap-key-a-valid.json` — old A remains valid before revocation;
- `raw/gateway-key-a-ready.json` / `raw/gateway-key-b-ready.json` — gateway
  signer transition;
- `raw/revoke-key-a-step-*.json` — quorum checkpoints during revocation rollout;
- `raw/revoked-gateway-key-a.json` / `raw/revoked-peer-key-a.json` — precise old
  credential failures;
- `raw/valid-gateway-key-b.json` — current credential success;
- `raw/before-revoked-attacks.json` / `raw/after-revoked-attacks.json` — stable
  term and revision across the rejected high-term request;
- `raw/request.json` / `raw/stream.json` — real worker JSON/SSE evidence; and
- `raw/service-credential-rotation-proof.svg` — data-driven rollout chart.
