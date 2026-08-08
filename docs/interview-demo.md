# Interview demo and hosted-showcase guide

This guide turns InferLab into a short, repeatable portfolio demonstration. It
does not replace deployment or security review. Record only from a tagged
release with green CI, and state the exact topology and release tag on screen.

## Recommended recording: about four minutes

Use two environments. Send the real completion to the hosted gateway. Run the
exact observability/failure proof on a disposable local topology so the
recording never takes down the public demo.

For rehearsal, start the complete local topology with
`./deploy/interview/start.sh`, open the printed showcase URL, and enter the
local-only demo key. The topology contains three persistent controls, two real
CPU workers, one signed committed route, one authenticated gateway, and pinned
Prometheus v3.13.1. Only the showcase and Prometheus UI are host-published,
both on loopback. Stop it with `./deploy/interview/stop.sh`; add `--purge-data`
only for an intentional clean-volume reset. Prometheus history is always
ephemeral.

| Time | Show | Say | Evidence to keep visible |
| --- | --- | --- | --- |
| 0:00–0:25 | The current architecture diagram and release tag | “InferLab is a from-first-principles learning system for the path from an HTTP request to generated CPU tokens. This recording uses release `<TAG>`.” | Tag and commit SHA |
| 0:25–0:50 | The showcase, health/readiness, and Prometheus targets page | Explain process health versus readiness to accept routed inference. Show the private gateway, three controls, and two CPU-worker scrapes; only the collector UI is host-published. Name the actual hosted topology if it differs. | Release label, healthy/ready status, six `UP` targets |
| 0:50–1:30 | Submit a prompt and watch the completion stream | Point out token-by-token SSE and the bounded `x-inferlab-request-id` plus worker/attempt/revision headers. Explain that the tiny checkpoint is deterministic and educational. | Stable request ID, worker, attempts, cluster/revision/term, `[DONE]` |
| 1:30–2:10 | Three bounded PromQL queries | Show service request rate, status-class rate, and p95 handler latency by service. Explain why request IDs, prompts, worker IDs, and raw paths belong in logs rather than labels. | Queries from the checklist; finite service/route/method/status domains |
| 2:10–3:10 | `./scripts/proof-v0.26.sh` and its retained chart | Explain the design-time budget (≤256 per target and 1,721 for the proof topology), the 737→957→957→1,047 observations, and why 24 unique prompts adding zero series is the important result. | Final 36/36 count, cardinality JSON, and SVG |
| 3:10–3:40 | Request-ID and failure-delta evidence | Trace one ID through client→gateway→worker and across both retry attempts. Show exact retry, queue, trust, and link deltas plus histogram algebra. | Sanitized request/log captures, `deltas.json`, `histograms.json` |
| 3:40–4:10 | Limitations and next boundary | State that this is one single-host exact schedule, not a soak/load test or production monitoring stack. Prometheus is ephemeral; Grafana, alerts/SLOs, traces, remote write, HA, auth/TLS, and scrape-safe cache metrics are not claimed. The next boundary comes from the explicit backlog; CUDA remains hardware-gated v1.0. | RFC 0031 limitations and Phase 31 glossary |

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
- Request IDs are bounded correlation values, not authentication or global
  uniqueness. Metrics listeners use private HTTP and create no security
  boundary.
- Optional bearer-key authentication now protects public inference and
  diagnostics when `INFERLAB_PUBLIC_API_KEYS` is configured. Public-gateway
  HTTPS, global service mTLS, rate/cost limits, certificate lifecycle,
  production checkpoint/tokenizer integration, CUDA attention, persistent/HA
  Prometheus, dashboards, alerts/SLOs, traces, and remote write remain future
  hosting or engineering work unless a later tagged release explicitly adds
  them.

Avoid “production-ready,” “zero downtime,” “secure,” “exactly once,” and
“internet scale” unless a later release supplies and documents the missing
evidence.

## Reset and rehearsal checklist

Before each rehearsal:

- Check out the exact recording tag and confirm `git status --short` is empty.
- Confirm CI passed for that commit and download its v0.26 proof artifact.
- Verify Rust, C++20, Python 3, `curl`, and OpenSSL are available.
- Confirm application/fixture ports `10060`–`10070` and metrics ports
  `10160`–`10168` are unused.
- Start from fresh disposable local state; never reuse trust floors or Raft
  data from an earlier take.
- Exercise the hosted health, readiness, and completion requests once with the
  same account, token, payload, and network used for recording.
- Open the showcase from a fresh browser profile, enter the disposable demo
  key, and confirm that the key is not persisted after a reload.
- Confirm the streaming response ends in `[DONE]` and exposes the expected
  non-secret diagnostic headers.
- Run `./scripts/proof-v0.26.sh` once and confirm all 36 assertions pass.
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

After a failed take, stop all disposable processes, remove only the dedicated
demo data directory, verify the proof ports are free, and restart from the same
tag. Reset hosted state through the deployment’s documented, reversible reset
procedure; do not manually edit production volumes during a recording.

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
