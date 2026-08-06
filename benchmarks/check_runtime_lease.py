#!/usr/bin/env python3
"""Check retained v0.16 runtime routing-lease claims."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


def load(directory: Path, name: str) -> dict[str, Any]:
    return json.loads((directory / name).read_text())


def assertion(name: str, passed: bool, observed: Any) -> dict[str, Any]:
    return {"name": name, "passed": bool(passed), "observed": observed}


def lease_from_state(observation: dict[str, Any]) -> dict[str, Any]:
    return observation["status"]["body"]["routing_lease"]


def control_from_state(observation: dict[str, Any]) -> dict[str, Any]:
    return observation["status"]["body"]["control_plane"]


def worker_requests(observation: dict[str, Any]) -> int:
    return observation["workers"]["cpu-lease"]["body"]["requests"]


def exact_owned_event(record: dict[str, Any], expected_pids: int) -> bool:
    if expected_pids == 1:
        return (
            record.get("scope") == "owned-child-process"
            and record.get("pid", 0) > 0
        )
    return (
        record.get("scope") == "owned-child-processes"
        and len(record.get("pids", [])) == expected_pids
        and all(pid > 0 for pid in record["pids"])
    )


def one_leader(election: dict[str, Any]) -> bool:
    return sum(
        item.get("status") == 200
        and item.get("body", {}).get("role") == "leader"
        for item in election["statuses"]
    ) == 1


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    evidence = args.evidence_dir

    initial_election = load(evidence, "initial-election.json")
    config = load(evidence, "config-initial.json")["committed"]
    live = load(evidence, "lease-live-fresh.json")
    ready_live = load(evidence, "readiness-live.json")
    request_live = load(evidence, "request-live.json")
    before_crossing = load(evidence, "worker-before-crossing-stream.json")
    stream_crossing = load(evidence, "stream-crossing-expiry.json")
    outage = load(evidence, "control-outage.json")
    expired = load(evidence, "lease-expired-rejecting.json")
    ready_expired = load(evidence, "readiness-expired.json")
    before_rejection = load(evidence, "worker-before-rejection.json")
    rejected = load(evidence, "request-rejected.json")
    after_rejection = load(evidence, "worker-after-rejection.json")
    recovered_election = load(evidence, "recovered-election.json")
    renewed = load(evidence, "lease-renewed.json")
    ready_renewed = load(evidence, "readiness-renewed.json")
    request_renewed = load(evidence, "request-renewed.json")
    gateway_reject_stop = load(evidence, "gateway-reject-stop.json")
    second_outage = load(evidence, "control-second-outage.json")
    stale = load(evidence, "lease-expired-serving-stale.json")
    ready_stale = load(evidence, "readiness-serving-stale.json")
    before_stale = load(evidence, "worker-before-stale-request.json")
    request_stale = load(evidence, "request-serving-stale.json")
    after_stale = load(evidence, "worker-after-stale-request.json")
    stream_final = load(evidence, "stream-final.json")
    gateway_final_stop = load(evidence, "gateway-final-stop.json")
    directory = load(evidence, "snapshot-directory.json")

    revision = config["revision"]
    term = config["term"]
    live_lease = lease_from_state(live)
    expired_lease = lease_from_state(expired)
    renewed_lease = lease_from_state(renewed)
    stale_lease = lease_from_state(stale)
    expected_text = "InferLab turns prompts into real tokens."
    accepted_requests = [request_live, request_renewed, request_stale]
    exact_scope = (
        exact_owned_event(outage, 3)
        and exact_owned_event(gateway_reject_stop, 1)
        and exact_owned_event(second_outage, 3)
        and exact_owned_event(gateway_final_stop, 1)
    )

    assertions = [
        assertion(
            "three-node Raft cluster elects exactly one initial leader",
            one_leader(initial_election),
            {
                "leader": initial_election["leader_id"],
                "term": initial_election["term"],
            },
        ),
        assertion(
            "committed route names the real online-attention CPU worker",
            config["configuration"]["routing_policy"] == "round-robin"
            and [
                worker["id"]
                for worker in config["configuration"]["workers"]
            ]
            == ["cpu-lease"],
            config,
        ),
        assertion(
            "live same-revision verification keeps the runtime lease fresh",
            live_lease["enabled"]
            and live_lease["duration_ms"] == 700
            and live_lease["expiry_action"] == "reject-new"
            and live_lease["state"] == "fresh"
            and live_lease["accepting_new_requests"]
            and live_lease["renewals"] >= 1
            and control_from_state(live)["bootstrap_source"]
            == "live-control-plane",
            live_lease,
        ),
        assertion(
            "fresh readiness and a real-model request succeed",
            ready_live["status"] == 200
            and ready_live["body"]["status"] == "ready"
            and request_live["status"] == 200
            and request_live["config_revision"] == revision
            and request_live["content"] == expected_text,
            {"readiness": ready_live, "request": request_live},
        ),
        assertion(
            "the total control outage targets exactly three harness children",
            exact_owned_event(outage, 3)
            and outage["event"] == "control_cluster_stopped",
            outage,
        ),
        assertion(
            "a stream admitted before the outage finishes after lease expiry",
            stream_crossing["started_at_ms"] < outage["at_ms"]
            and stream_crossing["observed_at_ms"] >= expired["observed_at_ms"]
            and stream_crossing["status"] == 200
            and stream_crossing["done_received"]
            and stream_crossing["config_revision"] == revision
            and stream_crossing["content"] == expected_text
            and worker_requests(before_crossing) + 1
            <= worker_requests(before_rejection),
            {
                "stream": stream_crossing,
                "outage_at_ms": outage["at_ms"],
                "lease_expired_at_ms": expired["observed_at_ms"],
            },
        ),
        assertion(
            "reject-new expiry closes readiness without changing route identity",
            expired_lease["state"] == "expired-rejecting-new"
            and not expired_lease["accepting_new_requests"]
            and expired_lease["expiry_action"] == "reject-new"
            and expired["status"]["body"]["routing_snapshot"][
                "control_revision"
            ]
            == revision
            and expired["status"]["body"]["routing_snapshot"]["control_term"]
            == term
            and ready_expired["status"] == 503
            and ready_expired["body"]["reason"] == "routing_lease_expired",
            {"lease": expired_lease, "readiness": ready_expired},
        ),
        assertion(
            "an expired reject-new request returns structured 503 before routing",
            rejected["status"] == 503
            and rejected["attempts"] == 0
            and rejected["retry_after"] == 1
            and rejected["body"]["error"]["type"]
            == "routing_lease_expired"
            and rejected["body"]["error"]["reason"]
            == "runtime_routing_lease_expired"
            and rejected["body"]["error"]["retryable"] is True,
            rejected,
        ),
        assertion(
            "reject-new expiry causes zero worker attempts",
            worker_requests(before_rejection) == worker_requests(after_rejection),
            {
                "before": worker_requests(before_rejection),
                "after": worker_requests(after_rejection),
            },
        ),
        assertion(
            "persisted Raft nodes recover in a newer term",
            one_leader(recovered_election)
            and recovered_election["term"] > initial_election["term"],
            {
                "leader": recovered_election["leader_id"],
                "term": recovered_election["term"],
            },
        ),
        assertion(
            "valid equal-revision live control renews the expired lease",
            renewed_lease["state"] == "fresh"
            and renewed_lease["renewals"] > expired_lease["renewals"]
            and renewed["status"]["body"]["routing_snapshot"][
                "control_revision"
            ]
            == revision
            and ready_renewed["status"] == 200,
            {"before": expired_lease, "after": renewed_lease},
        ),
        assertion(
            "new real-model traffic resumes after renewal",
            request_renewed["status"] == 200
            and request_renewed["config_revision"] == revision
            and request_renewed["content"] == expected_text,
            request_renewed,
        ),
        assertion(
            "serve-stale is an explicit expired-but-ready operator policy",
            stale_lease["state"] == "expired-serving-stale"
            and stale_lease["expiry_action"] == "serve-stale"
            and stale_lease["accepting_new_requests"]
            and control_from_state(stale)["bootstrap_source"] == "disk-snapshot"
            and ready_stale["status"] == 200
            and ready_stale["body"]["status"] == "ready",
            {"lease": stale_lease, "readiness": ready_stale},
        ),
        assertion(
            "serve-stale admits a new real-model request after expiry",
            request_stale["status"] == 200
            and request_stale["config_revision"] == revision
            and request_stale["content"] == expected_text
            and worker_requests(after_stale) == worker_requests(before_stale) + 1,
            {
                "request": request_stale,
                "worker_before": worker_requests(before_stale),
                "worker_after": worker_requests(after_stale),
            },
        ),
        assertion(
            "final speculative SSE reaches DONE while serving stale",
            stream_final["status"] == 200
            and stream_final["done_received"]
            and stream_final["config_revision"] == revision
            and stream_final["content"] == expected_text
            and stream_final["generation"]["speculation"]["enabled"]
            and stream_final["generation"]["speculation"][
                "target_forward_calls"
            ]
            == 2,
            stream_final,
        ),
        assertion(
            "all process faults are exact and atomic persistence leaves no temp file",
            exact_scope and directory["temporary_snapshot_files"] == [],
            {
                "exact_process_scope": exact_scope,
                "directory": directory,
            },
        ),
        assertion(
            "all three permitted requests and both SSE streams succeed",
            all(request["status"] == 200 for request in accepted_requests)
            and stream_crossing["status"] == 200
            and stream_crossing["done_received"]
            and stream_final["status"] == 200
            and stream_final["done_received"],
            {
                "non_stream_successes": sum(
                    request["status"] == 200 for request in accepted_requests
                ),
                "non_stream_total": len(accepted_requests),
                "streams_succeeded": 2,
            },
        ),
    ]

    passed_count = sum(item["passed"] for item in assertions)
    result = {
        "schema": "inferlab.runtime-routing-lease-check.v0.16",
        "passed": passed_count == len(assertions),
        "assertions_passed": passed_count,
        "assertions_total": len(assertions),
        "revision": revision,
        "term": term,
        "lease_duration_ms": live_lease["duration_ms"],
        "renewals_before_outage": expired_lease["renewals"],
        "renewals_after_recovery": renewed_lease["renewals"],
        "crossing_stream_duration_ms": stream_crossing["duration_ms"],
        "rejected_worker_attempts": rejected["attempts"],
        "non_stream_requests_succeeded": sum(
            request["status"] == 200 for request in accepted_requests
        ),
        "streams_succeeded": 2,
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
