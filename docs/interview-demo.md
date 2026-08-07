# Interview demo and hosted-showcase guide

This guide turns InferLab into a short, repeatable portfolio demonstration. It
does not replace deployment or security review. Record only from a tagged
release with green CI, and state the exact topology and release tag on screen.

## Recommended recording: about four minutes

Use two environments. Send the real completion to the hosted gateway. Run the
failure and trust-policy experiment on a disposable local topology so the
recording never takes down the public demo.

For rehearsal, start the complete local topology with
`./deploy/interview/start.sh`, open the printed showcase URL, and enter the
local-only demo key. The topology contains three persistent controls, two real
CPU workers, one signed committed route, and one authenticated gateway. Stop it
with `./deploy/interview/stop.sh`; add `--purge-data` only for an intentional
clean-volume reset.

| Time | Show | Say | Evidence to keep visible |
| --- | --- | --- | --- |
| 0:00–0:25 | The current architecture diagram and release tag | “InferLab is a from-first-principles learning system for the path from an HTTP request to generated CPU tokens. This recording uses release `<TAG>`.” | Tag and commit SHA |
| 0:25–0:50 | The InferLab showcase page, then its health and readiness checks | Explain the difference between process health and readiness to accept routed inference. Name the actual hosted topology; do not imply three controls if the hosted deployment uses fewer. | HTTPS hostname, release label, healthy/ready status |
| 0:50–1:35 | Submit a prompt in the browser showcase and watch the completion stream | Point out token-by-token SSE and the evidence panel populated from the real response headers. Explain that the checked-in tiny checkpoint is deterministic and educational. | Worker, attempts, cluster, configuration revision, term, route key, completed `[DONE]` state |
| 1:35–2:05 | An operator-only view of worker and control status | Trace gateway → selected worker and identify the current control revision. Keep `/internal/*`, Raft, and service-authentication endpoints off the public internet. | Worker identity, in-flight count, control role/revision; no secrets |
| 2:05–3:15 | In a local checkout of the same tag, run `./scripts/proof-v0.25.sh` | Keep all controls alive, isolate old leader A through four directed Raft-link drops, and show the difference between appended, committed, and applied. Call A's `503` ambiguous; point to unchanged commit 2, B+C commit 4, then healed suffix replacement. Label this as a controlled loopback proof, not public-network chaos. | Final 45/45 assertion count and generated proof evidence |
| 3:15–3:45 | The v0.25 evidence chart, Figure-8 report, and RFC | Connect one observed event to one design rule: an old-term entry on a majority is not directly committed by replica count; a quorum-replicated current-term entry commits its prior prefix indirectly. Distinguish the live three-process topology from the deterministic five-server replay. | v0.25 raw evidence and RFC 0030 |
| 3:45–4:10 | Limitations and next step | State that the proof drops whole loopback Raft HTTP RPCs, not packets; it is not Jepsen, arbitrary partitions, independent hosts, formal verification, membership change, or a live five-node runtime. The model remains tiny and CUDA is not implemented. End with v0.26 bounded-cardinality Prometheus observability. | A short limitations slide or document section |

Keep a second, pre-recorded successful take only as recovery insurance. The
published video should still be a continuous live run; do not splice stored
JSON into a claimed live request.

## Honest claims

Claims that the implementation and retained evidence support:

- InferLab runs a real Rust gateway, control plane, queue, and CPU inference
  worker, with a C++20 runtime and attention kernel.
- The CPU worker emits generated tokens from the checked-in teaching
  checkpoint; it is not a wrapper around a hosted LLM API.
- The exact v0.25 proof starts three control, six directed Raft-link proxy, one
  gateway, and one real CPU-worker OS process; a four-link cut leaves A at
  commit 2 while B+C reach commit 4, healing replaces A's uncommitted suffix,
  and all process identities plus durable logs are checked.
- A separate deterministic five-server Figure-8 report calls the production
  commit and vote-freshness predicates; it is algorithmic evidence, not a live
  five-node deployment.
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
- Optional bearer-key authentication now protects public inference and
  diagnostics when `INFERLAB_PUBLIC_API_KEYS` is configured. Public-gateway
  HTTPS, global service mTLS, rate/cost limits, certificate lifecycle,
  production checkpoint/tokenizer integration, CUDA attention, and full
  production observability remain future hosting or engineering work unless a
  later tagged release explicitly adds them.

Avoid “production-ready,” “zero downtime,” “secure,” “exactly once,” and
“internet scale” unless a later release supplies and documents the missing
evidence.

## Reset and rehearsal checklist

Before each rehearsal:

- Check out the exact recording tag and confirm `git status --short` is empty.
- Confirm CI passed for that commit and download its v0.25 proof artifact.
- Verify Rust, C++20, Python 3, `curl`, and OpenSSL are available.
- Confirm proof ports `9960`–`9964` and `9971`–`9976` are unused.
- Start from fresh disposable local state; never reuse trust floors or Raft
  data from an earlier take.
- Exercise the hosted health, readiness, and completion requests once with the
  same account, token, payload, and network used for recording.
- Open the showcase from a fresh browser profile, enter the disposable demo
  key, and confirm that the key is not persisted after a reload.
- Confirm the streaming response ends in `[DONE]` and exposes the expected
  non-secret diagnostic headers.
- Run `./scripts/proof-v0.25.sh` once and confirm all 45 assertions pass.
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
