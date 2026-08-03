# InferLab

InferLab is one evolving system for learning how production LLM inference works—from an HTTP request entering a distributed gateway to a token leaving an optimized kernel.

The project has two equally important outputs:

1. a working distributed inference platform; and
2. reproducible evidence that explains why it works, where it fails, and what each design choice buys us.

Start with the [product requirements](docs/PRD.md), then read [RFC 0001](docs/rfcs/0001-serving-path.md) alongside the first implementation.

## Current milestone: v0.10 sampling and structured decoding

```mermaid
flowchart LR
    C["OpenAI-compatible client"] --> G["Rust gateway"]
    G -->|"consistent-hash affinity"| H["selected Rust worker"]
    H --> Q["bounded submission queue"]
    Q --> S["continuous scheduler<br/>up to 4 active sessions"]
    S -->|"one step per active session"| D["C++ paged session<br/>22 raw logits"]
    D --> P["repetition penalty → bans → grammar mask<br/>temperature → top-k → top-p"]
    F["Rust JSON-schema compiler<br/>seven-state token DFA"] -->|"allowed token IDs"| P
    P -->|"greedy or SplitMix64 sample"| T["selected token + probability<br/>entropy + support size"]
    T -->|"advance grammar and KV state"| S
    S --> H
    H -->|"JSON or one SSE event per visible token"| C
```

v0.10 preserves raw model logits, then applies one fixed C++ selection
pipeline: repetition penalty, token bans, grammar mask, temperature, top-k,
top-p, and deterministic greedy or seeded categorical choice. Rust compiles a
supported strict JSON schema into a seven-state token DFA and supplies the legal
token IDs on every step. Unsupported schemas fail with HTTP 400 before any
streaming bytes are sent.

The v1 checkpoint remains byte-identical. An append-only v2 checkpoint expands
the teaching vocabulary from 16 to 22 tokens without changing old greedy
tokens or logits. In retained evidence, all 30,000 temperature samples remain
within 0.581 percentage points of exact softmax probabilities; 10,000/10,000
structured generations parse, satisfy the exact schema, and reach EOS; gateway
non-streaming and SSE paths both preserve the guarantee; and all 27 release
assertions pass. v2 logits remain within `4.1975708e-06` of PyTorch.

The grammar guarantees syntax and enum membership, not semantic quality. The
untrained checkpoint chooses `InferLab` 9,991 times out of 10,000, which makes
the limitation observable instead of hiding it behind a perfect-validity rate.

![Sampling support, temperature distributions, JSON automaton, validity, and answer skew](docs/results/v0.10/raw/structured-decoding-proof.svg)

## Run it

Prerequisites: stable Rust, a C++20 compiler, Python 3, and `curl`. The v0.7
through v0.10 oracle proofs additionally need PyTorch 2.2.2 or a compatible
CPU build.

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
INFERLAB_CPU_KV_PAGE_TOKENS=4 \
INFERLAB_CPU_KV_PAGE_COUNT=64 \
INFERLAB_CPU_PREFIX_CACHE_CAPACITY=32 \
INFERLAB_CPU_MAX_BATCH_SIZE=4 \
  cargo run -p cpu-worker

INFERLAB_WORKERS='cpu-worker-a=http://127.0.0.1:9101' \
  cargo run -p gateway

curl -N http://127.0.0.1:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"inferlab-tiny","stream":true,"temperature":0,"max_tokens":8,"messages":[{"role":"user","content":"teach me streaming"}]}'
```

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

For the current runtime milestone, see
[RFC 0015](docs/rfcs/0015-sampling-structured-decoding.md), the
[phase 15 learning guide](docs/learning/phase-15-sampling-and-structured-decoding.md), and the
[retained v0.10 evidence](docs/results/v0.10/README.md). RFC 0014 and the
[phase 14 guide](docs/learning/phase-14-paged-kv-cache-and-prefix-ownership.md)
remain the physical-memory and prefix-ownership reference; RFC 0013 and the
[phase 13 guide](docs/learning/phase-13-kv-cache-and-continuous-batching.md)
remain the contiguous-cache and scheduler reference.

## Repository map

```text
gateway/          Rust data-plane gateway
fake-worker/      deterministic inference simulator used for tests
batch-queue/      Rust durable batch API, WAL replay, leases, fencing, and DLQ
control-plane/    persistent three-node Raft election, log, commit, and config API
worker/           C++ CPU decoder, Rust transport adapter, CLI, and tests
models/           explicit tiny checkpoint and reproducibility metadata
oracle/           checkpoint generator and independent PyTorch reference
kernels/          future CPU and CUDA kernels
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
