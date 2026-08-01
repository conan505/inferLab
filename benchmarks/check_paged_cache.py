#!/usr/bin/env python3
"""Check the retained v0.9 paged-cache and prefix-ownership claims."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


def assertion(name: str, passed: bool, observed) -> dict:
    return {"name": name, "passed": bool(passed), "observed": observed}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--paged-parity", type=Path, nargs="+", required=True)
    parser.add_argument("--torch-parity", type=Path, nargs="+", required=True)
    parser.add_argument("--page-probe", type=Path, required=True)
    parser.add_argument("--prefix-probe", type=Path, required=True)
    parser.add_argument("--gateway-stream", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    parities = [json.loads(path.read_text()) for path in args.paged_parity]
    torch = [json.loads(path.read_text()) for path in args.torch_parity]
    pages = json.loads(args.page_probe.read_text())
    prefixes = json.loads(args.prefix_probe.read_text())
    stream = json.loads(args.gateway_stream.read_text())
    teach = next(item for item in parities if item["prompt"] == "teach me streaming")
    capacity = pages["capacity"]
    fragmentation = pages["fragmentation"]
    sharing = pages["sharing"]
    eviction = pages["eviction"]
    pairs = prefixes["prefix_pairs"]
    topology = prefixes["topology"]
    maximum_page_error = max(item["max_abs_logit_error"] for item in parities)
    maximum_torch_error = max(item["max_abs_logit_error"] for item in torch)
    fragment_by_page = {
        item["page_tokens"]: item["internal_fragmentation_percent"]
        for item in fragmentation
    }
    warm_metrics = [pair["warm"]["generation"] for pair in pairs]
    cold_metrics = [pair["cold"]["generation"] for pair in pairs]
    health = stream["worker_health"]["body"]

    assertions = [
        assertion(
            "three prompts compare contiguous and paged KV layouts",
            len(parities) == 3,
            len(parities),
        ),
        assertion(
            "all paged-layout parity reports pass",
            all(item["passed"] for item in parities),
            [item["passed"] for item in parities],
        ),
        assertion(
            "paged logits are bit-identical to contiguous logits",
            maximum_page_error == 0,
            maximum_page_error,
        ),
        assertion(
            "paged greedy tokens, text, and finish reasons are unchanged",
            all(
                item["token_ids_match"]
                and item["text_match"]
                and item["finish_reason_match"]
                for item in parities
            ),
            [item["generated_text"] for item in parities],
        ),
        assertion(
            "paged decoder remains within 1e-4 of independent PyTorch",
            len(torch) == 3
            and all(item["passed"] for item in torch)
            and maximum_torch_error <= 1.0e-4,
            maximum_torch_error,
        ),
        assertion(
            "three pages represent the retained eleven-token cache",
            teach["paged_metrics"]["cache_pages"] == 3
            and teach["paged_metrics"]["cache_bytes"] == 1_408
            and teach["paged_metrics"]["reserved_cache_bytes"] == 1_536
            and teach["paged_metrics"]["internal_fragmentation_bytes"] == 128,
            teach["paged_metrics"],
        ),
        assertion(
            "paged and contiguous layouts perform the same decoder work",
            all(
                teach["paged_metrics"][field]
                == teach["contiguous_metrics"][field]
                for field in [
                    "query_tokens",
                    "kv_tokens",
                    "attention_score_elements",
                ]
            ),
            {
                "contiguous": teach["contiguous_metrics"],
                "paged": teach["paged_metrics"],
            },
        ),
        assertion(
            "fixed pool fits eight short sessions versus two max reservations",
            capacity["paged_concurrent_sessions"] == 8
            and capacity["contiguous_max_context_reservation_sessions"] == 2
            and capacity["capacity_gain"] == 4.0,
            {
                "paged": capacity["paged_concurrent_sessions"],
                "contiguous": capacity[
                    "contiguous_max_context_reservation_sessions"
                ],
                "gain": capacity["capacity_gain"],
            },
        ),
        assertion(
            "page capacity rejects the ninth session deterministically",
            capacity["at_capacity"]["free_pages"] == 0
            and "capacity exhausted" in capacity["exhausted_error"],
            {
                "free_pages": capacity["at_capacity"]["free_pages"],
                "error": capacity["exhausted_error"],
            },
        ),
        assertion(
            "dropping sessions returns every page and reference",
            capacity["after_all_released"]["allocated_pages"] == 0
            and capacity["after_all_released"]["free_pages"] == 16
            and capacity["after_all_released"]["live_references"] == 0,
            capacity["after_all_released"],
        ),
        assertion(
            "larger pages show the expected internal-fragmentation tradeoff",
            list(fragment_by_page) == [1, 2, 4, 8]
            and fragment_by_page[1] == 0
            and 9.0 < fragment_by_page[2] < 9.2
            and 23.0 < fragment_by_page[4] < 23.2
            and fragment_by_page[8] == 37.5,
            fragment_by_page,
        ),
        assertion(
            "two warm sessions share one physical prefix page",
            sharing["two_warm_sessions_before_decode"]["allocated_pages"] == 1
            and sharing["two_warm_sessions_before_decode"]["shared_pages"] == 1
            and sharing["two_warm_sessions_before_decode"]["maximum_refcount"] == 3
            and sharing["two_warm_sessions_before_decode"][
                "bytes_saved_by_sharing"
            ]
            == 768,
            sharing["two_warm_sessions_before_decode"],
        ),
        assertion(
            "both warm forks reuse three prefix tokens and copy before mutation",
            all(
                metrics["prefix_cache_hit"]
                and metrics["prefix_tokens_reused"] == 3
                and metrics["kv_tokens"] == 1
                and metrics["copy_on_write_copies"] == 1
                for metrics in [
                    sharing["warm_a_metrics_after_fork"],
                    sharing["warm_b_metrics_after_fork"],
                ]
            ),
            {
                "warm_a": sharing["warm_a_metrics_after_fork"],
                "warm_b": sharing["warm_b_metrics_after_fork"],
            },
        ),
        assertion(
            "copy-on-write gives both forks private writable pages",
            sharing["after_copy_on_write"]["allocated_pages"] == 3
            and sharing["after_copy_on_write"]["copy_on_write_copies"] == 2
            and sharing["after_copy_on_write"]["maximum_refcount"] == 1,
            sharing["after_copy_on_write"],
        ),
        assertion(
            "longer prompt reuses the longest cached token prefix",
            sharing["longer_prompt_metrics"]["prefix_cache_hit"]
            and sharing["longer_prompt_metrics"]["prefix_tokens_reused"] == 3
            and sharing["longer_prompt_metrics"]["kv_tokens"] == 1,
            sharing["longer_prompt_metrics"],
        ),
        assertion(
            "LRU eviction keeps two entries and makes the oldest prefix miss",
            eviction["after_three_prompts"]["prefix_entries"] == 2
            and eviction["after_three_prompts"]["evictions"] == 1
            and eviction["evicted_prefix_was_a_miss"]
            and eviction["after_reloading_oldest"]["evictions"] == 2,
            eviction,
        ),
        assertion(
            "gateway cold/warm pairs keep one prefix owner",
            len(pairs) == 6 and all(pair["same_owner"] for pair in pairs),
            [
                {
                    "prompt": pair["prompt"],
                    "cold": pair["cold"]["worker"],
                    "warm": pair["warm"]["worker"],
                }
                for pair in pairs
            ],
        ),
        assertion(
            "all cold prompts miss and all warm prompts hit",
            all(not metrics["prefix_cache_hit"] for metrics in cold_metrics)
            and all(metrics["prefix_cache_hit"] for metrics in warm_metrics),
            {
                "cold": [metrics["prefix_cache_hit"] for metrics in cold_metrics],
                "warm": [metrics["prefix_cache_hit"] for metrics in warm_metrics],
            },
        ),
        assertion(
            "warm prefix hits reduce K/V token projections",
            all(pair["kv_projection_reduction"] > 0 for pair in pairs),
            [pair["kv_projection_reduction"] for pair in pairs],
        ),
        assertion(
            "consistent ownership is stable before the topology change",
            topology["keys"] == 256 and topology["stable_before_change"],
            {
                "keys": topology["keys"],
                "stable": topology["stable_before_change"],
            },
        ),
        assertion(
            "topology addition remaps only keys acquired by the new worker",
            topology["only_new_worker_received_remapped_keys"]
            and 0.20 <= topology["remapped_fraction"] <= 0.45,
            {
                "remapped_keys": topology["remapped_keys"],
                "remapped_fraction": topology["remapped_fraction"],
                "counts": topology["three_worker_counts"],
            },
        ),
        assertion(
            "gateway still streams through the paged worker",
            stream["status"] == 200
            and stream["done_received"]
            and stream["content"] == "InferLab turns prompts into real tokens."
            and stream["headers"].get("x-inferlab-worker")
            in {"cpu-page-a", "cpu-page-b"}
            and health["decoder_mode"] == "paged-kv-cache"
            and health["paged_cache"]["page_tokens"] == 4,
            {
                "status": stream["status"],
                "done": stream["done_received"],
                "content": stream["content"],
                "worker": stream["headers"].get("x-inferlab-worker"),
                "mode": health["decoder_mode"],
            },
        ),
    ]
    passed_count = sum(item["passed"] for item in assertions)
    result = {
        "passed": passed_count == len(assertions),
        "assertions_passed": passed_count,
        "assertions_total": len(assertions),
        "maximum_paged_vs_contiguous_logit_error": maximum_page_error,
        "maximum_paged_vs_torch_logit_error": maximum_torch_error,
        "capacity_gain": capacity["capacity_gain"],
        "fragmentation_percent_by_page_tokens": fragment_by_page,
        "shared_prefix_bytes_saved": sharing[
            "two_warm_sessions_before_decode"
        ]["bytes_saved_by_sharing"],
        "copy_on_write_copies": sharing["after_copy_on_write"][
            "copy_on_write_copies"
        ],
        "gateway_warm_hits": sum(
            metrics["prefix_cache_hit"] for metrics in warm_metrics
        ),
        "topology_remapped_fraction": topology["remapped_fraction"],
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
