#!/usr/bin/env python3
"""Check that EWMA routing learns when the previously fast worker slows down."""

import argparse
import json


def load(path: str) -> dict:
    with open(path, encoding="utf-8") as source:
        return json.load(source)


def worker(status: dict, worker_id: str) -> dict:
    return next(item for item in status["workers"] if item["id"] == worker_id)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--warmup", required=True)
    parser.add_argument("--after-slowdown", required=True)
    parser.add_argument("--status-before", required=True)
    parser.add_argument("--status-after", required=True)
    args = parser.parse_args()

    warmup = load(args.warmup)
    after = load(args.after_slowdown)
    status_before = load(args.status_before)
    status_after = load(args.status_after)
    warm_counts = warmup["summary"]["worker_counts"]
    after_counts = after["summary"]["worker_counts"]
    before_a = worker(status_before, "worker-a")
    before_b = worker(status_before, "worker-b")
    after_a = worker(status_after, "worker-a")
    after_b = worker(status_after, "worker-b")

    checks = {
        "all_requests_succeeded": (
            warmup["summary"]["failed"] == 0 and after["summary"]["failed"] == 0
        ),
        "warmup_preferred_initially_fast_a": (
            warm_counts.get("worker-a", 0) > warm_counts.get("worker-b", 0)
        ),
        "slowdown_shifted_traffic_to_b": (
            after_counts.get("worker-b", 0) > after_counts.get("worker-a", 0)
        ),
        "ewma_learned_that_a_became_slower": (
            after_a["ewma_ttft_ms"] > after_b["ewma_ttft_ms"]
        ),
        "probes_kept_sampling_a": after_counts.get("worker-a", 0) > 1,
        "both_workers_received_fresh_observations": (
            after_a["ewma_observations"] > before_a["ewma_observations"]
            and after_b["ewma_observations"] > before_b["ewma_observations"]
        ),
    }
    report = {
        "schema": "inferlab.ewma-adaptation.v0.0.4",
        "warmup_worker_counts": warm_counts,
        "after_slowdown_worker_counts": after_counts,
        "ewma_ttft_ms_before": {
            "worker-a": before_a["ewma_ttft_ms"],
            "worker-b": before_b["ewma_ttft_ms"],
        },
        "ewma_ttft_ms_after": {
            "worker-a": after_a["ewma_ttft_ms"],
            "worker-b": after_b["ewma_ttft_ms"],
        },
        "observations_before": {
            "worker-a": before_a["ewma_observations"],
            "worker-b": before_b["ewma_observations"],
        },
        "observations_after": {
            "worker-a": after_a["ewma_observations"],
            "worker-b": after_b["ewma_observations"],
        },
        "checks": checks,
    }
    print(json.dumps(report, indent=2, sort_keys=True))

    if not all(checks.values()):
        raise SystemExit("EWMA routing did not satisfy every adaptation check")


if __name__ == "__main__":
    main()
