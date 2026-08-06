# InferLab

InferLab is one evolving system for learning how production LLM inference works—from an HTTP request entering a distributed gateway to a token leaving an optimized kernel.

The project has two equally important outputs:

1. a working distributed inference platform; and
2. reproducible evidence that explains why it works, where it fails, and what each design choice buys us.

Start with the [product requirements](docs/PRD.md), then read [RFC 0001](docs/rfcs/0001-serving-path.md) alongside the first implementation.

## Current milestone: v0.20 cryptographic service identities

```mermaid
flowchart TD
    Peer["Raft peer<br/>node service key"] -->|"signed vote / append<br/>exact audience + body"| Gate{"identity · freshness<br/>replay · peer scope"}
    Gate -->|"fail"| R["401 or 403<br/>no Raft mutation"]
    Gate -->|"pass"| Raft["Raft state transition"]
    Gateway["gateway-primary<br/>service key"] -->|"signed GET for exact node"| Read{"identity · freshness<br/>replay · gateway scope"}
    Read -->|"fail"| R
    Read -->|"pass"| Route["separately signed<br/>committed route"]
    Route --> Gateway
    Gateway --> Worker["real online-attention CPU worker"]
```

v0.20 gives every control node and the gateway a separate Ed25519 service
identity. Raft vote/append RPCs are signed for the exact destination and bind
method, path, cluster, timestamp, nonce, and canonical body. The receiver
checks cryptography, freshness, local replay history, and peer-ID scope before
Raft may see the request. Gateway control reads use the same request protocol
with a distinct gateway role and an exact URL-to-node identity map.

The retained proof elects and replicates through signed peer requests; rejects
missing, unknown, stale, replayed, and tampered requests with 401; rejects a
peer acting as gateway and a gateway acting as peer with 403; and proves high-
term rejected inputs leave term 1 and route revision 2 unchanged. The real
gateway publishes the separately route-signed revision, completes a 185.707 ms
request, and reaches `[DONE]` on a 186.723 ms SSE. All 20 assertions pass.

This is request-level identity and integrity, not encryption or mTLS. HTTP
content remains visible; trust/rotation are static; replay memory resets on
restart; private seeds remain educational environment configuration.

![Cryptographic service-identity decisions](docs/results/v0.20/raw/service-auth-proof.svg)

## Run it

Prerequisites: stable Rust, a C++20 compiler, Python 3, and `curl`. The v0.7
through v0.13 oracle/environment proofs additionally need PyTorch 2.2.2 or a
compatible CPU build.

```bash
cargo test --workspace
./scripts/proof-v0.1.sh
./scripts/proof-v0.0.2.sh
./scripts/proof-v0.0.3.sh
./scripts/proof-v0.0.4.sh
./scripts/proof-v0.0.5.sh
./scripts/proof-v0.0.6.sh
./scripts/proof-v0.0.7.sh
./scripts/proof-v0.0.8.sh
./scripts/proof-v0.0.9.sh
./scripts/proof-v0.5.sh
./scripts/proof-v0.6.sh
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python ./scripts/proof-v0.7.sh
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python ./scripts/proof-v0.8.sh
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python ./scripts/proof-v0.9.sh
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python ./scripts/proof-v0.10.sh
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python ./scripts/proof-v0.11.sh
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python ./scripts/proof-v0.12.sh
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python ./scripts/proof-v0.13.sh
./scripts/proof-v0.14.sh
./scripts/proof-v0.15.sh
./scripts/proof-v0.16.sh
./scripts/proof-v0.17.sh
./scripts/proof-v0.18.sh
./scripts/proof-v0.19.sh
./scripts/proof-v0.20.sh
```

Earlier routing and resilience experiments still use deterministic fake workers:

```bash
FAKE_WORKER_ID=worker-a FAKE_WORKER_BIND=127.0.0.1:9001 cargo run -p fake-worker
FAKE_WORKER_ID=worker-b FAKE_WORKER_BIND=127.0.0.1:9002 cargo run -p fake-worker
FAKE_WORKER_ID=worker-c FAKE_WORKER_BIND=127.0.0.1:9003 cargo run -p fake-worker
INFERLAB_ROUTING_POLICY=least-in-flight \
INFERLAB_WORKER_CONCURRENCY=2 \
INFERLAB_ADMISSION_QUEUE_CAPACITY=4 \
INFERLAB_REQUEST_DEADLINE_MS=30000 \
INFERLAB_ATTEMPT_TIMEOUT_MS=5000 \
INFERLAB_MAX_RETRIES=2 \
INFERLAB_RETRY_BUDGET_PERCENT=10 \
INFERLAB_CIRCUIT_WINDOW_SIZE=10 \
INFERLAB_CIRCUIT_MIN_REQUESTS=5 \
INFERLAB_CIRCUIT_FAILURE_RATE_PERCENT=50 \
INFERLAB_CIRCUIT_OPEN_MS=5000 \
INFERLAB_WORKERS='worker-a=http://127.0.0.1:9001,worker-b=http://127.0.0.1:9002,worker-c=http://127.0.0.1:9003' \
  cargo run -p gateway
```

Then stream a completion:

```bash
curl -iN http://127.0.0.1:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"inferlab-fake","stream":true,"messages":[{"role":"user","content":"teach me streaming"}]}'
```

The `x-inferlab-worker` response header proves which worker served the request. The SSE body ends with `data: [DONE]`.

To serve real CPU decoder tokens instead:

```bash
INFERLAB_CPU_WORKER_ID=cpu-worker-a \
INFERLAB_CPU_BIND=127.0.0.1:9101 \
INFERLAB_MODEL_PATH=models/tiny-inferlab-v2.bin \
INFERLAB_CPU_DECODER_MODE=paged-kv-cache \
INFERLAB_CPU_QUANTIZATION=fp32 \
INFERLAB_CPU_SPECULATIVE_DRAFT_QUANTIZATION=int8 \
INFERLAB_CPU_ATTENTION_KERNEL=online-tiled \
INFERLAB_CPU_ATTENTION_PRECISION=fp32 \
INFERLAB_CPU_ATTENTION_TILE_TOKENS=32 \
INFERLAB_CPU_KV_PAGE_TOKENS=4 \
INFERLAB_CPU_KV_PAGE_COUNT=64 \
INFERLAB_CPU_PREFIX_CACHE_CAPACITY=32 \
INFERLAB_CPU_MAX_BATCH_SIZE=4 \
  cargo run -p cpu-worker

INFERLAB_WORKERS='cpu-worker-a=http://127.0.0.1:9101' \
  cargo run -p gateway

curl -N http://127.0.0.1:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"inferlab-tiny","stream":true,"temperature":0,"speculative_tokens":3,"max_tokens":8,"messages":[{"role":"user","content":"teach me streaming"}]}'
```

Set `INFERLAB_CPU_QUANTIZATION` to `int8` or `int4` to serve a quantized target.
Set `INFERLAB_CPU_SPECULATIVE_DRAFT_QUANTIZATION` to `int8`, `int4`, or `off`.
Speculation currently supports text responses only and accepts windows up to
eight; omitting `speculative_tokens` or setting it to zero uses the ordinary
target path.

Set `INFERLAB_CPU_ATTENTION_KERNEL` to `materialized` or `online-tiled`.
`INFERLAB_CPU_ATTENTION_PRECISION` accepts `fp32`, `fp16`, or `bf16`; the latter
two are CPU storage-rounding simulations with FP32 accumulation. Tile size is
selected by `INFERLAB_CPU_ATTENTION_TILE_TOKENS`.

To exercise the current structured path, add `temperature`, `seed`, and the
supported strict response format:

```bash
curl -N http://127.0.0.1:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"inferlab-tiny","stream":true,"temperature":1,"seed":7007,"max_tokens":6,"messages":[{"role":"user","content":"teach me streaming"}],"response_format":{"type":"json_schema","json_schema":{"name":"inference_summary","strict":true,"schema":{"type":"object","properties":{"answer":{"type":"string","enum":["InferLab","systems","tokens"]},"confidence":{"type":"string","enum":["high","medium","low"]}},"required":["answer","confidence"],"additionalProperties":false}}}}'
```

The circuit defaults use a 10-outcome window, at least 5 samples, a 50%
transient-failure threshold, and a 5-second open period. `/internal/workers`
exposes each state, recent error rate, rejected routes, probes, openings, and
recoveries.

Run the batch service separately:

```bash
INFERLAB_BATCH_WAL=./data/inferlab-batch.wal \
  cargo run -p batch-queue
```

It listens on `127.0.0.1:8081` by default.

To bound cold-start disk fallback, configure the durable path and a positive
maximum age. Future skew defaults to 1,000 ms when omitted:

```bash
INFERLAB_ROUTING_SNAPSHOT_PATH=./data/gateway-routing.json \
INFERLAB_ROUTING_SNAPSHOT_MAX_AGE_MS=300000 \
INFERLAB_ROUTING_SNAPSHOT_MAX_FUTURE_SKEW_MS=1000 \
INFERLAB_CONTROL_PLANE_URLS='http://127.0.0.1:7001,http://127.0.0.1:7002,http://127.0.0.1:7003' \
  cargo run -p gateway
```

The cold-start age gate still applies only when a new process considers disk.
To govern a running process, add a positive lease duration and choose the
expiry action:

```bash
INFERLAB_ROUTING_LEASE_MS=30000 \
INFERLAB_ROUTING_LEASE_EXPIRY_ACTION=reject-new \
INFERLAB_CONTROL_CLUSTER_ID=prod-inference-eu1 \
INFERLAB_CONTROL_TRUSTED_KEYS='route-2026-a=<base64-public-key>,route-2026-b=<base64-public-key>' \
INFERLAB_CONTROL_REVOKED_KEY_IDS='' \
INFERLAB_ROUTING_SNAPSHOT_PATH=./data/gateway-routing.json \
INFERLAB_ROUTING_SNAPSHOT_MAX_AGE_MS=300000 \
INFERLAB_CONTROL_PLANE_URLS='http://127.0.0.1:7001,http://127.0.0.1:7002,http://127.0.0.1:7003' \
  cargo run -p gateway
```

Use `serve-stale` only when continuing new traffic after loss of recent control
verification is the intended availability policy. Every control node serving
this gateway must use the matching
`INFERLAB_RAFT_CLUSTER_ID=prod-inference-eu1`. Set explicit, unique values in
real deployments; `inferlab-default` is only a compatibility/teaching default.
To authenticate route bytes, every control process also uses the matching active
`INFERLAB_CONTROL_SIGNING_KEY_ID` and
`INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64`. Provision public key B before
switching controls to B, confirm gateways persist B, then revoke A. List trusted
keys oldest to newest; the gateway refuses a later downgrade after B is active.

To require authorized route creation, configure the same writer trust policy on
every control node:

```bash
INFERLAB_CONTROL_WRITER_KEYS='deploy-bot=<base64-public-key>,break-glass=<base64-public-key>' \
INFERLAB_CONTROL_REVOKED_WRITER_IDS='' \
INFERLAB_CONTROL_WRITE_MAX_AGE_MS=30000 \
INFERLAB_CONTROL_WRITE_MAX_FUTURE_SKEW_MS=5000
```

To require service identity on Raft RPCs and gateway route reads, give every
control node its matching ID/private seed and the same public trust/scope
policy:

```bash
INFERLAB_SERVICE_ID=node-a
INFERLAB_SERVICE_PRIVATE_KEY_B64='<node-a-private-seed>'
INFERLAB_SERVICE_TRUSTED_KEYS='node-a=<public>,node-b=<public>,node-c=<public>,gateway-primary=<public>'
INFERLAB_SERVICE_REVOKED_IDS=''
INFERLAB_GATEWAY_SERVICE_IDS='gateway-primary'
INFERLAB_SERVICE_AUTH_MAX_AGE_MS=5000
INFERLAB_SERVICE_AUTH_MAX_FUTURE_SKEW_MS=1000
```

The gateway uses its own identity plus an exact URL-to-control-node map:

```bash
INFERLAB_GATEWAY_SERVICE_ID=gateway-primary
INFERLAB_GATEWAY_SERVICE_PRIVATE_KEY_B64='<gateway-private-seed>'
INFERLAB_CONTROL_SERVICE_TARGETS='node-a=http://127.0.0.1:7001,node-b=http://127.0.0.1:7002,node-c=http://127.0.0.1:7003'
```

For the current service-identity milestone, see
[RFC 0025](docs/rfcs/0025-cryptographic-service-identities.md), the
[phase 25 learning guide](docs/learning/phase-25-cryptographic-service-identities.md),
and the [retained v0.20 evidence](docs/results/v0.20/README.md). RFC 0024 and the
[phase 24 learning guide](docs/learning/phase-24-authorized-control-writers.md)
remain the administrative creation reference; RFC 0023 and the
[phase 23 learning guide](docs/learning/phase-23-signed-control-configurations.md)
remain the signed route-delivery and key-rotation reference; RFC 0022 and the
[phase 22 guide](docs/learning/phase-22-control-cluster-identity-fencing.md)
remain the cluster-namespace reference; RFC 0021 and the
[phase 21 guide](docs/learning/phase-21-runtime-routing-lease.md) remain the
runtime admission reference; RFC 0020 and the
[phase 20 guide](docs/learning/phase-20-bounded-age-routing-fallback.md) remain
the cold-start time-policy reference; RFC 0019 and the
[phase 19 guide](docs/learning/phase-19-restart-safe-routing-snapshots.md)
remain the durable routing snapshot reference; RFC 0018 and the
[phase 18 guide](docs/learning/phase-18-real-worker-full-stack-integration.md)
remain the integrated request-snapshot reference; RFC 0017 and the
[phase 17 guide](docs/learning/phase-17-tiled-online-softmax-attention.md)
remain the online-attention reference; RFC 0016 and the
[phase 16 guide](docs/learning/phase-16-quantization-and-speculative-decoding.md)
remain the quantization and speculative-decoding reference; RFC 0015 and the
[phase 15 guide](docs/learning/phase-15-sampling-and-structured-decoding.md)
remain the sampling and structured-decoding reference; RFC 0014 and the
[phase 14 guide](docs/learning/phase-14-paged-kv-cache-and-prefix-ownership.md)
remain the physical-memory and prefix-ownership reference; RFC 0013 and the
[phase 13 guide](docs/learning/phase-13-kv-cache-and-continuous-batching.md)
remain the contiguous-cache and scheduler reference.

## Repository map

```text
gateway/          Rust data-plane gateway
control-auth/     canonical route payload, Ed25519 signatures, trust, revocation
service-auth/     signed service requests, audience binding, trust, revocation
fake-worker/      deterministic inference simulator used for tests
batch-queue/      Rust durable batch API, WAL replay, leases, fencing, and DLQ
control-plane/    persistent three-node Raft election, log, commit, and config API
worker/           C++ CPU decoder, Rust transport adapter, CLI, and tests
models/           explicit tiny checkpoint and reproducibility metadata
oracle/           checkpoint generator and independent PyTorch reference
kernels/          CPU attention algorithms and future CUDA kernels
benchmarks/       load clients, analyzers, evidence checkers, SVG renderers
scripts/          reproducible proof and safe orchestration entry points
docs/rfcs/        decisions, invariants, and trade-offs
docs/learning/    milestone explanations and experiments
docs/results/     benchmark evidence and conclusions
```

## Learning loop

Every milestone follows the same loop:

1. **Predict:** write the mental model and expected result.
2. **Build:** implement the smallest vertical behavior.
3. **Break:** inject the failure the design claims to handle.
4. **Measure:** save raw data and latency/throughput summaries.
5. **Explain:** compare prediction with evidence and record surprises.

This is the scientific method applied to systems engineering. A green test proves a specified behavior; a benchmark tests a performance hypothesis; a failure test validates a recovery claim. None is interchangeable with the others.
