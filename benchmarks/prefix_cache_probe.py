#!/usr/bin/env python3
"""Exercise prefix ownership and topology remapping through real gateways."""

from __future__ import annotations

import argparse
import json
import time
import urllib.request
from collections import Counter
from pathlib import Path

PREFIX_PROMPTS = [
    ("tenant/hello", "hello systems"),
    ("tenant/teach", "teach me"),
    ("tenant/systems", "systems hello"),
    ("tenant/unknown", "why does"),
    ("tenant/inferlab", "InferLab turns"),
    ("tenant/real", "real tokens"),
]


def completion(url: str, cache_key: str, prompt: str, max_tokens: int) -> dict:
    payload = {
        "model": "inferlab-tiny",
        "stream": False,
        "temperature": 0,
        "max_tokens": max_tokens,
        "messages": [{"role": "user", "content": prompt}],
    }
    request = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "content-type": "application/json",
            "x-inferlab-cache-key": cache_key,
        },
        method="POST",
    )
    started = time.perf_counter_ns()
    with urllib.request.urlopen(request, timeout=20) as response:
        body = json.loads(response.read())
        return {
            "status": response.status,
            "worker": response.headers["x-inferlab-worker"],
            "latency_ms": (time.perf_counter_ns() - started) / 1_000_000.0,
            "text": body["choices"][0]["message"]["content"],
            "finish_reason": body["choices"][0]["finish_reason"],
            "generation": body["inferlab"]["generation"],
        }


def get_json(url: str) -> dict:
    with urllib.request.urlopen(url, timeout=10) as response:
        return json.loads(response.read())


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--two-worker-url", required=True)
    parser.add_argument("--three-worker-url", required=True)
    parser.add_argument(
        "--worker-cache",
        action="append",
        required=True,
        help="worker-id=http://host/internal/cache",
    )
    parser.add_argument("--keys", type=int, default=256)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.keys <= 0:
        raise SystemExit("--keys must be positive")
    cache_urls = dict(value.split("=", 1) for value in args.worker_cache)
    before_stats = {
        worker: get_json(url)["cache"] for worker, url in cache_urls.items()
    }

    prefix_pairs = []
    for cache_key, prompt in PREFIX_PROMPTS:
        cold = completion(args.two_worker_url, cache_key, prompt, 2)
        warm = completion(args.two_worker_url, cache_key, prompt, 2)
        prefix_pairs.append(
            {
                "cache_key": cache_key,
                "prompt": prompt,
                "cold": cold,
                "warm": warm,
                "same_owner": cold["worker"] == warm["worker"],
                "kv_projection_reduction": (
                    cold["generation"]["kv_tokens"]
                    - warm["generation"]["kv_tokens"]
                ),
            }
        )

    ownership = []
    for index in range(args.keys):
        cache_key = f"topology-key-{index:04d}"
        before = completion(
            args.two_worker_url,
            cache_key,
            "tokens real",
            1,
        )["worker"]
        repeated = completion(
            args.two_worker_url,
            cache_key,
            "tokens real",
            1,
        )["worker"]
        after = completion(
            args.three_worker_url,
            cache_key,
            "tokens real",
            1,
        )["worker"]
        ownership.append(
            {
                "cache_key": cache_key,
                "two_worker_owner": before,
                "two_worker_repeat_owner": repeated,
                "three_worker_owner": after,
                "stable_before_change": before == repeated,
                "remapped": before != after,
                "moved_to_new_worker": before != after and after == "cpu-page-c",
            }
        )

    after_stats = {
        worker: get_json(url)["cache"] for worker, url in cache_urls.items()
    }
    remapped = sum(item["remapped"] for item in ownership)
    result = {
        "prefix_pairs": prefix_pairs,
        "topology": {
            "keys": args.keys,
            "virtual_nodes": 128,
            "two_worker_counts": dict(
                Counter(item["two_worker_owner"] for item in ownership)
            ),
            "three_worker_counts": dict(
                Counter(item["three_worker_owner"] for item in ownership)
            ),
            "stable_before_change": all(
                item["stable_before_change"] for item in ownership
            ),
            "remapped_keys": remapped,
            "remapped_fraction": remapped / args.keys,
            "only_new_worker_received_remapped_keys": all(
                not item["remapped"] or item["moved_to_new_worker"]
                for item in ownership
            ),
            "ownership": ownership,
        },
        "worker_cache_before": before_stats,
        "worker_cache_after": after_stats,
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    warm_hits = sum(pair["warm"]["generation"]["prefix_cache_hit"] for pair in prefix_pairs)
    print(
        f"prefix warm hits={warm_hits}/{len(prefix_pairs)} "
        f"stable={result['topology']['stable_before_change']} "
        f"remapped={remapped}/{args.keys} "
        f"only_to_new={result['topology']['only_new_worker_received_remapped_keys']}"
    )


if __name__ == "__main__":
    main()
