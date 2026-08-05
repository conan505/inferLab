#!/usr/bin/env python3
"""Check retained v0.12 tiled online-softmax attention claims."""

from __future__ import annotations

import argparse
import json
import math
from pathlib import Path


def assertion(name: str, passed: bool, observed) -> dict:
    return {"name": name, "passed": bool(passed), "observed": observed}


def maximum_error(left: list[float], right: list[float]) -> float:
    return max(abs(left_value - right_value) for left_value, right_value in zip(left, right))


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--attention-probe", type=Path, required=True)
    parser.add_argument("--torch-attention", type=Path, required=True)
    parser.add_argument("--gateway-probe", type=Path, required=True)
    parser.add_argument("--materialized-model", type=Path, required=True)
    parser.add_argument("--online-model", type=Path, required=True)
    parser.add_argument("--torch-parity", type=Path, required=True)
    parser.add_argument("--environment", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    probe = json.loads(args.attention_probe.read_text())
    torch_attention = json.loads(args.torch_attention.read_text())
    gateway = json.loads(args.gateway_probe.read_text())
    materialized_model = json.loads(args.materialized_model.read_text())["generation"]
    online_model = json.loads(args.online_model.read_text())["generation"]
    torch_parity = json.loads(args.torch_parity.read_text())
    environment = json.loads(args.environment.read_text())

    variants = {
        (item["algorithm"], item["precision"]): item
        for item in probe["fixture"]["variants"]
    }
    references = {
        item["precision"]: item["output"]
        for item in torch_attention["references"]
    }
    oracle_errors = {
        f"{algorithm}/{precision}": maximum_error(item["output"], references[precision])
        for (algorithm, precision), item in variants.items()
    }
    algorithm_errors = {
        precision: maximum_error(
            variants[("materialized", precision)]["output"],
            variants[("online-tiled", precision)]["output"],
        )
        for precision in ["fp32", "fp16", "bf16"]
    }
    scaling = {item["tokens"]: item for item in probe["sequence_scaling"]}

    def profiles(tokens: int) -> dict:
        return {
            item["algorithm"]: item for item in scaling[tokens]["profiles"]
        }

    scaling_profiles = {tokens: profiles(tokens) for tokens in scaling}
    materialized_256 = scaling_profiles[256]["materialized"]
    online_256 = scaling_profiles[256]["online-tiled"]
    health_materialized = gateway["materialized_health"]["model"]["attention"]
    health_online = gateway["online_health"]["model"]["attention"]
    cuda = environment["cuda"]
    materialized_steps = materialized_model["steps"]
    online_steps = online_model["steps"]
    full_model_logit_error = max(
        maximum_error(left["logits"], right["logits"])
        for left, right in zip(materialized_steps, online_steps)
    )

    expected_variants = {
        (algorithm, precision)
        for algorithm in ["materialized", "online-tiled"]
        for precision in ["fp32", "fp16", "bf16"]
    }
    assertions = [
        assertion(
            "fixture covers two algorithms and three storage precisions",
            set(variants) == expected_variants,
            sorted(f"{algorithm}/{precision}" for algorithm, precision in variants),
        ),
        assertion(
            "every C++ attention output matches the independent PyTorch oracle",
            all(error <= 2.0e-6 for error in oracle_errors.values()),
            oracle_errors,
        ),
        assertion(
            "online and materialized algorithms agree at every storage precision",
            all(error <= 2.0e-6 for error in algorithm_errors.values()),
            algorithm_errors,
        ),
        assertion(
            "FP16 storage drift from FP32 remains below five ten-thousandths",
            variants[("online-tiled", "fp16")][
                "maximum_absolute_error_to_materialized_fp32"
            ]
            <= 5.0e-4,
            variants[("online-tiled", "fp16")][
                "maximum_absolute_error_to_materialized_fp32"
            ],
        ),
        assertion(
            "BF16 storage drift from FP32 remains below three thousandths",
            variants[("online-tiled", "bf16")][
                "maximum_absolute_error_to_materialized_fp32"
            ]
            <= 3.0e-3,
            variants[("online-tiled", "bf16")][
                "maximum_absolute_error_to_materialized_fp32"
            ],
        ),
        assertion(
            "future key and value changes cannot affect a causal query",
            probe["causal_isolation"]["materialized_maximum_output_change"] == 0
            and probe["causal_isolation"]["online_tiled_maximum_output_change"] == 0,
            probe["causal_isolation"],
        ),
        assertion(
            "stable softmax keeps large-score outputs finite in all variants",
            all(item["all_outputs_finite"] for item in probe["large_score_stability"]),
            probe["large_score_stability"],
        ),
        assertion(
            "large-score online and materialized results remain within two millionths",
            all(
                item["maximum_absolute_algorithm_difference"] <= 2.0e-6
                for item in probe["large_score_stability"]
            ),
            {
                item["precision"]: item["maximum_absolute_algorithm_difference"]
                for item in probe["large_score_stability"]
            },
        ),
        assertion(
            "scaling sweep covers 32 through 256 tokens",
            set(scaling) == {32, 64, 128, 256}
            and probe["benchmark_tile_tokens"] == 32,
            {
                "tokens": sorted(scaling),
                "tile_tokens": probe["benchmark_tile_tokens"],
            },
        ),
        assertion(
            "causal score counts follow heads times the triangular token count",
            all(
                all(
                    profile["stats"]["score_elements"]
                    == item["heads"] * tokens * (tokens + 1) // 2
                    for profile in item["profiles"]
                )
                for tokens, item in scaling.items()
            ),
            {
                tokens: [profile["stats"]["score_elements"] for profile in item["profiles"]]
                for tokens, item in scaling.items()
            },
        ),
        assertion(
            "online scratch stays constant while the materialized score matrix grows quadratically",
            all(
                scaling_profiles[tokens]["online-tiled"]["stats"]["score_buffer_bytes"]
                == 128
                for tokens in scaling
            )
            and materialized_256["stats"]["score_buffer_bytes"] == 1_048_576
            and online_256["stats"]["score_buffer_bytes"] == 128,
            {
                tokens: {
                    algorithm: profile["stats"]["score_buffer_bytes"]
                    for algorithm, profile in scaling_profiles[tokens].items()
                }
                for tokens in scaling
            },
        ),
        assertion(
            "256-token online score scratch is eight-thousand-one-hundred-ninety-two times smaller",
            materialized_256["stats"]["score_buffer_bytes"]
            // online_256["stats"]["score_buffer_bytes"]
            == 8_192,
            materialized_256["stats"]["score_buffer_bytes"]
            / online_256["stats"]["score_buffer_bytes"],
        ),
        assertion(
            "modeled external traffic is halved at every measured sequence length",
            all(
                scaling_profiles[tokens]["materialized"]["stats"][
                    "modeled_external_total_bytes"
                ]
                == 2
                * scaling_profiles[tokens]["online-tiled"]["stats"][
                    "modeled_external_total_bytes"
                ]
                for tokens in scaling
            ),
            {
                tokens: {
                    algorithm: profile["stats"]["modeled_external_total_bytes"]
                    for algorithm, profile in scaling_profiles[tokens].items()
                }
                for tokens in scaling
            },
        ),
        assertion(
            "all wall-time observations are finite and ordered",
            all(
                math.isfinite(profile["median_us"])
                and profile["median_us"] > 0
                and math.isfinite(profile["p95_us"])
                and profile["p95_us"] >= profile["median_us"]
                for item in scaling.values()
                for profile in item["profiles"]
            ),
            {
                tokens: {
                    algorithm: [profile["median_us"], profile["p95_us"]]
                    for algorithm, profile in scaling_profiles[tokens].items()
                }
                for tokens in scaling
            },
        ),
        assertion(
            "full-model CLI token IDs match and logits remain within two millionths",
            [step["token_id"] for step in materialized_steps]
            == [step["token_id"] for step in online_steps]
            and materialized_model["text"] == online_model["text"]
            and full_model_logit_error <= 2.0e-6,
            {
                "materialized_token_ids": [step["token_id"] for step in materialized_steps],
                "online_token_ids": [step["token_id"] for step in online_steps],
                "maximum_logit_error": full_model_logit_error,
            },
        ),
        assertion(
            "full-model materialized and online workers return the same greedy completion",
            gateway["same_direct_output"]
            and gateway["materialized"]["status"] == gateway["online_tiled"]["status"] == 200,
            {
                "materialized": gateway["materialized"]["content"],
                "online_tiled": gateway["online_tiled"]["content"],
            },
        ),
        assertion(
            "worker health exposes the selected attention algorithm and FP32 accumulation boundary",
            health_materialized["algorithm"] == "materialized"
            and health_online["algorithm"] == "online-tiled"
            and health_materialized["precision"] == health_online["precision"] == "fp32",
            {"materialized": health_materialized, "online_tiled": health_online},
        ),
        assertion(
            "gateway routes to the online worker and preserves its completion",
            gateway["gateway_matches_online"]
            and gateway["gateway"]["worker"] == gateway["online_tiled"]["worker"],
            gateway["gateway"],
        ),
        assertion(
            "gateway SSE reconstructs the same online completion and terminates",
            gateway["stream_reconstructs_gateway"]
            and gateway["stream"]["done_received"]
            and gateway["stream"]["finish_reason"] == "stop",
            gateway["stream"],
        ),
        assertion(
            "historical FP32 full-model output remains within PyTorch tolerance",
            torch_parity["passed"] and torch_parity["max_abs_logit_error"] <= 1.0e-4,
            torch_parity["max_abs_logit_error"],
        ),
        assertion(
            "environment records CUDA toolchain and runtime availability explicitly",
            isinstance(cuda["toolchain_available"], bool)
            and isinstance(cuda["pytorch_available"], bool)
            and "milestone_boundary" in environment,
            cuda,
        ),
    ]
    passed_count = sum(item["passed"] for item in assertions)
    result = {
        "passed": passed_count == len(assertions),
        "assertions_passed": passed_count,
        "assertions_total": len(assertions),
        "maximum_oracle_error": max(oracle_errors.values()),
        "maximum_algorithm_error": max(algorithm_errors.values()),
        "full_model_maximum_logit_error": full_model_logit_error,
        "fp16_storage_drift": variants[("online-tiled", "fp16")][
            "maximum_absolute_error_to_materialized_fp32"
        ],
        "bf16_storage_drift": variants[("online-tiled", "bf16")][
            "maximum_absolute_error_to_materialized_fp32"
        ],
        "score_scratch_reduction_at_256x": (
            materialized_256["stats"]["score_buffer_bytes"]
            / online_256["stats"]["score_buffer_bytes"]
        ),
        "modeled_traffic_reduction_at_256x": (
            materialized_256["stats"]["modeled_external_total_bytes"]
            / online_256["stats"]["modeled_external_total_bytes"]
        ),
        "observed_wall_time_speedup_at_256x": (
            materialized_256["median_us"] / online_256["median_us"]
        ),
        "cuda_toolchain_available": cuda["toolchain_available"],
        "cuda_runtime_available": cuda["pytorch_available"],
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
