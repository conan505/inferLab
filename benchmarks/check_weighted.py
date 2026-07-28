#!/usr/bin/env python3
"""Check a weighted-routing benchmark against configured proportions."""

import argparse
import json


def parse_weight(raw: str) -> tuple:
    worker, separator, weight = raw.partition("=")
    if not separator:
        raise argparse.ArgumentTypeError("expected WORKER=WEIGHT")
    try:
        parsed_weight = int(weight)
    except ValueError as error:
        raise argparse.ArgumentTypeError(str(error)) from error
    if not worker or parsed_weight <= 0:
        raise argparse.ArgumentTypeError("worker must be named and weight must be positive")
    return worker, parsed_weight


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--result", required=True)
    parser.add_argument("--expected", action="append", type=parse_weight, required=True)
    parser.add_argument("--max-error-percentage-points", type=float, default=0.01)
    args = parser.parse_args()

    with open(args.result, encoding="utf-8") as source:
        benchmark = json.load(source)

    summary = benchmark["summary"]
    actual_counts = summary["worker_counts"]
    expected_weights = dict(args.expected)
    total_weight = sum(expected_weights.values())
    total_requests = benchmark["config"]["requests"]
    successful = summary["successful"]

    expected_counts = {
        worker: total_requests * weight / total_weight
        for worker, weight in expected_weights.items()
    }
    errors = {
        worker: round(
            abs(
                (
                    actual_counts.get(worker, 0) / successful
                    if successful
                    else 0.0
                )
                - weight / total_weight
            )
            * 100,
            6,
        )
        for worker, weight in expected_weights.items()
    }
    checks = {
        "all_requests_succeeded": successful == total_requests,
        "only_configured_workers_selected": set(actual_counts) == set(expected_weights),
        "distribution_within_tolerance": (
            max(errors.values()) <= args.max_error_percentage_points
        ),
    }
    report = {
        "schema": "inferlab.weighted-check.v0.0.3",
        "actual_counts": actual_counts,
        "expected_counts": expected_counts,
        "configured_weights": expected_weights,
        "distribution_error_percentage_points": errors,
        "checks": checks,
    }
    print(json.dumps(report, indent=2, sort_keys=True))

    if not all(checks.values()):
        raise SystemExit("weighted routing did not satisfy every proof check")


if __name__ == "__main__":
    main()
