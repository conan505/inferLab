#!/usr/bin/env python3
"""Check retained v0.17 control-cluster identity fencing claims."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


PRIMARY = "inferlab-primary"
FOREIGN = "inferlab-foreign"
EXPECTED_TEXT = "InferLab turns prompts into real tokens."


def load(directory: Path, name: str) -> dict[str, Any]:
    return json.loads((directory / name).read_text())


def assertion(name: str, passed: bool, observed: Any) -> dict[str, Any]:
    return {"name": name, "passed": bool(passed), "observed": observed}


def status_body(state: dict[str, Any]) -> dict[str, Any]:
    return state["status"]["body"]


def worker_requests(observation: dict[str, Any], worker_id: str) -> int:
    return observation["workers"][worker_id]["body"]["requests"]


def one_leader(election: dict[str, Any], cluster_id: str) -> bool:
    return (
        sum(
            item.get("status") == 200
            and item.get("body", {}).get("role") == "leader"
            for item in election["statuses"]
        )
        == 1
        and all(
            item.get("body", {}).get("cluster_id") == cluster_id
            for item in election["statuses"]
            if item.get("status") == 200
        )
    )


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


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    evidence = args.evidence_dir

    initial = load(evidence, "initial-primary-election.json")
    primary_config = load(evidence, "config-primary.json")["committed"]
    gateway_primary = load(evidence, "gateway-primary-fresh.json")
    snapshot_primary = load(evidence, "snapshot-primary.json")
    request_primary = load(evidence, "request-primary.json")
    before_stream = load(evidence, "worker-primary-before-stream.json")
    stream_crossing = load(evidence, "stream-crossing-foreign-cluster.json")
    primary_outage = load(evidence, "primary-control-outage.json")
    foreign_election = load(evidence, "foreign-election.json")
    foreign_config = load(evidence, "config-foreign.json")["committed"]
    gateway_foreign = load(evidence, "gateway-foreign-rejected.json")
    readiness_foreign = load(evidence, "readiness-foreign-rejected.json")
    primary_before_reject = load(evidence, "worker-primary-before-rejection.json")
    primary_after_reject = load(evidence, "worker-primary-after-rejection.json")
    foreign_before_reject = load(evidence, "worker-foreign-before-rejection.json")
    foreign_after_reject = load(evidence, "worker-foreign-after-rejection.json")
    request_rejected = load(evidence, "request-foreign-rejected.json")
    foreign_stop = load(evidence, "foreign-control-stop.json")
    recovered = load(evidence, "recovered-primary-election.json")
    gateway_renewed = load(evidence, "gateway-primary-renewed.json")
    readiness_renewed = load(evidence, "readiness-primary-renewed.json")
    request_renewed = load(evidence, "request-primary-renewed.json")
    gateway_primary_stop = load(evidence, "gateway-primary-stop.json")
    primary_second_stop = load(evidence, "primary-control-second-stop.json")
    fixture = load(evidence, "foreign-snapshot-fixture.json")
    bootstrap_rejected = load(evidence, "foreign-disk-bootstrap-rejected.json")
    second_recovered = load(evidence, "second-recovered-primary-election.json")
    gateway_repair = load(evidence, "gateway-live-repair.json")
    snapshot_repaired = load(evidence, "snapshot-live-repaired.json")
    request_repair = load(evidence, "request-live-repair.json")
    stream_final = load(evidence, "stream-final.json")
    gateway_final_stop = load(evidence, "gateway-final-stop.json")
    primary_final_stop = load(evidence, "primary-control-final-stop.json")
    directory = load(evidence, "snapshot-directory.json")

    revision = primary_config["revision"]
    primary_body = status_body(gateway_primary)
    foreign_body = status_body(gateway_foreign)
    renewed_body = status_body(gateway_renewed)
    repair_body = status_body(gateway_repair)
    foreign_lease = foreign_body["routing_lease"]
    accepted_requests = [request_primary, request_renewed, request_repair]
    events = [
        primary_outage,
        foreign_stop,
        gateway_primary_stop,
        primary_second_stop,
        gateway_final_stop,
        primary_final_stop,
    ]
    exact_scope = all(
        exact_owned_event(event, 1 if "pid" in event else 3) for event in events
    )

    assertions = [
        assertion(
            "primary Raft nodes elect one leader under one cluster identity",
            one_leader(initial, PRIMARY),
            {
                "leader": initial["leader_id"],
                "term": initial["term"],
                "cluster_ids": [
                    item["body"]["cluster_id"] for item in initial["statuses"]
                ],
            },
        ),
        assertion(
            "primary committed configuration carries cluster identity",
            primary_config["cluster_id"] == PRIMARY
            and primary_config["configuration"]["routing_policy"]
            == "round-robin"
            and [
                worker["id"]
                for worker in primary_config["configuration"]["workers"]
            ]
            == ["cpu-primary"],
            primary_config,
        ),
        assertion(
            "gateway publishes expected primary identity in state, disk, and response",
            primary_body["control_plane"]["expected_cluster_id"] == PRIMARY
            and primary_body["control_plane"]["cluster_mismatch_rejections"] == 0
            and primary_body["routing_snapshot"]["control_cluster_id"] == PRIMARY
            and snapshot_primary["cluster_id"] == PRIMARY
            and request_primary["status"] == 200
            and request_primary["control_cluster_id"] == PRIMARY
            and request_primary["content"] == EXPECTED_TEXT,
            {
                "control": primary_body["control_plane"],
                "routing": primary_body["routing_snapshot"],
                "request": request_primary,
            },
        ),
        assertion(
            "foreign Raft cluster can independently reach the same revision number",
            one_leader(foreign_election, FOREIGN)
            and foreign_config["cluster_id"] == FOREIGN
            and foreign_config["revision"] == revision
            and [
                worker["id"]
                for worker in foreign_config["configuration"]["workers"]
            ]
            == ["cpu-foreign"],
            {"election": foreign_election, "configuration": foreign_config},
        ),
        assertion(
            "primary outage and foreign replacement target exact child processes",
            exact_owned_event(primary_outage, 3)
            and primary_outage["cluster_id"] == PRIMARY
            and exact_owned_event(foreign_stop, 3)
            and foreign_stop["cluster_id"] == FOREIGN,
            {"primary_outage": primary_outage, "foreign_stop": foreign_stop},
        ),
        assertion(
            "existing primary stream finishes after the foreign identity is rejected",
            stream_crossing["started_at_ms"] < primary_outage["at_ms"]
            and stream_crossing["observed_at_ms"] >= gateway_foreign["observed_at_ms"]
            and stream_crossing["status"] == 200
            and stream_crossing["done_received"]
            and stream_crossing["control_cluster_id"] == PRIMARY
            and stream_crossing["content"] == EXPECTED_TEXT
            and worker_requests(before_stream, "cpu-primary") + 1
            <= worker_requests(primary_before_reject, "cpu-primary"),
            {
                "stream": stream_crossing,
                "foreign_rejected_at_ms": gateway_foreign["observed_at_ms"],
            },
        ),
        assertion(
            "gateway records foreign identity but preserves the primary route",
            foreign_body["control_plane"]["expected_cluster_id"] == PRIMARY
            and foreign_body["control_plane"]["last_rejected_cluster_id"]
            == FOREIGN
            and foreign_body["control_plane"]["cluster_mismatch_rejections"] > 0
            and "identity mismatch" in foreign_body["control_plane"]["last_error"]
            and foreign_body["routing_snapshot"]["control_cluster_id"] == PRIMARY
            and foreign_body["routing_snapshot"]["control_revision"] == revision,
            {
                "control": foreign_body["control_plane"],
                "routing": foreign_body["routing_snapshot"],
            },
        ),
        assertion(
            "foreign responses do not renew the primary runtime lease",
            foreign_lease["state"] == "expired-rejecting-new"
            and not foreign_lease["accepting_new_requests"]
            and readiness_foreign["status"] == 503
            and readiness_foreign["body"]["reason"] == "routing_lease_expired",
            {"lease": foreign_lease, "readiness": readiness_foreign},
        ),
        assertion(
            "new request is rejected before either primary or foreign worker",
            request_rejected["status"] == 503
            and request_rejected["attempts"] == 0
            and request_rejected["body"]["error"]["type"]
            == "routing_lease_expired"
            and worker_requests(primary_before_reject, "cpu-primary")
            == worker_requests(primary_after_reject, "cpu-primary")
            and worker_requests(foreign_before_reject, "cpu-foreign")
            == worker_requests(foreign_after_reject, "cpu-foreign")
            == 0,
            {
                "request": request_rejected,
                "primary_before": worker_requests(
                    primary_before_reject, "cpu-primary"
                ),
                "primary_after": worker_requests(
                    primary_after_reject, "cpu-primary"
                ),
                "foreign_before": worker_requests(
                    foreign_before_reject, "cpu-foreign"
                ),
                "foreign_after": worker_requests(
                    foreign_after_reject, "cpu-foreign"
                ),
            },
        ),
        assertion(
            "persisted primary cluster recovers in a newer leadership term",
            one_leader(recovered, PRIMARY) and recovered["term"] > initial["term"],
            {
                "leader": recovered["leader_id"],
                "term": recovered["term"],
                "cluster_id": PRIMARY,
            },
        ),
        assertion(
            "expected primary identity renews the same routing snapshot",
            renewed_body["routing_lease"]["state"] == "fresh"
            and renewed_body["routing_snapshot"]["control_cluster_id"] == PRIMARY
            and renewed_body["routing_snapshot"]["control_revision"] == revision
            and renewed_body["control_plane"]["last_error"] is None
            and readiness_renewed["status"] == 200
            and request_renewed["status"] == 200
            and request_renewed["control_cluster_id"] == PRIMARY
            and request_renewed["content"] == EXPECTED_TEXT,
            {
                "gateway": renewed_body,
                "request": request_renewed,
            },
        ),
        assertion(
            "wrong-cluster disk fixture changes identity without changing route content",
            fixture["original_cluster_id"] == PRIMARY
            and fixture["mutated_cluster_id"] == FOREIGN
            and fixture["revision"] == revision
            and fixture["configuration"] == primary_config["configuration"],
            fixture,
        ),
        assertion(
            "foreign disk cannot bootstrap a gateway expecting primary",
            bootstrap_rejected["exit_code"] != 0
            and "control cluster identity mismatch" in bootstrap_rejected["log"]
            and PRIMARY in bootstrap_rejected["log"]
            and FOREIGN in bootstrap_rejected["log"],
            bootstrap_rejected,
        ),
        assertion(
            "expected live primary repairs a foreign-identity disk snapshot",
            one_leader(second_recovered, PRIMARY)
            and repair_body["control_plane"]["bootstrap_source"]
            == "live-control-plane"
            and repair_body["routing_snapshot"]["control_cluster_id"] == PRIMARY
            and snapshot_repaired["cluster_id"] == PRIMARY
            and snapshot_repaired["revision"] == revision
            and snapshot_repaired["configuration"]
            == primary_config["configuration"],
            {
                "election_term": second_recovered["term"],
                "gateway": repair_body,
                "snapshot": snapshot_repaired,
            },
        ),
        assertion(
            "live-repair request preserves primary cluster identity",
            request_repair["status"] == 200
            and request_repair["control_cluster_id"] == PRIMARY
            and request_repair["config_revision"] == revision
            and request_repair["content"] == EXPECTED_TEXT,
            request_repair,
        ),
        assertion(
            "final speculative SSE reaches DONE under primary identity",
            stream_final["status"] == 200
            and stream_final["done_received"]
            and stream_final["control_cluster_id"] == PRIMARY
            and stream_final["config_revision"] == revision
            and stream_final["content"] == EXPECTED_TEXT
            and stream_final["generation"]["speculation"]["enabled"],
            stream_final,
        ),
        assertion(
            "all process faults are exact and atomic snapshots leave no temp file",
            exact_scope and directory["temporary_snapshot_files"] == [],
            {"exact_process_scope": exact_scope, "directory": directory},
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
        "schema": "inferlab.control-cluster-identity-check.v0.17",
        "passed": passed_count == len(assertions),
        "assertions_passed": passed_count,
        "assertions_total": len(assertions),
        "expected_cluster_id": PRIMARY,
        "rejected_cluster_id": FOREIGN,
        "revision": revision,
        "primary_initial_term": initial["term"],
        "primary_recovered_term": recovered["term"],
        "cluster_mismatch_rejections": foreign_body["control_plane"][
            "cluster_mismatch_rejections"
        ],
        "crossing_stream_duration_ms": stream_crossing["duration_ms"],
        "rejected_worker_attempts": request_rejected["attempts"],
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
