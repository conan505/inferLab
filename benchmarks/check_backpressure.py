#!/usr/bin/env python3
"""Turn the v0.0.6 overload measurements into explicit pass/fail claims."""

import argparse
import json


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--analysis", required=True)
    args = parser.parse_args()

    with open(args.analysis, encoding="utf-8") as source:
        analysis = json.load(source)

    samples = analysis["samples"]
    accepted = [sample for sample in samples if sample["status"] == 200]
    rejected = [sample for sample in samples if sample["status"] == 429]
    admission = analysis["gateway_status_after"]["admission"]
    rss = analysis["summary"]["rss"]
    checks = {
        "schema_is_v0_0_6": (
            analysis["schema"] == "inferlab.backpressure-overload.v0.0.6"
        ),
        "load_was_at_least_5x_capacity": (
            analysis["config"]["offered_load_multiple"] >= 5.0
        ),
        "some_work_completed": len(accepted) > 0,
        "excess_work_was_rejected": len(rejected) > 0,
        "no_transport_failures": (
            analysis["summary"]["transport_errors"] == 0
        ),
        "only_success_or_intentional_overload": (
            len(accepted) + len(rejected) == len(samples)
        ),
        "open_loop_dispatch_stayed_on_schedule": (
            analysis["summary"]["dispatch_lag_ms"]["p99"] < 50
        ),
        "every_rejection_is_machine_readable": all(
            sample.get("error_type") == "gateway_overloaded"
            for sample in rejected
        ),
        "every_rejection_has_retry_after": all(
            sample.get("retry_after") == "1" for sample in rejected
        ),
        "gateway_rejection_counter_matches_client": (
            admission["rejected_total"] == len(rejected)
        ),
        "executing_never_exceeded_two": (
            admission["max_observed_executing"] <= 2
        ),
        "execution_limit_was_exercised": (
            admission["max_observed_executing"] == 2
        ),
        "queue_never_exceeded_four": (
            admission["max_observed_queued"] <= 4
        ),
        "queue_limit_was_exercised": (
            admission["max_observed_queued"] == 4
        ),
        "outstanding_never_exceeded_six": (
            admission["max_observed_outstanding"] <= 6
        ),
        "admission_limit_was_exercised": (
            admission["max_observed_outstanding"] == 6
        ),
        "gateway_drained_after_load": (
            admission["outstanding"] == 0
            and admission["executing"] == 0
            and admission["queued"] == 0
        ),
        "accepted_p99_stayed_below_one_second": (
            analysis["summary"]["accepted_latency_ms"]["p99"] < 1_000
        ),
        "gateway_rss_increase_stayed_below_32_mib": (
            rss["max_increase_from_first_kib"] is not None
            and rss["max_increase_from_first_kib"] < 32 * 1024
        ),
    }
    report = {
        "schema": "inferlab.backpressure-check.v0.0.6",
        "offered_load_multiple": analysis["config"]["offered_load_multiple"],
        "accepted": len(accepted),
        "rejected": len(rejected),
        "accepted_p99_ms": analysis["summary"]["accepted_latency_ms"]["p99"],
        "rejected_p99_ms": analysis["summary"]["rejected_latency_ms"]["p99"],
        "dispatch_lag_p99_ms": analysis["summary"]["dispatch_lag_ms"]["p99"],
        "max_observed_executing": admission["max_observed_executing"],
        "max_observed_queued": admission["max_observed_queued"],
        "max_observed_outstanding": admission["max_observed_outstanding"],
        "gateway_rss_max_increase_kib": rss["max_increase_from_first_kib"],
        "checks": checks,
    }
    print(json.dumps(report, indent=2, sort_keys=True))

    if not all(checks.values()):
        raise SystemExit("backpressure did not satisfy every overload check")


if __name__ == "__main__":
    main()
