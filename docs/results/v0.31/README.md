# v0.31 deadline-safe automated trust-policy renewal evidence

This directory retains the manifest-last local proof for automatic renewal of
root-signed service-trust policy under a fixed semantic template and authority.
The separately supervised `trust-renewer` owns the signing seed, persists its
outbox state, publishes over static mTLS, and reconciles every publication with
an exact distributor `GET` before committing it locally.

The proof observes four consecutive automatic generations: cold start,
ordinary pre-deadline renewal, recovery from an ambiguous committed response,
and late recovery after a deliberately induced outage crosses the active
policy's exclusive expiry boundary. Each generation converges on all three
controls and carries three independently Ed25519-verified receiver receipts.

The canonical bundle is in [`raw`](raw). Its manifest authenticates every
non-manifest file by byte length and SHA-256. `assertions.json` is the
dependency-free offline result, and `trust-policy-renewal-proof.svg` is a
deterministic visualization derived from the same checked evidence.

![Deadline-safe automated trust-policy renewal proof](raw/trust-policy-renewal-proof.svg)

## Result

- **19/19 deterministic assertions passed.** Checker JSON and SVG replay
  byte-for-byte. The manifest was written last and records **22 total files / 21
  hashed non-manifest files**. Its SHA-256 is
  `fc404a84196f36b25dd6635bd41ad960416732ed1842046bbc07e6a141c86c27`.
- Generations `1 → 2 → 3 → 4` retain one exact policy meaning, fixed authority,
  20,000 ms lifetime, and 10,000 ms renewal margin. All four root signatures,
  exact snapshot bytes and ETags, three-control convergence observations, and
  **12 receiver receipts** verify offline.
- Normal generation `1 → 2` publishes and receives all acknowledgements before
  generation 1 expires. Expiration-rejection counters remain unchanged and no
  protected-request authorization gap is observed.
- For generation 3, the proof-only TLS fault gate forwards and commits the
  publication, then discards its response. Exact pending snapshot bytes survive
  the one expected renewer restart and reconcile against distributor bytes,
  without a fork, duplicate generation, or skipped generation.
- A renewer-only transport outage then crosses generation 3 expiry. A protected
  request succeeds immediately before the boundary; repeated signed and missing
  authorization attempts after it all return the same redacted `401`, proving
  no hidden grace. Generation 4 is staged after expiry, retained while
  publication is unavailable, and released within its own validity window. The
  late-recovery counter moves from **0 to 1**.
- The retained suite includes the exact `expired_pending_fails_closed_without_post`
  regression. Recovery is therefore bounded by the pending candidate's own
  validity; the evidence does not claim unbounded automatic recovery.
- Eight corrupt, oversized, linked, unsafe-permission, and concurrently locked
  startup sources fail nonzero before their status listeners appear. Eighteen
  exact production regressions run once each across semantic equality, signing,
  timing, clock movement, outbox persistence, ambiguous reconciliation,
  fail-closed state validation, post-rename durability uncertainty, and
  supervision.
- Each of the initial, post-restart, and final process captures contains seven
  runtime processes plus the explicit proof-only fault gate. Six runtime
  services and the gate retain exact PID, proof-shell parent, start token,
  executable hash, liveness, and non-zombie identity. Only `trust-renewer` is
  replaced, exactly once.
- Real CPU JSON completes with `200` in **827.528 ms**. Incremental SSE completes
  with `200`, `[DONE]`, and EOF in **828.044 ms**, emitting seven content pieces
  across ten events. These loopback observations are not latency SLOs.
- Only the renewer receives the private root environment variable. Discarded-log,
  retained-surface, and deterministic private-material scans pass without a
  retained root, route, writer, or service seed representation, private PEM
  marker, fixed private prompt, host path, or sensitive JSON field.

## Evidence map

| Question | Retained evidence |
|---|---|
| Did every offline predicate pass? | `raw/assertions.json` |
| What timing, topology, process, and static-TLS boundary was exercised? | `raw/proof-contract.json` |
| Which public authority and exact semantic template were pinned? | `raw/authority.json` |
| Did unsafe startup sources fail before listening? | `raw/renewer-startup-rejections.json` |
| Do all four policies, controls, renewer states, and receipts agree? | `raw/automatic-generations.json` |
| Did ordinary renewal finish before expiry without an authorization gap? | `raw/normal-renewal.json` |
| Did the lost response reconcile the same pending bytes across restart? | `raw/ambiguous-retry.json`, `raw/state-projections.json` |
| Did expiry reject without grace and a fresh valid generation recover service? | `raw/expiry-outage-recovery.json`, `raw/protected-request-continuity.json` |
| Is the application fault gate explicitly outside runtime authority and HA? | `raw/fault-gate.json` |
| Which processes stayed exact, and which one was deliberately replaced? | `raw/process-continuity.json` |
| Was the root seed confined to the renewer? | `raw/secret-boundaries.json` |
| Did the final Raft cluster converge on generation 4? | `raw/final-cluster.json` |
| Did real CPU JSON and incremental SSE complete after recovery? | `raw/final-json.json`, `raw/final-sse.json` |
| Did every named production regression run exactly once? | `raw/production-tests.json` |
| Were discarded and retained surfaces scanned? | `raw/discarded-log-scan.json`, `raw/sanitizer.json`, `raw/private-material-scan.json` |
| Is the exact final inventory byte- and SHA-bound? | `raw/manifest.json` |

## Claim boundary

The proof covers one loopback schedule, one active renewer, one distributor,
three controls, one gateway, and one CPU worker. The TLS identities and their
verification CAs are static; this is policy renewal, not certificate issuance or
rotation. The proof-only fault gate is neither runtime authority nor an HA
component.

The semantic policy template and signing authority remain fixed. This does not
establish semantic policy rollout, root rotation, secure-time provisioning,
multi-host behavior, fleet-atomic activation, cancellation of already-started
work, or renewer/distributor HA. The expiry boundary is exclusive for newly
authorized protected work; already-started work is outside this claim.

Generation 4 demonstrates recovery from an outage only while its newly staged
candidate remains valid. A candidate that expires before publication fails
closed, as bound by the retained exact regression.

## Reproduce the live proof

Prerequisites are the normal InferLab build toolchain, Python 3 with TLS 1.3
support, `curl`, and OpenSSL. The live topology uses loopback ports `12580`–
`12587`; startup-failure probes use `12600`–`12607` and `12620`. Every OpenSSL
CA serial is an explicit proof-owned file below the guarded temporary PKI
directory.

Run without retention:

```bash
./scripts/proof-v0.31.sh
```

To publish a fresh manifest-last bundle, point the script at an empty directory:

```bash
output="$(mktemp -d)"
INFERLAB_V31_OUTPUT_DIR="$output" ./scripts/proof-v0.31.sh
```

The script refuses a nonempty destination and copies evidence only after its
discarded-log scan, sanitizer, private-material scan, checker, renderer,
byte-replay, and manifest-last gates pass.

## Reproduce the retained derivations

From the repository root:

```bash
python3 benchmarks/check_trust_policy_renewal.py \
  --evidence-dir docs/results/v0.31/raw \
  --require-manifest \
  --output /tmp/inferlab-v031-assertions.json
cmp docs/results/v0.31/raw/assertions.json \
  /tmp/inferlab-v031-assertions.json

python3 benchmarks/render_trust_policy_renewal_svg.py \
  --evidence-dir docs/results/v0.31/raw \
  --output /tmp/inferlab-v031-proof.svg
cmp docs/results/v0.31/raw/trust-policy-renewal-proof.svg \
  /tmp/inferlab-v031-proof.svg
```

## Read next

- [RFC 0036](../../rfcs/0036-deadline-safe-automated-signed-service-trust-renewal.md)
  defines the normative template, timing, persistent-outbox, reconciliation,
  expiry, and supervision contract.
- [Phase 36](../../learning/phase-36-deadline-safe-automated-signed-service-trust-renewal.md)
  explains the lifecycle in learning order.
- [Interview guide](../../interview-demo.md) turns the retained proof into a
  recording sequence without broadening its claim boundary.
