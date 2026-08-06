#!/usr/bin/env python3
"""Check the retained v0.19 control-writer authorization proof."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


def load(directory: Path, name: str) -> dict:
    with (directory / name).open(encoding="utf-8") as source:
        return json.load(source)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    args = parser.parse_args()
    evidence = args.evidence_dir

    election = load(evidence, "initial-election.json")
    initial = load(evidence, "status-initial.json")["response"]["body"]
    after_rejections = load(evidence, "status-after-rejections.json")["response"]["body"]
    unsigned = load(evidence, "write-unsigned-rejected.json")
    unknown = load(evidence, "write-unknown-rejected.json")
    tampered = load(evidence, "write-tampered-rejected.json")
    stale = load(evidence, "write-stale-rejected.json")
    revoked = load(evidence, "write-revoked-rejected.json")
    valid = load(evidence, "write-valid-committed.json")
    replay = load(evidence, "write-replay-rejected.json")
    gateway_r2 = load(evidence, "gateway-revision-2.json")
    request_r2 = load(evidence, "request-revision-2.json")
    update = load(evidence, "write-update-committed.json")
    gateway_r3 = load(evidence, "gateway-revision-3.json")
    final_stream = load(evidence, "stream-final.json")
    final_status = load(evidence, "status-final.json")["response"]["body"]
    final_cluster = load(evidence, "final-cluster.json")
    snapshot = load(evidence, "gateway-routing-snapshot.json")
    process_stop = load(evidence, "process-stop.json")
    directory = load(evidence, "snapshot-directory.json")

    assertions: list[dict] = []

    def check(name: str, passed: bool, observed: object) -> None:
        assertions.append({"name": name, "passed": bool(passed), "observed": observed})

    initial_auth = initial["write_authorization"]
    rejected_auth = after_rejections["write_authorization"]
    final_auth = final_status["write_authorization"]
    initial_shape = {
        "last_log_index": initial["last_log_index"],
        "commit_index": initial["commit_index"],
        "committed_configuration": initial["committed_configuration"],
    }
    rejected_shape = {
        "last_log_index": after_rejections["last_log_index"],
        "commit_index": after_rejections["commit_index"],
        "committed_configuration": after_rejections["committed_configuration"],
    }

    check(
        "three-node control cluster elects one leader",
        election["leader_id"] in {"node-a", "node-b", "node-c"}
        and len([s for s in election["statuses"] if s["body"]["role"] == "leader"]) == 1,
        {"leader": election["leader_id"], "term": election["term"]},
    )
    check(
        "writer authorization is required with explicit trust and revocation",
        initial_auth["required"]
        and initial_auth["trusted_writer_ids"] == ["deploy-bot", "revoked-bot"]
        and initial_auth["revoked_writer_ids"] == ["revoked-bot"]
        and initial_auth["max_age_ms"] == 1000
        and initial_auth["max_future_skew_ms"] == 100,
        initial_auth,
    )

    rejection_cases = [
        ("unsigned write is rejected", unsigned, "authorization is required"),
        ("unknown writer is rejected", unknown, "is not authorized"),
        ("tampered signed route is rejected", tampered, "signature verification failed"),
        ("stale signed intent is rejected", stale, "maximum age"),
        ("revoked writer is rejected", revoked, "is revoked"),
    ]
    for name, attempt, expected_text in rejection_cases:
        body = attempt["response"]["body"]
        detail = body.get("error", {}) if isinstance(body, dict) else {}
        check(
            name,
            attempt["response"]["status"] == 401
            and detail.get("code") == "unauthorized"
            and expected_text in detail.get("message", ""),
            attempt["response"],
        )

    check(
        "all rejected writes leave the Raft log and committed route unchanged",
        initial_shape == rejected_shape and rejected_shape["committed_configuration"] is None,
        {"before": initial_shape, "after": rejected_shape},
    )
    check(
        "leader diagnostics distinguish authentication from freshness rejection",
        rejected_auth["authentication_rejections"] == 4
        and rejected_auth["freshness_rejections"] == 1
        and rejected_auth["verified_intents"] == 1,
        rejected_auth,
    )

    valid_body = valid["response"]["body"]
    check(
        "fresh deploy-bot intent commits revision 2 with durable writer provenance",
        valid["response"]["status"] == 200
        and valid_body["revision"] == 2
        and valid_body["writer"]["writer_id"] == "deploy-bot"
        and valid_body["writer"]["nonce"] == "deploy-write-00001",
        valid_body,
    )
    check(
        "committed response uses the separate route-delivery signing identity",
        valid_body["authentication"]["key_id"] == "route-2026-b"
        and valid_body["writer"]["writer_id"] != valid_body["authentication"]["key_id"],
        {
            "writer_id": valid_body["writer"]["writer_id"],
            "route_key_id": valid_body["authentication"]["key_id"],
        },
    )
    replay_body = replay["response"]["body"]
    check(
        "exact signed replay is fenced by the already-advanced revision",
        replay["response"]["status"] == 409
        and replay_body["error"]["code"] == "revision_conflict"
        and "current revision is 2" in replay_body["error"]["message"],
        replay["response"],
    )
    check(
        "gateway publishes only the authorized revision-2 route",
        gateway_r2["status"]["body"]["routing_snapshot"]["control_revision"] == 2
        and gateway_r2["status"]["body"]["routing_policy"] == "round-robin"
        and [worker["id"] for worker in gateway_r2["status"]["body"]["workers"]]
        == ["cpu-authorized"],
        gateway_r2,
    )
    first_request = request_r2["requests"][0]
    check(
        "authorized revision serves a real model request with route identity",
        request_r2["succeeded"] == 1
        and first_request["worker"] == "cpu-authorized"
        and first_request["config_revision"] == 2
        and first_request["control_signing_key_id"] == "route-2026-b",
        first_request,
    )

    update_body = update["response"]["body"]
    check(
        "new signed intent names revision 2 and commits revision 3",
        update["response"]["status"] == 200
        and update["request"]["expected_revision"] == 2
        and update_body["revision"] == 3
        and update_body["writer"]["nonce"] == "deploy-update-0001",
        update,
    )
    check(
        "gateway advances to the authorized revision-3 route",
        gateway_r3["status"]["body"]["routing_snapshot"]["control_revision"] == 3
        and gateway_r3["status"]["body"]["routing_policy"] == "least-in-flight"
        and [worker["id"] for worker in gateway_r3["status"]["body"]["workers"]]
        == ["cpu-authorized"],
        gateway_r3,
    )
    check(
        "final leader diagnostics count two commits and one revision conflict",
        final_auth["committed_writes"] == 2
        and final_auth["revision_conflicts"] == 1
        and final_auth["last_authorized_writer_id"] == "deploy-bot",
        final_auth,
    )

    durable = [
        status["body"]["committed_configuration"]
        for status in final_cluster["statuses"]
        if status["status"] == 200 and status["body"]["committed_configuration"] is not None
    ]
    check(
        "writer provenance is replicated with the committed revision on all nodes",
        len(durable) == 3
        and all(item["revision"] == 3 for item in durable)
        and all(item["writer"]["writer_id"] == "deploy-bot" for item in durable)
        and all(item["writer"]["nonce"] == "deploy-update-0001" for item in durable),
        durable,
    )
    check(
        "gateway disk snapshot retains route signature but not administrative private material",
        snapshot["revision"] == 3
        and snapshot["authentication"]["key_id"] == "route-2026-b"
        and "writer" not in snapshot
        and "private" not in json.dumps(snapshot).lower(),
        snapshot,
    )
    check(
        "final speculative SSE reaches DONE under authorized revision 3",
        final_stream["status"] == 200
        and final_stream["done_received"]
        and final_stream["config_revision"] == 3
        and final_stream["worker"] == "cpu-authorized",
        final_stream,
    )
    check(
        "process shutdown targets only exact owned children",
        process_stop["scope"] == "owned-child-processes"
        and len(process_stop["pids"]) == 5
        and len(set(process_stop["pids"])) == 5,
        process_stop,
    )
    check(
        "atomic gateway persistence leaves no temporary snapshot file",
        directory["temporary_snapshot_files"] == [],
        directory,
    )

    passed = sum(assertion["passed"] for assertion in assertions)
    result = {
        "schema": "inferlab.control-write-auth-check.v0.19",
        "passed": passed == len(assertions),
        "assertions_passed": passed,
        "assertions_total": len(assertions),
        "cluster_id": "inferlab-primary",
        "writer_id": "deploy-bot",
        "route_signing_key_id": "route-2026-b",
        "authentication_rejections": final_auth["authentication_rejections"],
        "freshness_rejections": final_auth["freshness_rejections"],
        "revision_conflicts": final_auth["revision_conflicts"],
        "committed_writes": final_auth["committed_writes"],
        "final_revision": final_status["committed_configuration"]["revision"],
        "final_stream_duration_ms": final_stream["duration_ms"],
        "assertions": assertions,
    }
    print(json.dumps(result, indent=2, sort_keys=True))
    if not result["passed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
