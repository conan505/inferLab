#!/usr/bin/env python3
"""Compare full-prefix recomputation with the v0.8 KV-cached decoder."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


def reduction(before: int, after: int) -> float:
    if before == 0:
        return 0.0
    return (before - after) / before * 100.0


def compare(recompute: dict, cached: dict, tolerance: float) -> dict:
    before = recompute["generation"]
    after = cached["generation"]
    before_steps = before["steps"]
    after_steps = after["steps"]
    paired = min(len(before_steps), len(after_steps))
    per_step = []
    errors = []
    for index in range(paired):
        before_logits = before_steps[index]["logits"]
        after_logits = after_steps[index]["logits"]
        step_errors = [
            abs(left - right)
            for left, right in zip(before_logits, after_logits)
        ]
        errors.extend(step_errors)
        per_step.append(
            {
                "index": index,
                "recompute_token_id": before_steps[index]["token_id"],
                "cached_token_id": after_steps[index]["token_id"],
                "token_match": (
                    before_steps[index]["token_id"]
                    == after_steps[index]["token_id"]
                ),
                "max_abs_logit_error": max(step_errors, default=0.0),
            }
        )

    before_metrics = before["metrics"]
    after_metrics = after["metrics"]
    token_ids_match = [step["token_id"] for step in before_steps] == [
        step["token_id"] for step in after_steps
    ]
    max_error = max(errors, default=float("inf"))
    passed = all(
        [
            len(before_steps) == len(after_steps),
            token_ids_match,
            before["prompt_token_ids"] == after["prompt_token_ids"],
            before["text"] == after["text"],
            before["finish_reason"] == after["finish_reason"],
            before_metrics["mode"] == "recompute",
            after_metrics["mode"] == "kv-cache",
            max_error <= tolerance,
        ]
    )
    return {
        "passed": passed,
        "prompt": before["prompt"],
        "tolerance": tolerance,
        "steps_compared": paired,
        "max_abs_logit_error": max_error,
        "mean_abs_logit_error": (
            sum(errors) / len(errors) if errors else float("inf")
        ),
        "token_ids_match": token_ids_match,
        "prompt_tokens_match": (
            before["prompt_token_ids"] == after["prompt_token_ids"]
        ),
        "text_match": before["text"] == after["text"],
        "finish_reason_match": (
            before["finish_reason"] == after["finish_reason"]
        ),
        "generated_text": after["text"],
        "recompute_metrics": before_metrics,
        "cached_metrics": after_metrics,
        "work_reduction_percent": {
            "query_tokens": reduction(
                before_metrics["query_tokens"],
                after_metrics["query_tokens"],
            ),
            "kv_tokens": reduction(
                before_metrics["kv_tokens"],
                after_metrics["kv_tokens"],
            ),
            "attention_score_elements": reduction(
                before_metrics["attention_score_elements"],
                after_metrics["attention_score_elements"],
            ),
        },
        "timing": {
            "recompute_median_generation_us": recompute[
                "median_generation_us"
            ],
            "cached_median_generation_us": cached["median_generation_us"],
            "recompute_p95_generation_us": recompute["p95_generation_us"],
            "cached_p95_generation_us": cached["p95_generation_us"],
            "note": (
                "Tiny-model wall time is retained as an observation; the "
                "deterministic work counters are the optimization proof."
            ),
        },
        "per_step": per_step,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--recompute", type=Path, required=True)
    parser.add_argument("--cached", type=Path, required=True)
    parser.add_argument("--tolerance", type=float, default=1.0e-6)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    result = compare(
        json.loads(args.recompute.read_text()),
        json.loads(args.cached.read_text()),
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
