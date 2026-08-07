#!/usr/bin/env python3
"""Evaluate retained v0.24 mTLS service-trust evidence."""

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

    handshake = load(args.evidence_dir, "tls-handshake.json")
    initial = load(args.evidence_dir, "initial-controls.json")
    g1 = load(args.evidence_dir, "generation-1-receipts.json")
    before_transport = load(args.evidence_dir, "state-before-transport-attacks.json")
    after_transport = load(args.evidence_dir, "state-after-transport-attacks.json")
    post_transport = load(args.evidence_dir, "post-transport-status.json")
    after_transport_controls = load(args.evidence_dir, "after-transport-controls.json")
    plaintext = load(args.evidence_dir, "plaintext-downgrade.json")
    no_client = load(args.evidence_dir, "no-client-certificate.json")
    rogue_client = load(args.evidence_dir, "rogue-client-ca.json")
    wrong_ca = load(args.evidence_dir, "wrong-server-ca.json")
    wrong_hostname = load(args.evidence_dir, "wrong-server-hostname.json")
    tampered = load(args.evidence_dir, "tampered-snapshot.json")
    forged = load(args.evidence_dir, "forged-receipt.json")
    post_application = load(args.evidence_dir, "post-application-attacks.json")
    publish_g2 = load(args.evidence_dir, "publish-g2.json")
    g2_controls = load(args.evidence_dir, "generation-2-convergence.json")
    g2_receipts = load(args.evidence_dir, "generation-2-receipts.json")
    after_g2 = load(args.evidence_dir, "state-after-generation-2.json")
    continuity = load(args.evidence_dir, "online-process-continuity.json")
    distributor_outage = load(args.evidence_dir, "distributor-outage.json")
    cache_restart = load(args.evidence_dir, "cache-restart.json")
    final_cluster = load(args.evidence_dir, "final-cluster.json")
    request = load(args.evidence_dir, "request.json")
    stream = load(args.evidence_dir, "stream.json")
    final_gateway = load(args.evidence_dir, "final-gateway.json")
    sanitization = load(args.evidence_dir, "evidence-sanitization.json")
    private_material = load(args.evidence_dir, "private-material-scan.json")

    assertions: list[dict[str, Any]] = []

    def check(name: str, passed: bool, observation: Any) -> None:
        assertions.append({"name": name, "passed": bool(passed), "observation": observation})

    check(
        "the valid distributor handshake negotiates exactly TLS 1.3",
        handshake["tls_version"] == "TLSv1.3",
        {"tls_version": handshake["tls_version"], "cipher": handshake["cipher"]},
    )
    check(
        "the valid handshake presents a client certificate and authenticates localhost",
        handshake["client_certificate_presented"]
        and handshake["server_hostname"] == "localhost"
        and "localhost" in handshake["peer_subject_alt_names"],
        handshake,
    )

    initial_auth = [status["body"]["service_authentication"] for status in initial["statuses"]]
    check(
        "all three controls remotely boot root-signed generation 1 over mTLS",
        len(initial_auth) == 3
        and all(auth["trust_policy_generation"] == 1 for auth in initial_auth)
        and all(auth["trust_policy_bootstrap_source"] == "remote" for auth in initial_auth),
        initial_auth,
    )
    check(
        "every control reports server and client authentication in mutual-TLS mode",
        all(auth["trust_policy_transport_mode"] == "mutual-tls" for auth in initial_auth)
        and all(auth["trust_policy_server_authentication"] is True for auth in initial_auth)
        and all(auth["trust_policy_client_authentication"] is True for auth in initial_auth),
        initial_auth,
    )
    g1_body = g1["status"]["body"]
    g1_transport = g1_body["transport_security"]
    check(
        "the distributor reports required-client mutual TLS with a TLS 1.3 minimum",
        g1_transport == {
            "mode": "mutual-tls",
            "client_certificate_required": True,
            "minimum_protocol": "TLSv1.3",
        },
        g1_transport,
    )
    g1_receipts = g1_body["receipts"]
    check(
        "the distributor retains three structurally signed generation-1 receipt objects",
        g1_body["snapshot"]["generation"] == 1
        and g1_body["receipt_count"] == 3
        and g1_body["acked_receivers"] == ["node-a/key-a", "node-b/key-a", "node-c/key-a"]
        and g1_body["pending_receivers"] == []
        and len(g1_receipts) == 3
        and all(item["schema"] == "inferlab.service-trust-receipt.v1" for item in g1_receipts)
        and all(item["generation"] == 1 for item in g1_receipts)
        and all(item["authentication"]["algorithm"] == "ed25519" for item in g1_receipts)
        and all(bool(item["authentication"]["signature"]) for item in g1_receipts),
        {
            "receipt_count": g1_body["receipt_count"],
            "acked_receivers": g1_body["acked_receivers"],
        },
    )

    disallowed_transport_errors = {
        "ConnectionRefusedError",
        "TimeoutError",
        "ProxyError",
        "gaierror",
    }
    for label, sample in [
        ("plaintext downgrade", plaintext),
        ("missing client certificate", no_client),
        ("client certificate from a rogue CA", rogue_client),
        ("server certificate under the wrong CA", wrong_ca),
        ("wrong server hostname", wrong_hostname),
    ]:
        check(
            f"{label} fails before any HTTP response",
            sample["failed_before_http_response"] is True
            and sample["observation"]["status"] is None
            and sample["observation"].get("transport_error")
            not in disallowed_transport_errors,
            sample,
        )
    check(
        "wrong server CA and wrong hostname are certificate-verification failures",
        wrong_ca["observation"].get("transport_error")
        == "SSLCertVerificationError"
        and wrong_hostname["observation"].get("transport_error")
        == "SSLCertVerificationError",
        {
            "wrong_server_ca": wrong_ca["observation"],
            "wrong_hostname": wrong_hostname["observation"],
        },
    )

    check(
        "all transport attacks leave every cache and rollback floor byte-for-byte unchanged",
        before_transport["nodes"] == after_transport["nodes"],
        {"before": before_transport["nodes"], "after": after_transport["nodes"]},
    )
    post_transport_body = post_transport["observation"]["body"]
    check(
        "transport attacks leave active distributor generation 1 and its receipt set unchanged",
        post_transport_body["snapshot"]["generation"] == 1
        and post_transport_body["receipt_count"] == 3
        and post_transport_body["pending_receivers"] == [],
        post_transport_body,
    )
    post_transport_auth = [
        status["body"]["service_authentication"]
        for status in after_transport_controls["statuses"]
    ]
    check(
        "all three live controls retain active generation 1 and mutual-TLS diagnostics after transport attacks",
        len(post_transport_auth) == 3
        and all(auth["trust_policy_generation"] == 1 for auth in post_transport_auth)
        and all(auth["trust_policy_transport_mode"] == "mutual-tls" for auth in post_transport_auth)
        and all(auth["trust_policy_server_authentication"] is True for auth in post_transport_auth)
        and all(auth["trust_policy_client_authentication"] is True for auth in post_transport_auth),
        post_transport_auth,
    )

    check(
        "mTLS does not authorize a signature-tampered trust snapshot",
        tampered["observation"]["status"] == 400
        and tampered["observation"]["body"]["error"]["code"] == "invalid_snapshot"
        and "signature" in tampered["observation"]["body"]["error"]["message"].lower(),
        tampered["observation"],
    )
    check(
        "mTLS does not make a signature-forged activation receipt authoritative",
        forged["observation"]["status"] == 400
        and forged["observation"]["body"]["error"]["code"]
        == "invalid_receipt_signature"
        and "signature" in forged["observation"]["body"]["error"]["message"].lower(),
        forged["observation"],
    )
    post_application_body = post_application["observation"]["body"]
    check(
        "application-layer attacks leave generation 1 and its exact three receipts authoritative",
        post_application_body["snapshot"]["generation"] == 1
        and post_application_body["receipt_count"] == 3
        and post_application_body["receipts"] == g1_body["receipts"],
        {
            "generation": post_application_body["snapshot"]["generation"],
            "receipt_count": post_application_body["receipt_count"],
        },
    )

    check(
        "a valid root-signed generation 2 publication succeeds over mTLS",
        publish_g2["observation"]["status"] in {200, 201}
        and publish_g2["observation"]["body"]["generation"] == 2
        and publish_g2["observation"]["body"]["outcome"] == "published",
        publish_g2["observation"],
    )
    g2_auth = [status["body"]["service_authentication"] for status in g2_controls["statuses"]]
    check(
        "all three live controls converge to generation 2 without process replacement",
        len(g2_auth) == 3
        and all(auth["trust_policy_generation"] == 2 for auth in g2_auth)
        and continuity["unchanged_before_cache_restart"] is True,
        {"controls": g2_auth, "continuity": continuity},
    )
    g2_body = g2_receipts["status"]["body"]
    g2_receipt_objects = g2_body["receipts"]
    check(
        "the distributor subsequently retains three structurally signed generation-2 receipt objects",
        g2_body["snapshot"]["generation"] == 2
        and g2_body["receipt_count"] == 3
        and g2_body["pending_receivers"] == []
        and len(g2_receipt_objects) == 3
        and all(item["schema"] == "inferlab.service-trust-receipt.v1" for item in g2_receipt_objects)
        and all(item["generation"] == 2 for item in g2_receipt_objects)
        and all(item["authentication"]["algorithm"] == "ed25519" for item in g2_receipt_objects)
        and all(bool(item["authentication"]["signature"]) for item in g2_receipt_objects),
        {"receipt_count": g2_body["receipt_count"], "pending": g2_body["pending_receivers"]},
    )
    check(
        "generation 2 advances every durable cache and floor from generation 1",
        all(
            after_g2["nodes"][node]["cache_sha256"]
            != before_transport["nodes"][node]["cache_sha256"]
            and after_g2["nodes"][node]["floor_sha256"]
            != before_transport["nodes"][node]["floor_sha256"]
            for node in before_transport["nodes"]
        ),
        {"generation_1": before_transport["nodes"], "generation_2": after_g2["nodes"]},
    )

    restart_auth = cache_restart["status"]["body"]["service_authentication"]
    check(
        "the exact distributor is unavailable during the receiver restart",
        distributor_outage["stopped_pid_alive"] is False
        and distributor_outage["connection_observation"]["scenario"] == "distributor-stopped"
        and distributor_outage["connection_observation"]["failed_before_http_response"] is True
        and distributor_outage["connection_observation"]["observation"]["status"] is None,
        distributor_outage,
    )
    check(
        "a follower restarts as a different PID from its complete generation 2 cache",
        cache_restart["old_pid"] != cache_restart["new_pid"]
        and restart_auth["trust_policy_generation"] == 2
        and restart_auth["trust_policy_bootstrap_source"] == "cache",
        cache_restart,
    )
    check(
        "the cache-bootstrapped receiver retains mTLS diagnostics while receipt delivery fails closed",
        restart_auth["trust_policy_transport_mode"] == "mutual-tls"
        and restart_auth["trust_policy_server_authentication"] is True
        and restart_auth["trust_policy_client_authentication"] is True
        and restart_auth["trust_policy_receipt_failures"] >= 1,
        restart_auth,
    )
    final_statuses = final_cluster["statuses"]
    check(
        "the cache-bootstrapped follower rejoins the three-node revision-2 cluster",
        len(final_statuses) == 3
        and sum(status["body"]["role"] == "leader" for status in final_statuses) == 1
        and all(
            status["body"]["committed_configuration"]["revision"] == 2
            for status in final_statuses
        ),
        final_statuses,
    )

    request_sample = request["requests"][0]
    check(
        "real CPU JSON inference succeeds while the distributor remains down",
        request["succeeded"] == 1
        and request_sample["worker"] == "cpu-mtls-trust"
        and request_sample["config_revision"] == 2,
        request_sample,
    )
    check(
        "real CPU SSE reaches DONE while the distributor remains down",
        stream["status"] == 200
        and stream["done_received"]
        and stream["worker"] == "cpu-mtls-trust"
        and stream["config_revision"] == 2,
        stream,
    )
    final_gateway_status = final_gateway["status"]["body"]
    check(
        "the final gateway remains ready on committed route revision 2",
        final_gateway_status["routing_lease"]["accepting_new_requests"] is True
        and final_gateway_status["control_plane"]["revision"] == 2,
        final_gateway_status,
    )
    check(
        "retained JSON contains no proof-root path or PEM wrapper",
        sanitization["remaining_proof_root_or_certificate_strings"] == 0,
        sanitization,
    )
    check(
        "retained evidence contains no known Ed25519 seed or generated PKI private-key payload",
        private_material["matches"] == 0
        and private_material["known_ed25519_seed_count"] == 7
        and private_material["generated_pki_private_key_count"] == 8
        and private_material["normalized_base64_and_escaped_newlines"] is True,
        private_material,
    )

    failed = [item for item in assertions if not item["passed"]]
    output = {
        "schema": "inferlab.mtls-service-trust-check.v0.24",
        "passed": len(assertions) - len(failed),
        "failed": len(failed),
        "total": len(assertions),
        "assertions": assertions,
    }
    print(json.dumps(output, indent=2, sort_keys=True))
    if failed:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
