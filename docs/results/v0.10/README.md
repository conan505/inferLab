# v0.10 proof: sampling and structured decoding

This retained experiment proves the production C++ selector's processor order,
seeded categorical behavior, append-only checkpoint compatibility, token-DFA
enforcement, and real gateway JSON/SSE integration.

## Hypothesis

- Golden cases expose the intended effect of repetition penalty, token bans,
  grammar masks, top-k, and top-p.
- Ten thousand seeded selections at each of temperatures 0.5, 1.0, and 2.0
  remain within one percentage point of exact softmax probabilities.
- Identical seeds replay identical sample sequences.
- Every token selected under the supported schema follows its seven-state DFA.
- 10,000 real structured generations parse, match the exact enum schema, and
  terminate through the grammar's EOS transition.
- The v2 append-only vocabulary preserves all v1 greedy tokens and old logits.
- The v2 C++ decoder remains within `1e-4` of the independent PyTorch oracle.
- Non-streaming and SSE gateway responses remain schema-valid, while an
  unsupported schema fails before streaming.

## Result chart

![Processor support, expected and observed temperature distributions, DFA states, structured validity, and answer skew](raw/structured-decoding-proof.svg)

## Retained result

| Observation | Result |
|---|---:|
| Machine-readable release assertions | 27 / 27 passed |
| Golden processor cases | 6 / 6 |
| Samples per positive temperature | 10,000 |
| Temperatures checked | 0.5 / 1.0 / 2.0 |
| Largest observed/theoretical probability error | 0.5804 percentage points |
| Distribution replay checks | 3 / 3 exact |
| Structured generations | 10,000 |
| Parser-valid objects | 10,000 / 10,000 |
| Exact-schema-valid objects | 10,000 / 10,000 |
| EOS finishes | 10,000 / 10,000 |
| Structured same-seed replay checks | 4 / 4 exact |
| Distinct valid objects reached | 7 |
| Answer counts | InferLab 9,991 / systems 6 / tokens 3 |
| Confidence counts | high 3,121 / medium 3,494 / low 3,385 |
| DFA states / selected tokens | 7 / 6 |
| Candidate total across one structured generation | 10 |
| Grammar-masked step-token positions | 122 |
| v1 / v2 vocabulary entries | 16 / 22 |
| Old-vocabulary v1/v2 maximum logit error | 0 |
| v2 C++ / PyTorch maximum logit error | `4.1975708e-06` |
| Gateway non-stream / replay / SSE status | 200 / 200 / 200 |
| Unsupported-schema / grammar-exhausting-ban status | 400 / 400 |

The 0.5804-percentage-point maximum is statistical error in the retained
temperature-2.0 sample, below the declared one-point bound. It is not model
logit error; C++/PyTorch logit parity is measured separately.

The answer histogram is a negative result worth preserving. Grammar
enforcement guarantees syntax and enum membership but cannot repair the tiny
untrained model's strong probability bias. The milestone makes no factuality,
calibration, diversity, fairness, or useful-model claim.

## Reproduce

Use Python with PyTorch 2.2.2 or compatible:

```bash
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python \
  ./scripts/proof-v0.10.sh
```

To replace this retained evidence:

```bash
INFERLAB_ORACLE_PYTHON=.tools/v0.7-python/bin/python \
INFERLAB_V10_OUTPUT_DIR=docs/results/v0.10/raw \
  ./scripts/proof-v0.10.sh
```

The script regenerates v1 and v2 in a temporary directory and byte-compares
them with the committed files before running the remaining proof.

## Raw artifacts

- [`structured-decoding-check.json`](raw/structured-decoding-check.json) — 27
  machine-readable assertions and release summary
- [`structured-decoding-proof.svg`](raw/structured-decoding-proof.svg) — chart
  rendered from retained JSON rather than hand-entered values
- [`decoding-probe.json`](raw/decoding-probe.json) — six C++ golden cases,
  three 10,000-draw distributions, and 10,000 real structured generations
- [`gateway-structured.json`](raw/gateway-structured.json) — non-streaming,
  same-seed replay, SSE reconstruction, metrics, and rejected schema
- [`vocabulary-parity.json`](raw/vocabulary-parity.json) — v1/v2 token, text,
  finish, and first-16-logit comparison
- [`v1-greedy.json`](raw/v1-greedy.json) and
  [`v2-greedy.json`](raw/v2-greedy.json) — complete C++ traces used for that
  comparison
- [`torch-v2.json`](raw/torch-v2.json) — independent PyTorch v2 trace
- [`torch-parity-v2.json`](raw/torch-parity-v2.json) — step-level C++/PyTorch
  comparison
- [`environment.json`](raw/environment.json) — host, toolchain, PyTorch, and
  v2 checkpoint identity

## Limitations

The supported schema is exactly one strict two-property object with required
`answer` and `confidence` string enums. Enum strings must each be one complete
model token. There is no arbitrary JSON Schema, regex, nesting, whitespace,
optional property, number, array, Unicode-prefix, or multi-token enum support.

The probe uses a one-layer, 16-dimensional untrained FP32 model on one Apple
ARM64 host. SplitMix64 replay is implementation-scoped, not compatible with an
external engine's seed convention and not cryptographic. The proof measures
correctness rather than latency or throughput. Raw logits stay unchanged for
oracle checks; selected probabilities are calculated only over the final
candidate support.
