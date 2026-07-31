#!/usr/bin/env python3
"""Capture the local environment for retained CPU-decoder evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import platform
import subprocess
import sys
from pathlib import Path

import torch


def first_line(command: list[str]) -> str:
    completed = subprocess.run(
        command,
        check=True,
        capture_output=True,
        text=True,
    )
    return completed.stdout.splitlines()[0]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    data = {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "processor": platform.processor(),
        "python": sys.version,
        "pytorch": torch.__version__,
        "torch_threads": torch.get_num_threads(),
        "compiler": first_line(["clang++", "--version"]),
        "model_path": str(args.model),
        "model_sha256": hashlib.sha256(args.model.read_bytes()).hexdigest(),
        "model_bytes": args.model.stat().st_size,
        "benchmark_note": (
            "Single-process warm repetitions; tiny-model latency measures "
            "reference overhead and is not representative of production LLMs."
        ),
    }
    args.output.write_text(json.dumps(data, indent=2) + "\n")


if __name__ == "__main__":
    main()
