#!/usr/bin/env python3
"""Capture hardware and toolchain boundaries for retained attention evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import platform
import shutil
import subprocess
import sys
from pathlib import Path

import torch


def first_line(command: list[str]) -> str:
    completed = subprocess.run(command, check=True, capture_output=True, text=True)
    return completed.stdout.splitlines()[0]


def apple_accelerators() -> list[dict]:
    if platform.system() != "Darwin" or shutil.which("system_profiler") is None:
        return []
    completed = subprocess.run(
        ["system_profiler", "SPDisplaysDataType", "-json"],
        check=True,
        capture_output=True,
        text=True,
    )
    displays = json.loads(completed.stdout).get("SPDisplaysDataType", [])
    return [
        {
            "name": item.get("sppci_model", item.get("_name", "unknown")),
            "gpu_cores": int(item["sppci_cores"])
            if str(item.get("sppci_cores", "")).isdigit()
            else None,
            "metal_family": item.get("spdisplays_mtlgpufamilysupport"),
            "vendor": item.get("spdisplays_vendor"),
        }
        for item in displays
    ]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--benchmark-note",
        default=(
            "The v0.12 kernel runs on the host CPU. External traffic is an "
            "algorithmic byte model, not a hardware performance-counter reading. "
            "Wall time measures this scalar teaching implementation only."
        ),
    )
    parser.add_argument(
        "--milestone-boundary",
        default=(
            "CUDA is retained for v1.0 because this host has no NVIDIA CUDA "
            "toolchain or device; v0.12 validates tiling and online softmax on CPU."
        ),
    )
    args = parser.parse_args()
    nvcc = shutil.which("nvcc")
    data = {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "processor": platform.processor(),
        "python": sys.version,
        "pytorch": torch.__version__,
        "torch_threads": torch.get_num_threads(),
        "compiler": first_line(["clang++", "--version"]),
        "accelerators": apple_accelerators(),
        "cuda": {
            "nvcc_path": nvcc,
            "toolchain_available": nvcc is not None,
            "pytorch_available": torch.cuda.is_available(),
            "device_count": torch.cuda.device_count(),
        },
        "metal": {
            "pytorch_mps_built": torch.backends.mps.is_built(),
            "pytorch_mps_available": torch.backends.mps.is_available(),
        },
        "model_path": str(args.model),
        "model_sha256": hashlib.sha256(args.model.read_bytes()).hexdigest(),
        "model_bytes": args.model.stat().st_size,
        "benchmark_note": args.benchmark_note,
        "milestone_boundary": args.milestone_boundary,
    }
    args.output.write_text(json.dumps(data, indent=2) + "\n")


if __name__ == "__main__":
    main()
