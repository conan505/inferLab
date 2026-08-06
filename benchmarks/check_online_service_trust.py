#!/usr/bin/env python3
"""Check the retained v0.22 signed online service-trust proof."""

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


def auth(status: dict[str, Any]) -> dict[str, Any]:
    return status["service_authentication"]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    args = parser.parse_args()
    evidence = args.evidence_dir

    initial = load(evidence, "initial-cluster.json")
    write = load(evidence, "write-committed.json")
    gateway_a = load(evidence, "gateway-key-a-ready.json")
    generation_two = load(evidence, "generation-2-convergence.json")
    key_b_at_two = load(evidence, "generation-2-key-b-valid.json")
    gateway_b = load(evidence, "gateway-key-b-ready.json")
    generation_three = load(evidence, "generation-3-convergence.json")
    continuity = load(evidence, "online-process-continuity.json")
    key_a_revoked = load(evidence, "generation-3-key-a-revoked.json")
    key_b_at_three = load(evidence, "generation-3-key-b-valid.json")
    rollback = load(evidence, "rollback-rejected.json")
    tamper = load(evidence, "tamper-rejected.json")
    floor_restart = load(evidence, "restart-floor-rejection.json")
    final_cluster = load(evidence, "final-cluster.json")
    final_trust = load(evidence, "final-trust.json")
    request = load(evidence, "request.json")
    stream = load(evidence, "stream.json")
    final_gateway = load(evidence, "final-gateway.json")

    assertions: list[dict[str, Any]] = []

    def check(name: str, passed: bool, observed: Any) -> None:
        assertions.append({"name": name, "passed": bool(passed), "observed": observed})

    initial_statuses = status_bodies(initial)
    check(
        "three controls bootstrap from root-signed generation 1 and elect one leader",
        len(initial_statuses) == 3
        and len([status for status in initial_statuses if status["role"] == "leader"]) == 1
        and all(
            auth(status)["trust_policy_source"] == "signed-snapshot"
            and auth(status)["trust_policy_generation"] == 1
            and auth(status)["trust_policy_signing_key_id"] == "service-trust-root-a"
            for status in initial_statuses
        ),
        {
            status["node_id"]: {
                "role": status["role"],
                "generation": auth(status)["trust_policy_generation"],
                "root": auth(status)["trust_policy_signing_key_id"],
            }
            for status in initial_statuses
        },
    )
    check(
        "generation 1 trusts gateway key-a but not key-b",
        all(
            "gateway-primary/key-a" in auth(status)["trusted_service_credentials"]
            and "gateway-primary/key-b" not in auth(status)["trusted_service_credentials"]
            for status in initial_statuses
        ),
        auth(initial_statuses[0])["trusted_service_credentials"],
    )
    committed = write["response"]["body"]
    check(
        "signed writer intent commits route revision 2 under generation 1",
        write["response"]["status"] == 200
        and committed["revision"] == 2
        and committed["writer"]["writer_id"] == "deploy-bot",
        committed,
    )
    gateway_a_status = gateway_a["status"]["body"]
    check(
        "gateway key-a initially reads and publishes revision 2",
        gateway_a_status["control_plane"]["service_credential_id"] == "key-a"
        and gateway_a_status["routing_snapshot"]["control_revision"] == 2,
        {
            "credential": gateway_a_status["control_plane"]["service_credential_id"],
            "revision": gateway_a_status["routing_snapshot"]["control_revision"],
        },
    )

    generation_two_statuses = status_bodies(generation_two)
    check(
        "all receivers converge online to generation 2",
        all(
            auth(status)["trust_policy_generation"] == 2
            and auth(status)["trust_policy_reloads"] == 1
            for status in generation_two_statuses
        ),
        {
            status["node_id"]: {
                "generation": auth(status)["trust_policy_generation"],
                "reloads": auth(status)["trust_policy_reloads"],
            }
            for status in generation_two_statuses
        },
    )
    check(
        "online generation-2 convergence finishes within the proof timeout",
        generation_two["duration_ms"] < 5_000
        and len(generation_two["observations"]) == 3,
        {
            "duration_ms": generation_two["duration_ms"],
            "per_node_ms": {
                observation["url"]: observation["convergence_latency_ms"]
                for observation in generation_two["observations"]
            },
        },
    )
    check(
        "generation 2 overlaps gateway keys A and B",
        all(
            "gateway-primary/key-a" in auth(status)["trusted_service_credentials"]
            and "gateway-primary/key-b" in auth(status)["trusted_service_credentials"]
            and auth(status)["revoked_service_credentials"] == []
            for status in generation_two_statuses
        ),
        auth(generation_two_statuses[0]),
    )
    check(
        "new gateway key-b works immediately after online trust expansion",
        key_b_at_two["response"]["status"] == 200
        and key_b_at_two["response"]["body"]["revision"] == 2,
        key_b_at_two["response"],
    )
    gateway_b_status = gateway_b["status"]["body"]
    check(
        "gateway rotates to key-b before old-key revocation",
        gateway_b_status["control_plane"]["service_credential_id"] == "key-b"
        and gateway_b_status["routing_snapshot"]["control_revision"] == 2,
        {
            "credential": gateway_b_status["control_plane"]["service_credential_id"],
            "revision": gateway_b_status["routing_snapshot"]["control_revision"],
        },
    )

    generation_three_statuses = status_bodies(generation_three)
    check(
        "all unchanged control processes converge online to generation 3",
        continuity["unchanged"]
        and all(
            auth(status)["trust_policy_generation"] == 3
            and auth(status)["trust_policy_reloads"] == 2
            for status in generation_three_statuses
        ),
        {
            "pids": continuity,
            "generations": {
                status["node_id"]: auth(status)["trust_policy_generation"]
                for status in generation_three_statuses
            },
        },
    )
    check(
        "generation 3 precisely revokes gateway key-a while retaining key-b",
        all(
            auth(status)["revoked_service_credentials"]
            == ["gateway-primary/key-a"]
            and "gateway-primary/key-b" in auth(status)["trusted_service_credentials"]
            for status in generation_three_statuses
        ),
        auth(generation_three_statuses[0]),
    )
    key_a_error = key_a_revoked["response"]["body"].get("error", {})
    check(
        "old gateway key-a is rejected after online revocation",
        key_a_revoked["response"]["status"] == 401
        and "gateway-primary/key-a" in key_a_error.get("message", "")
        and "revoked" in key_a_error.get("message", ""),
        key_a_revoked["response"],
    )
    check(
        "current gateway key-b still reads revision 2",
        key_b_at_three["response"]["status"] == 200
        and key_b_at_three["response"]["body"]["revision"] == 2,
        key_b_at_three["response"],
    )

    rollback_statuses = status_bodies(rollback)
    check(
        "signed generation-2 rollback is rejected everywhere",
        all(
            auth(status)["trust_policy_generation"] == 3
            and auth(status)["trust_policy_rejections"] >= 1
            and "rollback" in (auth(status)["last_trust_policy_error"] or "")
            for status in rollback_statuses
        ),
        {
            status["node_id"]: {
                "generation": auth(status)["trust_policy_generation"],
                "rejections": auth(status)["trust_policy_rejections"],
                "error": auth(status)["last_trust_policy_error"],
            }
            for status in rollback_statuses
        },
    )
    tamper_statuses = status_bodies(tamper)
    check(
        "tampered higher-generation snapshot is rejected without replacing generation 3",
        all(
            auth(status)["trust_policy_generation"] == 3
            and auth(status)["trust_policy_rejections"] >= 2
            and "signature verification failed"
            in (auth(status)["last_trust_policy_error"] or "")
            for status in tamper_statuses
        ),
        {
            status["node_id"]: {
                "generation": auth(status)["trust_policy_generation"],
                "rejections": auth(status)["trust_policy_rejections"],
                "error": auth(status)["last_trust_policy_error"],
            }
            for status in tamper_statuses
        },
    )
    check(
        "durable generation floor rejects rollback on process restart",
        floor_restart["exit_status"] != 0
        and "rollback rejected" in floor_restart["log"]
        and "durable floor 3" in floor_restart["log"],
        floor_restart,
    )

    final_statuses = status_bodies(final_cluster)
    final_trust_statuses = status_bodies(final_trust)
    check(
        "restored generation 3 lets the stopped follower rejoin the r2 cluster",
        len(final_statuses) == 3
        and len([status for status in final_statuses if status["role"] == "leader"]) == 1
        and all(status["committed_configuration"]["revision"] == 2 for status in final_statuses)
        and all(auth(status)["trust_policy_generation"] == 3 for status in final_trust_statuses),
        {
            status["node_id"]: {
                "role": status["role"],
                "revision": status["committed_configuration"]["revision"],
                "generation": auth(status)["trust_policy_generation"],
            }
            for status in final_statuses
        },
    )

    request_sample = request["requests"][0]
    check(
        "key-b gateway serves a real inference request after rejected policies",
        request["succeeded"] == 1
        and request_sample["worker"] == "cpu-online-trust"
        and request_sample["config_revision"] == 2,
        request_sample,
    )
    check(
        "key-b gateway streams SSE through DONE after rejected policies",
        stream["status"] == 200
        and stream["done_received"]
        and stream["worker"] == "cpu-online-trust"
        and stream["config_revision"] == 2,
        {
            "status": stream["status"],
            "done_received": stream["done_received"],
            "worker": stream["worker"],
            "duration_ms": stream["duration_ms"],
        },
    )
    final_gateway_status = final_gateway["status"]["body"]
    check(
        "final gateway remains on key-b and publishes route revision 2",
        final_gateway_status["control_plane"]["service_credential_id"] == "key-b"
        and final_gateway_status["routing_snapshot"]["control_revision"] == 2,
        {
            "credential": final_gateway_status["control_plane"]["service_credential_id"],
            "routing_snapshot": final_gateway_status["routing_snapshot"],
        },
    )

    passed = sum(assertion["passed"] for assertion in assertions)
    result = {
        "schema": "inferlab.online-service-trust-assertions.v0.22",
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
