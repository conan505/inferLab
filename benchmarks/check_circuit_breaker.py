#!/usr/bin/env python3
"""Validate the v0.0.8 circuit trip, isolation, probe, and recovery evidence."""

import argparse
import json


def load(path: str) -> dict:
    with open(path, encoding="utf-8") as source:
        return json.load(source)


def worker(report: dict, worker_id: str) -> dict:
    return next(
        item
        for item in report["gateway_status_after"]["workers"]
        if item["id"] == worker_id
    )


def health(report: dict, worker_id: str) -> dict:
    return next(
        item
        for item in report["worker_health_after"]
        if item["worker_id"] == worker_id
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--trip", required=True)
    parser.add_argument("--open", required=True)
    parser.add_argument("--half-open", required=True)
    parser.add_argument("--probe", required=True)
    parser.add_argument("--recovered", required=True)
    args = parser.parse_args()

    trip = load(args.trip)
    opened = load(args.open)
    half_open = load(args.half_open)
    probe = load(args.probe)
    recovered = load(args.recovered)
    trip_a = worker(trip, "worker-a")["circuit"]
    open_a = worker(opened, "worker-a")["circuit"]
    probe_a = worker(probe, "worker-a")["circuit"]
    recovered_a = worker(recovered, "worker-a")["circuit"]
    final_resilience = recovered["gateway_status_after"]["resilience"]
    all_reports = [trip, opened, probe, recovered]

    checks = {
        "schemas_are_v0_0_8": all(
            report["schema"] == "inferlab.circuit-probe.v0.0.8"
            for report in all_reports
        ),
        "four_failures_tripped_worker_a": (
            trip["summary"]["status_counts"] == {"200": 4, "503": 4}
            and health(trip, "worker-a")["requests"] == 4
            and trip_a["state"] == "open"
            and trip_a["failed_attempts_total"] == 4
            and trip_a["opened_total"] == 1
        ),
        "healthy_worker_remained_closed": (
            worker(trip, "worker-b")["circuit"]["state"] == "closed"
        ),
        "open_worker_received_no_more_requests": (
            opened["summary"]["status_counts"] == {"200": 4}
            and opened["summary"]["worker_counts"] == {"worker-b": 4}
            and health(opened, "worker-a")["requests"] == 4
        ),
        "routing_observed_open_circuit": open_a["rejected_total"] >= 2,
        "cooldown_transitioned_to_half_open": (
            next(
                item for item in half_open["workers"] if item["id"] == "worker-a"
            )["circuit"]["state"]
            == "half-open"
        ),
        "half_open_had_no_probe_before_traffic": (
            next(
                item for item in half_open["workers"] if item["id"] == "worker-a"
            )["circuit"]["probe_in_flight"]
            is False
        ),
        "one_probe_reached_healed_worker": (
            probe["summary"]["status_counts"] == {"200": 1}
            and probe["summary"]["worker_counts"] == {"worker-a": 1}
            and probe["samples"][0]["attempts"] == 1
            and health(probe, "worker-a")["requests"] == 1
        ),
        "successful_probe_closed_circuit": (
            probe_a["state"] == "closed"
            and probe_a["half_open_probes_total"] == 1
            and probe_a["recoveries_total"] == 1
        ),
        "recovered_worker_rejoined_rotation": (
            recovered["summary"]["worker_counts"]
            == {"worker-a": 2, "worker-b": 2}
            and health(recovered, "worker-a")["requests"] == 3
        ),
        "recovered_circuit_stayed_closed": (
            recovered_a["state"] == "closed"
            and recovered_a["opened_total"] == 1
            and recovered_a["recoveries_total"] == 1
        ),
        "no_retry_amplification": (
            final_resilience["original_requests"] == 17
            and final_resilience["attempts"] == 17
            and final_resilience["retries_granted"] == 0
        ),
        "retry_policy_was_enabled_and_budget_denied_early_failures": (
            final_resilience["max_retries"] == 1
            and final_resilience["retry_budget_percent"] == 10
            and final_resilience["retries_denied_budget"] == 4
        ),
        "only_real_worker_failures_counted_transient": (
            final_resilience["transient_failures"] == 4
        ),
        "all_probe_phases_had_no_transport_errors": all(
            report["summary"]["transport_errors"] == 0
            for report in all_reports
        ),
    }

    report = {
        "schema": "inferlab.circuit-check.v0.0.8",
        "states": {
            "after_trip": trip_a["state"],
            "after_cooldown": next(
                item for item in half_open["workers"] if item["id"] == "worker-a"
            )["circuit"]["state"],
            "after_probe": probe_a["state"],
            "after_recovery": recovered_a["state"],
        },
        "worker_a_failed_attempts": recovered_a["failed_attempts_total"],
        "worker_a_opened_total": recovered_a["opened_total"],
        "worker_a_open_route_rejections": recovered_a["rejected_total"],
        "half_open_probes": recovered_a["half_open_probes_total"],
        "recoveries": recovered_a["recoveries_total"],
        "original_requests": final_resilience["original_requests"],
        "upstream_attempts": final_resilience["attempts"],
        "retries_granted": final_resilience["retries_granted"],
        "checks": checks,
    }
    print(json.dumps(report, indent=2, sort_keys=True))

    if not all(checks.values()):
        raise SystemExit("circuit-breaker evidence did not satisfy every check")


if __name__ == "__main__":
    main()
