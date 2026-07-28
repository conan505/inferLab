#!/usr/bin/env python3
"""Compare round-robin and least-in-flight benchmark artifacts."""

import argparse
import json


def load(path: str) -> dict:
    with open(path, encoding="utf-8") as source:
        return json.load(source)


def improvement_percent(baseline: float, candidate: float) -> float:
    return round((baseline - candidate) / baseline * 100, 3)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--round-robin", required=True)
    parser.add_argument("--least-in-flight", required=True)
    parser.add_argument("--slow-worker", default="worker-c")
    args = parser.parse_args()

    round_robin = load(args.round_robin)
    least_in_flight = load(args.least_in_flight)
    rr_summary = round_robin["summary"]
    lif_summary = least_in_flight["summary"]
    expected_requests = round_robin["config"]["requests"]

    rr_slow = rr_summary["worker_counts"].get(args.slow_worker, 0)
    lif_slow = lif_summary["worker_counts"].get(args.slow_worker, 0)
    rr_p90 = rr_summary["e2e_ms"]["p90"]
    lif_p90 = lif_summary["e2e_ms"]["p90"]
    rr_p95 = rr_summary["e2e_ms"]["p95"]
    lif_p95 = lif_summary["e2e_ms"]["p95"]

    checks = {
        "all_requests_succeeded": (
            rr_summary["successful"] == expected_requests
            and lif_summary["successful"] == expected_requests
        ),
        "least_in_flight_reduced_slow_worker_assignments": lif_slow < rr_slow,
        "least_in_flight_improved_p90": lif_p90 < rr_p90,
        "least_in_flight_improved_throughput": (
            lif_summary["requests_per_second"] > rr_summary["requests_per_second"]
        ),
    }
    report = {
        "schema": "inferlab.routing-comparison.v0.0.2",
        "slow_worker": args.slow_worker,
        "round_robin": {
            "worker_counts": rr_summary["worker_counts"],
            "slow_worker_fraction": round(rr_slow / expected_requests, 4),
            "e2e_p90_ms": rr_p90,
            "e2e_p95_ms": rr_p95,
            "requests_per_second": rr_summary["requests_per_second"],
        },
        "least_in_flight": {
            "worker_counts": lif_summary["worker_counts"],
            "slow_worker_fraction": round(lif_slow / expected_requests, 4),
            "e2e_p90_ms": lif_p90,
            "e2e_p95_ms": lif_p95,
            "requests_per_second": lif_summary["requests_per_second"],
        },
        "change": {
            "slow_worker_assignments": lif_slow - rr_slow,
            "p90_improvement_percent": improvement_percent(rr_p90, lif_p90),
            "p95_improvement_percent": improvement_percent(rr_p95, lif_p95),
            "throughput_improvement_percent": round(
                (lif_summary["requests_per_second"] - rr_summary["requests_per_second"])
                / rr_summary["requests_per_second"]
                * 100,
                3,
            ),
        },
        "checks": checks,
    }
    print(json.dumps(report, indent=2, sort_keys=True))

    if not all(checks.values()):
        raise SystemExit("routing comparison did not satisfy every proof check")


if __name__ == "__main__":
    main()
