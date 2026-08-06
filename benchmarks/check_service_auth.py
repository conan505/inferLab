#!/usr/bin/env python3
"""Check the retained v0.20 cryptographic service-identity proof."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


def load(directory: Path, name: str) -> dict[str, Any]:
    with (directory / name).open(encoding="utf-8") as source:
        return json.load(source)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    args = parser.parse_args()
    evidence = args.evidence_dir

    election = load(evidence, "election.json")
    write = load(evidence, "write-committed.json")
    missing = load(evidence, "missing-rejected.json")
    unknown = load(evidence, "unknown-rejected.json")
    stale = load(evidence, "stale-rejected.json")
    peer_read = load(evidence, "peer-read-forbidden.json")
    gateway_read = load(evidence, "gateway-read-valid.json")
    replay = load(evidence, "replay-rejected.json")
    tampered_raft = load(evidence, "tampered-raft-rejected.json")
    gateway_peer = load(evidence, "gateway-peer-forbidden.json")
    leader_after = load(evidence, "leader-after-rejections.json")["response"]["body"]
    gateway_ready = load(evidence, "gateway-ready.json")
    request = load(evidence, "request.json")
    stream = load(evidence, "stream.json")
    final_cluster = load(evidence, "final-cluster.json")
    final_leader = load(evidence, "final-leader.json")["response"]["body"]

    assertions: list[dict[str, Any]] = []

    def check(name: str, passed: bool, observed: Any) -> None:
        assertions.append({"name": name, "passed": bool(passed), "observed": observed})

    statuses = [sample["body"] for sample in election["statuses"]]
    leader_id = election["leader_id"]
    check(
        "three service-authenticated Raft nodes elect exactly one leader",
        leader_id in {"node-a", "node-b", "node-c"}
        and len([status for status in statuses if status["role"] == "leader"]) == 1,
        {"leader_id": leader_id, "term": election["term"]},
    )
    check(
        "every control node requires the same service trust boundary",
        all(
            status["service_authentication"]["required"]
            and status["service_authentication"]["trusted_service_ids"]
            == ["node-a", "node-b", "node-c", "gateway-primary"]
            and status["service_authentication"]["gateway_service_ids"]
            == ["gateway-primary"]
            for status in statuses
        ),
        [status["service_authentication"] for status in statuses],
    )
    follower_auth = [
        status["service_authentication"]["authorized_peer_rpcs"]
        for status in statuses
        if status["node_id"] != leader_id
    ]
    check(
        "followers accept signed peer RPCs before election can stabilize",
        len(follower_auth) == 2 and all(count > 0 for count in follower_auth),
        follower_auth,
    )

    committed = write["response"]["body"]
    check(
        "authorized administrative intent still commits signed route revision 2",
        write["response"]["status"] == 200
        and committed["revision"] == 2
        and committed["writer"]["writer_id"] == "deploy-bot"
        and committed["authentication"]["key_id"] == "route-2026-b",
        committed,
    )

    rejection_cases = [
        ("missing service identity is rejected", missing, 401, "required"),
        ("unknown service identity is rejected", unknown, 401, "not trusted"),
        ("stale signed service request is rejected", stale, 401, "maximum age"),
        (
            "trusted Raft identity cannot use the gateway read scope",
            peer_read,
            403,
            "not authorized as a gateway",
        ),
        ("accepted service nonce cannot be replayed", replay, 401, "already accepted"),
        (
            "tampering a signed Raft body fails authentication",
            tampered_raft,
            401,
            "signature verification failed",
        ),
        (
            "gateway identity cannot claim a Raft peer ID",
            gateway_peer,
            403,
            "cannot act as Raft peer",
        ),
    ]
    for name, attempt, status, text in rejection_cases:
        error = attempt["response"]["body"].get("error", {})
        check(
            name,
            attempt["response"]["status"] == status
            and text in error.get("message", ""),
            attempt["response"],
        )

    gateway_body = gateway_read["response"]["body"]
    check(
        "fresh gateway identity reads the committed route from its exact audience",
        gateway_read["response"]["status"] == 200
        and gateway_body["cluster_id"] == "inferlab-primary"
        and gateway_body["revision"] == 2,
        gateway_read["response"],
    )
    check(
        "rejected high-term Raft attempts cannot change leader term or log",
        leader_after["term"] == election["term"]
        and leader_after["committed_configuration"]["revision"] == 2,
        {
            "term_before": election["term"],
            "term_after": leader_after["term"],
            "revision": leader_after["committed_configuration"]["revision"],
        },
    )
    auth_after = leader_after["service_authentication"]
    check(
        "leader diagnostics distinguish auth, freshness, replay, and scope rejection",
        auth_after["authentication_rejections"] >= 3
        and auth_after["freshness_rejections"] >= 1
        and auth_after["replay_rejections"] >= 1
        and auth_after["authorization_rejections"] >= 2,
        auth_after,
    )

    gateway_status = gateway_ready["status"]["body"]
    control_status = gateway_status["control_plane"]
    check(
        "real gateway exposes its service identity and exact control targets",
        control_status["service_authentication_enabled"]
        and control_status["service_id"] == "gateway-primary"
        and control_status["control_service_targets"]
        == [
            "node-a=http://127.0.0.1:9921",
            "node-b=http://127.0.0.1:9922",
            "node-c=http://127.0.0.1:9923",
        ],
        {
            "service_id": control_status["service_id"],
            "targets": control_status["control_service_targets"],
        },
    )
    check(
        "gateway accepts the separately signed route only after service-authenticated fetch",
        gateway_status["routing_snapshot"]["control_revision"] == 2
        and gateway_status["routing_snapshot"]["control_signing_key_id"] == "route-2026-b"
        and control_status["signature_verifications"] >= 1,
        {
            "routing_snapshot": gateway_status["routing_snapshot"],
            "signature_verifications": control_status["signature_verifications"],
        },
    )

    request_sample = request["requests"][0]
    check(
        "service-authenticated route serves a real inference request",
        request["succeeded"] == 1
        and request_sample["worker"] == "cpu-service-auth"
        and request_sample["config_revision"] == 2,
        request_sample,
    )
    check(
        "service-authenticated route serves SSE through DONE",
        stream["status"] == 200
        and stream["done_received"]
        and stream["worker"] == "cpu-service-auth"
        and stream["config_revision"] == 2,
        {
            "status": stream["status"],
            "done_received": stream["done_received"],
            "worker": stream["worker"],
            "duration_ms": stream["duration_ms"],
        },
    )

    final_statuses = [sample["body"] for sample in final_cluster["statuses"]]
    check(
        "all control replicas retain route revision 2 under authenticated replication",
        len(final_statuses) == 3
        and all(
            status["committed_configuration"]["revision"] == 2
            and status["service_authentication"]["required"]
            for status in final_statuses
        ),
        [
            {
                "node_id": status["node_id"],
                "revision": status["committed_configuration"]["revision"],
                "authorized_peer_rpcs": status["service_authentication"][
                    "authorized_peer_rpcs"
                ],
            }
            for status in final_statuses
        ],
    )
    check(
        "gateway polling produces authorized service reads without new rejection classes",
        sum(
            status["service_authentication"]["authorized_gateway_reads"]
            for status in final_statuses
        )
        >= 1
        and final_leader["service_authentication"]["required"],
        {
            status["node_id"]: status["service_authentication"][
                "authorized_gateway_reads"
            ]
            for status in final_statuses
        },
    )

    passed = sum(assertion["passed"] for assertion in assertions)
    result = {
        "schema": "inferlab.service-auth-assertions.v0.20",
        "passed": passed,
        "total": len(assertions),
        "all_passed": passed == len(assertions),
        "assertions": assertions,
    }
    print(json.dumps(result, indent=2, sort_keys=True))
    if not result["all_passed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
