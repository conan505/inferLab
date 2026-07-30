#!/usr/bin/env python3
"""Derive phases, recovery metrics, and chart data from a v0.0.9 chaos run."""

import argparse
import json
import math
from collections import Counter


WORKER_EVENTS = {
    "worker-a": ("worker_a_killed", "worker_a_restarted"),
    "worker-b": ("worker_b_slowed", "worker_b_restored"),
    "worker-c": ("worker_c_disconnected", "worker_c_reconnected"),
}

PHASES = [
    ("healthy-baseline", "traffic_started", "worker_a_killed"),
    ("worker-a-down", "worker_a_killed", "worker_a_restarted"),
    ("worker-a-recovery", "worker_a_restarted", "worker_b_slowed"),
    ("worker-b-slow", "worker_b_slowed", "worker_b_restored"),
    ("worker-b-recovery", "worker_b_restored", "worker_c_disconnected"),
    ("worker-c-disconnected", "worker_c_disconnected", "worker_c_reconnected"),
    ("final-healthy", "worker_c_reconnected", "traffic_completed"),
]


def load_json(path: str) -> dict:
    with open(path, encoding="utf-8") as source:
        return json.load(source)


def load_events(path: str) -> list[dict]:
    with open(path, encoding="utf-8") as source:
        return [json.loads(line) for line in source if line.strip()]


def percentile(values: list[float], quantile: float):
    if not values:
        return None
    ordered = sorted(values)
    index = max(0, math.ceil(len(ordered) * quantile) - 1)
    return round(ordered[index], 3)


def event_map(events: list[dict]) -> dict[str, float]:
    return {event["event"]: event["elapsed_ms"] for event in events}


def summarize_requests(requests: list[dict]) -> dict:
    successful = [request for request in requests if request["status"] == 200]
    statuses = Counter(request["status"] for request in requests)
    workers = Counter(
        request["worker"]
        for request in successful
        if request["worker"] is not None
    )
    attempts = [
        request["attempts"]
        for request in requests
        if request.get("attempts") is not None
    ]
    latencies = [request["e2e_ms"] for request in requests]
    return {
        "requests": len(requests),
        "successful": len(successful),
        "errors": len(requests) - len(successful),
        "success_rate_percent": (
            round(len(successful) / len(requests) * 100, 3)
            if requests
            else None
        ),
        "status_counts": {
            ("transport" if status is None else str(status)): count
            for status, count in sorted(
                statuses.items(), key=lambda item: str(item[0])
            )
        },
        "worker_counts": dict(sorted(workers.items())),
        "attempts": sum(attempts),
        "retry_amplification": (
            round(sum(attempts) / len(requests), 4) if requests else None
        ),
        "latency_ms": {
            "p50": percentile(latencies, 0.50),
            "p95": percentile(latencies, 0.95),
            "p99": percentile(latencies, 0.99),
            "max": round(max(latencies), 3) if latencies else None,
        },
    }


def worker_circuit(sample: dict, worker_id: str):
    if "error" in sample:
        return None
    return next(
        worker["circuit"]
        for worker in sample["workers"]
        if worker["id"] == worker_id
    )


def first_open_after(
    samples: list[dict], worker_id: str, start_ms: float, end_ms: float
):
    for sample in samples:
        if not start_ms <= sample["elapsed_ms"] < end_ms:
            continue
        circuit = worker_circuit(sample, worker_id)
        if circuit is not None and circuit["state"] in {"open", "half-open"}:
            return sample
    return None


def first_recovered_after(
    samples: list[dict],
    worker_id: str,
    start_ms: float,
    end_ms: float,
    previous_recoveries: int,
):
    for sample in samples:
        if not start_ms <= sample["elapsed_ms"] < end_ms:
            continue
        circuit = worker_circuit(sample, worker_id)
        if (
            circuit is not None
            and circuit["state"] == "closed"
            and circuit["recoveries_total"] > previous_recoveries
        ):
            return sample
    return None


def last_circuit_before(
    samples: list[dict], worker_id: str, elapsed_ms: float
) -> dict:
    matching = [
        worker_circuit(sample, worker_id)
        for sample in samples
        if sample["elapsed_ms"] < elapsed_ms and "error" not in sample
    ]
    return matching[-1] if matching else {"recoveries_total": 0}


def first_failover_success(
    requests: list[dict],
    failed_worker: str,
    start_ms: float,
    end_ms: float,
):
    candidates = [
        request
        for request in requests
        if (
            start_ms <= request["scheduled_ms"] < end_ms
            and request["status"] == 200
            and request["worker"] != failed_worker
        )
    ]
    return min(candidates, key=lambda request: request["completed_ms"]) if candidates else None


def timeline_bins(requests: list[dict], duration_ms: float) -> list[dict]:
    bin_width_ms = 500.0
    bin_count = math.ceil(duration_ms / bin_width_ms)
    bins = []
    for index in range(bin_count):
        start = index * bin_width_ms
        end = min(duration_ms, start + bin_width_ms)
        selected = [
            request
            for request in requests
            if start <= request["scheduled_ms"] < end
        ]
        summary = summarize_requests(selected)
        bins.append(
            {
                "start_ms": round(start, 3),
                "end_ms": round(end, 3),
                "successful": summary["successful"],
                "errors": summary["errors"],
                "attempts": summary["attempts"],
                "p95_latency_ms": summary["latency_ms"]["p95"],
            }
        )
    return bins


def circuit_segments(
    samples: list[dict], worker_id: str, duration_ms: float
) -> list[dict]:
    points = []
    for sample in samples:
        circuit = worker_circuit(sample, worker_id)
        if circuit is not None:
            points.append(
                {
                    "elapsed_ms": sample["elapsed_ms"],
                    "state": circuit["state"],
                }
            )
    if not points:
        return []
    segments = []
    current_state = points[0]["state"]
    current_start = 0.0
    for point in points[1:]:
        if point["state"] != current_state:
            segments.append(
                {
                    "start_ms": round(current_start, 3),
                    "end_ms": round(point["elapsed_ms"], 3),
                    "state": current_state,
                }
            )
            current_start = point["elapsed_ms"]
            current_state = point["state"]
    segments.append(
        {
            "start_ms": round(current_start, 3),
            "end_ms": round(duration_ms, 3),
            "state": current_state,
        }
    )
    return segments


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--run", required=True)
    parser.add_argument("--events", required=True)
    args = parser.parse_args()

    run = load_json(args.run)
    events = load_events(args.events)
    event_times = event_map(events)
    requests = run["requests"]
    samples = run["gateway_samples"]
    duration_ms = event_times["traffic_completed"]

    phase_summaries = {}
    for name, start_event, end_event in PHASES:
        start_ms = event_times[start_event]
        end_ms = event_times[end_event]
        selected = [
            request
            for request in requests
            if start_ms <= request["scheduled_ms"] < end_ms
        ]
        phase_summaries[name] = {
            "start_ms": start_ms,
            "end_ms": end_ms,
            **summarize_requests(selected),
        }

    recovery = {}
    ordered_faults = [
        ("worker-a", "worker_b_slowed"),
        ("worker-b", "worker_c_disconnected"),
        ("worker-c", "traffic_completed"),
    ]
    for worker_id, recovery_end_event in ordered_faults:
        fault_event, heal_event = WORKER_EVENTS[worker_id]
        fault_ms = event_times[fault_event]
        heal_ms = event_times[heal_event]
        recovery_end_ms = event_times[recovery_end_event]
        before = last_circuit_before(samples, worker_id, fault_ms)
        opened = first_open_after(samples, worker_id, fault_ms, heal_ms)
        recovered = first_recovered_after(
            samples,
            worker_id,
            heal_ms,
            recovery_end_ms,
            before["recoveries_total"],
        )
        failover = first_failover_success(
            requests, worker_id, fault_ms, heal_ms
        )
        recovery[worker_id] = {
            "fault_event": fault_event,
            "heal_event": heal_event,
            "detection_ms": (
                round(opened["elapsed_ms"] - fault_ms, 3)
                if opened is not None
                else None
            ),
            "failover_ms": (
                round(failover["completed_ms"] - fault_ms, 3)
                if failover is not None
                else None
            ),
            "recovery_ms": (
                round(recovered["elapsed_ms"] - heal_ms, 3)
                if recovered is not None
                else None
            ),
            "mttr_ms": (
                round(recovered["elapsed_ms"] - fault_ms, 3)
                if recovered is not None
                else None
            ),
            "opened_at_ms": (
                opened["elapsed_ms"] if opened is not None else None
            ),
            "recovered_at_ms": (
                recovered["elapsed_ms"] if recovered is not None else None
            ),
        }

    valid_gateway_samples = [
        sample for sample in samples if "error" not in sample
    ]
    rss_values = [
        sample["rss_kib"]
        for sample in valid_gateway_samples
        if sample["rss_kib"] is not None
    ]
    final_status = run["gateway_status_after"]
    resilience = final_status["resilience"]
    admission = final_status["admission"]
    final_circuits = {
        worker["id"]: worker["circuit"] for worker in final_status["workers"]
    }
    analysis = {
        "schema": "inferlab.chaos-analysis.v0.0.9",
        "environment": run["environment"],
        "config": run["config"],
        "events": events,
        "duration_ms": duration_ms,
        "overall": summarize_requests(requests),
        "dispatch_lag_p99_ms": run["summary"]["dispatch_lag_ms"]["p99"],
        "phases": phase_summaries,
        "recovery": recovery,
        "mean_mttr_ms": round(
            sum(item["mttr_ms"] for item in recovery.values())
            / len(recovery),
            3,
        ),
        "bounds": {
            "configured_queue_capacity": admission["queue_capacity"],
            "configured_execution_capacity": admission[
                "worker_execution_capacity"
            ],
            "configured_outstanding_capacity": admission[
                "outstanding_capacity"
            ],
            "max_observed_queued": admission["max_observed_queued"],
            "max_observed_executing": admission["max_observed_executing"],
            "max_observed_outstanding": admission[
                "max_observed_outstanding"
            ],
            "request_deadline_ms": resilience["request_deadline_ms"],
            "maximum_client_latency_ms": max(
                request["e2e_ms"] for request in requests
            ),
            "rss_first_kib": rss_values[0] if rss_values else None,
            "rss_max_kib": max(rss_values) if rss_values else None,
            "rss_max_increase_kib": (
                max(rss_values) - rss_values[0] if rss_values else None
            ),
        },
        "retry_accounting": {
            "original_requests": resilience["original_requests"],
            "upstream_attempts": resilience["attempts"],
            "retries_granted": resilience["retries_granted"],
            "retries_denied_budget": resilience[
                "retries_denied_budget"
            ],
            "retry_budget_percent": resilience[
                "retry_budget_percent"
            ],
            "amplification": round(
                resilience["attempts"] / resilience["original_requests"], 4
            ),
        },
        "final_circuits": final_circuits,
        "gateway_status_sample_errors": run["summary"][
            "gateway_status_sample_errors"
        ],
        "timeline_bins": timeline_bins(requests, duration_ms),
        "circuit_segments": {
            worker_id: circuit_segments(samples, worker_id, duration_ms)
            for worker_id in WORKER_EVENTS
        },
    }
    print(json.dumps(analysis, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
