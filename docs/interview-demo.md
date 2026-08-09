# Interview demo and hosted-showcase guide

This guide turns InferLab into a short, repeatable portfolio demonstration. It
does not replace deployment or security review. Record only from a tagged
release with green CI, and state the exact topology and release tag on screen.

## Recommended recording: about four minutes

Use two loopback environments. Rehearse the product in strict hosted-edge
Compose mode, then show the retained v0.28 exact-process proof from a separate
disposable gateway/CPU-worker topology. This avoids presenting an unsafe public
URL as “hosted” and keeps the v0.26 Compose Prometheus view available as a
separate observability demonstration.

Follow the [interview topology guide](../deploy/interview/README.md): copy the
hosted environment template outside the repository with mode `0600`, replace
every placeholder, load it without echoing values, and run
`./deploy/interview/start.sh --hosted-edge`. The full topology contains three
persistent controls, two real CPU workers, one signed committed route, one
gateway with separate public/private-operator listeners, and pinned Prometheus
v3.13.1. Only the loopback public showcase and Prometheus UI are host-published;
the operator listener remains inside the private Compose network. Stop with
`./deploy/interview/stop.sh --hosted-edge`; add `--purge-data` only for an
intentional clean-volume reset. Prometheus history is always ephemeral.

| Time | Show | Say | Evidence to keep visible |
| --- | --- | --- | --- |
| 0:00–0:25 | The current architecture diagram and release tag | “InferLab is a from-first-principles learning system for the path from an HTTP request to generated CPU tokens. This recording uses release `<TAG>`.” | Tag and commit SHA |
| 0:25–0:55 | Hosted-edge startup summary, showcase, and route diagram | Explain that the public and operator listeners are separate capabilities in one gateway process. Public `/internal/*` is absent; the private operator listener has a distinct credential. | Loopback public URL, private-operator statement, release `0.28.0` |
| 0:55–1:30 | Submit one real prompt and watch the completion stream | Point out token-by-token SSE, attempts/worker headers, and terminal `[DONE]`. Explain that the tiny checkpoint is deterministic and educational. | One accepted attempt, real CPU worker, `[DONE]` observed |
| 1:30–2:05 | Safe bounded failures on the disposable local edge | Show missing authentication, the 65,536/65,537-byte boundary, and the configured burst followed by `429`/`Retry-After`. Never print credential values. | Exact 401/413/429 envelopes and `x-inferlab-attempts: 0` |
| 2:05–2:50 | Retained v0.28 chart plus offline checker replay | Explain auth→body→input→bucket→admission ordering, credential isolation, and why the manifest is written last. Run the checker against retained bytes rather than pretending a cold build completed off camera. | **29/29**, exact 27 files/26 hashes, five exact tests, byte-identical replay |
| 2:50–3:25 | SSE permit/disconnect and metric reconciliation | Show one normal SSE drained through `[DONE]`+EOF and the deliberate disconnect returning local ownership to idle. Connect 18 finite rejections to the unlabeled scalar and 9 gateway attempts to 9 worker accepts. | 8 success/1 cancellation; zero error/deadline outcomes |
| 3:25–4:05 | Limits, `$0` claim, and next boundary | State that the proof/rehearsal are local and free, not internet hosting. Name HTTPS/network/DDoS/WAF/secret/cost controls, distributed buckets, slow-upload aggregation, and worker-owned schema as limits. | RFC 0033 boundary and Phase 33 failure matrix/glossary |

The `$0` claim covers local execution, recording, and retained evidence. This
repository does not select or guarantee a free public host. Free tiers may
sleep, cap CPU/memory/bandwidth, change terms, or require billing information.
If managed HTTPS, private operator networking, secret storage, provider abuse/
cost controls, and a quick disable path cannot all be met at zero cost, publish
the repository, evidence, and local recording—not an unsafe public endpoint.

Keep a second, pre-recorded successful take only as recovery insurance. The
published video should still be a continuous live run; do not splice stored
JSON into a claimed live request.

## Honest claims

Claims that the implementation and retained evidence support:

- InferLab runs a real Rust gateway, control plane, queue, and CPU inference
  worker, with a C++20 runtime and attention kernel.
- The CPU worker emits generated tokens from the checked-in teaching
  checkpoint; it is not a wrapper around a hosted LLM API.
- The exact v0.26 proof starts nine service OS processes across all six service
  classes and scrapes nine metrics targets four times. Every process retains
  its PID/start/command identity, exact route/method and `UNIT` metadata pass,
  36/36 assertions pass, and the 62-file evidence bundle is published
  manifest-last.
- The complete design-time budget is at most 256 series per target and 1,721
  for the proof topology; the observed maximum is 159 per target and 1,047
  total. Twenty-four unique prompts create no new series.
- The same bounded request ID crosses client, gateway, every retry, and CPU
  worker logs while request IDs, prompts, worker identity, job IDs, and other
  canaries are absent from metric text.
- v0.25 remains separate evidence for a three-process directed Raft cut and a
  deterministic five-server Figure-8 replay through production predicates;
  v0.26 does not replace or broaden that network-safety claim.
- The repository contains reproducible proof scripts and retained milestone
  artifacts, not only screenshots.
- The exact v0.28 proof runs one real gateway and one real CPU worker with
  separate public/operator/metrics listeners. Public `/internal/*` is absent
  under three credential conditions; the operator route accepts only its own
  key; authentication/body/input/rate/admission reasons reject with zero
  attempts; and 29/29 assertions pass in an exact 27-file/26-hash manifest-
  last bundle.
- The retained two-key schedule proves a two-request burst, independent second
  bucket, 1,317.514 ms observed refill, and charged admission-full rejection.
  Real CPU JSON takes 824.449 ms; seven SSE content pieces span 616.046 ms and
  complete in 825.350 ms through `[DONE]` plus EOF. One deliberate disconnect
  returns local gateway/worker ownership to idle.
- Final finite accounting is 18 detailed rejections = 18 unlabeled scalar,
  9 gateway attempts = 9 worker accepts, and 8 successful completion bodies +
  1 intentional cancellation. Five exact production tests execute once each.
- The prior exact v0.27 proof runs three control receivers behind TLS 1.3 mTLS
  distributor plus a real gateway/CPU worker, proves the exclusive service-
  trust cutoff and higher-generation recovery, executes seven exact production
  regressions non-vacuously, and passes 40/40 assertions in an exact 38-file
  manifest-last bundle.
- A real SSE admitted 1,498 ms before the signed deadline finishes 2,538 ms
  after it through `[DONE]`; new signed and missing-authentication requests
  starting after the deadline receive the same redacted expired-policy 401.

Always qualify these statements:

- The current 3,232-parameter model is an educational deterministic fixture,
  not a useful general-purpose chatbot and not evidence of production model
  quality.
- Retained latency numbers describe the recorded machine and proof workload;
  they are not general throughput or cloud-performance guarantees.
- Signed service requests provide application-level identity and integrity.
  v0.24 additionally encrypts/authenticates only the control/distributor trust
  channel; it does not make every InferLab hop TLS-protected.
- v0.24 keeps root policy authority separate from mTLS transport. Activation
  is atomic inside one receiver, not fleet-atomic across all controls; a
  missing receipt does not by itself identify a partition, rejection, process
  failure, or receipt-upload failure.
- Earlier routing experiments use deterministic fake workers. Clearly label
  whether a recorded request uses a fake worker or the real CPU decoder.
- v0.25 is one controlled single-host A-vs-{B,C} cut of whole Raft HTTP RPCs.
  It does not establish arbitrary partition safety, packet-level fault
  behavior, Jepsen history checking, or formal verification.
- v0.26 is one controlled single-host request/failure schedule with four
  scrapes per target. Its 156.298 ms JSON and 175.969 ms SSE are observations,
  not a load test, capacity result, or latency SLO.
- v0.27 expiry governs new service-authenticated control requests. It is not a
  kill switch for admitted inference, a gateway routing-lease revocation, or a
  guarantee that public inference stops at the same instant.
- The v0.27 maximum-observed clock is process-local, not persisted secure time.
  Receiver validity can cross the deadline at boundedly different instants;
  distributor status reports signed schema/expiry, not fleet validity.
- v0.28 is an application-edge boundary, not HTTPS, a reverse proxy, WAF,
  network-level DDoS protection, a user identity provider, or billing. Its
  fixture limits and timings are enforcement observations, not capacity
  recommendations, load-test results, or SLOs.
- Public buckets are in-memory per credential and gateway process. They reset
  on restart, do not coordinate across replicas, and do not bound authenticated
  slow uploads, aggregate concurrent pre-gate buffering/parsing, sockets,
  bandwidth, TLS handshakes, stolen keys, or botnets.
- The edge validates only JSON syntax, messages, prompt bytes, and `max_tokens`.
  Worker-owned sampling/response-format fields may still start an attempt. A
  local disconnect proves guard/permit release, not cancellation of arbitrary
  already-started remote effects.
- Request IDs are bounded correlation values, not authentication or global
  uniqueness. Metrics listeners use private HTTP and create no security
  boundary.
- Hosted mode separates public and operator bearer credentials and enforces a
  local per-credential request budget. Public-gateway HTTPS, provider abuse/
  cost controls, global service mTLS, certificate lifecycle, production
  checkpoint/tokenizer integration, CUDA attention, persistent/HA Prometheus,
  dashboards, alerts/SLOs, traces, and remote write remain future hosting or
  engineering work unless a later tagged release explicitly adds them.

Avoid “production-ready,” “zero downtime,” “secure,” “exactly once,” and
“internet scale” unless a later release supplies and documents the missing
evidence.

## Reset and rehearsal checklist

Before each rehearsal:

- Check out the exact recording tag and confirm `git status --short` is empty.
- Confirm CI passed for that commit and download its v0.28 proof artifact.
- Verify Rust, C++20, Python 3, `curl`, Perl `Time::HiRes`, and OpenSSL are
  available.
- Confirm live proof ports `11080`–`11084` and startup-failure ports
  `11180`–`11183` are unused.
- Start from fresh disposable local state; never reuse trust floors or Raft
  data from an earlier take.
- Copy `deploy/interview/hosted-edge.env.example` to a private mode-`0600` path,
  replace every placeholder, load it without shell tracing, and start with
  `./deploy/interview/start.sh --hosted-edge`.
- Exercise the loopback hosted-edge health, readiness, and completion requests
  once with the same disposable public credential and payload used for
  recording.
- Open the showcase from a fresh browser profile, enter the disposable demo
  key, and confirm that the key is not persisted after a reload.
- Confirm the streaming response ends in `[DONE]` and exposes the expected
  non-secret diagnostic headers.
- Run `./scripts/proof-v0.28.sh` once before recording and confirm all 29
  assertions pass. During the four-minute take, replay the retained checker and
  SVG instead of presenting precompiled output as a fresh build.

  ```bash
  python3 benchmarks/check_public_edge.py \
    --evidence-dir docs/results/v0.28/raw --require-manifest
  python3 benchmarks/render_public_edge_svg.py \
    --evidence-dir docs/results/v0.28/raw \
    --output /tmp/inferlab-v028-replay.svg
  ```

- Confirm public `/internal/workers` is `404` with missing/public/operator
  credentials and that operator status is reachable only through the private
  operator path. Keep all credential values off screen and out of history.
- Open the Compose Prometheus targets page and confirm all six configured
  gateway/control/worker targets are `UP`.
- Prepare these three bounded PromQL examples:

  ```promql
  sum by (service) (rate(inferlab_http_requests_total[1m]))
  sum by (service, status_class) (rate(inferlab_http_requests_total[1m]))
  histogram_quantile(0.95, sum by (le, service) (rate(inferlab_http_handler_duration_seconds_bucket[5m])))
  ```

  Do not query or display secrets, full logs, prompts, or per-request labels.
- Prepare a sanitized status view; make sure terminals, shell history, logs,
  environment variables, browser tabs, and notifications reveal no secrets or
  personal data.
- Increase terminal font size, disable notifications, fix the window layout,
  and keep commands in a paste-safe notes file.
- Rehearse the narration against a timer. Prefer one concrete design decision
  over listing every project feature.

After a failed take, stop hosted rehearsal with
`./deploy/interview/stop.sh --hosted-edge`, stop all disposable proof
processes, remove only the dedicated demo data directory, verify the proof
ports are free, and restart from the same tag. Reset hosted state through the
deployment’s documented, reversible reset procedure; do not manually edit
production volumes during a recording.

## Hosted-readiness checklist

Do not publish the URL until all applicable items are true:

- A tagged, reproducible image or artifact is deployed, with the tag and commit
  SHA exposed in operator diagnostics.
- CI requires formatting, Clippy, workspace tests, Python compilation, shell
  parsing, and the current exact-process proof before release.
- The public hostname uses valid HTTPS. Plain internal HTTP is confined to a
  private network.
- Public inference requires a revocable demo credential and enforces request,
  concurrency, body-size, token, and rate limits.
- Only the showcase, its sanitized `/showcase/status`, health/readiness, static
  assets, and completion routes are public. `/internal/*`, control status, Raft
  RPCs, worker ports, storage, logs, and metrics require private operator
  access.
- Secrets come from the hosting platform’s secret store, are absent from the
  image/repository/logs, and have a documented rotation procedure.
- Persistent Raft, routing-snapshot, WAL, and trust-floor paths use deliberate
  volumes. Backup, restore, and clean-demo reset procedures have been tested.
- Services have health checks, readiness checks, restart policies, CPU/memory
  limits, bounded queues, and graceful shutdown behavior.
- Central logs and basic availability, error-rate, latency, saturation, and
  storage alerts exist. A cost/budget alert and automatic idle policy protect
  the owner from an unattended demo.
- Abuse cases return bounded errors without leaking internals. CORS, proxy
  headers, timeouts, and maximum upload/request sizes are explicitly set.
- The hosted topology and its differences from the three-node local proof are
  documented. A low-cost single-node showcase must be called single-node.
- The public page includes a short purpose statement, repository link, release
  version, usage warning, and honest limitations.
- The URL has been tested from a signed-out browser and a separate network, and
  the owner has a one-command rollback or disable procedure.

## Recording evidence bundle

Archive the following next to the final video:

- release tag and commit SHA;
- CI run URL and downloaded proof artifact;
- sanitized request and response headers;
- exact inference request body;
- hosted topology summary;
- recording date and broad machine/cloud configuration; and
- the limitations stated in the video.

This bundle lets an interviewer distinguish a repeatable engineering
demonstration from a one-off screen recording.
