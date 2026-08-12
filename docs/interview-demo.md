# Interview demo and hosted-showcase guide

This guide turns InferLab into a short, repeatable portfolio demonstration. It
does not replace deployment or security review. Record only from a tagged
release with green CI, and show the exact tag, commit SHA, topology, and limits.

The current engineering story is v0.30 restart-free same-CA mTLS leaf renewal.
The browser showcase and hosted-edge rehearsal remain the v0.28 public product
surface. Say that distinction out loud: one live prompt demonstrates the
product; the retained v0.30 exact-process bundle demonstrates the new TLS
identity-lifecycle boundary. The retained values below come from one canonical
loopback run; they are evidence, not latency promises for the recorded browser
showcase.

## Recommended recording: five minutes

Use two loopback environments:

1. strict hosted-edge Compose for the live product interaction; and
2. the disposable v0.30 exact-process topology for TLS-leaf evidence.

Do not expose either as an unsafe public URL. Follow the
[interview topology guide](../deploy/interview/README.md): install the hosted
environment template at a private mode-`0600` path outside the repository,
replace every placeholder, load it without echoing values, and run:

```bash
./deploy/interview/start.sh --hosted-edge
```

The Compose topology has three persistent controls, two real CPU workers, one
signed committed route, a gateway with separate public/private-operator
listeners, and pinned Prometheus. Only the loopback public showcase and
Prometheus UI are host-published; the operator listener stays inside the
private network. Stop explicitly with:

```bash
./deploy/interview/stop.sh --hosted-edge
```

Add `--purge-data` only for an intentional clean-volume reset. Prometheus
history is ephemeral.

| Time | Show | Say | Evidence visible |
|---|---|---|---|
| 0:00–0:25 | Release tag, commit SHA, and v0.30 diagram | “InferLab is one system from an HTTP request to generated CPU tokens. v0.30 replaces trust-channel TLS leaves in running processes while keeping the CAs fixed.” | Exact tag/SHA; distributor plus three control clients; same-CA boundary |
| 0:25–1:05 | Hosted-edge startup summary and browser showcase | Explain that v0.28 separated public/operator listener capabilities and v0.30 does not broaden public exposure or add public HTTPS. | Loopback URL; operator listener private; no credentials visible |
| 1:05–1:40 | Submit one prompt and watch real streaming | Point out the real CPU decoder, incremental SSE, request headers, `[DONE]`, and EOF. | One accepted attempt; real CPU worker; terminal completion |
| 1:40–2:20 | v0.30 server/client snapshot diagram | Explain one whole mode-`0600` identity bundle and pinned issuer CA. Connections accepted after the server swap capture B; a control swap creates an entirely new HTTP client/pool. | Pre-accepted/established/in-flight A may finish; a post-publication accept/client snapshot uses B |
| 2:20–3:05 | Sequential renewal chart | Walk through distributor A→B, then three control clients A→B, with exact higher generations and same CA. | Long-running process identity, quorum, trust/cache/floor, and route continuity; exact observed values remain proof-owned |
| 3:05–3:40 | Policy/receipt and publisher panel | Explain that application authority remains root/service signatures. Policy g1 is sent by a fresh publisher-A client; policy g2 by a separately constructed publisher-B client. | Three verified control receipts per policy; no persistent publisher process, watcher, continuity, or handoff claim |
| 3:40–4:20 | Failure/LKG panel and checker replay | Show malformed/unsafe/misbound/expired/wrong-EKU/wrong-host/wrong-CA/fork/rollback candidates retaining LKG. Replay only the retained checker against published bytes. | 23/23 assertions; 15 startup + 31 live rejections; 12 exact tests; 24 total / 23 hashed files |
| 4:20–5:00 | Limits and next boundary | State local custody, old references/connections, restart-reset floors, sequential rollout, and absent CA migration/revocation/ACME/HSM/HA/global-mTLS guarantees. | RFC 0035 limits; Phase 35 failure matrix |

The tagged [v0.30 evidence](results/v0.30/README.md) passes **23/23
deterministic assertions** in 24 total files / 23 manifest-hashed files. It
retains 15 pre-listener startup rejections, 19 live server and 12 live client
rejections, 12 exact production regressions, six unchanged long-running
processes, and three verified receipts at each of policy generations 1 and 2.
Real CPU JSON completes in 819.971 ms. SSE completes in 825.317 ms with ten
events, seven nonempty content pieces, and an 817.285 ms first-to-last
event-offset span, then `[DONE]` and EOF. Checker and SVG replay are
byte-identical. The 3,710-byte manifest
SHA-256 is
`697562f9f10016bae043fa763ff752e16b89013e998c89192e4521e2c1c52506`.
These are one loopback proof run's retained values, not promised timings for
the browser request recorded today.

![Retained restart-free TLS identity handoff proof](results/v0.30/raw/tls-identity-handoff-proof.svg)

For historical context, the tagged [v0.29 evidence](results/v0.29/README.md) passes 28/28 deterministic
assertions in 28 total files / 27 hashed non-manifest files. It retains nine
startup rejections, eleven live rejections with `rejected_reloads` moving
exactly `0 → 11`, four signing senders, three A and three B receipts, eleven
exact single-test regressions, and all six proof processes unchanged. After B
and route revision 3, retained real CPU JSON completes in 831.582 ms; retained
SSE completes in 833.124 ms with seven nonempty content pieces spanning
721.919 ms. The manifest SHA-256 is
`a21b3a8ddf5bd0f1f7e8a64fcfeb8485cd78c7d66d6247b6bbfa828bd94cc5a2`.
These are one loopback proof run's retained values, not promised timings for
the browser request recorded today.

![Retained restart-free signer handoff proof](results/v0.29/raw/signer-handoff-proof.svg)

The `$0` claim covers local execution, recording, and retained repository
evidence. This repository does not select or guarantee a free public host.
Free tiers may sleep, cap CPU/memory/bandwidth, change terms, or require billing
information. If managed HTTPS, private operator networking, secret storage,
provider abuse/cost controls, monitoring, and a quick disable path cannot all
be met at zero cost, publish the repository, evidence, and local recording—not
an unsafe endpoint.

Keep a second successful rehearsal as recovery insurance. The published live
product segment should be continuous; do not splice stored JSON into a claimed
live request. It is fine to show a retained proof chart and replay its checker
as retained evidence, provided you label it that way.

## The v0.30 diagram to explain

```mermaid
sequenceDiagram
    participant Old as "established A / in-flight A"
    participant Watcher as "whole identity-bundle watcher"
    participant Runtime as "server config / whole control client"
    participant New as "new accepted connection / operation"
    Old->>Runtime: "negotiate or capture g1 / leaf A"
    Watcher->>Watcher: "validate exact higher g2 / B under pinned CA"
    Watcher->>Runtime: "publish complete runtime B"
    Old-->>Old: "may finish using A"
    New->>Runtime: "begin after activation"
    Runtime-->>New: "server leaf B or fresh client-B pool"
```

There are four separate claims:

- **same-CA identity validation:** generation 1 pins the issuer CA; a candidate
  cannot redefine its own trust boundary;
- **server accept/handshake semantics:** B is captured by connections accepted
  after publication; pre-accepted handshake futures and established A
  connections may retain A;
- **client-pool semantics:** a control publishes a whole new HTTP client, so an
  operation starting after activation cannot enter the old pool; and
- **process continuity:** leaf activation does not replace the long-running
  distributor or control process.

Do not collapse them into “zero-downtime certificate rotation.” Each has
different evidence and limitations, and ordinary overlap explicitly permits
already-established/in-flight A.

## The sequential renewal to narrate

```mermaid
sequenceDiagram
    participant D as "trust distributor"
    participant PA as "fresh publisher-A client"
    participant C as "three running controls"
    participant PB as "fresh publisher-B client"
    PA->>D: "publish policy g1 over mTLS A"
    D-->>C: "fetch g1; control client leaf A"
    C->>D: "three g1 application receipts"
    D->>D: "server bundle 1/A → 2/B"
    Note over D,C: "pre-accepted/established A may finish; post-publication accept sees B"
    C->>C: "rotate each client bundle 1/A → 2/B"
    Note over C: "new operation snapshots whole client B + fresh pool"
    PB->>D: "new connection; publish policy g2 over mTLS B"
    D-->>C: "fetch g2 through client B"
    C->>D: "three g2 application receipts through client B"
```

The TLS leaves authorize the transport peer under the fixed CA; they do not
authorize policy contents. Root-signed policy and service-signed receipt bytes
remain independent application authority. Publisher A and publisher B are
separately constructed fresh clients. There is no persistent publisher
process, publisher watcher, process-continuity observation, or publisher
handoff.

## Honest claims

Claims supported by the implementation boundary:

- InferLab runs a Rust gateway, control plane, queue, and CPU inference worker
  with a C++20 runtime and attention kernel; the browser request does not call
  a hosted LLM API.
- The distributor and controls can opt into one complete, bounded TLS identity
  bundle loaded before service; exact mode `0600` and a regular non-symlink
  source are required on Unix, and watched/static identity mixing fails closed.
- Generation 1 pins the local issuer CA. A higher candidate must preserve that
  decoded CA and pass exact cluster/identity/purpose/server-name binding,
  certificate/private-key matching, current validity, EKU, server SAN,
  generation, and complete runtime construction before publication.
- Same-generation identity equality uses decoded semantics rather than JSON/PEM
  formatting. An equivalent encoding of the already leaf-matched private key
  is harmless. Lower generation is rollback; a changed certificate, purpose,
  name, or CA at the same generation is a fork; invalid live observations
  retain LKG.
- Distributor activation affects connections accepted after publication.
  Pre-accepted handshake futures and established connections may keep A and
  are not falsely reported as B or forcibly terminated by ordinary renewal.
- Each control operation captures one complete HTTP client. Activation builds
  and swaps a new client with a fresh connection pool; a post-activation
  operation cannot enter the old pool, while an already-started operation may
  finish using its captured A client.
- The configured remote-verification CAs do not change. X.509 peer admission
  remains separate from root-signed trust policy and service-signed receipt
  verification.
- Publisher A/B are two fresh connections, not a persistent publisher process.
  Do not claim publisher watching, process continuity, or a publisher handoff.
- Watcher rejection status is bounded and truthful. Transient source failures
  and unchanged not-yet-valid candidates are retried; identical time-dependent
  and deterministic observations do not repeat the counter/report. Unexpected
  watcher exit/panic/cancellation is supervised rather than silently freezing
  renewal. Status may expose the active leaf's SHA-256 DER fingerprint for A/B
  observation, but not its subject, serial, PEM, CA, key, or path.
- The retained v0.30 bundle passes 23/23 assertions over 24 total / 23 hashed
  files, covers 15 startup and 31 live rejection cases plus 12 exact tests,
  keeps six long-running process identities unchanged, and authenticates the
  bundle with manifest SHA-256
  `697562f9f10016bae043fa763ff752e16b89013e998c89192e4521e2c1c52506`.

Historical v0.29 claims remain supported separately:

- Gateway and control watched modes load a whole bounded signer bundle before
  listening, require mode `0600` on Unix, and reject ambiguous watched/static
  identity configuration.
- One stable `ServiceSigner` owns one process nonce sequence. Every outbound
  operation snapshots one credential; exact higher activation replaces the
  whole signer state atomically for future snapshots. Its sequence suffix is
  unique and increasing, while intervening validation and a regressing clock
  mean neither adjacency nor a monotonic complete nonce is claimed.
- Same-generation equality compares decoded signer semantics, not file bytes.
  JSON formatting and credential ordering alone may be unchanged; different
  decoded semantics are a fork. Lower generation is rollback, and invalid live
  input retains last known good.
- In required service-auth mode—including the proof topology—control activation
  checks the candidate's exact policy key and uses signer-before-authorizer lock
  order. Explicitly disabled compatibility mode has no policy gate. A silent
  watcher exit/panic/cancellation is supervised as process failure.
- Gateway trust readiness is deliberately an operator precondition, not a
  fleet-atomic protocol claim.
- Service-ID expected-receiver mode does not weaken receipt signatures: receipt
  v1 remains bound to the actual service and credential. Signer activation by
  itself creates no receipt.
- The retained v0.29 bundle passes 28/28 assertions, records all nine startup
  and eleven live rejection cases, runs eleven exact single-test regressions,
  and preserves all six process identities while four senders move A→B.
- Receipt evidence contains exactly three A receipts before g2 and three B
  receipts after g2. Its final real CPU JSON is 831.582 ms; SSE is 833.124 ms
  with seven pieces spanning 721.919 ms. Those are retained loopback timings,
  not an SLO.
- The v0.28 retained edge proof remains valid historical evidence: public
  `/internal/*` is absent in hosted mode, public/operator credentials are
  distinct, work is bounded before attempts, and its published 29/29,
  27-file/26-hash results belong specifically to v0.28.
- The v0.27 and v0.26 retained claims remain separate evidence for signed
  trust expiry and bounded observability; v0.29 does not broaden them.

Use measured v0.29 claims only from the exact recording tag after the retained
checker and SVG renderer replay byte-for-byte. The canonical bundle contains
28 total files / 27 hashed non-manifest files and its manifest SHA-256 is
`a21b3a8ddf5bd0f1f7e8a64fcfeb8485cd78c7d66d6247b6bbfa828bd94cc5a2`.

## Always qualify these statements

- The 3,232-parameter model is a deterministic educational fixture, not a
  useful general-purpose chatbot or evidence of model quality.
- Retained latency values describe one recorded machine and proof workload,
  not a capacity result, SLO, or cloud-performance guarantee.
- TLS identity bundles use local filesystem custody; neither their private keys
  nor signer seeds receive KMS/HSM isolation from v0.30.
- Old server configurations, established A connections, and outstanding
  client-A clones can retain old key material after B activates. Ordinary
  renewal neither forcibly closes A nor proves immediate erase/zeroization.
- TLS identity generation and issuer-CA pins are process-memory floors. Restart
  with an older otherwise valid bundle is not durably fenced by v0.30.
- The distributor and controls activate independently. “Implemented
  restart-free same-CA leaf renewal” is not a fleet-atomic or emergency-
  revocation claim.
- Publisher A/B are fresh connections only. Do not include the publisher in a
  process-continuity count or call the two clients a publisher handoff.
- Private signer bundles use local filesystem custody. The application does not
  encrypt the seeds at rest or provide KMS/HSM isolation.
- A+B private keys remain in process memory while the accepted bundle contains
  them. Selecting B does not erase A. If a later bundle omits A, outstanding
  `Arc` snapshots can retain it until they drop; immediate erasure and memory
  zeroization are not claimed.
- Bundle-generation rollback is prevented only while the process retains its
  in-memory state. Restarting with an older otherwise valid file is not durably
  fenced by v0.29.
- Restart resets the nonce counter. Request timestamps, freshness/future-skew
  limits, and receiver replay caches still bound acceptance, but the nonce
  sequence is not durable.
- Atomic activation is inside one sender. Four processes switch sequentially;
  there is no fleet-atomic cutover.
- A gateway bundle may activate locally even if an operator failed to make its
  B key ready everywhere. Remote authenticated success is the proof of
  readiness.
- Receipt convergence is eventual. A missing receipt does not distinguish a
  partition, rejected policy, crashed receiver, or failed upload.
- v0.24 encrypts/authenticates only the trust-distribution channel. v0.30 adds
  same-CA leaf replacement on that same channel, not global service mTLS,
  certificate revocation, CA migration, or automated issuance/scheduling.
- v0.28 hosted mode is application-edge isolation, not HTTPS, reverse proxy,
  WAF/DDoS protection, identity provider, billing, or public hosting.
- Public buckets are in-memory per credential/process and do not bound sockets,
  bandwidth, botnets, authenticated slow uploads, or aggregate pre-gate memory.
- Request IDs are bounded correlation values, not authentication or global
  uniqueness. Metrics listeners remain private HTTP and create no security
  boundary.

Avoid “production-ready,” “zero downtime,” “secure rotation,” “exactly once,”
and “internet scale.” Prefer the precise claim: “restart-free, per-process,
same-CA mTLS leaf renewal with post-publication-accept/fresh-client-pool
activation, truthful overlap, and LKG.”

## Reset and rehearsal checklist

Before each rehearsal:

- Check out the exact recording tag; require an empty `git status --short` and
  green CI for that commit.
- Download or generate the retained v0.30 proof from that same tag. Do not mix
  a chart from one commit with a browser demo from another.
- Verify Rust, C++20, Python 3, `curl`, and the proof script's documented local
  dependencies are available.
- Start from fresh disposable proof state; do not reuse Raft data, trust floors,
  bundles, or ports from an earlier take.
- Install `deploy/interview/hosted-edge.env.example` at a private mode-`0600`
  path outside the repository, replace every known fixture/placeholder, load it
  without shell tracing, and run `./deploy/interview/start.sh --hosted-edge`.
- Open the showcase in a fresh browser profile, enter the disposable public key,
  and confirm the page code does not persist it.
- Send one prompt and require incremental SSE, exact `[DONE]`, then EOF. Keep
  all credentials, seeds, bundle paths, raw environment, and private operator
  status off screen.
- Before recording, run the exact v0.30 proof once. During the short take,
  replay the retained checker/chart rather than implying a cold build and full
  process schedule completed off camera. Use only filenames and commands that
  exist in the tagged release.
- Replay `benchmarks/check_tls_identity_handoff.py` and
  `benchmarks/render_tls_identity_handoff_svg.py` with the exact commands in the
  [retained result](results/v0.30/README.md), require byte-equal outputs, and
  confirm retained manifest SHA-256
  `697562f9f10016bae043fa763ff752e16b89013e998c89192e4521e2c1c52506`.
- Inspect the retained process-identity evidence for exactly the long-running
  services named by the v0.30 proof. Do not count publisher A/B: those are
  fresh clients, not one persistent publisher process.
- Confirm an A-established distributor connection remains A across server
  activation and a separately opened connection accepted afterward sees B. Confirm each
  post-activation control fetch/receipt records client bundle B and a fresh
  client pool rather than merely reread certificate bytes.
- Confirm the receipt evidence preserves root-signed policy and service-signed
  application authority independently of TLS leaf admission.
- Confirm malformed, unsafe, misbound, expired/not-yet-valid, wrong-EKU,
  wrong-host, wrong-CA, fork, and rollback candidates leave LKG unchanged.
- Confirm proof output and retained bytes contain no known private seed, bundle
  path, absolute host path, bearer credential, or raw secret-bearing error.
- Open Prometheus only for the separate Compose observability segment. Prepare
  bounded aggregate queries, not per-request or secret-bearing labels.
- Disable notifications, enlarge terminal text, fix window placement, and
  rehearse the explanation against a timer.

After a failed take, stop hosted rehearsal with
`./deploy/interview/stop.sh --hosted-edge`, stop only the disposable proof
processes, and remove only their dedicated temporary state. Never manually edit
persistent demo volumes, signer bundles, or TLS identity bundles during a
recording unless the exact proof owns that action.

## Hosted-readiness checklist

Do not publish an internet URL until all applicable items are true:

- A tagged reproducible artifact is deployed, with tag and commit visible in
  private operator diagnostics.
- Public traffic uses provider-managed HTTPS and network controls; plain HTTP,
  controls, workers, storage, metrics, and operator routes remain private.
- Public inference has revocable credentials plus request, concurrency, body,
  output, and rate bounds. Provider-level abuse and cost limits also exist.
- Secrets come from a platform secret store, not the image/repository/logs or a
  world-readable mounted file. Rotation and emergency disable are rehearsed.
- Persistent Raft, route, queue, trust cache/floor, signer, and TLS identity
  paths have deliberate ownership, backup, restore, and reset procedures.
- Health/readiness, restart policy, CPU/memory limits, bounded queues, logging,
  availability/error/latency/saturation alerts, and cost alerts are configured.
- Proxy headers, CORS, upload limits, timeouts, and public/private route maps are
  explicit and tested from a signed-out browser and a separate network.
- The hosted topology and any reduction from the three-control proof are stated
  plainly. A low-cost single-node deployment is called single-node.
- One-command rollback or disable exists and has been tested.

The repository's hosted-edge Compose profile intentionally does not claim to
satisfy this checklist. It is a loopback rehearsal topology.

## Recording evidence bundle

Archive next to the final video:

- exact release tag, commit SHA, CI URL, and downloaded proof artifact;
- retained v0.30 manifest and checker result;
- sanitized process-continuity and A→B rollout summary;
- sanitized request/response headers and exact non-secret inference body;
- hosted topology summary;
- recording date and broad machine configuration; and
- the limitations stated verbatim in the video.

This lets an interviewer distinguish a repeatable engineering demonstration
from a one-off screen recording.
