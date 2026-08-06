#!/usr/bin/env python3
"""Check the retained v0.21 overlap-safe service credential rotation proof."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


def load(directory: Path, name: str) -> dict[str, Any]:
    with (directory / name).open(encoding="utf-8") as source:
        return json.load(source)


def status_bodies(sample: dict[str, Any]) -> list[dict[str, Any]]:
    return [status["body"] for status in sample["statuses"]]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    args = parser.parse_args()
    evidence = args.evidence_dir

    initial = load(evidence, "initial-cluster.json")
    write = load(evidence, "write-committed.json")
    gateway_a = load(evidence, "gateway-key-a-ready.json")
    after_control_b = load(evidence, "after-control-key-b.json")
    overlap_a = load(evidence, "overlap-key-a-valid.json")
    gateway_b = load(evidence, "gateway-key-b-ready.json")
    after_revoke = load(evidence, "after-key-a-revocation.json")
    before_attacks = load(evidence, "before-revoked-attacks.json")["response"]["body"]
    revoked_gateway = load(evidence, "revoked-gateway-key-a.json")
    revoked_peer = load(evidence, "revoked-peer-key-a.json")
    valid_gateway = load(evidence, "valid-gateway-key-b.json")
    after_attacks = load(evidence, "after-revoked-attacks.json")["response"]["body"]
    request = load(evidence, "request.json")
    stream = load(evidence, "stream.json")
    final_gateway = load(evidence, "final-gateway.json")
    final_cluster = load(evidence, "final-cluster.json")

    assertions: list[dict[str, Any]] = []

    def check(name: str, passed: bool, observed: Any) -> None:
        assertions.append({"name": name, "passed": bool(passed), "observed": observed})

    expected_credentials = [
        "node-a/key-a",
        "node-a/key-b",
        "node-b/key-a",
        "node-b/key-b",
        "node-c/key-a",
        "node-c/key-b",
        "gateway-primary/key-a",
        "gateway-primary/key-b",
    ]
    revoked_credentials = [
        "gateway-primary/key-a",
        "node-a/key-a",
        "node-b/key-a",
        "node-c/key-a",
    ]

    initial_statuses = status_bodies(initial)
    check(
        "three key-a control nodes elect one leader",
        len(initial_statuses) == 3
        and len([status for status in initial_statuses if status["role"] == "leader"]) == 1
        and all(status["local_service_credential_id"] == "key-a" for status in initial_statuses),
        {
            status["node_id"]: {
                "role": status["role"],
                "credential": status["local_service_credential_id"],
            }
            for status in initial_statuses
        },
    )
    check(
        "every node trusts both bounded credentials before rotation",
        all(
            status["service_authentication"]["trusted_service_credentials"]
            == expected_credentials
            and status["service_authentication"]["revoked_service_credentials"] == []
            for status in initial_statuses
        ),
        initial_statuses[0]["service_authentication"],
    )

    committed = write["response"]["body"]
    check(
        "the authenticated cluster commits route revision 2 before rotation",
        write["response"]["status"] == 200
        and committed["revision"] == 2
        and committed["writer"]["writer_id"] == "deploy-bot",
        committed,
    )
    gateway_a_status = gateway_a["status"]["body"]["control_plane"]
    check(
        "gateway begins on key-a and fetches the committed route",
        gateway_a_status["service_credential_id"] == "key-a"
        and gateway_a["status"]["body"]["routing_snapshot"]["control_revision"] == 2,
        {
            "credential": gateway_a_status["service_credential_id"],
            "revision": gateway_a["status"]["body"]["routing_snapshot"]["control_revision"],
        },
    )

    rolling_samples = [
        load(evidence, path.name)
        for path in sorted(evidence.glob("control-key-b-step-*.json"))
    ] + [
        load(evidence, path.name)
        for path in sorted(evidence.glob("revoke-key-a-step-*.json"))
    ]
    check(
        "every rolling restart preserves a three-node quorum and one leader",
        len(rolling_samples) == 6
        and all(
            len(status_bodies(sample)) == 3
            and len(
                [status for status in status_bodies(sample) if status["role"] == "leader"]
            )
            == 1
            for sample in rolling_samples
        ),
        [
            {
                "leader": sample["leader_id"],
                "term": sample["term"],
                "nodes": len(status_bodies(sample)),
            }
            for sample in rolling_samples
        ],
    )

    control_b_statuses = status_bodies(after_control_b)
    check(
        "control signing credentials rotate to key-b without losing revision 2",
        all(
            status["local_service_credential_id"] == "key-b"
            and status["committed_configuration"]["revision"] == 2
            for status in control_b_statuses
        ),
        {
            status["node_id"]: {
                "credential": status["local_service_credential_id"],
                "revision": status["committed_configuration"]["revision"],
            }
            for status in control_b_statuses
        },
    )
    verification_counts: dict[str, int] = {}
    for status in control_b_statuses:
        for credential, count in status["service_authentication"][
            "verifications_by_credential"
        ].items():
            verification_counts[credential] = verification_counts.get(credential, 0) + count
    check(
        "mixed-version traffic proves both key-a and key-b were accepted during overlap",
        any(name.endswith("/key-a") and count > 0 for name, count in verification_counts.items())
        and any(
            name.endswith("/key-b") and count > 0
            for name, count in verification_counts.items()
        ),
        verification_counts,
    )
    check(
        "old gateway key-a remains valid during the overlap window",
        overlap_a["response"]["status"] == 200
        and overlap_a["response"]["body"]["revision"] == 2,
        overlap_a["response"],
    )
    gateway_b_status = gateway_b["status"]["body"]["control_plane"]
    check(
        "gateway rotates to key-b before old credentials are revoked",
        gateway_b_status["service_credential_id"] == "key-b"
        and gateway_b["status"]["body"]["routing_snapshot"]["control_revision"] == 2,
        gateway_b_status,
    )

    revoked_statuses = status_bodies(after_revoke)
    check(
        "all nodes keep key-b local while explicitly revoking every key-a credential",
        all(
            status["local_service_credential_id"] == "key-b"
            and status["service_authentication"]["revoked_service_credentials"]
            == revoked_credentials
            for status in revoked_statuses
        ),
        {
            status["node_id"]: {
                "local": status["local_service_credential_id"],
                "revoked": status["service_authentication"][
                    "revoked_service_credentials"
                ],
            }
            for status in revoked_statuses
        },
    )

    for name, attempt in [
        ("revoked gateway key-a cannot read control state", revoked_gateway),
        ("revoked peer key-a cannot send a high-term Raft vote", revoked_peer),
    ]:
        error = attempt["response"]["body"].get("error", {})
        check(
            name,
            attempt["response"]["status"] == 401
            and "credential" in error.get("message", "")
            and "revoked" in error.get("message", ""),
            attempt["response"],
        )

    check(
        "revoked high-term traffic cannot change term or committed revision",
        before_attacks["term"] == after_attacks["term"]
        and before_attacks["committed_configuration"]["revision"] == 2
        and after_attacks["committed_configuration"]["revision"] == 2,
        {
            "term_before": before_attacks["term"],
            "term_after": after_attacks["term"],
            "revision_before": before_attacks["committed_configuration"]["revision"],
            "revision_after": after_attacks["committed_configuration"]["revision"],
        },
    )
    check(
        "current gateway key-b still reads control state after revocation",
        valid_gateway["response"]["status"] == 200
        and valid_gateway["response"]["body"]["revision"] == 2,
        valid_gateway["response"],
    )
    auth_after = after_attacks["service_authentication"]
    check(
        "diagnostics attribute both revocation failures to credentials",
        auth_after["credential_revocation_rejections"] >= 2
        and (auth_after["last_rejected_service_credential"] or "").endswith("/key-a"),
        {
            "credential_revocation_rejections": auth_after[
                "credential_revocation_rejections"
            ],
            "last_rejected_service_credential": auth_after[
                "last_rejected_service_credential"
            ],
        },
    )

    request_sample = request["requests"][0]
    check(
        "the rotated gateway serves a real inference request",
        request["succeeded"] == 1
        and request_sample["worker"] == "cpu-credential-rotation"
        and request_sample["config_revision"] == 2,
        request_sample,
    )
    check(
        "the rotated gateway streams SSE through DONE",
        stream["status"] == 200
        and stream["done_received"]
        and stream["worker"] == "cpu-credential-rotation"
        and stream["config_revision"] == 2,
        {
            "status": stream["status"],
            "done_received": stream["done_received"],
            "worker": stream["worker"],
            "duration_ms": stream["duration_ms"],
        },
    )

    final_gateway_status = final_gateway["status"]["body"]
    final_statuses = status_bodies(final_cluster)
    check(
        "final gateway and every control replica retain key-b and revision 2",
        final_gateway_status["control_plane"]["service_credential_id"] == "key-b"
        and final_gateway_status["routing_snapshot"]["control_revision"] == 2
        and all(
            status["local_service_credential_id"] == "key-b"
            and status["committed_configuration"]["revision"] == 2
            for status in final_statuses
        ),
        {
            "gateway_credential": final_gateway_status["control_plane"][
                "service_credential_id"
            ],
            "controls": {
                status["node_id"]: status["local_service_credential_id"]
                for status in final_statuses
            },
        },
    )

    passed = sum(assertion["passed"] for assertion in assertions)
    result = {
        "schema": "inferlab.service-credential-rotation-assertions.v0.21",
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
