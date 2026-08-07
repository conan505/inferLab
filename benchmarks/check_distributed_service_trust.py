#!/usr/bin/env python3
"""Check the exact-process v0.23 distributed service-trust evidence."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


def load(directory: Path, name: str) -> dict[str, Any]:
    with (directory / name).open(encoding="utf-8") as source:
        return json.load(source)


def control_bodies(sample: dict[str, Any]) -> list[dict[str, Any]]:
    return [status["body"] for status in sample["statuses"]]


def authentication(status: dict[str, Any]) -> dict[str, Any]:
    return status["service_authentication"]


def distributor_body(sample: dict[str, Any]) -> dict[str, Any]:
    return sample["status"]["body"]


def response_error_code(sample: dict[str, Any]) -> str | None:
    body = sample.get("body", {})
    if not isinstance(body, dict):
        return None
    error = body.get("error", {})
    return error.get("code") if isinstance(error, dict) else None


def signed_receivers(status: dict[str, Any], generation: int) -> list[str]:
    receivers: list[str] = []
    for receipt in status.get("receipts", []):
        authentication = receipt.get("authentication", {})
        if (
            receipt.get("schema") != "inferlab.service-trust-receipt.v1"
            or receipt.get("generation") != generation
            or receipt.get("cluster_id") != "inferlab-primary"
            or authentication.get("schema")
            != "inferlab.service-trust-receipt-authentication.v1"
            or authentication.get("algorithm") != "ed25519"
            or not authentication.get("signature")
        ):
            return []
        receivers.append(
            f"{receipt.get('receiver_service_id')}/{receipt.get('receiver_credential_id')}"
        )
    return sorted(receivers)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    args = parser.parse_args()
    evidence = args.evidence_dir

    empty = load(evidence, "distributor-empty.json")
    publish_g1 = load(evidence, "publish-g1.json")
    initial = load(evidence, "initial-controls.json")
    receipts_g1 = load(evidence, "generation-1-receipts.json")
    route_write = load(evidence, "write-committed.json")
    gateway_a = load(evidence, "gateway-key-a-ready.json")
    partial_controls = load(evidence, "generation-2-partial-controls.json")
    withheld_c = load(evidence, "generation-2-withheld-c.json")
    partial_receipts = load(evidence, "generation-2-partial-receipts.json")
    generation_two = load(evidence, "generation-2-convergence.json")
    receipts_g2 = load(evidence, "generation-2-receipts.json")
    key_b_g2 = load(evidence, "generation-2-key-b-valid.json")
    gateway_b = load(evidence, "gateway-key-b-ready.json")
    generation_three = load(evidence, "generation-3-convergence.json")
    receipts_g3 = load(evidence, "generation-3-receipts.json")
    old_a = load(evidence, "generation-3-key-a-revoked.json")
    key_b_g3 = load(evidence, "generation-3-key-b-valid.json")
    rollback = load(evidence, "rollback-publication.json")
    fork = load(evidence, "fork-publication.json")
    tamper = load(evidence, "tamper-publication.json")
    after_attacks = load(evidence, "after-attacks-controls.json")
    continuity = load(evidence, "online-process-continuity.json")
    cache_restart = load(evidence, "cache-restart.json")
    final_cluster = load(evidence, "final-cluster.json")
    request = load(evidence, "request.json")
    stream = load(evidence, "stream.json")
    final_gateway = load(evidence, "final-gateway.json")

    assertions: list[dict[str, Any]] = []

    def check(name: str, passed: bool, observed: Any) -> None:
        assertions.append({"name": name, "passed": bool(passed), "observed": observed})

    empty_observation = empty["observation"]
    check(
        "an empty distributor is healthy but has no current snapshot",
        empty_observation["status"] == 200
        and empty_observation["body"]["snapshot"] is None
        and empty_observation["body"]["receipt_count"] == 0
        and empty_observation["body"]["receipts"] == []
        and not empty_observation["body"]["storage"]["mutation_poisoned"]
        and empty_observation["body"]["storage"]["error_code"] is None,
        empty_observation,
    )
    check(
        "root-signed generation 1 is remotely published",
        publish_g1["status"] == 201
        and publish_g1["body"]["generation"] == 1
        and publish_g1["body"]["outcome"] == "published",
        publish_g1,
    )

    initial_statuses = control_bodies(initial)
    check(
        "three controls remotely bootstrap generation 1 and elect one leader",
        len(initial_statuses) == 3
        and len([item for item in initial_statuses if item["role"] == "leader"]) == 1
        and all(
            authentication(item)["trust_policy_generation"] == 1
            and authentication(item)["trust_policy_distribution_mode"]
            == "remote-http"
            and authentication(item)["trust_policy_bootstrap_source"] == "remote"
            for item in initial_statuses
        ),
        {
            item["node_id"]: {
                "role": item["role"],
                "generation": authentication(item)["trust_policy_generation"],
                "mode": authentication(item)["trust_policy_distribution_mode"],
                "bootstrap": authentication(item)["trust_policy_bootstrap_source"],
            }
            for item in initial_statuses
        },
    )
    g1_status = distributor_body(receipts_g1)
    check(
        "generation-1 receipts attest reported activation for all expected receivers",
        g1_status["snapshot"]["generation"] == 1
        and g1_status["acked_receivers"]
        == ["node-a/key-a", "node-b/key-a", "node-c/key-a"]
        and g1_status["pending_receivers"] == []
        and g1_status["receipt_count"] == 3
        and signed_receivers(g1_status, 1)
        == ["node-a/key-a", "node-b/key-a", "node-c/key-a"]
        and not g1_status["storage"]["mutation_poisoned"],
        g1_status,
    )
    committed = route_write["response"]["body"]
    check(
        "signed writer intent commits route revision 2 under remote g1 trust",
        route_write["response"]["status"] == 200
        and committed["revision"] == 2
        and committed["writer"]["writer_id"] == "deploy-bot",
        committed,
    )
    gateway_a_status = gateway_a["status"]["body"]
    check(
        "gateway key A publishes route revision 2 before rotation",
        gateway_a_status["control_plane"]["service_credential_id"] == "key-a"
        and gateway_a_status["routing_snapshot"]["control_revision"] == 2,
        gateway_a_status,
    )

    partial_statuses = control_bodies(partial_controls)
    c_status = withheld_c["observation"]["body"]
    check(
        "A and B activate overlap g2 while C remains on g1 behind the partition",
        len(partial_statuses) == 2
        and all(authentication(item)["trust_policy_generation"] == 2 for item in partial_statuses)
        and authentication(c_status)["trust_policy_generation"] == 1
        and authentication(c_status)["trust_policy_consecutive_fetch_failures"] >= 1,
        {
            "connected": {
                item["node_id"]: authentication(item)["trust_policy_generation"]
                for item in partial_statuses
            },
            "withheld": {
                "node_id": c_status["node_id"],
                "generation": authentication(c_status)["trust_policy_generation"],
                "fetch_failures": authentication(c_status)[
                    "trust_policy_consecutive_fetch_failures"
                ],
            },
        },
    )
    partial = distributor_body(partial_receipts)
    check(
        "partial receipt status acknowledges A and B and leaves C pending",
        partial["snapshot"]["generation"] == 2
        and partial["acked_receivers"] == ["node-a/key-a", "node-b/key-a"]
        and partial["pending_receivers"] == ["node-c/key-a"]
        and partial["receipt_count"] == 2
        and signed_receivers(partial, 2) == ["node-a/key-a", "node-b/key-a"],
        partial,
    )
    generation_two_statuses = control_bodies(generation_two)
    check(
        "healing C converges all controls to overlap generation 2",
        len(generation_two_statuses) == 3
        and all(
            authentication(item)["trust_policy_generation"] == 2
            for item in generation_two_statuses
        ),
        {
            item["node_id"]: authentication(item)["trust_policy_generation"]
            for item in generation_two_statuses
        },
    )
    converged_g2 = distributor_body(receipts_g2)
    check(
        "generation-2 receipt set contains every expected activation attestation",
        converged_g2["acked_receivers"]
        == ["node-a/key-a", "node-b/key-a", "node-c/key-a"]
        and converged_g2["pending_receivers"] == []
        and converged_g2["receipt_count"] == 3
        and signed_receivers(converged_g2, 2)
        == ["node-a/key-a", "node-b/key-a", "node-c/key-a"],
        converged_g2,
    )
    check(
        "new gateway key B is accepted during A+B overlap",
        key_b_g2["response"]["status"] == 200
        and key_b_g2["response"]["body"]["revision"] == 2,
        key_b_g2["response"],
    )
    gateway_b_status = gateway_b["status"]["body"]
    check(
        "gateway rotates to key B before old-key revocation",
        gateway_b_status["control_plane"]["service_credential_id"] == "key-b"
        and gateway_b_status["routing_snapshot"]["control_revision"] == 2,
        gateway_b_status,
    )

    generation_three_statuses = control_bodies(generation_three)
    check(
        "all unchanged controls activate generation 3",
        continuity["unchanged_before_cache_restart"]
        and all(
            authentication(item)["trust_policy_generation"] == 3
            for item in generation_three_statuses
        ),
        {
            "continuity": continuity,
            "generations": {
                item["node_id"]: authentication(item)["trust_policy_generation"]
                for item in generation_three_statuses
            },
        },
    )
    converged_g3 = distributor_body(receipts_g3)
    check(
        "generation-3 receipt set contains every expected activation attestation",
        converged_g3["snapshot"]["generation"] == 3
        and converged_g3["acked_receivers"]
        == ["node-a/key-a", "node-b/key-a", "node-c/key-a"]
        and converged_g3["pending_receivers"] == []
        and signed_receivers(converged_g3, 3)
        == ["node-a/key-a", "node-b/key-a", "node-c/key-a"]
        and not converged_g3["storage"]["mutation_poisoned"],
        converged_g3,
    )
    old_error = old_a["response"].get("body", {}).get("error", {})
    check(
        "old gateway key A is rejected after distributed g3 activation",
        old_a["response"]["status"] == 401
        and "gateway-primary/key-a" in old_error.get("message", "")
        and "revoked" in old_error.get("message", ""),
        old_a["response"],
    )
    check(
        "current gateway key B still reads committed route revision 2",
        key_b_g3["response"]["status"] == 200
        and key_b_g3["response"]["body"]["revision"] == 2,
        key_b_g3["response"],
    )

    check(
        "the distributor rejects a valid signed generation-2 rollback",
        rollback["status"] == 409
        and response_error_code(rollback) == "snapshot_rollback",
        rollback,
    )
    check(
        "the distributor rejects a different valid generation-3 fork",
        fork["status"] == 409 and response_error_code(fork) == "snapshot_fork",
        fork,
    )
    check(
        "the distributor rejects a signature-tampered higher generation",
        tamper["status"] == 400
        and response_error_code(tamper) == "invalid_snapshot",
        tamper,
    )
    after_attack_statuses = control_bodies(after_attacks)
    check(
        "all controls retain active generation 3 after rejected publications",
        all(
            authentication(item)["trust_policy_generation"] == 3
            for item in after_attack_statuses
        ),
        {
            item["node_id"]: authentication(item)["trust_policy_generation"]
            for item in after_attack_statuses
        },
    )

    cache_status = cache_restart["status"]["body"]
    check(
        "a follower restarts from its complete generation-3 cache during distributor outage",
        cache_restart["old_pid"] != cache_restart["new_pid"]
        and cache_restart["distributor_unavailable"]
        and authentication(cache_status)["trust_policy_generation"] == 3
        and authentication(cache_status)["trust_policy_bootstrap_source"] == "cache"
        and authentication(cache_status)["trust_policy_receipt_failures"] >= 1,
        cache_restart,
    )
    final_statuses = [item["body"] for item in final_cluster["statuses"]]
    check(
        "the cache-bootstrapped follower rejoins one revision-2 Raft cluster",
        len(final_statuses) == 3
        and len([item for item in final_statuses if item["role"] == "leader"]) == 1
        and all(item["committed_configuration"]["revision"] == 2 for item in final_statuses)
        and all(authentication(item)["trust_policy_generation"] == 3 for item in final_statuses),
        {
            item["node_id"]: {
                "role": item["role"],
                "revision": item["committed_configuration"]["revision"],
                "generation": authentication(item)["trust_policy_generation"],
            }
            for item in final_statuses
        },
    )

    request_sample = request["requests"][0]
    check(
        "gateway B serves a real CPU inference request while the distributor is down",
        request["succeeded"] == 1
        and request_sample["worker"] == "cpu-distributed-trust"
        and request_sample["config_revision"] == 2,
        request_sample,
    )
    check(
        "gateway B streams real SSE through DONE while the distributor is down",
        stream["status"] == 200
        and stream["done_received"]
        and stream["worker"] == "cpu-distributed-trust"
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
        "final gateway remains on key B and route revision 2",
        final_gateway_status["control_plane"]["service_credential_id"] == "key-b"
        and final_gateway_status["routing_snapshot"]["control_revision"] == 2,
        {
            "credential": final_gateway_status["control_plane"][
                "service_credential_id"
            ],
            "routing_snapshot": final_gateway_status["routing_snapshot"],
        },
    )

    passed = sum(item["passed"] for item in assertions)
    result = {
        "schema": "inferlab.distributed-service-trust-assertions.v0.23",
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
