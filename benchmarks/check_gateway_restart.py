#!/usr/bin/env python3
"""Check retained v0.14 restart-safe routing snapshot claims."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


def load(directory: Path, name: str) -> dict:
    return json.loads((directory / name).read_text())


def assertion(name: str, passed: bool, observed) -> dict:
    return {"name": name, "passed": bool(passed), "observed": observed}


def status_body(observation: dict) -> dict:
    return observation["status"]["body"]


def revisions(request_set: dict) -> list[int | None]:
    return [request.get("config_revision") for request in request_set["requests"]]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    evidence = args.evidence_dir

    initial_election = load(evidence, "initial-election.json")
    initial_config = load(evidence, "config-initial.json")["committed"]
    gateway_live = load(evidence, "gateway-live.json")
    snapshot_initial = load(evidence, "snapshot-initial.json")
    requests_live = load(evidence, "requests-live.json")
    gateway_first_stop = load(evidence, "gateway-first-stop.json")
    first_outage = load(evidence, "control-first-outage.json")
    gateway_offline = load(evidence, "gateway-offline.json")
    requests_offline = load(evidence, "requests-offline.json")
    recovered_election = load(evidence, "recovered-election.json")
    updated_config = load(evidence, "config-updated.json")["committed"]
    gateway_reconciled = load(evidence, "gateway-reconciled.json")
    snapshot_updated = load(evidence, "snapshot-updated.json")
    requests_weighted = load(evidence, "requests-weighted.json")
    gateway_second_stop = load(evidence, "gateway-second-stop.json")
    second_outage = load(evidence, "control-second-outage.json")
    stale_control = load(evidence, "stale-control-config.json")["committed"]
    gateway_stale = load(evidence, "gateway-stale-control.json")
    stream = load(evidence, "stream-final.json")
    divergent = load(evidence, "divergent-bootstrap.json")
    third_outage = load(evidence, "control-third-outage.json")
    corrupt = load(evidence, "corrupt-bootstrap.json")
    directory = load(evidence, "snapshot-directory.json")

    initial_revision = initial_config["revision"]
    updated_revision = updated_config["revision"]
    live_body = status_body(gateway_live)
    offline_body = status_body(gateway_offline)
    reconciled_body = status_body(gateway_reconciled)
    stale_body = status_body(gateway_stale)
    live_control = live_body["control_plane"]
    offline_control = offline_body["control_plane"]
    reconciled_control = reconciled_body["control_plane"]
    stale_status = stale_body["control_plane"]
    initial_workers = {
        worker["id"] for worker in initial_config["configuration"]["workers"]
    }
    updated_workers = {
        worker["id"] for worker in updated_config["configuration"]["workers"]
    }
    request_sets = [requests_live, requests_offline, requests_weighted]
    all_requests = [
        request for request_set in request_sets for request in request_set["requests"]
    ]
    successful_requests = sum(request.get("status") == 200 for request in all_requests)
    heavy_worker = updated_config["configuration"]["workers"][0]["id"]
    light_worker = updated_config["configuration"]["workers"][1]["id"]

    event_records = [
        gateway_first_stop,
        first_outage,
        gateway_second_stop,
        second_outage,
        third_outage,
    ]
    exact_fault_scope = all(
        record["scope"] in {"owned-child-process", "owned-child-processes"}
        and (
            record.get("pid", 1) > 0
            if "pid" in record
            else len(record.get("pids", [])) == 3
            and all(pid > 0 for pid in record["pids"])
        )
        for record in event_records
    )

    assertions = [
        assertion(
            "three-node Raft cluster elects one initial leader",
            initial_election["leader_id"] in {"node-a", "node-b", "node-c"}
            and sum(
                item.get("status") == 200
                and item.get("body", {}).get("role") == "leader"
                for item in initial_election["statuses"]
            )
            == 1,
            {
                "leader": initial_election["leader_id"],
                "term": initial_election["term"],
            },
        ),
        assertion(
            "initial committed configuration names two real CPU workers",
            initial_config["configuration"]["routing_policy"] == "round-robin"
            and initial_workers == {"cpu-restart-a", "cpu-restart-b"},
            initial_config,
        ),
        assertion(
            "live startup persists its applied revision before serving",
            live_control["bootstrap_source"] == "live-control-plane"
            and live_body["routing_snapshot"]["control_revision"]
            == initial_revision
            and live_control["persisted_revision"] == initial_revision
            and live_control["persisted_at_ms"] is not None,
            {
                "boot_latency_ms": gateway_live["boot_latency_ms"],
                "routing": live_body["routing_snapshot"],
                "control": live_control,
            },
        ),
        assertion(
            "initial disk document is versioned and matches the committed configuration",
            snapshot_initial["schema"]
            == "inferlab.gateway-routing-snapshot.v1"
            and snapshot_initial["revision"] == initial_revision
            and snapshot_initial["term"] == initial_config["term"]
            and snapshot_initial["configuration"]
            == initial_config["configuration"],
            snapshot_initial,
        ),
        assertion(
            "live-start requests carry the initial revision",
            requests_live["succeeded"] == requests_live["requested"] == 2
            and revisions(requests_live) == [initial_revision] * 2,
            {
                "succeeded": requests_live["succeeded"],
                "revisions": revisions(requests_live),
            },
        ),
        assertion(
            "gateway and control outages target only exact harness children",
            exact_fault_scope,
            event_records,
        ),
        assertion(
            "gateway restarts from disk while every control node is unavailable",
            offline_control["bootstrap_source"] == "disk-snapshot"
            and offline_control["source_url"] is None
            and offline_body["routing_snapshot"]["control_revision"]
            == initial_revision
            and offline_control["persisted_revision"] == initial_revision,
            {
                "boot_latency_ms": gateway_offline["boot_latency_ms"],
                "routing": offline_body["routing_snapshot"],
                "control": offline_control,
            },
        ),
        assertion(
            "all real-model requests succeed during the full control-plane outage",
            requests_offline["succeeded"] == requests_offline["requested"] == 4
            and revisions(requests_offline) == [initial_revision] * 4,
            {
                "succeeded": requests_offline["succeeded"],
                "revisions": revisions(requests_offline),
                "workers": [item["worker"] for item in requests_offline["requests"]],
            },
        ),
        assertion(
            "the persisted Raft cluster elects a leader after all three nodes restart",
            recovered_election["leader_id"] in {"node-a", "node-b", "node-c"}
            and recovered_election["term"] > initial_election["term"],
            {
                "leader": recovered_election["leader_id"],
                "term": recovered_election["term"],
            },
        ),
        assertion(
            "a newer committed weighted configuration is strictly monotonic",
            updated_revision > initial_revision
            and updated_config["configuration"]["routing_policy"]
            == "weighted-round-robin"
            and updated_workers == initial_workers,
            updated_config,
        ),
        assertion(
            "the running gateway persists the newer revision before applying it",
            reconciled_body["routing_snapshot"]["control_revision"]
            == updated_revision
            and reconciled_control["persisted_revision"] == updated_revision
            and reconciled_control["source_url"] is not None
            and reconciled_control["last_error"] is None,
            {
                "routing": reconciled_body["routing_snapshot"],
                "control": reconciled_control,
            },
        ),
        assertion(
            "the replacement disk document contains the newer committed revision",
            snapshot_updated["schema"] == snapshot_initial["schema"]
            and snapshot_updated["revision"] == updated_revision
            and snapshot_updated["term"] == updated_config["term"]
            and snapshot_updated["configuration"]
            == updated_config["configuration"]
            and snapshot_updated["saved_at_ms"]
            >= snapshot_initial["saved_at_ms"],
            snapshot_updated,
        ),
        assertion(
            "three-to-one weights still produce an exact six-to-two schedule",
            requests_weighted["succeeded"] == requests_weighted["requested"] == 8
            and requests_weighted["worker_counts"].get(heavy_worker) == 6
            and requests_weighted["worker_counts"].get(light_worker) == 2
            and revisions(requests_weighted) == [updated_revision] * 8,
            {
                "counts": requests_weighted["worker_counts"],
                "revisions": revisions(requests_weighted),
            },
        ),
        assertion(
            "a stale live control cluster cannot roll the durable gateway backward",
            stale_control["revision"] == initial_revision
            and updated_revision > stale_control["revision"]
            and stale_body["routing_snapshot"]["control_revision"]
            == updated_revision
            and stale_status["bootstrap_source"] == "disk-snapshot"
            and stale_status["persisted_revision"] == updated_revision
            and "ignored stale control-plane revision"
            in (stale_status["last_error"] or ""),
            {
                "stale_control_revision": stale_control["revision"],
                "routing_revision": stale_body["routing_snapshot"][
                    "control_revision"
                ],
                "boot_latency_ms": gateway_stale["boot_latency_ms"],
                "last_error": stale_status["last_error"],
            },
        ),
        assertion(
            "final speculative SSE uses the newer revision and reaches DONE",
            stream["status"] == 200
            and stream["done_received"]
            and stream["config_revision"] == updated_revision
            and stream["content"] == "InferLab turns prompts into real tokens."
            and stream["generation"]["speculation"]["enabled"]
            and stream["generation"]["speculation"]["target_forward_calls"]
            == 2,
            {
                "revision": stream["config_revision"],
                "worker": stream["worker"],
                "done": stream["done_received"],
                "speculation": stream["generation"]["speculation"],
            },
        ),
        assertion(
            "equal-revision divergent content fails closed",
            divergent["exit_code"] != 0
            and "durable snapshot disagree at routing revision"
            in divergent["log"],
            divergent,
        ),
        assertion(
            "corrupt disk state fails closed when no control node is available",
            corrupt["exit_code"] != 0
            and "no valid durable routing snapshot" in corrupt["log"]
            and "cannot decode routing snapshot" in corrupt["log"],
            corrupt,
        ),
        assertion(
            "atomic replacement leaves no temporary routing-snapshot file",
            directory["temporary_snapshot_files"] == [],
            directory,
        ),
        assertion(
            "all fourteen non-stream requests and final SSE succeed",
            len(all_requests) == 14
            and successful_requests == 14
            and stream["status"] == 200
            and stream["done_received"],
            {
                "non_stream_successes": successful_requests,
                "non_stream_total": len(all_requests),
                "stream_status": stream["status"],
            },
        ),
    ]

    passed_count = sum(item["passed"] for item in assertions)
    result = {
        "passed": passed_count == len(assertions),
        "assertions_passed": passed_count,
        "assertions_total": len(assertions),
        "initial_revision": initial_revision,
        "updated_revision": updated_revision,
        "stale_control_revision": stale_control["revision"],
        "live_boot_latency_ms": gateway_live["boot_latency_ms"],
        "offline_boot_latency_ms": gateway_offline["boot_latency_ms"],
        "stale_guard_boot_latency_ms": gateway_stale["boot_latency_ms"],
        "weighted_distribution": requests_weighted["worker_counts"],
        "non_stream_requests_succeeded": successful_requests,
        "stream_succeeded": stream["status"] == 200 and stream["done_received"],
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
