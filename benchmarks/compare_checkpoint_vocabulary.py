#!/usr/bin/env python3
"""Prove the v2 append-only vocabulary preserves the v1 greedy path."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--v1", type=Path, required=True)
    parser.add_argument("--v2", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    v1 = json.loads(args.v1.read_text())["generation"]
    v2 = json.loads(args.v2.read_text())["generation"]
    old_vocabulary = v1["model"]["vocabulary"]
    per_step_error = []
    appended_logits = []
    for before, after in zip(v1["steps"], v2["steps"]):
        errors = [
            abs(left - right)
            for left, right in zip(
                before["logits"], after["logits"][:old_vocabulary]
            )
        ]
        per_step_error.append(max(errors, default=0.0))
        appended_logits.append(after["logits"][old_vocabulary:])
    result = {
        "passed": (
            v1["text"] == v2["text"]
            and v1["finish_reason"] == v2["finish_reason"]
            and [step["token_id"] for step in v1["steps"]]
            == [step["token_id"] for step in v2["steps"]]
            and max(per_step_error, default=float("inf")) == 0
            and v2["model"]["vocabulary"] > old_vocabulary
        ),
        "v1_vocabulary": old_vocabulary,
        "v2_vocabulary": v2["model"]["vocabulary"],
        "appended_tokens": v2["model"]["vocabulary"] - old_vocabulary,
        "text_match": v1["text"] == v2["text"],
        "finish_reason_match": v1["finish_reason"] == v2["finish_reason"],
        "token_ids_match": [step["token_id"] for step in v1["steps"]]
        == [step["token_id"] for step in v2["steps"]],
        "maximum_old_logit_error": max(per_step_error, default=float("inf")),
        "appended_logits": appended_logits,
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(
        f"{'PASS' if result['passed'] else 'FAIL'} "
        f"vocabulary={old_vocabulary}->{result['v2_vocabulary']} "
        f"old_logit_error={result['maximum_old_logit_error']:.9g}"
    )
    if not result["passed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
