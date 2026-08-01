#!/usr/bin/env python3
"""Compare contiguous and paged KV-cache decoder traces."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


def compare(contiguous: dict, paged: dict, tolerance: float) -> dict:
    left = contiguous["generation"]
    right = paged["generation"]
    paired = min(len(left["steps"]), len(right["steps"]))
    errors = []
    per_step = []
    for index in range(paired):
        left_step = left["steps"][index]
        right_step = right["steps"][index]
        step_errors = [
            abs(before - after)
            for before, after in zip(left_step["logits"], right_step["logits"])
        ]
        errors.extend(step_errors)
        per_step.append(
            {
                "index": index,
                "contiguous_token_id": left_step["token_id"],
                "paged_token_id": right_step["token_id"],
                "token_match": left_step["token_id"] == right_step["token_id"],
                "max_abs_logit_error": max(step_errors, default=0.0),
            }
        )
    token_ids_match = [step["token_id"] for step in left["steps"]] == [
        step["token_id"] for step in right["steps"]
    ]
    max_error = max(errors, default=float("inf"))
    passed = all(
        [
            len(left["steps"]) == len(right["steps"]),
            token_ids_match,
            left["prompt_token_ids"] == right["prompt_token_ids"],
            left["text"] == right["text"],
            left["finish_reason"] == right["finish_reason"],
            left["metrics"]["mode"] == "kv-cache",
            right["metrics"]["mode"] == "paged-kv-cache",
            left["metrics"]["query_tokens"] == right["metrics"]["query_tokens"],
            left["metrics"]["kv_tokens"] == right["metrics"]["kv_tokens"],
            left["metrics"]["attention_score_elements"]
            == right["metrics"]["attention_score_elements"],
            max_error <= tolerance,
        ]
    )
    return {
        "passed": passed,
        "prompt": left["prompt"],
        "tolerance": tolerance,
        "steps_compared": paired,
        "max_abs_logit_error": max_error,
        "mean_abs_logit_error": sum(errors) / len(errors) if errors else float("inf"),
        "token_ids_match": token_ids_match,
        "prompt_tokens_match": left["prompt_token_ids"] == right["prompt_token_ids"],
        "text_match": left["text"] == right["text"],
        "finish_reason_match": left["finish_reason"] == right["finish_reason"],
        "generated_text": right["text"],
        "contiguous_metrics": left["metrics"],
        "paged_metrics": right["metrics"],
        "paged_pool": paged["paged_cache"],
        "timing": {
            "contiguous_median_generation_us": contiguous["median_generation_us"],
            "paged_median_generation_us": paged["median_generation_us"],
            "contiguous_p95_generation_us": contiguous["p95_generation_us"],
            "paged_p95_generation_us": paged["p95_generation_us"],
            "note": (
                "The v0.9 paged path materializes rows for the unchanged attention "
                "oracle; timing is descriptive, while parity and allocator counters "
                "are acceptance evidence."
            ),
        },
        "per_step": per_step,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--contiguous", type=Path, required=True)
    parser.add_argument("--paged", type=Path, required=True)
    parser.add_argument("--tolerance", type=float, default=1.0e-6)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    result = compare(
        json.loads(args.contiguous.read_text()),
        json.loads(args.paged.read_text()),
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
