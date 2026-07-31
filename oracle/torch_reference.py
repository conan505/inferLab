#!/usr/bin/env python3
"""Independent PyTorch oracle for the InferLab tiny decoder format."""

from __future__ import annotations

import argparse
import json
import math
import struct
import time
from pathlib import Path
from typing import BinaryIO

import torch
import torch.nn.functional as functional

MAGIC = b"INFLAB1\0"
EOS_TOKEN = 2
UNKNOWN_TOKEN = 3


class TinyDecoder:
    def __init__(self, path: Path) -> None:
        with path.open("rb") as source:
            if source.read(8) != MAGIC:
                raise ValueError("model magic does not match INFLAB1")
            (
                self.version,
                self.vocab_size,
                self.context_length,
                self.dimension,
                self.heads,
                self.feed_forward_dimension,
                self.layers,
            ) = read_values(source, "7I")
            if self.version != 1 or self.layers != 1:
                raise ValueError("unsupported tiny model version or layer count")
            self.vocabulary = [
                read_string(source) for _ in range(self.vocab_size)
            ]
            self.lookup = {
                token.lower(): token_id
                for token_id, token in enumerate(self.vocabulary)
            }
            self.token_embedding = read_tensor(
                source, self.vocab_size, self.dimension
            )
            self.position_embedding = read_tensor(
                source, self.context_length, self.dimension
            )
            self.ln1_weight = read_tensor(source, self.dimension)
            self.ln1_bias = read_tensor(source, self.dimension)
            self.query_weight = read_tensor(
                source, self.dimension, self.dimension
            )
            self.key_weight = read_tensor(
                source, self.dimension, self.dimension
            )
            self.value_weight = read_tensor(
                source, self.dimension, self.dimension
            )
            self.attention_output_weight = read_tensor(
                source, self.dimension, self.dimension
            )
            self.ln2_weight = read_tensor(source, self.dimension)
            self.ln2_bias = read_tensor(source, self.dimension)
            self.feed_forward_in_weight = read_tensor(
                source, self.feed_forward_dimension, self.dimension
            )
            self.feed_forward_in_bias = read_tensor(
                source, self.feed_forward_dimension
            )
            self.feed_forward_out_weight = read_tensor(
                source, self.dimension, self.feed_forward_dimension
            )
            self.feed_forward_out_bias = read_tensor(source, self.dimension)
            self.final_norm_weight = read_tensor(source, self.dimension)
            self.final_norm_bias = read_tensor(source, self.dimension)
            self.lm_head_weight = read_tensor(
                source, self.vocab_size, self.dimension
            )
            self.lm_head_bias = read_tensor(source, self.vocab_size)
            if source.read(1):
                raise ValueError("model has unexpected trailing bytes")

    def tokenize(self, prompt: str) -> list[int]:
        words: list[str] = []
        current: list[str] = []

        def flush() -> None:
            if current:
                words.append("".join(current).lower())
                current.clear()

        for character in prompt:
            if character.isascii() and (
                character.isalnum() or character == "'"
            ):
                current.append(character)
            else:
                flush()
                if character == ".":
                    words.append(".")
        flush()
        token_ids = [1]
        token_ids.extend(
            self.lookup.get(word, UNKNOWN_TOKEN) for word in words
        )
        if len(token_ids) > self.context_length:
            token_ids = [token_ids[0], *token_ids[-(self.context_length - 1) :]]
        return token_ids

    def forward(self, token_ids: list[int]) -> torch.Tensor:
        tokens = torch.tensor(token_ids, dtype=torch.long)
        positions = torch.arange(len(token_ids), dtype=torch.long)
        hidden = self.token_embedding[tokens] + self.position_embedding[positions]

        normalized = functional.layer_norm(
            hidden,
            (self.dimension,),
            self.ln1_weight,
            self.ln1_bias,
            1.0e-5,
        )
        query = functional.linear(normalized, self.query_weight)
        key = functional.linear(normalized, self.key_weight)
        value = functional.linear(normalized, self.value_weight)
        head_dimension = self.dimension // self.heads
        query = query.reshape(len(token_ids), self.heads, head_dimension)
        key = key.reshape(len(token_ids), self.heads, head_dimension)
        value = value.reshape(len(token_ids), self.heads, head_dimension)
        scores = torch.einsum("thd,shd->hts", query, key) / math.sqrt(
            head_dimension
        )
        causal_mask = torch.triu(
            torch.ones(
                len(token_ids), len(token_ids), dtype=torch.bool
            ),
            diagonal=1,
        )
        scores = scores.masked_fill(causal_mask.unsqueeze(0), float("-inf"))
        probabilities = torch.softmax(scores, dim=-1)
        context = torch.einsum(
            "hts,shd->thd", probabilities, value
        ).reshape(len(token_ids), self.dimension)
        hidden = hidden + functional.linear(
            context, self.attention_output_weight
        )

        normalized = functional.layer_norm(
            hidden,
            (self.dimension,),
            self.ln2_weight,
            self.ln2_bias,
            1.0e-5,
        )
        expanded = functional.linear(
            normalized,
            self.feed_forward_in_weight,
            self.feed_forward_in_bias,
        )
        activated = functional.gelu(expanded, approximate="tanh")
        hidden = hidden + functional.linear(
            activated,
            self.feed_forward_out_weight,
            self.feed_forward_out_bias,
        )
        hidden = functional.layer_norm(
            hidden,
            (self.dimension,),
            self.final_norm_weight,
            self.final_norm_bias,
            1.0e-5,
        )
        return functional.linear(
            hidden[-1], self.lm_head_weight, self.lm_head_bias
        )

    def generate(self, prompt: str, max_tokens: int) -> dict:
        context = self.tokenize(prompt)
        prompt_token_ids = list(context)
        started = time.perf_counter_ns()
        steps = []
        text = ""
        emitted = False
        finish_reason = "length"
        for index in range(max_tokens):
            step_started = time.perf_counter_ns()
            logits = self.forward(context)
            token_id = int(torch.argmax(logits).item())
            duration_us = (time.perf_counter_ns() - step_started) / 1_000.0
            token = self.vocabulary[token_id]
            eos = token_id == EOS_TOKEN
            punctuation = token in {".", ",", "!", "?"}
            piece = "" if eos else (
                f" {token}" if emitted and not punctuation else token
            )
            steps.append(
                {
                    "index": index,
                    "token_id": token_id,
                    "token": token,
                    "piece": piece,
                    "eos": eos,
                    "duration_us": duration_us,
                    "logits": [float(value) for value in logits.tolist()],
                }
            )
            context.append(token_id)
            if len(context) > self.context_length:
                del context[1]
            if eos:
                finish_reason = "stop"
                break
            emitted = True
            text += piece
        generation_us = (time.perf_counter_ns() - started) / 1_000.0
        completion_tokens = sum(not step["eos"] for step in steps)
        return {
            "model": {
                "name": "inferlab-tiny",
                "format": "inferlab-tiny-fp32-v1",
                "dtype": "float32",
                "vocabulary": self.vocab_size,
                "context_length": self.context_length,
                "dimension": self.dimension,
                "heads": self.heads,
                "feed_forward_dimension": self.feed_forward_dimension,
                "layers": self.layers,
            },
            "model_path": "",
            "prompt": prompt,
            "prompt_token_ids": prompt_token_ids,
            "max_tokens": max_tokens,
            "text": text,
            "finish_reason": finish_reason,
            "completion_tokens": completion_tokens,
            "generation_us": generation_us,
            "tokens_per_second": (
                completion_tokens / (generation_us / 1_000_000.0)
                if generation_us
                else 0.0
            ),
            "steps": steps,
        }


def read_values(source: BinaryIO, layout: str) -> tuple:
    size = struct.calcsize(f"<{layout}")
    data = source.read(size)
    if len(data) != size:
        raise ValueError("model ended before all values were read")
    return struct.unpack(f"<{layout}", data)


def read_string(source: BinaryIO) -> str:
    (length,) = read_values(source, "I")
    data = source.read(length)
    if len(data) != length:
        raise ValueError("model ended inside a vocabulary token")
    return data.decode("utf-8")


def read_tensor(source: BinaryIO, *shape: int) -> torch.Tensor:
    count = math.prod(shape)
    values = read_values(source, f"{count}f")
    return torch.tensor(values, dtype=torch.float32).reshape(shape)


def percentile(sorted_values: list[float], fraction: float) -> float:
    index = math.ceil((len(sorted_values) - 1) * fraction)
    return sorted_values[index]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--model",
        type=Path,
        default=Path("models/tiny-inferlab-v1.bin"),
    )
    parser.add_argument("--prompt", default="teach me streaming")
    parser.add_argument("--max-tokens", type=int, default=8)
    parser.add_argument("--repetitions", type=int, default=1)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.max_tokens <= 0 or args.repetitions <= 0:
        parser.error("max-tokens and repetitions must be positive")

    torch.set_num_threads(1)
    torch.use_deterministic_algorithms(True)
    model = TinyDecoder(args.model)
    generations = [
        model.generate(args.prompt, args.max_tokens)
        for _ in range(args.repetitions)
    ]
    durations = sorted(
        generation["generation_us"] for generation in generations
    )
    generation = generations[0]
    generation["model_path"] = str(args.model)
    output = {
        "implementation": "pytorch",
        "torch_version": torch.__version__,
        "repetitions": args.repetitions,
        "median_generation_us": percentile(durations, 0.50),
        "p95_generation_us": percentile(durations, 0.95),
        "generation": generation,
    }
    encoded = json.dumps(output, indent=2) + "\n"
    if args.output:
        args.output.write_text(encoded)
    else:
        print(encoded, end="")


if __name__ == "__main__":
    main()
