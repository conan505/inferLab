#!/usr/bin/env python3
"""Validate v0.0.7 retry-budget, deadline, and jitter evidence."""

import argparse
import json


def load(path: str) -> dict:
    with open(path, encoding="utf-8") as source:
        return json.load(source)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--retry", required=True)
    parser.add_argument("--deadline", required=True)
    parser.add_argument("--jitter", required=True)
    args = parser.parse_args()

    retry = load(args.retry)
    deadline = load(args.deadline)
    jitter = load(args.jitter)
    retry_state = retry["gateway_status_after"]["resilience"]
    deadline_state = deadline["gateway_status_after"]["resilience"]
    retry_samples = retry["samples"]
    deadline_sample = deadline["samples"][0]
    no_jitter = jitter["synchronized_backoff"]
    full_jitter = jitter["full_jitter"]
    expected_retry_events = (
        jitter["config"]["clients"] * jitter["config"]["retries_per_client"]
    )

    checks = {
        "schemas_are_v0_0_7": (
            retry["schema"] == "inferlab.resilience-probe.v0.0.7"
            and deadline["schema"] == "inferlab.resilience-probe.v0.0.7"
            and jitter["schema"] == "inferlab.retry-jitter-simulation.v0.0.7"
        ),
        "warmup_plus_probe_counted_twenty_originals": (
            retry_state["original_requests"] == 20
        ),
        "retry_budget_is_exactly_ten_percent": (
            retry_state["retries_granted"] == 2
            and retry_state["retries_granted"]
            <= retry_state["original_requests"] * 0.10
        ),
        "budget_denied_excess_retries": (
            retry_state["retries_denied_budget"] > 0
        ),
        "two_requests_recovered_on_alternate_worker": (
            retry["summary"]["retried_successes"] == 2
            and all(
                sample["worker"] == "worker-b"
                for sample in retry_samples
                if sample["status"] == 200 and sample["attempts"] == 2
            )
        ),
        "retry_attempt_accounting_matches": (
            retry_state["attempts"]
            == retry_state["original_requests"] + retry_state["retries_granted"]
        ),
        "retry_probe_had_no_transport_errors": (
            retry["summary"]["transport_errors"] == 0
        ),
        "remaining_timeout_reached_retry_worker": (
            any(
                worker["worker_id"] == "worker-b"
                and worker["last_attempt"] == 2
                and 0 < worker["last_timeout_ms"] <= 300
                for worker in retry["worker_health_after"]
            )
        ),
        "deadline_returned_504_with_machine_readable_error": (
            deadline_sample["status"] == 504
            and deadline_sample["error_type"] == "request_deadline_exceeded"
        ),
        "deadline_finished_near_configured_budget": (
            140 <= deadline_sample["e2e_ms"] < 350
        ),
        "deadline_did_not_retry": (
            deadline_state["attempts"] == 1
            and deadline_state["retries_granted"] == 0
            and deadline_state["deadline_exceeded"] == 1
        ),
        "deadline_was_propagated_to_worker": (
            deadline["worker_health_after"][0]["last_attempt"] == 1
            and 0
            < deadline["worker_health_after"][0]["last_timeout_ms"]
            <= 180
        ),
        "simulation_counts_every_retry_event": (
            sum(item["retries"] for item in no_jitter["timeline"])
            == expected_retry_events
            and sum(item["retries"] for item in full_jitter["timeline"])
            == expected_retry_events
        ),
        "synchronized_retries_created_a_full_client_spike": (
            no_jitter["peak_retries_in_one_bucket"] == jitter["config"]["clients"]
        ),
        "full_jitter_cut_peak_by_more_than_half": (
            jitter["peak_reduction_percent"] > 50
        ),
        "full_jitter_spread_retries_across_more_buckets": (
            full_jitter["occupied_buckets"] > no_jitter["occupied_buckets"] * 5
        ),
    }

    report = {
        "schema": "inferlab.resilience-check.v0.0.7",
        "original_requests": retry_state["original_requests"],
        "attempts": retry_state["attempts"],
        "transient_failures": retry_state["transient_failures"],
        "retries_granted": retry_state["retries_granted"],
        "retries_denied_budget": retry_state["retries_denied_budget"],
        "retried_successes": retry["summary"]["retried_successes"],
        "deadline_e2e_ms": deadline_sample["e2e_ms"],
        "synchronized_peak_retries": no_jitter["peak_retries_in_one_bucket"],
        "full_jitter_peak_retries": full_jitter["peak_retries_in_one_bucket"],
        "jitter_peak_reduction_percent": jitter["peak_reduction_percent"],
        "checks": checks,
    }
    print(json.dumps(report, indent=2, sort_keys=True))

    if not all(checks.values()):
        raise SystemExit("resilience evidence did not satisfy every check")


if __name__ == "__main__":
    main()
