#!/usr/bin/env python3
"""Generate InferLab's deterministic educational FP32 checkpoint."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import struct
from pathlib import Path

MAGIC = b"INFLAB1\0"
VERSION = 1
VOCAB = [
    "<pad>",
    "<bos>",
    "<eos>",
    "<unk>",
    "InferLab",
    "turns",
    "prompts",
    "into",
    "real",
    "tokens",
    ".",
    "hello",
    "teach",
    "me",
    "streaming",
    "systems",
]
CONTEXT_LENGTH = 32
DIMENSION = 16
HEADS = 4
FEED_FORWARD_DIMENSION = 32
LAYERS = 1


def patterned(count: int, scale: float, phase: float) -> list[float]:
    return [
        scale * math.sin((index + 1) * 0.731 + phase)
        for index in range(count)
    ]


def build_tensors(
    vocabulary: list[str] = VOCAB,
) -> list[tuple[str, list[float]]]:
    vocab_size = len(vocabulary)
    token_embedding = patterned(vocab_size * DIMENSION, 0.001, 0.13)
    for token_id in range(vocab_size):
        token_embedding[
            token_id * DIMENSION + token_id % DIMENSION
        ] += 1.0

    position_embedding = patterned(
        CONTEXT_LENGTH * DIMENSION, 0.002, 0.37
    )
    one = [1.0] * DIMENSION
    zero = [0.0] * DIMENSION
    query = patterned(DIMENSION * DIMENSION, 0.012, 0.53)
    key = patterned(DIMENSION * DIMENSION, 0.011, 0.71)
    value = patterned(DIMENSION * DIMENSION, 0.013, 0.89)
    attention_output = patterned(DIMENSION * DIMENSION, 0.010, 1.07)
    feed_forward_in = patterned(
        FEED_FORWARD_DIMENSION * DIMENSION, 0.014, 1.31
    )
    feed_forward_in_bias = patterned(FEED_FORWARD_DIMENSION, 0.002, 1.49)
    feed_forward_out = patterned(
        DIMENSION * FEED_FORWARD_DIMENSION, 0.012, 1.67
    )
    feed_forward_out_bias = patterned(DIMENSION, 0.002, 1.83)

    # The head makes the tiny model readable without training. Every tensor in
    # the transformer remains active; these strong transition columns simply
    # make greedy output deterministic and easy to inspect.
    lm_head = [0.0] * (vocab_size * DIMENSION)
    transitions = {
        0: 4,
        1: 4,
        2: 4,
        3: 4,
        4: 5,
        5: 6,
        6: 7,
        7: 8,
        8: 9,
        9: 10,
        10: 2,
        11: 4,
        12: 4,
        13: 4,
        14: 4,
        15: 4,
    }
    for previous, following in transitions.items():
        lm_head[following * DIMENSION + previous] = 4.0

    return [
        ("token_embedding", token_embedding),
        ("position_embedding", position_embedding),
        ("ln1_weight", one),
        ("ln1_bias", zero),
        ("query_weight", query),
        ("key_weight", key),
        ("value_weight", value),
        ("attention_output_weight", attention_output),
        ("ln2_weight", one),
        ("ln2_bias", zero),
        ("feed_forward_in_weight", feed_forward_in),
        ("feed_forward_in_bias", feed_forward_in_bias),
        ("feed_forward_out_weight", feed_forward_out),
        ("feed_forward_out_bias", feed_forward_out_bias),
        ("final_norm_weight", one),
        ("final_norm_bias", zero),
        ("lm_head_weight", lm_head),
        ("lm_head_bias", [0.0] * vocab_size),
    ]


def generate(
    model_path: Path,
    metadata_path: Path,
    vocabulary: list[str] = VOCAB,
    generator: str = "oracle/generate_tiny_model.py",
) -> None:
    model_path.parent.mkdir(parents=True, exist_ok=True)
    tensors = build_tensors(vocabulary)
    with model_path.open("wb") as output:
        output.write(MAGIC)
        output.write(
            struct.pack(
                "<7I",
                VERSION,
                len(vocabulary),
                CONTEXT_LENGTH,
                DIMENSION,
                HEADS,
                FEED_FORWARD_DIMENSION,
                LAYERS,
            )
        )
        for token in vocabulary:
            encoded = token.encode("utf-8")
            output.write(struct.pack("<I", len(encoded)))
            output.write(encoded)
        for _, values in tensors:
            output.write(struct.pack(f"<{len(values)}f", *values))

    digest = hashlib.sha256(model_path.read_bytes()).hexdigest()
    parameter_count = sum(len(values) for _, values in tensors)
    metadata = {
        "format": "inferlab-tiny-fp32",
        "version": VERSION,
        "sha256": digest,
        "bytes": model_path.stat().st_size,
        "parameter_count": parameter_count,
        "architecture": {
            "vocab_size": len(vocabulary),
            "context_length": CONTEXT_LENGTH,
            "dimension": DIMENSION,
            "heads": HEADS,
            "feed_forward_dimension": FEED_FORWARD_DIMENSION,
            "layers": LAYERS,
            "activation": "GELU tanh approximation",
            "normalization": "pre-layernorm",
            "dtype": "float32",
        },
        "special_tokens": {"pad": 0, "bos": 1, "eos": 2, "unknown": 3},
        "vocabulary": vocabulary,
        "tensor_order": [
            {"name": name, "values": len(values)} for name, values in tensors
        ],
        "generator": generator,
    }
    metadata_path.write_text(json.dumps(metadata, indent=2) + "\n")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--model", type=Path, default=Path("models/tiny-inferlab-v1.bin")
    )
    parser.add_argument(
        "--metadata",
        type=Path,
        default=Path("models/tiny-inferlab-v1.json"),
    )
    args = parser.parse_args()
    generate(args.model, args.metadata)


if __name__ == "__main__":
    main()
