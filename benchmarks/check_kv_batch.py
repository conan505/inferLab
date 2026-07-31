#!/usr/bin/env python3
"""Check the retained v0.8 KV-cache and continuous-batching claims."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


def assertion(name: str, passed: bool, observed) -> dict:
    return {"name": name, "passed": bool(passed), "observed": observed}


def configuration(load: dict, name: str) -> dict:
    return next(item for item in load["configurations"] if item["name"] == name)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--kv-parity", type=Path, nargs="+", required=True)
    parser.add_argument("--torch-parity", type=Path, nargs="+", required=True)
    parser.add_argument("--load", type=Path, required=True)
    parser.add_argument("--gateway-stream", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    kv_parities = [json.loads(path.read_text()) for path in args.kv_parity]
    torch_parities = [
        json.loads(path.read_text()) for path in args.torch_parity
    ]
    load = json.loads(args.load.read_text())
    gateway = json.loads(args.gateway_stream.read_text())
    serial = configuration(load, "one-slot")
    continuous = configuration(load, "continuous-four-slot")
    teach = next(
        report
        for report in kv_parities
        if report["prompt"] == "teach me streaming"
    )
    before = teach["recompute_metrics"]
    after = teach["cached_metrics"]
    reductions = teach["work_reduction_percent"]
    all_load_requests = [
        request
        for config in load["configurations"]
        for level in config["levels"]
        for request in level["requests"]
    ] + load["backfill"]["requests"]
    highest_concurrency = load["workload"]["concurrency_levels"][-1]
    serial_high = next(
        level
        for level in serial["levels"]
        if level["concurrency"] == highest_concurrency
    )
    continuous_high = next(
        level
        for level in continuous["levels"]
        if level["concurrency"] == highest_concurrency
    )
    throughput_gain = (
        continuous_high["request_throughput_per_second"]
        / serial_high["request_throughput_per_second"]
    )
    trace = sorted(load["backfill"]["trace"], key=lambda event: event["sequence"])
    completion_sequences = [
        event["sequence"] for event in trace if event["event"] == "completed"
    ]
    admission_sequences = [
        event["sequence"] for event in trace if event["event"] == "admitted"
    ]
    backfilled_between_completions = bool(completion_sequences) and any(
        min(completion_sequences) < admitted < max(completion_sequences)
        for admitted in admission_sequences
    )
    gateway_health = gateway["worker_health"]["body"]

    assertions = [
        assertion(
            "three prompts compare recomputation with KV caching",
            len(kv_parities) == 3,
            len(kv_parities),
        ),
        assertion(
            "all KV-cache parity reports pass",
            all(report["passed"] for report in kv_parities),
            [report["passed"] for report in kv_parities],
        ),
        assertion(
            "cached logits match recomputation within 1e-6",
            max(report["max_abs_logit_error"] for report in kv_parities)
            <= 1.0e-6,
            max(report["max_abs_logit_error"] for report in kv_parities),
        ),
        assertion(
            "cached greedy tokens and text exactly match recomputation",
            all(
                report["token_ids_match"]
                and report["text_match"]
                and report["finish_reason_match"]
                for report in kv_parities
            ),
            [
                {
                    "tokens": report["token_ids_match"],
                    "text": report["text_match"],
                    "finish": report["finish_reason_match"],
                }
                for report in kv_parities
            ],
        ),
        assertion(
            "cached decoder still matches the independent PyTorch oracle",
            len(torch_parities) == 3
            and all(report["passed"] for report in torch_parities),
            [report["max_abs_logit_error"] for report in torch_parities],
        ),
        assertion(
            "recomputation performs the expected deterministic work",
            before["query_tokens"] == 60
            and before["kv_tokens"] == 60
            and before["attention_score_elements"] == 1_104
            and before["peak_cache_bytes"] == 0,
            before,
        ),
        assertion(
            "KV caching performs the expected deterministic work",
            after["query_tokens"] == 8
            and after["kv_tokens"] == 11
            and after["attention_score_elements"] == 240
            and after["peak_cache_bytes"] == 1_408
            and after["cache_rebuilds"] == 0,
            after,
        ),
        assertion(
            "KV caching cuts every measured work dimension by over 75%",
            reductions["query_tokens"] > 75
            and reductions["kv_tokens"] > 75
            and reductions["attention_score_elements"] > 75,
            reductions,
        ),
        assertion(
            "load matrix covers concurrency 1, 2, 4, and 8",
            load["workload"]["concurrency_levels"] == [1, 2, 4, 8]
            and all(len(config["levels"]) == 4 for config in load["configurations"]),
            load["workload"]["concurrency_levels"],
        ),
        assertion(
            "all load requests complete through the KV-cached worker",
            all(
                request["status"] == 200
                and request["generation"]["mode"] == "kv-cache"
                for request in all_load_requests
            ),
            {
                "requests": len(all_load_requests),
                "statuses": sorted({request["status"] for request in all_load_requests}),
                "modes": sorted(
                    {request["generation"]["mode"] for request in all_load_requests}
                ),
            },
        ),
        assertion(
            "one-slot baseline never has more than one active sequence",
            serial["scheduler"]["max_active"] == 1,
            serial["scheduler"]["max_active"],
        ),
        assertion(
            "continuous scheduler fills all four active slots",
            continuous["scheduler"]["max_active"] == 4,
            continuous["scheduler"]["max_active"],
        ),
        assertion(
            "continuous scheduler backfills after an early completion",
            backfilled_between_completions,
            {
                "completion_sequences": completion_sequences,
                "admission_sequences": admission_sequences,
            },
        ),
        assertion(
            "continuous scheduling improves high-concurrency throughput by at least 2x",
            throughput_gain >= 2.0,
            {
                "concurrency": highest_concurrency,
                "one_slot_requests_per_second": serial_high[
                    "request_throughput_per_second"
                ],
                "continuous_requests_per_second": continuous_high[
                    "request_throughput_per_second"
                ],
                "gain": throughput_gain,
            },
        ),
        assertion(
            "continuous scheduling lowers high-concurrency p95 latency",
            continuous_high["latency_ms"]["p95"]
            < serial_high["latency_ms"]["p95"],
            {
                "one_slot_p95_ms": serial_high["latency_ms"]["p95"],
                "continuous_p95_ms": continuous_high["latency_ms"]["p95"],
            },
        ),
        assertion(
            "gateway still streams the cached decoder contract",
            gateway["status"] == 200
            and gateway["done_received"]
            and gateway["content"] == "InferLab turns prompts into real tokens."
            and gateway_health["decoder_mode"] == "kv-cache"
            and gateway_health["scheduler"]["max_batch_size"] == 4,
            {
                "status": gateway["status"],
                "done": gateway["done_received"],
                "content": gateway["content"],
                "decoder_mode": gateway_health["decoder_mode"],
                "max_batch_size": gateway_health["scheduler"]["max_batch_size"],
            },
        ),
    ]
    passed_count = sum(item["passed"] for item in assertions)
    result = {
        "passed": passed_count == len(assertions),
        "assertions_passed": passed_count,
        "assertions_total": len(assertions),
        "maximum_recompute_vs_cache_logit_error": max(
            report["max_abs_logit_error"] for report in kv_parities
        ),
        "maximum_cache_vs_torch_logit_error": max(
            report["max_abs_logit_error"] for report in torch_parities
        ),
        "work_reduction_percent": reductions,
        "high_concurrency_throughput_gain": throughput_gain,
        "high_concurrency_p95_latency_ms": {
            "one_slot": serial_high["latency_ms"]["p95"],
            "continuous": continuous_high["latency_ms"]["p95"],
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
