#!/usr/bin/env python3
"""Check retained v0.15 bounded-age routing snapshot claims."""

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


def exact_owned_event(record: dict, expected_pids: int) -> bool:
    if expected_pids == 1:
        return (
            record["scope"] == "owned-child-process"
            and record.get("pid", 0) > 0
        )
    return (
        record["scope"] == "owned-child-processes"
        and len(record.get("pids", [])) == expected_pids
        and all(pid > 0 for pid in record["pids"])
    )


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
    gateway_fresh = load(evidence, "gateway-fresh-disk.json")
    requests_fresh = load(evidence, "requests-fresh-disk.json")
    gateway_second_stop = load(evidence, "gateway-second-stop.json")
    expired_fixture = load(evidence, "expired-fixture.json")
    expired_bootstrap = load(evidence, "expired-bootstrap.json")
    future_fixture = load(evidence, "future-fixture.json")
    future_bootstrap = load(evidence, "future-bootstrap.json")
    recovered_election = load(evidence, "recovered-election.json")
    gateway_repair = load(evidence, "gateway-live-repair.json")
    snapshot_repaired = load(evidence, "snapshot-repaired.json")
    requests_repair = load(evidence, "requests-live-repair.json")
    stream = load(evidence, "stream-final.json")
    gateway_final_stop = load(evidence, "gateway-final-stop.json")
    final_outage = load(evidence, "control-final-outage.json")
    directory = load(evidence, "snapshot-directory.json")

    revision = initial_config["revision"]
    live_body = status_body(gateway_live)
    fresh_body = status_body(gateway_fresh)
    repair_body = status_body(gateway_repair)
    live_control = live_body["control_plane"]
    fresh_control = fresh_body["control_plane"]
    repair_control = repair_body["control_plane"]
    maximum_age_ms = expired_fixture["maximum_age_ms"]
    maximum_future_skew_ms = future_fixture["maximum_future_skew_ms"]
    request_sets = [requests_live, requests_fresh, requests_repair]
    all_requests = [
        request for request_set in request_sets for request in request_set["requests"]
    ]
    successful_requests = sum(request.get("status") == 200 for request in all_requests)
    expected_workers = {"cpu-fresh-a", "cpu-fresh-b"}

    event_scope = (
        exact_owned_event(gateway_first_stop, 1)
        and exact_owned_event(first_outage, 3)
        and exact_owned_event(gateway_second_stop, 1)
        and exact_owned_event(gateway_final_stop, 1)
        and exact_owned_event(final_outage, 3)
    )

    assertions = [
        assertion(
            "three-node Raft cluster elects exactly one initial leader",
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
            "committed route names two real online-attention CPU workers",
            initial_config["configuration"]["routing_policy"] == "round-robin"
            and {
                worker["id"]
                for worker in initial_config["configuration"]["workers"]
            }
            == expected_workers,
            initial_config,
        ),
        assertion(
            "live bootstrap exposes the configured age and skew policy",
            live_control["bootstrap_source"] == "live-control-plane"
            and live_control["snapshot_max_age_ms"] == maximum_age_ms
            and live_control["snapshot_max_future_skew_ms"]
            == maximum_future_skew_ms
            and live_control["bootstrap_snapshot_age_ms"] is None
            and live_control["persisted_revision"] == revision
            and live_control["persisted_expires_at_ms"]
            - live_control["persisted_at_ms"]
            == maximum_age_ms,
            live_control,
        ),
        assertion(
            "initial durable document exactly matches the committed identity",
            snapshot_initial["schema"]
            == "inferlab.gateway-routing-snapshot.v1"
            and snapshot_initial["revision"] == revision
            and snapshot_initial["term"] == initial_config["term"]
            and snapshot_initial["configuration"]
            == initial_config["configuration"],
            snapshot_initial,
        ),
        assertion(
            "all scripted process stops target exact harness children",
            event_scope,
            [
                gateway_first_stop,
                first_outage,
                gateway_second_stop,
                gateway_final_stop,
                final_outage,
            ],
        ),
        assertion(
            "fresh disk snapshot remains eligible during total control outage",
            fresh_control["bootstrap_source"] == "disk-snapshot"
            and fresh_control["source_url"] is None
            and isinstance(fresh_control["bootstrap_snapshot_age_ms"], int)
            and 0 <= fresh_control["bootstrap_snapshot_age_ms"] <= maximum_age_ms
            and fresh_control["persisted_expires_at_ms"]
            == fresh_control["persisted_at_ms"] + maximum_age_ms
            and fresh_body["routing_snapshot"]["control_revision"] == revision,
            {
                "boot_latency_ms": gateway_fresh["boot_latency_ms"],
                "control": fresh_control,
            },
        ),
        assertion(
            "all real-model requests succeed from fresh disk without control",
            requests_fresh["succeeded"] == requests_fresh["requested"] == 3
            and revisions(requests_fresh) == [revision] * 3,
            {
                "succeeded": requests_fresh["succeeded"],
                "workers": [item["worker"] for item in requests_fresh["requests"]],
                "revisions": revisions(requests_fresh),
            },
        ),
        assertion(
            "snapshot beyond its age budget fails closed",
            expired_fixture["observed_age_ms"] > maximum_age_ms
            and expired_bootstrap["exit_code"] != 0
            and "routing snapshot expired" in expired_bootstrap["log"]
            and "not eligible for fallback" in expired_bootstrap["log"],
            {"fixture": expired_fixture, "bootstrap": expired_bootstrap},
        ),
        assertion(
            "timestamp beyond allowed future skew fails closed",
            future_fixture["future_delta_ms"] > maximum_future_skew_ms
            and future_bootstrap["exit_code"] != 0
            and "in the future" in future_bootstrap["log"]
            and "not eligible for fallback" in future_bootstrap["log"],
            {"fixture": future_fixture, "bootstrap": future_bootstrap},
        ),
        assertion(
            "persisted Raft nodes recover after the complete outage",
            recovered_election["leader_id"] in {"node-a", "node-b", "node-c"}
            and recovered_election["term"] > initial_election["term"],
            {
                "leader": recovered_election["leader_id"],
                "term": recovered_election["term"],
            },
        ),
        assertion(
            "live control repairs an ineligible future-dated disk document",
            repair_control["bootstrap_source"] == "live-control-plane"
            and repair_control["persisted_revision"] == revision
            and repair_control["persisted_expires_at_ms"]
            - repair_control["persisted_at_ms"]
            == maximum_age_ms
            and snapshot_repaired["revision"] == revision
            and snapshot_repaired["configuration"]
            == initial_config["configuration"]
            and snapshot_repaired["saved_at_ms"]
            <= gateway_repair["observed_at_ms"] + maximum_future_skew_ms,
            {
                "boot_latency_ms": gateway_repair["boot_latency_ms"],
                "control": repair_control,
                "snapshot": snapshot_repaired,
            },
        ),
        assertion(
            "live-repair requests preserve the committed revision",
            requests_repair["succeeded"] == requests_repair["requested"] == 2
            and revisions(requests_repair) == [revision] * 2,
            {
                "succeeded": requests_repair["succeeded"],
                "revisions": revisions(requests_repair),
            },
        ),
        assertion(
            "final speculative SSE reaches DONE through a real worker",
            stream["status"] == 200
            and stream["done_received"]
            and stream["config_revision"] == revision
            and stream["content"] == "InferLab turns prompts into real tokens."
            and stream["generation"]["speculation"]["enabled"]
            and stream["generation"]["speculation"]["target_forward_calls"] == 2,
            {
                "revision": stream["config_revision"],
                "worker": stream["worker"],
                "done": stream["done_received"],
                "speculation": stream["generation"]["speculation"],
            },
        ),
        assertion(
            "atomic replacement leaves no temporary routing file",
            directory["temporary_snapshot_files"] == [],
            directory,
        ),
        assertion(
            "all seven non-stream requests and final SSE succeed",
            len(all_requests) == successful_requests == 7
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
        "schema": "inferlab.snapshot-freshness-check.v0.15",
        "passed": passed_count == len(assertions),
        "assertions_passed": passed_count,
        "assertions_total": len(assertions),
        "revision": revision,
        "maximum_age_ms": maximum_age_ms,
        "maximum_future_skew_ms": maximum_future_skew_ms,
        "fresh_disk_age_ms": fresh_control["bootstrap_snapshot_age_ms"],
        "fresh_disk_boot_latency_ms": gateway_fresh["boot_latency_ms"],
        "live_repair_boot_latency_ms": gateway_repair["boot_latency_ms"],
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
