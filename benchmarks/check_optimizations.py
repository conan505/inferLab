#!/usr/bin/env python3
"""Check retained v0.11 quantization and speculative-decoding claims."""

from __future__ import annotations

import argparse
import json
import math
from pathlib import Path


def assertion(name: str, passed: bool, observed) -> dict:
    return {"name": name, "passed": bool(passed), "observed": observed}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--optimization-probe", type=Path, required=True)
    parser.add_argument("--gateway-probe", type=Path, required=True)
    parser.add_argument("--torch-parity", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    probe = json.loads(args.optimization_probe.read_text())
    gateway = json.loads(args.gateway_probe.read_text())
    torch = json.loads(args.torch_parity.read_text())
    quantization = {item["mode"]: item for item in probe["quantization"]}
    greedy = probe["greedy_speculation"]
    profiles = {
        (item["draft_quantization"], item["draft_tokens_per_cycle"]): item
        for item in greedy["profiles"]
    }
    sampled = {
        item["draft_quantization"]: item
        for item in probe["sampled_speculation"]
    }
    quality = {item["name"]: item for item in probe["draft_quality"]}
    baseline = gateway["baseline"]
    speculative = gateway["speculative"]
    streamed = gateway["stream"]
    int4 = gateway["int4"]
    int4_health = gateway["int4_health"]
    invalid = gateway["invalid_structured_speculation"]

    assertions = [
        assertion(
            "probe covers FP32, per-row INT8, and groupwise INT4",
            set(quantization) == {"fp32", "int8", "int4"},
            sorted(quantization),
        ),
        assertion(
            "all modes evaluate three prompts and twenty-four steps",
            all(item["prompts"] == 3 and item["steps"] == 24 for item in quantization.values()),
            {key: [value["prompts"], value["steps"]] for key, value in quantization.items()},
        ),
        assertion(
            "quantized modes preserve every greedy token",
            quantization["int8"]["greedy_token_mismatches"] == 0
            and quantization["int4"]["greedy_token_mismatches"] == 0,
            {key: value["greedy_token_mismatches"] for key, value in quantization.items()},
        ),
        assertion(
            "INT8 maximum logit error stays within two times ten to the minus four",
            quantization["int8"]["maximum_absolute_logit_error"] <= 2.0e-4,
            quantization["int8"]["maximum_absolute_logit_error"],
        ),
        assertion(
            "INT4 maximum logit error stays within four times ten to the minus three",
            quantization["int4"]["maximum_absolute_logit_error"] <= 4.0e-3,
            quantization["int4"]["maximum_absolute_logit_error"],
        ),
        assertion(
            "greedy-path perplexity changes by less than one ten-thousandth",
            all(
                abs(item["greedy_path_perplexity"] - quantization["fp32"]["greedy_path_perplexity"])
                < 1.0e-4
                for item in [quantization["int8"], quantization["int4"]]
            ),
            {key: value["greedy_path_perplexity"] for key, value in quantization.items()},
        ),
        assertion(
            "INT8 active tensor payload is exactly 7,056 bytes",
            quantization["int8"]["memory"]["active_tensor_bytes"] == 7_056,
            quantization["int8"]["memory"],
        ),
        assertion(
            "INT4 active tensor payload is exactly 6,820 bytes",
            quantization["int4"]["memory"]["active_tensor_bytes"] == 6_820,
            quantization["int4"]["memory"],
        ),
        assertion(
            "quantization reduces the 13,720-byte FP32 tensor payload",
            quantization["fp32"]["memory"]["active_tensor_bytes"] == 13_720
            and quantization["int8"]["memory"]["active_tensor_bytes"]
            < quantization["fp32"]["memory"]["active_tensor_bytes"]
            and quantization["int4"]["memory"]["active_tensor_bytes"]
            < quantization["int8"]["memory"]["active_tensor_bytes"],
            {key: value["memory"]["active_tensor_bytes"] for key, value in quantization.items()},
        ),
        assertion(
            "INT8 stores one scale per output row and no zero points",
            quantization["int8"]["memory"]["scale_count"] == 134
            and quantization["int8"]["memory"]["zero_point_count"] == 0,
            quantization["int8"]["memory"],
        ),
        assertion(
            "INT4 stores group-of-eight scales and zero points",
            quantization["int4"]["memory"]["group_size"] == 8
            and quantization["int4"]["memory"]["scale_count"] == 300
            and quantization["int4"]["memory"]["zero_point_count"] == 300,
            quantization["int4"]["memory"],
        ),
        assertion(
            "all quantization timings are finite observations",
            all(
                math.isfinite(item["median_generation_us"])
                and item["median_generation_us"] > 0
                and math.isfinite(item["p95_generation_us"])
                and item["p95_generation_us"] >= item["median_generation_us"]
                for item in quantization.values()
            ),
            {
                key: [value["median_generation_us"], value["p95_generation_us"]]
                for key, value in quantization.items()
            },
        ),
        assertion(
            "greedy speculation covers two drafts and windows one through three",
            set(profiles)
            == {
                ("int8", 1), ("int8", 2), ("int8", 3),
                ("int4", 1), ("int4", 2), ("int4", 3),
            },
            sorted(f"{mode}/{window}" for mode, window in profiles),
        ),
        assertion(
            "every greedy speculative profile exactly matches target output",
            all(item["output_matches_target"] for item in profiles.values()),
            {f"{key[0]}/{key[1]}": value["output_matches_target"] for key, value in profiles.items()},
        ),
        assertion(
            "matching drafts accept every proposed greedy token",
            all(
                item["proposed_tokens"] == item["accepted_tokens"]
                and item["acceptance_rate_percent"] == 100.0
                for item in profiles.values()
            ),
            {
                f"{key[0]}/{key[1]}": [value["accepted_tokens"], value["proposed_tokens"]]
                for key, value in profiles.items()
            },
        ),
        assertion(
            "draft windows one, two, and three reduce target calls to four, three, and two",
            all(
                profiles[(mode, window)]["target_forward_calls"] == calls
                for mode in ["int8", "int4"]
                for window, calls in [(1, 4), (2, 3), (3, 2)]
            ),
            {
                f"{key[0]}/{key[1]}": value["target_forward_calls"]
                for key, value in profiles.items()
            },
        ),
        assertion(
            "three-token drafts reduce target calls by seventy-five percent",
            profiles[("int8", 3)]["target_call_reduction_percent"] == 75.0
            and profiles[("int4", 3)]["target_call_reduction_percent"] == 75.0,
            {
                mode: profiles[(mode, 3)]["target_call_reduction_percent"]
                for mode in ["int8", "int4"]
            },
        ),
        assertion(
            "speculative wall time is measured rather than assumed",
            all(
                math.isfinite(item["wall_time_speedup"])
                and item["wall_time_speedup"] > 0
                for item in profiles.values()
            ),
            {f"{key[0]}/{key[1]}": value["wall_time_speedup"] for key, value in profiles.items()},
        ),
        assertion(
            "two real quantized drafts each run ten thousand sampled requests",
            set(sampled) == {"int8", "int4"}
            and all(item["samples"] == 10_000 for item in sampled.values()),
            {key: value["samples"] for key, value in sampled.items()},
        ),
        assertion(
            "real speculative first-token distributions stay within one percentage point of target",
            all(item["speculative_maximum_probability_error"] < 0.01 for item in sampled.values()),
            {key: value["speculative_maximum_probability_error"] for key, value in sampled.items()},
        ),
        assertion(
            "target-only and speculative observations differ by less than one percentage point",
            all(item["target_vs_speculative_maximum_error"] < 0.01 for item in sampled.values()),
            {key: value["target_vs_speculative_maximum_error"] for key, value in sampled.items()},
        ),
        assertion(
            "all sampled speculative replay checks match",
            all(item["replay_matches"] == item["replay_checks"] == 4 for item in sampled.values()),
            {key: [value["replay_matches"], value["replay_checks"]] for key, value in sampled.items()},
        ),
        assertion(
            "synthetic quality sweep covers identical, softened, and reversed drafts",
            set(quality) == {"identical", "softened", "reversed"},
            sorted(quality),
        ),
        assertion(
            "rejection correction preserves the target distribution for every draft quality",
            all(item["maximum_target_probability_error"] < 0.01 for item in quality.values()),
            {key: value["maximum_target_probability_error"] for key, value in quality.items()},
        ),
        assertion(
            "acceptance falls as synthetic draft quality worsens",
            quality["identical"]["acceptance_rate_percent"] == 100.0
            and quality["identical"]["acceptance_rate_percent"]
            > quality["softened"]["acceptance_rate_percent"]
            > quality["reversed"]["acceptance_rate_percent"],
            {key: value["acceptance_rate_percent"] for key, value in quality.items()},
        ),
        assertion(
            "poor synthetic draft exercises thousands of rejection corrections",
            quality["reversed"]["rejected"] > 5_000,
            quality["reversed"],
        ),
        assertion(
            "FP32 target path remains within PyTorch tolerance",
            torch["passed"] and torch["max_abs_logit_error"] <= 1.0e-4,
            torch["max_abs_logit_error"],
        ),
        assertion(
            "gateway greedy baseline and speculation return the same completion",
            baseline["status"] == 200
            and speculative["status"] == 200
            and gateway["same_output"]
            and baseline["finish_reason"] == speculative["finish_reason"] == "stop",
            {"baseline": baseline["content"], "speculative": speculative["content"]},
        ),
        assertion(
            "gateway exposes two target calls and full draft acceptance",
            speculative["metrics"]["speculation"]["target_forward_calls"] == 2
            and speculative["metrics"]["speculation"]["accepted_tokens"] == 6
            and speculative["metrics"]["speculation"]["acceptance_rate_percent"] == 100.0,
            speculative["metrics"]["speculation"],
        ),
        assertion(
            "gateway sampled speculation replays the same seed",
            gateway["sample_replay"]["matches"],
            gateway["sample_replay"],
        ),
        assertion(
            "gateway SSE reconstructs the speculative completion",
            streamed["status"] == 200
            and streamed["done_received"]
            and streamed["finish_reason"] == "stop"
            and streamed["content"] == speculative["content"]
            and streamed["metrics"]["speculation"]["enabled"],
            streamed,
        ),
        assertion(
            "structured speculation fails before streaming",
            invalid["status"] == 400
            and invalid["body"]["error"]["type"] == "invalid_generation_request"
            and "text response_format only" in invalid["body"]["error"]["message"],
            invalid,
        ),
        assertion(
            "HTTP INT4 worker exposes quantized mode and preserves greedy output",
            int4["status"] == 200
            and int4["content"] == baseline["content"]
            and int4_health["model"]["dtype"] == "uint4-groupwise"
            and int4_health["model"]["quantization"]["mode"] == "int4",
            {"content": int4["content"], "model": int4_health["model"]},
        ),
    ]
    passed_count = sum(item["passed"] for item in assertions)
    best_profile = max(greedy["profiles"], key=lambda item: item["wall_time_speedup"])
    result = {
        "passed": passed_count == len(assertions),
        "assertions_passed": passed_count,
        "assertions_total": len(assertions),
        "int8_tensor_bytes": quantization["int8"]["memory"]["active_tensor_bytes"],
        "int4_tensor_bytes": quantization["int4"]["memory"]["active_tensor_bytes"],
        "int8_maximum_logit_error": quantization["int8"]["maximum_absolute_logit_error"],
        "int4_maximum_logit_error": quantization["int4"]["maximum_absolute_logit_error"],
        "three_token_target_call_reduction_percent": profiles[("int8", 3)]["target_call_reduction_percent"],
        "best_observed_speculative_wall_time_speedup": best_profile["wall_time_speedup"],
        "maximum_speculative_probability_error": max(
            item["speculative_maximum_probability_error"] for item in sampled.values()
        ),
        "synthetic_acceptance_rates": {
            key: value["acceptance_rate_percent"] for key, value in quality.items()
        },
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
