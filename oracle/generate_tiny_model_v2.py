#!/usr/bin/env python3
"""Generate the v0.10 teaching checkpoint with append-only JSON tokens."""

from __future__ import annotations

import argparse
from pathlib import Path

from generate_tiny_model import VOCAB, generate

VOCAB_V2 = [
    *VOCAB,
    '{"answer":"',
    '","confidence":"',
    "high",
    "medium",
    "low",
    '"}',
]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--model", type=Path, default=Path("models/tiny-inferlab-v2.bin")
    )
    parser.add_argument(
        "--metadata",
        type=Path,
        default=Path("models/tiny-inferlab-v2.json"),
    )
    args = parser.parse_args()
    generate(
        args.model,
        args.metadata,
        VOCAB_V2,
        "oracle/generate_tiny_model_v2.py",
    )


if __name__ == "__main__":
    main()
