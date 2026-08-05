#!/usr/bin/env python3
"""Independent PyTorch oracle for the retained causal-attention fixture."""

from __future__ import annotations

import argparse
import json
import math
from pathlib import Path

import torch


def storage_round(values: list[float], precision: str) -> torch.Tensor:
    tensor = torch.tensor(values, dtype=torch.float32)
    if precision == "fp16":
        return tensor.to(torch.float16).to(torch.float32)
    if precision == "bf16":
        return tensor.to(torch.bfloat16).to(torch.float32)
    if precision == "fp32":
        return tensor
    raise ValueError(f"unsupported precision: {precision}")


def attention(fixture: dict, precision: str) -> list[float]:
    tokens = fixture["tokens"]
    heads = fixture["heads"]
    dimension = fixture["head_dimension"]

    def shaped(name: str) -> torch.Tensor:
        return storage_round(fixture[name], precision).reshape(
            tokens, heads, dimension
        ).permute(1, 0, 2)

    queries = shaped("queries")
    keys = shaped("keys")
    values = shaped("values")
    scores = torch.matmul(queries, keys.transpose(-1, -2)) / math.sqrt(dimension)
    future = torch.triu(
        torch.ones((tokens, tokens), dtype=torch.bool), diagonal=1
    )
    probabilities = torch.softmax(scores.masked_fill(future, -torch.inf), dim=-1)
    output = torch.matmul(probabilities, values).permute(1, 0, 2).contiguous()
    return output.flatten().tolist()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--probe", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    probe = json.loads(args.probe.read_text())
    fixture = probe["fixture"]
    result = {
        "implementation": "pytorch-materialized-causal-attention",
        "pytorch_version": torch.__version__,
        "storage_precision_note": (
            "FP16 and BF16 inputs are rounded through PyTorch storage dtypes, "
            "then all matrix products, softmax, and accumulation run in FP32."
        ),
        "references": [
            {"precision": precision, "output": attention(fixture, precision)}
            for precision in ["fp32", "fp16", "bf16"]
        ],
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
