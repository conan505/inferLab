#!/usr/bin/env python3
"""Validate continuous-failure and recovery claims for InferLab v0.0.9."""

import argparse
import json
import math


FAULT_PHASES = [
    "worker-a-down",
    "worker-b-slow",
    "worker-c-disconnected",
]
EXPECTED_EVENTS = [
    "traffic_started",
    "worker_a_killed",
    "worker_a_restarted",
    "worker_b_slowed",
    "worker_b_restored",
    "worker_c_disconnected",
    "worker_c_reconnected",
    "traffic_completed",
]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--analysis", required=True)
    args = parser.parse_args()
    with open(args.analysis, encoding="utf-8") as source:
        analysis = json.load(source)

    phases = analysis["phases"]
    recovery = analysis["recovery"]
    retry = analysis["retry_accounting"]
    bounds = analysis["bounds"]
    final_circuits = analysis["final_circuits"]
    original_requests = retry["original_requests"]
    allowed_retries = (
        original_requests * retry["retry_budget_percent"] // 100
    )
    fault_events = [
        event
        for event in analysis["events"]
        if event["event"]
        not in {"traffic_started", "traffic_completed"}
    ]

    checks = {
        "schema_is_v0_0_9": (
            analysis["schema"] == "inferlab.chaos-analysis.v0.0.9"
        ),
        "event_order_is_complete": (
            [event["event"] for event in analysis["events"]]
            == EXPECTED_EVENTS
            and all(
                earlier["elapsed_ms"] < later["elapsed_ms"]
                for earlier, later in zip(
                    analysis["events"], analysis["events"][1:]
                )
            )
        ),
        "faults_targeted_only_owned_loopback_children": all(
            event["scope"] == "owned-child-process"
            and event["bind"] == "127.0.0.1"
            and isinstance(event["target_pid"], int)
            and event["target_pid"] > 0
            for event in fault_events
        ),
        "open_loop_dispatch_stayed_on_schedule": (
            analysis["dispatch_lag_p99_ms"] < 50
        ),
        "baseline_was_steady": (
            phases["healthy-baseline"]["requests"] > 0
            and phases["healthy-baseline"]["success_rate_percent"] == 100.0
        ),
        "each_fault_was_observable": all(
            phases[phase]["errors"] > 0
            or phases[phase]["attempts"] > phases[phase]["requests"]
            for phase in FAULT_PHASES
        ),
        "goodput_continued_during_every_fault": all(
            phases[phase]["successful"] > 0
            and phases[phase]["success_rate_percent"] >= 60.0
            for phase in FAULT_PHASES
        ),
        "healthy_workers_carried_each_incident": (
            set(phases["worker-a-down"]["worker_counts"])
            >= {"worker-b", "worker-c"}
            and set(phases["worker-b-slow"]["worker_counts"])
            >= {"worker-a", "worker-c"}
            and set(phases["worker-c-disconnected"]["worker_counts"])
            >= {"worker-a", "worker-b"}
        ),
        "every_fault_opened_its_worker_circuit": all(
            recovery[worker]["detection_ms"] is not None
            for worker in recovery
        ),
        "circuit_detection_was_bounded": all(
            recovery[worker]["detection_ms"] <= 1_500
            for worker in recovery
        ),
        "failover_was_bounded": all(
            recovery[worker]["failover_ms"] is not None
            and recovery[worker]["failover_ms"] <= 500
            for worker in recovery
        ),
        "every_healed_worker_recovered": all(
            recovery[worker]["recovery_ms"] is not None
            for worker in recovery
        ),
        "recovery_time_was_bounded": all(
            recovery[worker]["recovery_ms"] <= 1_800
            for worker in recovery
        ),
        "incident_mttr_was_bounded": all(
            recovery[worker]["mttr_ms"] <= 3_500 for worker in recovery
        ),
        "all_workers_rejoined_final_traffic": (
            set(phases["final-healthy"]["worker_counts"])
            == {"worker-a", "worker-b", "worker-c"}
        ),
        "final_steady_state_was_restored": (
            phases["final-healthy"]["success_rate_percent"] == 100.0
            and all(
                circuit["state"] == "closed"
                for circuit in final_circuits.values()
            )
        ),
        "each_breaker_exercised_open_probe_and_recovery": all(
            circuit["opened_total"] >= 1
            and circuit["rejected_total"] >= 1
            and circuit["half_open_probes_total"] >= 1
            and circuit["recoveries_total"] >= 1
            for circuit in final_circuits.values()
        ),
        "retry_accounting_identity_holds": (
            retry["upstream_attempts"]
            == retry["original_requests"] + retry["retries_granted"]
        ),
        "retry_budget_bounded_amplification": (
            retry["retries_granted"] <= allowed_retries
            and retry["amplification"]
            <= 1 + retry["retry_budget_percent"] / 100
        ),
        "admission_and_execution_bounds_held": (
            bounds["max_observed_queued"]
            <= bounds["configured_queue_capacity"]
            and bounds["max_observed_executing"]
            <= bounds["configured_execution_capacity"]
            and bounds["max_observed_outstanding"]
            <= bounds["configured_outstanding_capacity"]
        ),
        "request_deadline_bounded_latency": (
            bounds["maximum_client_latency_ms"]
            <= bounds["request_deadline_ms"] + 150
        ),
        "gateway_memory_remained_bounded": (
            bounds["rss_max_increase_kib"] is not None
            and bounds["rss_max_increase_kib"] < 32 * 1024
        ),
        "gateway_status_sampling_was_continuous": (
            analysis["gateway_status_sample_errors"] == 0
        ),
        "no_client_transport_failures": (
            "transport" not in analysis["overall"]["status_counts"]
        ),
    }

    report = {
        "schema": "inferlab.chaos-check.v0.0.9",
        "requests": analysis["overall"]["requests"],
        "successful": analysis["overall"]["successful"],
        "errors": analysis["overall"]["errors"],
        "overall_success_rate_percent": analysis["overall"][
            "success_rate_percent"
        ],
        "dispatch_lag_p99_ms": analysis["dispatch_lag_p99_ms"],
        "recovery": recovery,
        "mean_mttr_ms": analysis["mean_mttr_ms"],
        "retry_accounting": {
            **retry,
            "allowed_retries": allowed_retries,
        },
        "bounds": bounds,
        "checks": checks,
    }
    print(json.dumps(report, indent=2, sort_keys=True))
    if not all(checks.values()):
        failed = [name for name, passed in checks.items() if not passed]
        raise SystemExit(
            "chaos evidence did not satisfy: " + ", ".join(failed)
        )


if __name__ == "__main__":
    main()
