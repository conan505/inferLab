# v0.0.5 consistent-hash ownership experiment

## Hypothesis

Virtual nodes should produce a more even prefix distribution than one point per worker. Adding or removing worker D should remap roughly D's one-quarter share, and no unrelated key should move.

## Reproduce

```bash
INFERLAB_RESULTS_DIR=docs/results/v0.0.5/raw ./scripts/proof-v0.0.5.sh
```

## Environment and workload

- Date: 2026-07-28
- Host: Apple arm64
- Rust: 1.97.1
- Hash: FNV-1a-64 with MurmurHash3 `fmix64` avalanche
- Workers: A/B/C for distribution; D added and removed for topology analysis
- Virtual-node samples: 1, 16, 128 per worker
- Corpus: 20,000 deterministic `tenant-N/prompt-prefix-N` keys

## Recorded result

| Virtual nodes per worker | A keys | B keys | C keys | Maximum deviation from equal share |
|---:|---:|---:|---:|---:|
| 1 | 10,704 | 1,344 | 7,952 | 79.840% |
| 16 | 7,887 | 3,358 | 8,755 | 49.630% |
| 128 | 6,066 | 6,445 | 7,489 | 12.335% |

| Topology change | Remapped keys | Fraction | Unexpected remaps |
|---|---:|---:|---:|
| Add D to A/B/C | 4,892 | 24.46% | 0 |
| Remove D from A/B/C/D | 4,892 | 24.46% | 0 |

Rebuilding the same A/B/C ring produced identical owners for all 20,000 keys.

## Conclusion

The hypothesis was supported for this deterministic corpus. A one-point ring was extremely uneven; 128 virtual nodes reduced the worst deviation from equal share by 67.505 percentage points.

The join and leave experiments moved only D's 24.46% share. Zero unexpected remaps is the central correctness result: a key belonging to an unchanged worker retained its owner.

This proves routing ownership behavior, not cache performance. InferLab does not yet store KV-cache blocks, update the ring live, replicate hot prefixes, or combine affinity with health and load.

Raw evidence:

- [`ring-analysis.json`](raw/ring-analysis.json)
- [`ring-check.json`](raw/ring-check.json)
