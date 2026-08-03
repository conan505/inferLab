#!/usr/bin/env python3
"""Check the retained v0.10 sampling and structured-decoding claims."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path


def assertion(name: str, passed: bool, observed) -> dict:
    return {"name": name, "passed": bool(passed), "observed": observed}


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def valid_summary(value: dict) -> bool:
    return (
        set(value) == {"answer", "confidence"}
        and value["answer"] in {"InferLab", "systems", "tokens"}
        and value["confidence"] in {"high", "medium", "low"}
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--decoding-probe", type=Path, required=True)
    parser.add_argument("--vocabulary-parity", type=Path, required=True)
    parser.add_argument("--torch-parity", type=Path, required=True)
    parser.add_argument("--gateway-probe", type=Path, required=True)
    parser.add_argument("--v1-model", type=Path, required=True)
    parser.add_argument("--v1-metadata", type=Path, required=True)
    parser.add_argument("--v2-model", type=Path, required=True)
    parser.add_argument("--v2-metadata", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    probe = json.loads(args.decoding_probe.read_text())
    vocabulary = json.loads(args.vocabulary_parity.read_text())
    torch = json.loads(args.torch_parity.read_text())
    gateway = json.loads(args.gateway_probe.read_text())
    v1_metadata = json.loads(args.v1_metadata.read_text())
    v2_metadata = json.loads(args.v2_metadata.read_text())
    cases = {case["name"]: case for case in probe["processor_cases"]}
    distributions = probe["temperature_distributions"]
    structured = probe["structured"]
    streamed = gateway["stream"]
    non_stream = gateway["non_stream"]
    invalid = gateway["invalid_schema"]
    impossible_bans = gateway["impossible_bans"]

    assertions = [
        assertion(
            "six processor golden cases are retained",
            set(cases)
            == {
                "greedy",
                "token-ban",
                "repetition-penalty",
                "top-k",
                "top-p",
                "grammar-mask",
            },
            sorted(cases),
        ),
        assertion(
            "greedy selects the maximum logit",
            cases["greedy"]["selection"]["token_id"] == 1,
            cases["greedy"]["selection"],
        ),
        assertion(
            "a token ban removes the previous winner",
            cases["token-ban"]["selection"]["token_id"] == 2
            and cases["token-ban"]["selection"]["candidate_count"] == 3,
            cases["token-ban"]["selection"],
        ),
        assertion(
            "repetition penalty changes the winning token",
            cases["repetition-penalty"]["selection"]["token_id"] == 2,
            cases["repetition-penalty"]["selection"],
        ),
        assertion(
            "top-k retains exactly two candidates",
            cases["top-k"]["selection"]["candidate_count"] == 2,
            cases["top-k"]["selection"],
        ),
        assertion(
            "top-p retains the smallest probability prefix",
            cases["top-p"]["selection"]["candidate_count"] == 1,
            cases["top-p"]["selection"],
        ),
        assertion(
            "grammar mask selects only from its explicit support",
            cases["grammar-mask"]["selection"]["token_id"] == 3
            and cases["grammar-mask"]["selection"]["candidate_count"] == 2,
            cases["grammar-mask"]["selection"],
        ),
        assertion(
            "three temperature distributions use ten thousand samples each",
            [item["temperature"] for item in distributions] == [0.5, 1.0, 2.0]
            and all(item["samples"] == 10_000 for item in distributions),
            [item["samples"] for item in distributions],
        ),
        assertion(
            "observed categorical probabilities stay within one percentage point",
            all(
                item["maximum_absolute_probability_error"] < 0.01
                for item in distributions
            ),
            [item["maximum_absolute_probability_error"] for item in distributions],
        ),
        assertion(
            "every seeded distribution sequence replays exactly",
            all(item["replay_sequence_matches"] for item in distributions),
            [item["replay_sequence_matches"] for item in distributions],
        ),
        assertion(
            "temperature flattens the retained distribution",
            distributions[0]["observed_probability"][2]
            > distributions[1]["observed_probability"][2]
            > distributions[2]["observed_probability"][2],
            [item["observed_probability"] for item in distributions],
        ),
        assertion(
            "all ten thousand structured outputs parse as JSON",
            structured["parser_valid"] == structured["samples"] == 10_000,
            {
                "valid": structured["parser_valid"],
                "samples": structured["samples"],
            },
        ),
        assertion(
            "all ten thousand outputs satisfy the exact schema",
            structured["schema_valid"] == structured["samples"],
            structured["schema_valid"],
        ),
        assertion(
            "every structured generation reaches EOS",
            structured["stop_finished"] == structured["samples"],
            structured["stop_finished"],
        ),
        assertion(
            "four retained structured seeds replay exactly",
            structured["replay_matches"] == structured["replay_checks"] == 4,
            {
                "matches": structured["replay_matches"],
                "checks": structured["replay_checks"],
            },
        ),
        assertion(
            "sampling reaches multiple valid structured outputs",
            structured["distinct_outputs"] >= 3,
            structured["combination_counts"],
        ),
        assertion(
            "all three confidence enum values are sampled",
            set(structured["confidence_counts"]) == {"high", "medium", "low"},
            structured["confidence_counts"],
        ),
        assertion(
            "DFA metrics expose six constrained steps and ten total candidates",
            structured["first_metrics"]["decoding"]["grammar_constrained_steps"] == 6
            and structured["first_metrics"]["decoding"]["candidate_tokens_total"]
            == 10
            and structured["first_metrics"]["decoding"]["masked_tokens_total"]
            == 122,
            structured["first_metrics"]["decoding"],
        ),
        assertion(
            "v2 appends six tokens without changing v1 greedy behavior",
            vocabulary["passed"]
            and vocabulary["v1_vocabulary"] == 16
            and vocabulary["v2_vocabulary"] == 22
            and vocabulary["maximum_old_logit_error"] == 0,
            vocabulary,
        ),
        assertion(
            "v2 greedy logits remain within tolerance of PyTorch",
            torch["passed"] and torch["max_abs_logit_error"] <= 1.0e-4,
            torch["max_abs_logit_error"],
        ),
        assertion(
            "both committed checkpoints match their metadata hashes",
            digest(args.v1_model) == v1_metadata["sha256"]
            and digest(args.v2_model) == v2_metadata["sha256"],
            {
                "v1": digest(args.v1_model),
                "v2": digest(args.v2_model),
            },
        ),
        assertion(
            "gateway non-stream response is schema-valid",
            non_stream["status"] == 200
            and valid_summary(non_stream["parsed"])
            and non_stream["finish_reason"] == "stop",
            non_stream,
        ),
        assertion(
            "same-seed gateway requests replay exactly",
            gateway["same_seed_replay_matches"]
            and gateway["non_stream"]["content"] == gateway["replay"]["content"],
            {
                "first": gateway["non_stream"]["content"],
                "replay": gateway["replay"]["content"],
            },
        ),
        assertion(
            "gateway SSE pieces reconstruct one schema-valid object",
            streamed["status"] == 200
            and streamed["done_received"]
            and streamed["finish_reason"] == "stop"
            and len(streamed["pieces"]) == 5
            and valid_summary(streamed["parsed"]),
            streamed,
        ),
        assertion(
            "gateway exposes six grammar-constrained sampling steps",
            streamed["metrics"]["decoding"]["kind"] == "json_schema"
            and streamed["metrics"]["decoding"]["sampled_steps"] == 6
            and streamed["metrics"]["decoding"]["grammar_constrained_steps"] == 6,
            streamed["metrics"]["decoding"],
        ),
        assertion(
            "unsupported schema shape fails before streaming",
            invalid["status"] == 400
            and invalid["body"]["error"]["type"] == "invalid_generation_request"
            and "additionalProperties=false"
            in invalid["body"]["error"]["message"],
            invalid,
        ),
        assertion(
            "grammar-exhausting token bans fail before streaming",
            impossible_bans["status"] == 400
            and impossible_bans["body"]["error"]["type"]
            == "invalid_generation_request"
            and "remove every legal JSON token in grammar state 1"
            in impossible_bans["body"]["error"]["message"],
            impossible_bans,
        ),
    ]
    passed_count = sum(item["passed"] for item in assertions)
    result = {
        "passed": passed_count == len(assertions),
        "assertions_passed": passed_count,
        "assertions_total": len(assertions),
        "maximum_probability_error": max(
            item["maximum_absolute_probability_error"] for item in distributions
        ),
        "structured_valid": structured["schema_valid"],
        "structured_samples": structured["samples"],
        "structured_distinct_outputs": structured["distinct_outputs"],
        "maximum_cpp_vs_torch_logit_error": torch["max_abs_logit_error"],
        "v1_v2_old_logit_error": vocabulary["maximum_old_logit_error"],
        "assertions": assertions,
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    for item in assertions:
        print(f"{'PASS' if item['passed'] else 'FAIL'} {item['name']}")
    print(f"{passed_count}/{len(assertions)} assertions passed")
    if not result["passed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
