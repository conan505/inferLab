#!/usr/bin/env python3
"""Compare C++ decoder traces with the independent PyTorch oracle."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


def compare(cpp: dict, torch: dict, tolerance: float) -> dict:
    cpp_generation = cpp["generation"]
    torch_generation = torch["generation"]
    cpp_steps = cpp_generation["steps"]
    torch_steps = torch_generation["steps"]
    paired_steps = min(len(cpp_steps), len(torch_steps))
    per_step = []
    all_errors = []
    for index in range(paired_steps):
        cpp_logits = cpp_steps[index]["logits"]
        torch_logits = torch_steps[index]["logits"]
        errors = [
            abs(cpp_value - torch_value)
            for cpp_value, torch_value in zip(cpp_logits, torch_logits)
        ]
        all_errors.extend(errors)
        per_step.append(
            {
                "index": index,
                "cpp_token_id": cpp_steps[index]["token_id"],
                "torch_token_id": torch_steps[index]["token_id"],
                "token_match": (
                    cpp_steps[index]["token_id"]
                    == torch_steps[index]["token_id"]
                ),
                "max_abs_logit_error": max(errors, default=0.0),
                "mean_abs_logit_error": (
                    sum(errors) / len(errors) if errors else 0.0
                ),
            }
        )

    token_ids_match = [
        step["token_id"] for step in cpp_steps
    ] == [step["token_id"] for step in torch_steps]
    prompt_tokens_match = (
        cpp_generation["prompt_token_ids"]
        == torch_generation["prompt_token_ids"]
    )
    text_match = cpp_generation["text"] == torch_generation["text"]
    finish_reason_match = (
        cpp_generation["finish_reason"]
        == torch_generation["finish_reason"]
    )
    max_error = max(all_errors, default=float("inf"))
    mean_error = (
        sum(all_errors) / len(all_errors)
        if all_errors
        else float("inf")
    )
    passed = all(
        [
            len(cpp_steps) == len(torch_steps),
            token_ids_match,
            prompt_tokens_match,
            text_match,
            finish_reason_match,
            max_error <= tolerance,
        ]
    )
    return {
        "passed": passed,
        "prompt": cpp_generation["prompt"],
        "tolerance": tolerance,
        "max_abs_logit_error": max_error,
        "mean_abs_logit_error": mean_error,
        "steps_compared": paired_steps,
        "token_ids_match": token_ids_match,
        "prompt_tokens_match": prompt_tokens_match,
        "text_match": text_match,
        "finish_reason_match": finish_reason_match,
        "generated_text": cpp_generation["text"],
        "cpp_median_generation_us": cpp["median_generation_us"],
        "torch_median_generation_us": torch["median_generation_us"],
        "cpp_p95_generation_us": cpp["p95_generation_us"],
        "torch_p95_generation_us": torch["p95_generation_us"],
        "per_step": per_step,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cpp", type=Path, required=True)
    parser.add_argument("--torch", type=Path, required=True)
    parser.add_argument("--tolerance", type=float, default=1.0e-4)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    result = compare(
        json.loads(args.cpp.read_text()),
        json.loads(args.torch.read_text()),
        args.tolerance,
    )
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(
        f"{'PASS' if result['passed'] else 'FAIL'} "
        f"prompt={result['prompt']!r} "
        f"tokens={result['steps_compared']} "
        f"max_abs_error={result['max_abs_logit_error']:.9g}"
    )
    if not result["passed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
