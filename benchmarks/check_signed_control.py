#!/usr/bin/env python3
"""Check retained v0.18 signed-control and key-rotation claims."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


CLUSTER = "inferlab-primary"
OLD_KEY = "primary-2026-a"
NEW_KEY = "primary-2026-b"
ROGUE_KEY = "rogue-2026-x"
EXPECTED_TEXT = "InferLab turns prompts into real tokens."


def load(directory: Path, name: str) -> dict[str, Any]:
    return json.loads((directory / name).read_text())


def assertion(name: str, passed: bool, observed: Any) -> dict[str, Any]:
    return {"name": name, "passed": bool(passed), "observed": observed}


def status_body(state: dict[str, Any]) -> dict[str, Any]:
    return state["status"]["body"]


def worker_requests(observation: dict[str, Any], worker_id: str) -> int:
    return observation["workers"][worker_id]["body"]["requests"]


def one_leader(election: dict[str, Any]) -> bool:
    return (
        sum(
            item.get("status") == 200
            and item.get("body", {}).get("role") == "leader"
            for item in election["statuses"]
        )
        == 1
        and all(
            item.get("body", {}).get("cluster_id") == CLUSTER
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


def authentication(configuration: dict[str, Any]) -> dict[str, Any]:
    return configuration["authentication"]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    evidence = args.evidence_dir

    initial = load(evidence, "initial-primary-election.json")
    old_config = load(evidence, "config-primary-old-key.json")["committed"]
    gateway_old = load(evidence, "gateway-old-key-fresh.json")
    snapshot_old = load(evidence, "snapshot-old-key.json")
    request_old = load(evidence, "request-old-key.json")
    before_stream = load(evidence, "worker-primary-before-stream.json")
    stream_crossing = load(evidence, "stream-crossing-rogue-key.json")
    primary_outage = load(evidence, "primary-control-outage.json")
    rogue_election = load(evidence, "rogue-election.json")
    rogue_config = load(evidence, "config-rogue-key.json")["committed"]
    gateway_rogue = load(evidence, "gateway-rogue-rejected.json")
    readiness_rogue = load(evidence, "readiness-rogue-rejected.json")
    primary_before = load(evidence, "worker-primary-before-rejection.json")
    primary_after = load(evidence, "worker-primary-after-rejection.json")
    rogue_before = load(evidence, "worker-rogue-before-rejection.json")
    rogue_after = load(evidence, "worker-rogue-after-rejection.json")
    request_rejected = load(evidence, "request-rogue-rejected.json")
    rogue_stop = load(evidence, "rogue-control-stop.json")
    rotated_election = load(evidence, "rotated-primary-election.json")
    gateway_new = load(evidence, "gateway-new-key-renewed.json")
    readiness_new = load(evidence, "readiness-new-key-renewed.json")
    snapshot_new = load(evidence, "snapshot-new-key.json")
    request_new = load(evidence, "request-new-key.json")
    rotated_primary_stop = load(evidence, "rotated-primary-control-stop.json")
    rollback_election = load(evidence, "rollback-old-key-election.json")
    gateway_downgrade = load(evidence, "gateway-key-downgrade-rejected.json")
    rollback_stop = load(evidence, "rollback-old-key-control-stop.json")
    restored_new_election = load(evidence, "restored-new-key-election.json")
    gateway_rerenewed = load(evidence, "gateway-new-key-rerenewed.json")
    gateway_primary_stop = load(evidence, "gateway-primary-stop.json")
    primary_final_stop = load(evidence, "primary-control-final-stop.json")
    tamper_fixture = load(evidence, "tampered-snapshot-fixture.json")
    tamper_rejection = load(evidence, "tampered-disk-bootstrap-rejected.json")
    revoked_rejection = load(evidence, "revoked-old-key-bootstrap-rejected.json")
    gateway_disk = load(evidence, "gateway-new-key-disk.json")
    request_disk = load(evidence, "request-new-key-disk.json")
    stream_final = load(evidence, "stream-final.json")
    gateway_final_stop = load(evidence, "gateway-final-stop.json")
    directory = load(evidence, "snapshot-directory.json")

    revision = old_config["revision"]
    old_auth = authentication(old_config)
    rogue_auth = authentication(rogue_config)
    new_auth = authentication(snapshot_new)
    old_body = status_body(gateway_old)
    rogue_body = status_body(gateway_rogue)
    new_body = status_body(gateway_new)
    downgrade_body = status_body(gateway_downgrade)
    rerenewed_body = status_body(gateway_rerenewed)
    disk_body = status_body(gateway_disk)
    events = [
        primary_outage,
        rogue_stop,
        rotated_primary_stop,
        rollback_stop,
        gateway_primary_stop,
        primary_final_stop,
        gateway_final_stop,
    ]
    exact_scope = all(
        exact_owned_event(event, 1 if "pid" in event else 3) for event in events
    )

    assertions = [
        assertion(
            "primary Raft cluster elects one leader in the expected namespace",
            one_leader(initial),
            {"leader": initial["leader_id"], "term": initial["term"]},
        ),
        assertion(
            "control response signs cluster revision term policy and workers with the old key",
            old_config["cluster_id"] == CLUSTER
            and old_auth["schema"] == "inferlab.control-authentication.v1"
            and old_auth["algorithm"] == "ed25519"
            and old_auth["key_id"] == OLD_KEY
            and old_config["configuration"]["workers"][0]["id"]
            == "cpu-primary",
            old_config,
        ),
        assertion(
            "gateway requires authentication and exposes the verified old-key request identity",
            old_body["control_plane"]["authentication_required"]
            and old_body["control_plane"]["active_signing_key_id"] == OLD_KEY
            and old_body["control_plane"]["signature_verifications"] > 0
            and old_body["routing_snapshot"]["control_signing_key_id"]
            == OLD_KEY
            and request_old["status"] == 200
            and request_old["control_cluster_id"] == CLUSTER
            and request_old["control_signing_key_id"] == OLD_KEY
            and request_old["content"] == EXPECTED_TEXT,
            {"control": old_body["control_plane"], "request": request_old},
        ),
        assertion(
            "durable snapshot retains the exact verified old-key envelope",
            snapshot_old["cluster_id"] == CLUSTER
            and authentication(snapshot_old) == old_auth
            and snapshot_old["configuration"] == old_config["configuration"],
            snapshot_old,
        ),
        assertion(
            "rogue cluster reuses the namespace and revision but signs with an unknown key",
            one_leader(rogue_election)
            and rogue_config["cluster_id"] == CLUSTER
            and rogue_config["revision"] == revision
            and rogue_auth["key_id"] == ROGUE_KEY
            and rogue_config["configuration"]["workers"][0]["id"] == "cpu-rogue",
            {"election": rogue_election, "configuration": rogue_config},
        ),
        assertion(
            "primary outage and rogue replacement target exact owned control processes",
            exact_owned_event(primary_outage, 3)
            and primary_outage["signing_key_id"] == OLD_KEY
            and exact_owned_event(rogue_stop, 3)
            and rogue_stop["signing_key_id"] == ROGUE_KEY,
            {"primary_outage": primary_outage, "rogue_stop": rogue_stop},
        ),
        assertion(
            "already-admitted old-key stream completes across rogue rejection",
            stream_crossing["started_at_ms"] < primary_outage["at_ms"]
            and stream_crossing["observed_at_ms"]
            >= gateway_rogue["observed_at_ms"]
            and stream_crossing["status"] == 200
            and stream_crossing["done_received"]
            and stream_crossing["control_signing_key_id"] == OLD_KEY
            and stream_crossing["content"] == EXPECTED_TEXT
            and worker_requests(before_stream, "cpu-primary") + 1
            <= worker_requests(primary_before, "cpu-primary"),
            {
                "stream": stream_crossing,
                "rogue_rejected_at_ms": gateway_rogue["observed_at_ms"],
            },
        ),
        assertion(
            "gateway rejects the unknown signing key before route publication",
            rogue_body["control_plane"]["last_rejected_signing_key_id"]
            == ROGUE_KEY
            and rogue_body["control_plane"]["signature_rejections"] > 0
            and "not trusted"
            in rogue_body["control_plane"]["last_authentication_error"]
            and rogue_body["control_plane"]["cluster_mismatch_rejections"] == 0
            and rogue_body["routing_snapshot"]["control_signing_key_id"]
            == OLD_KEY
            and rogue_body["routing_snapshot"]["control_revision"] == revision,
            {
                "control": rogue_body["control_plane"],
                "routing": rogue_body["routing_snapshot"],
            },
        ),
        assertion(
            "unauthenticated live responses do not renew the runtime lease",
            rogue_body["routing_lease"]["state"] == "expired-rejecting-new"
            and not rogue_body["routing_lease"]["accepting_new_requests"]
            and readiness_rogue["status"] == 503,
            {
                "lease": rogue_body["routing_lease"],
                "readiness": readiness_rogue,
            },
        ),
        assertion(
            "new request is rejected before either primary or rogue worker",
            request_rejected["status"] == 503
            and request_rejected["attempts"] == 0
            and worker_requests(primary_before, "cpu-primary")
            == worker_requests(primary_after, "cpu-primary")
            and worker_requests(rogue_before, "cpu-rogue")
            == worker_requests(rogue_after, "cpu-rogue")
            == 0,
            {
                "request": request_rejected,
                "primary_before": worker_requests(primary_before, "cpu-primary"),
                "primary_after": worker_requests(primary_after, "cpu-primary"),
                "rogue_before": worker_requests(rogue_before, "cpu-rogue"),
                "rogue_after": worker_requests(rogue_after, "cpu-rogue"),
            },
        ),
        assertion(
            "persistent primary cluster returns under a newer Raft leadership term",
            one_leader(rotated_election)
            and rotated_election["term"] > initial["term"],
            {
                "initial_term": initial["term"],
                "recovered_term": rotated_election["term"],
            },
        ),
        assertion(
            "trusted new key rotates the same route and renews without gateway restart",
            new_body["routing_lease"]["state"] == "fresh"
            and new_body["control_plane"]["active_signing_key_id"] == NEW_KEY
            and new_body["control_plane"]["signing_key_downgrade_rejections"]
            == 0
            and new_body["routing_snapshot"]["control_signing_key_id"] == NEW_KEY
            and new_body["routing_snapshot"]["control_revision"] == revision
            and new_body["control_plane"]["last_error"] is None
            and readiness_new["status"] == 200
            and request_new["status"] == 200
            and request_new["control_signing_key_id"] == NEW_KEY,
            {"gateway": new_body, "request": request_new},
        ),
        assertion(
            "signature-only rotation durably replaces the old envelope without route change",
            new_auth["key_id"] == NEW_KEY
            and snapshot_new["revision"] == snapshot_old["revision"]
            and snapshot_new["term"] == snapshot_old["term"]
            and snapshot_new["configuration"] == snapshot_old["configuration"]
            and new_auth["signature"] != old_auth["signature"],
            {"old": snapshot_old, "new": snapshot_new},
        ),
        assertion(
            "valid lower-preference old key cannot downgrade active key B or renew its lease",
            one_leader(rollback_election)
            and rollback_election["term"] > rotated_election["term"]
            and downgrade_body["control_plane"]["active_signing_key_id"]
            == NEW_KEY
            and downgrade_body["control_plane"][
                "signing_key_downgrade_rejections"
            ]
            > 0
            and "signing-key downgrade"
            in downgrade_body["control_plane"]["last_error"]
            and downgrade_body["routing_snapshot"]["control_signing_key_id"]
            == NEW_KEY
            and downgrade_body["routing_lease"]["state"]
            == "expired-rejecting-new",
            {
                "election": rollback_election,
                "control": downgrade_body["control_plane"],
                "route": downgrade_body["routing_snapshot"],
                "lease": downgrade_body["routing_lease"],
            },
        ),
        assertion(
            "restored key B renews after the rejected downgrade",
            one_leader(restored_new_election)
            and restored_new_election["term"] > rollback_election["term"]
            and rerenewed_body["control_plane"]["active_signing_key_id"]
            == NEW_KEY
            and rerenewed_body["control_plane"]["last_error"] is None
            and rerenewed_body["routing_snapshot"]["control_signing_key_id"]
            == NEW_KEY
            and rerenewed_body["routing_lease"]["state"] == "fresh",
            {
                "election": restored_new_election,
                "gateway": rerenewed_body,
            },
        ),
        assertion(
            "tamper fixture changes signed route bytes but preserves the signature",
            tamper_fixture["signing_key_id"] == NEW_KEY
            and tamper_fixture["original_worker_id"] == "cpu-primary"
            and tamper_fixture["tampered_worker_id"] == "cpu-tampered"
            and tamper_fixture["signature_unchanged"],
            tamper_fixture,
        ),
        assertion(
            "tampered signed disk snapshot cannot bootstrap",
            tamper_rejection["exit_code"] != 0
            and "signature verification failed" in tamper_rejection["log"],
            tamper_rejection,
        ),
        assertion(
            "valid old-key disk snapshot cannot bootstrap after explicit revocation",
            revoked_rejection["exit_code"] != 0
            and OLD_KEY in revoked_rejection["log"]
            and "is revoked" in revoked_rejection["log"],
            revoked_rejection,
        ),
        assertion(
            "new-key disk snapshot remains eligible while the old key is revoked",
            disk_body["control_plane"]["bootstrap_source"] == "disk-snapshot"
            and disk_body["control_plane"]["active_signing_key_id"] == NEW_KEY
            and OLD_KEY in disk_body["control_plane"]["revoked_signing_key_ids"]
            and disk_body["routing_snapshot"]["control_signing_key_id"] == NEW_KEY,
            disk_body,
        ),
        assertion(
            "disk-bootstrapped request exposes the verified new key",
            request_disk["status"] == 200
            and request_disk["control_cluster_id"] == CLUSTER
            and request_disk["control_signing_key_id"] == NEW_KEY
            and request_disk["content"] == EXPECTED_TEXT,
            request_disk,
        ),
        assertion(
            "final speculative SSE reaches DONE under the new verified key",
            stream_final["status"] == 200
            and stream_final["done_received"]
            and stream_final["control_signing_key_id"] == NEW_KEY
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
            all(
                request["status"] == 200
                for request in [request_old, request_new, request_disk]
            )
            and stream_crossing["status"] == 200
            and stream_crossing["done_received"]
            and stream_final["status"] == 200
            and stream_final["done_received"],
            {"non_stream_successes": 3, "streams_succeeded": 2},
        ),
    ]

    passed_count = sum(item["passed"] for item in assertions)
    result = {
        "schema": "inferlab.signed-control-check.v0.18",
        "passed": passed_count == len(assertions),
        "assertions_passed": passed_count,
        "assertions_total": len(assertions),
        "cluster_id": CLUSTER,
        "old_key_id": OLD_KEY,
        "new_key_id": NEW_KEY,
        "rejected_key_id": ROGUE_KEY,
        "revision": revision,
        "initial_term": initial["term"],
        "recovered_term": rotated_election["term"],
        "signature_rejections_at_expiry": rogue_body["control_plane"][
            "signature_rejections"
        ],
        "key_downgrade_rejections": downgrade_body["control_plane"][
            "signing_key_downgrade_rejections"
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
