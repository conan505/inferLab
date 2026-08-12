#!/usr/bin/env python3
"""Adversarial offline checker for retained v0.30 TLS identity handoff evidence."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import math
import re
import sys
from pathlib import Path
from typing import Any


CONTROLS = ["control-a", "control-b", "control-c"]
SERVICES = [*CONTROLS, "gateway-primary"]
EXPECTED_FILES = {
    "assertions.json",
    "certificate-identities.json",
    "control-handoff.json",
    "discarded-log-scan.json",
    "final-cluster.json",
    "final-json.json",
    "final-sse.json",
    "generation-1-controls.json",
    "generation-1-receipts.json",
    "generation-2-controls.json",
    "generation-2-receipts.json",
    "live-rejections.json",
    "manifest.json",
    "private-material-scan.json",
    "process-continuity.json",
    "production-tests.json",
    "proof-contract.json",
    "publish-g1-publisher-a.json",
    "publish-g2-publisher-b.json",
    "sanitizer.json",
    "server-handoff.json",
    "startup-rejections.json",
    "tls-identity-handoff-proof.svg",
    "trust-generations.json",
}
NON_MANIFEST_FILES = EXPECTED_FILES - {"manifest.json"}
STARTUP_CASES = {
    "missing": "source_unavailable",
    "malformed": "invalid_json",
    "oversize": "bundle_too_large",
    "unsafe-permissions": "unsafe_permissions",
    "symlink": "not_regular_file",
    "wrong-cluster": "cluster_mismatch",
    "wrong-identity": "identity_mismatch",
    "wrong-purpose": "purpose_mismatch",
    "wrong-server-name": "server_name_mismatch",
    "mismatched-key": "private_key_mismatch",
    "expired": "certificate_expired",
    "not-yet-valid": "certificate_not_yet_valid",
    "wrong-eku": "wrong_eku",
    "wrong-san": "wrong_hostname",
    "wrong-ca": "wrong_ca",
}
SERVER_LIVE_CASES = {
    **STARTUP_CASES,
    "issuer-ca-change": "issuer_ca_mismatch",
    "stale": "stale_generation",
    "fork": "generation_fork",
}
CLIENT_LIVE_CASES = {
    "malformed": "invalid_json",
    "unsafe-permissions": "unsafe_permissions",
    "symlink": "not_regular_file",
    "wrong-cluster": "cluster_mismatch",
    "wrong-identity": "identity_mismatch",
    "wrong-purpose": "purpose_mismatch",
    "mismatched-key": "private_key_mismatch",
    "expired": "certificate_expired",
    "not-yet-valid": "certificate_not_yet_valid",
    "wrong-eku": "wrong_eku",
    "wrong-ca": "wrong_ca",
    "issuer-ca-change": "issuer_ca_mismatch",
}
EXPECTED_ROOT_PUBLIC_KEY = "U/5XcqY9UARAOD50ggOsuQFBDS4fkG/wT0mugjKw5U0="
EXPECTED_SERVICE_PUBLIC_KEYS = {
    "control-a": "uCnmw0eBwYLDkst+btjxrecsdtjJWQGlHuPn+TUpl0E=",
    "control-b": "WRyuOeetfxJzStqf40HjqPegdLCBFlBYMtOs6GtPy7o=",
    "control-c": "qV9kZwNbqhOKZfDH4AmgOz3D8sgAR8fUIk7TP9zmnAk=",
    "gateway-primary": "pZryG2zfszyPwgluS8mdLwPj0zwjESUq3Pn4zjsVPNU=",
}
PRODUCTION_TESTS = {
    "identity_bundle::tests::strict_bundle_is_bound_bounded_and_redacted":
        ("transport-security", ["--lib"]),
    "identity_bundle::tests::purpose_hostname_ca_and_private_key_fail_closed":
        ("transport-security", ["--lib"]),
    "identity_bundle::tests::current_time_and_eku_are_verified":
        ("transport-security", ["--lib"]),
    "identity_bundle::tests::file_loader_rejects_permissions_and_symlinks":
        ("transport-security", ["--lib"]),
    "identity_bundle::tests::activation_rejects_rollback_fork_ca_change_and_runtime_failure":
        ("transport-security", ["--lib"]),
    "identity_bundle::tests::concurrent_snapshots_are_entirely_old_or_new":
        ("transport-security", ["--lib"]),
    "identity_bundle::tests::watcher_loop_deduplicates_deterministic_errors_and_retries_time_dependent_sources":
        ("transport-security", ["--lib"]),
    "tests::watched_tls_identity_configuration_is_strictly_separate_and_bounded":
        ("trust-distributor", ["--bin", "trust-distributor"]),
    "tests::tls_identity_watcher_completion_is_process_supervised":
        ("trust-distributor", ["--bin", "trust-distributor"]),
    "service_trust::tests::watched_client_identity_swaps_the_whole_pool_for_new_operations":
        ("control-plane", ["--lib"]),
    "tests::supervisor_fails_when_the_tls_identity_watcher_completes":
        ("control-plane", ["--bin", "control-plane"]),
    "tests::malformed_unicode_tls_path_fails_closed":
        ("control-plane", ["--bin", "control-plane"]),
}
PROCESS_BINARIES = {
    "control-a": "control-plane",
    "control-b": "control-plane",
    "control-c": "control-plane",
    "cpu-worker": "cpu-worker",
    "gateway": "gateway",
    "trust-distributor": "trust-distributor",
}
SHA256 = re.compile(r"^[0-9a-f]{64}$")


Q = 2**255 - 19
L = 2**252 + 27742317777372353535851937790883648493
D = (-121665 * pow(121666, Q - 2, Q)) % Q
I = pow(2, (Q - 1) // 4, Q)


def xrecover(y: int) -> int:
    xx = (y * y - 1) * pow(D * y * y + 1, Q - 2, Q) % Q
    x = pow(xx, (Q + 3) // 8, Q)
    if (x * x - xx) % Q:
        x = x * I % Q
    if x & 1:
        x = Q - x
    return x


BY = 4 * pow(5, Q - 2, Q) % Q
BX = xrecover(BY)
B = (BX, BY)


def edwards(left: tuple[int, int], right: tuple[int, int]) -> tuple[int, int]:
    x1, y1 = left
    x2, y2 = right
    denominator = D * x1 * x2 * y1 * y2
    return (
        (x1 * y2 + x2 * y1) * pow(1 + denominator, Q - 2, Q) % Q,
        (y1 * y2 + x1 * x2) * pow(1 - denominator, Q - 2, Q) % Q,
    )


def scalarmult(point: tuple[int, int], scalar: int) -> tuple[int, int]:
    result = (0, 1)
    addend = point
    while scalar:
        if scalar & 1:
            result = edwards(result, addend)
        addend = edwards(addend, addend)
        scalar >>= 1
    return result


def decodepoint(encoded: bytes) -> tuple[int, int] | None:
    if len(encoded) != 32:
        return None
    value = int.from_bytes(encoded, "little")
    y = value & ((1 << 255) - 1)
    if y >= Q:
        return None
    sign = value >> 255
    x = xrecover(y)
    if x == 0 and sign == 1:
        return None
    if (x & 1) != sign:
        x = Q - x
    point = (x, y)
    canonical = (y | ((x & 1) << 255)).to_bytes(32, "little")
    if (
        canonical != encoded
        or (-x * x + y * y - 1 - D * x * x * y * y) % Q
        or scalarmult(point, L) != (0, 1)
        or scalarmult(point, 8) == (0, 1)
    ):
        return None
    return point


def verify_ed25519(public: str, message: bytes, signature: str) -> bool:
    try:
        public_bytes = base64.b64decode(public, validate=True)
        signature_bytes = base64.b64decode(signature, validate=True)
    except (ValueError, base64.binascii.Error):
        return False
    if len(public_bytes) != 32 or len(signature_bytes) != 64:
        return False
    public_point = decodepoint(public_bytes)
    r_point = decodepoint(signature_bytes[:32])
    scalar = int.from_bytes(signature_bytes[32:], "little")
    if public_point is None or r_point is None or scalar >= L:
        return False
    challenge = int.from_bytes(
        hashlib.sha512(signature_bytes[:32] + public_bytes + message).digest(), "little"
    ) % L
    return scalarmult(B, scalar) == edwards(r_point, scalarmult(public_point, challenge))


def append_string(buffer: bytearray, value: str) -> None:
    encoded = value.encode()
    buffer.extend(len(encoded).to_bytes(4, "big"))
    buffer.extend(encoded)


def append_count(buffer: bytearray, value: int) -> None:
    buffer.extend(value.to_bytes(4, "big"))


def canonical_snapshot(snapshot: dict[str, Any]) -> bytes:
    output = bytearray(b"inferlab.service-trust-policy.v2\0")
    append_string(output, snapshot["schema"])
    append_string(output, snapshot["cluster_id"])
    output.extend(snapshot["generation"].to_bytes(8, "big"))
    output.extend(snapshot["issued_at_ms"].to_bytes(8, "big"))
    output.extend(snapshot["expires_at_ms"].to_bytes(8, "big"))
    append_count(output, len(snapshot["trusted_credentials"]))
    for item in snapshot["trusted_credentials"]:
        append_string(output, item["service_id"])
        append_string(output, item["credential_id"])
        append_string(output, item["public_key_base64"])
    append_count(output, len(snapshot["revoked_service_ids"]))
    for item in snapshot["revoked_service_ids"]:
        append_string(output, item)
    append_count(output, len(snapshot["revoked_credentials"]))
    for item in snapshot["revoked_credentials"]:
        append_string(output, item["service_id"])
        append_string(output, item["credential_id"])
    append_count(output, len(snapshot["gateway_service_ids"]))
    for item in snapshot["gateway_service_ids"]:
        append_string(output, item)
    authentication = snapshot["authentication"]
    append_string(output, authentication["schema"])
    append_string(output, authentication["algorithm"])
    append_string(output, authentication["key_id"])
    return bytes(output)


def canonical_receipt(receipt: dict[str, Any]) -> bytes:
    output = bytearray(b"inferlab.service-trust-receipt.v1\0")
    append_string(output, receipt["schema"])
    append_string(output, receipt["cluster_id"])
    output.extend(receipt["generation"].to_bytes(8, "big"))
    append_string(output, receipt["root_key_id"])
    append_string(output, receipt["snapshot_signature"])
    append_string(output, receipt["receiver_service_id"])
    append_string(output, receipt["receiver_credential_id"])
    output.extend(receipt["applied_at_ms"].to_bytes(8, "big"))
    authentication = receipt["authentication"]
    append_string(output, authentication["schema"])
    append_string(output, authentication["algorithm"])
    return bytes(output)


class EvidenceError(Exception):
    pass


def reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON key")
        result[key] = value
    return result


def load(directory: Path, name: str) -> Any:
    try:
        with (directory / name).open(encoding="utf-8") as source:
            return json.load(source, object_pairs_hook=reject_duplicate_keys)
    except (OSError, UnicodeError, ValueError, RecursionError):
        raise EvidenceError(name) from None


def exact_int(value: Any, expected: int | None = None) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and (expected is None or value == expected)


def finite_number(value: Any) -> bool:
    return (
        (exact_int(value) or isinstance(value, float))
        and not isinstance(value, bool)
        and math.isfinite(value)
        and value >= 0
    )


def assertion(name: str, passed: bool, observations: dict[str, Any] | None = None) -> dict[str, Any]:
    return {"name": name, "passed": bool(passed), "observations": observations or {}}


def exact_inventory(directory: Path, require_manifest: bool) -> bool:
    try:
        entries = list(directory.iterdir())
    except OSError:
        return False
    manifest_present = (directory / "manifest.json").is_file()
    expected = EXPECTED_FILES if require_manifest or manifest_present else NON_MANIFEST_FILES
    return (
        {item.name for item in entries} == expected
        and all(item.is_file() and not item.is_symlink() for item in entries)
    )


def exact_manifest(directory: Path, document: Any) -> bool:
    if not isinstance(document, dict) or set(document) != {"schema", "file_count", "files"}:
        return False
    files = document["files"]
    if (
        document["schema"] != "inferlab.tls-identity-handoff-manifest.v0.30"
        or document["file_count"] != len(NON_MANIFEST_FILES)
        or not isinstance(files, list)
        or [item.get("name") for item in files] != sorted(NON_MANIFEST_FILES)
    ):
        return False
    for item in files:
        if not isinstance(item, dict) or set(item) != {"name", "bytes", "sha256"}:
            return False
        path = directory / item["name"]
        raw = path.read_bytes()
        if item["bytes"] != len(raw) or item["sha256"] != hashlib.sha256(raw).hexdigest():
            return False
    return True


def fingerprints(document: Any) -> bool:
    try:
        values = [document["server"]["A"], document["server"]["B"]]
        values += [document["publisher_proof_clients"]["A"], document["publisher_proof_clients"]["B"]]
        for control in CONTROLS:
            values += [document["controls"][control]["A"], document["controls"][control]["B"]]
        return (
            document["schema"] == "inferlab.tls-identity-handoff-certificates.v0.30"
            and document["digest"] == "sha256-der"
            and document["issuer_ca_unchanged"] is True
            and document["publisher_proof_clients"]["semantics"]
            == "fresh clients; no process continuity claim"
            and len(values) == 10
            and document["server"]["A"] != document["server"]["B"]
            and document["publisher_proof_clients"]["A"]
            != document["publisher_proof_clients"]["B"]
            and all(
                document["controls"][control]["A"] != document["controls"][control]["B"]
                for control in CONTROLS
            )
            and all(isinstance(value, str) and SHA256.fullmatch(value) for value in values)
        )
    except (KeyError, TypeError):
        return False


def startup_matrix(document: Any) -> bool:
    try:
        cases = document["cases"]
        observed = {case["scenario"]: case for case in cases}
        return (
            document["schema"] == "inferlab.tls-identity-handoff-startup-rejections.v0.30"
            and len(cases) == len(STARTUP_CASES)
            and set(observed) == set(STARTUP_CASES)
            and all(
                case["expected_error_kind"] == STARTUP_CASES[name]
                and exact_int(case["exit_code"])
                and case["exit_code"] != 0
                and case["listener_ever_open"] is False
                and exact_int(case["listener_probe_count"])
                and case["listener_probe_count"] >= 1
                and isinstance(case["diagnostic"], str)
                and case["diagnostic"].startswith("TLS identity")
                for name, case in observed.items()
            )
        )
    except (KeyError, TypeError):
        return False


def trust_documents(document: Any) -> bool:
    try:
        root = document["root_public_key_base64"]
        generations = document["generations"]
        if (
            document["schema"] != "inferlab.tls-identity-handoff-trust-generations.v0.30"
            or document["root_key_id"] != "service-trust-root-v030"
            or root != EXPECTED_ROOT_PUBLIC_KEY
            or set(generations) != {"1", "2"}
        ):
            return False
        for number in (1, 2):
            snapshot = generations[str(number)]
            authentication = snapshot["authentication"]
            credentials = snapshot["trusted_credentials"]
            if (
                snapshot["schema"] != "inferlab.service-trust-policy.v2"
                or snapshot["cluster_id"] != "inferlab-primary"
                or snapshot["generation"] != number
                or snapshot["issued_at_ms"] >= snapshot["expires_at_ms"]
                or [(item["service_id"], item["credential_id"]) for item in credentials]
                != [(service, "key-a") for service in SERVICES]
                or {item["service_id"]: item["public_key_base64"] for item in credentials}
                != EXPECTED_SERVICE_PUBLIC_KEYS
                or snapshot["revoked_service_ids"] != []
                or snapshot["revoked_credentials"] != []
                or snapshot["gateway_service_ids"] != ["gateway-primary"]
                or authentication["schema"] != "inferlab.service-trust-authentication.v2"
                or authentication["algorithm"] != "ed25519"
                or authentication["key_id"] != document["root_key_id"]
                or not verify_ed25519(root, canonical_snapshot(snapshot), authentication["signature"])
            ):
                return False
        return (
            generations["1"]["issued_at_ms"] == generations["2"]["issued_at_ms"]
            and generations["1"]["expires_at_ms"] == generations["2"]["expires_at_ms"]
        )
    except (KeyError, TypeError, OverflowError):
        return False


def exact_identity(
    identity: Any,
    identity_id: str,
    purpose: str,
    generation: int,
    expected_sha256: str | None = None,
) -> bool:
    if not isinstance(identity, dict):
        return False
    expected_scope = "newly-accepted-tls-connections" if purpose == "server" else "new-http-client-snapshots"
    return (
        identity.get("mode") == "watched-bundle"
        and identity.get("identity_id") == identity_id
        and identity.get("purpose") == purpose
        and identity.get("bundle_generation") == generation
        and isinstance(identity.get("leaf_certificate_sha256"), str)
        and SHA256.fullmatch(identity["leaf_certificate_sha256"]) is not None
        and (expected_sha256 is None or identity["leaf_certificate_sha256"] == expected_sha256)
        and identity.get("certificate_chain_length") == 1
        and identity.get("issuer_ca_count") == 1
        and exact_int(identity.get("successful_activations"))
        and exact_int(identity.get("rejected_reloads"))
        and identity.get("activation_scope") == expected_scope
        and (
            identity.get("preaccepted_or_established_connections") == "may-retain-captured-identity"
            if purpose == "server"
            else identity.get("in_flight_operations") == "retain-captured-client"
        )
    )


def control_document(
    document: Any,
    policy_generation: int,
    tls_generation: int,
    receipt: bool,
    certificates: Any,
) -> bool:
    try:
        result = document["result"]
        controls = result["controls"]
        leader = result["leader_id"]
        if (
            document["schema"] != "inferlab.tls-identity-handoff-controls.v0.30"
            or document["samples"] < 1
            or len(controls) != 3
            or {item["node_id"] for item in controls} != set(CONTROLS)
            or sum(item["role"] == "leader" for item in controls) != 1
            or any(item["leader_id"] != leader for item in controls)
            or len({item["term"] for item in controls}) != 1
            or any(item["committed_configuration"]["revision"] != 2 for item in controls)
            or any(item["storage_healthy"] is not True for item in controls)
        ):
            return False
        for control in controls:
            authentication = control["service_authentication"]
            if (
                authentication["trust_policy_generation"] != policy_generation
                or authentication["trust_policy_validity"] != "valid"
                or authentication["trust_policy_transport_mode"] != "mutual-tls"
                or authentication["trust_policy_server_authentication"] is not True
                or authentication["trust_policy_client_authentication"] is not True
                or authentication["trust_policy_tls_identity"].get("last_error_kind") is not None
                or authentication["trust_policy_last_fetch_tls_bundle_generation"] != tls_generation
                or not exact_identity(
                    authentication["trust_policy_tls_identity"],
                    control["node_id"],
                    "client",
                    tls_generation,
                    certificates["controls"][control["node_id"]]["A" if tls_generation == 1 else "B"],
                )
            ):
                return False
            if receipt and (
                authentication["trust_policy_last_receipt_generation"] != policy_generation
                or authentication["trust_policy_last_receipt_tls_bundle_generation"] != tls_generation
            ):
                return False
        return True
    except (KeyError, TypeError):
        return False


def receipt_document(
    document: Any,
    trust: Any,
    generation: int,
    tls_generation: int,
    peer: str,
) -> bool:
    try:
        observation = document["result"]
        body = observation["body"]
        snapshot_status = body["snapshot"]
        transport = body["transport_security"]
        identity = transport["identity"]
        receipts = body["receipts"]
        snapshot = trust["generations"][str(generation)]
        if (
            document["schema"] != "inferlab.tls-identity-handoff-distributor.v0.30"
            or document["samples"] < 1
            or observation["status"] != 200
            or observation["tls_peer_certificate_sha256"] != peer
            or body["cluster_id"] != "inferlab-primary"
            or body["expected_receiver_mode"] != "service-id"
            or body["expected_receivers"] != CONTROLS
            or body["acked_receivers"] != CONTROLS
            or body["pending_receivers"] != []
            or body["receipt_count"] != 3
            or snapshot_status["generation"] != generation
            or snapshot_status["root_key_id"] != trust["root_key_id"]
            or transport["mode"] != "mutual-tls"
            or transport["client_certificate_required"] is not True
            or transport["minimum_protocol"] != "TLSv1.3"
            or not exact_identity(identity, "trust-distributor", "server", tls_generation, peer)
            or identity["last_error_kind"] is not None
            or len(receipts) != 3
        ):
            return False
        keys = {
            item["service_id"]: item["public_key_base64"] for item in snapshot["trusted_credentials"]
        }
        seen = set()
        signatures = set()
        for receipt in receipts:
            authentication = receipt["authentication"]
            receiver = receipt["receiver_service_id"]
            if (
                receipt["schema"] != "inferlab.service-trust-receipt.v1"
                or receipt["cluster_id"] != "inferlab-primary"
                or receipt["generation"] != generation
                or receipt["root_key_id"] != trust["root_key_id"]
                or receipt["snapshot_signature"] != snapshot["authentication"]["signature"]
                or receiver not in CONTROLS
                or receipt["receiver_credential_id"] != "key-a"
                or authentication["schema"] != "inferlab.service-trust-receipt-authentication.v1"
                or authentication["algorithm"] != "ed25519"
                or not verify_ed25519(keys[receiver], canonical_receipt(receipt), authentication["signature"])
            ):
                return False
            seen.add(receiver)
            signatures.add(authentication["signature"])
        return seen == set(CONTROLS) and len(signatures) == 3
    except (KeyError, TypeError, OverflowError):
        return False


def publish_document(document: Any, generation: int, peer: str) -> bool:
    try:
        observation = document["observation"]
        return (
            document["schema"] == "inferlab.tls-identity-handoff-capture.v0.30"
            and document["connection_model"] == "fresh-process-fresh-client"
            and exact_int(document["probe_pid"])
            and document["probe_pid"] > 0
            and observation["method"] == "POST"
            and observation["path"] == "/v1/service-trust/snapshot"
            and observation["status"] == 201
            and observation["tls_peer_certificate_sha256"] == peer
            and observation["body"]["generation"] == generation
            and observation["body"]["root_key_id"] == "service-trust-root-v030"
        )
    except (KeyError, TypeError):
        return False


def live_case(case: Any, expected_kind: str, role: str) -> bool:
    try:
        before = case["before"]
        rejected = case["rejected"]
        recovered = case["recovered"]
        before_identity = before["identity"]
        rejected_identity = rejected["identity"]
        recovered_identity = recovered["identity"]
        identity_id = "trust-distributor" if role == "server" else "control-a"
        generation = before_identity["bundle_generation"]
        expected_activations = 1 if role == "server" and generation == 2 else 0
        if (
            case["expected_error_kind"] != expected_kind
            or not exact_int(case["pid"])
            or not exact_identity(before_identity, identity_id, role, generation)
            or not exact_identity(rejected_identity, identity_id, role, generation)
            or not exact_identity(recovered_identity, identity_id, role, generation)
            or before_identity["last_error_kind"] is not None
            or rejected_identity["last_error_kind"] != expected_kind
            or recovered_identity["last_error_kind"] is not None
            or rejected_identity["rejected_reloads"] != before_identity["rejected_reloads"] + 1
            or recovered_identity["rejected_reloads"] != rejected_identity["rejected_reloads"]
            or rejected_identity["successful_activations"] != before_identity["successful_activations"]
            or recovered_identity["successful_activations"] != before_identity["successful_activations"]
            or before_identity["successful_activations"] != expected_activations
            or before_identity["leaf_certificate_sha256"]
            != rejected_identity["leaf_certificate_sha256"]
            or before_identity["leaf_certificate_sha256"]
            != recovered_identity["leaf_certificate_sha256"]
        ):
            return False
        if role == "server":
            return (
                case["lkg_handshakes_succeeded"] is True
                and before["peer_sha256"] == rejected["peer_sha256"] == recovered["peer_sha256"]
                and SHA256.fullmatch(before["peer_sha256"]) is not None
            )
        return (
            case["lkg_fetches_succeeded"] is True
            and before["last_fetch_tls_bundle_generation"]
            == rejected["last_fetch_tls_bundle_generation"]
            == recovered["last_fetch_tls_bundle_generation"]
            == generation
            and rejected["last_fetch_outcome"] in {"not-modified", "accepted", "unchanged"}
        )
    except (KeyError, TypeError):
        return False


def live_matrix(document: Any) -> tuple[bool, dict[str, int]]:
    try:
        server_cases = document["server_cases"]
        client_cases = document["client_cases"]
        server_names = [case["scenario"] for case in server_cases]
        # issuer-ca-change is deliberately exercised against both A and B.
        expected_server_count = len(SERVER_LIVE_CASES) + 1
        server_ok = (
            len(server_cases) == expected_server_count
            and set(server_names) == set(SERVER_LIVE_CASES)
            and server_names.count("issuer-ca-change") == 2
            and all(live_case(case, SERVER_LIVE_CASES[case["scenario"]], "server") for case in server_cases)
        )
        client_ok = (
            len(client_cases) == len(CLIENT_LIVE_CASES)
            and {case["scenario"] for case in client_cases} == set(CLIENT_LIVE_CASES)
            and all(live_case(case, CLIENT_LIVE_CASES[case["scenario"]], "client") for case in client_cases)
        )
        return document["schema"] == "inferlab.tls-identity-handoff-live-rejections.v0.30" and server_ok and client_ok, {
            "server_cases": len(server_cases),
            "client_cases": len(client_cases),
        }
    except (KeyError, TypeError):
        return False, {"server_cases": 0, "client_cases": 0}


def server_handoff(document: Any, certificates: Any) -> bool:
    try:
        a = certificates["server"]["A"]
        b = certificates["server"]["B"]
        ready = document["ready"]
        status_barrier = document["status_activation_barrier"]["result"]
        new_connection = document["new_connection_after_activation"]["result"]
        held = document["held_connection"]
        status_identity = status_barrier["body"]["transport_security"]["identity"]
        return (
            document["schema"] == "inferlab.tls-identity-handoff-server.v0.30"
            and document["barriers"] == ["ready-A", "B-active", "release"]
            and document["expected_fingerprints"] == {"A": a, "B": b}
            and ready["tls_peer_certificate_sha256"] == a
            and ready["first_status"] == 200
            and exact_identity(status_identity, "trust-distributor", "server", 2, b)
            and status_identity["successful_activations"] == 1
            and status_identity["last_error_kind"] is None
            and status_barrier["observed_at_ms"] <= new_connection["started_at_ms"]
            and new_connection["tls_peer_certificate_sha256"] == b
            and new_connection["status"] == 200
            and held["tls_peer_certificate_sha256"] == a
            and held["first_status"] == held["second_status"] == 200
            and held["same_tls_connection"] is True
            and ready["pid"] == held["pid"]
            and ready["opened_at_ms"] == held["opened_at_ms"]
        )
    except (KeyError, TypeError):
        return False


def exact_processes(items: Any) -> bool:
    if not isinstance(items, list) or len(items) != len(PROCESS_BINARIES):
        return False
    return (
        {item.get("label") for item in items} == set(PROCESS_BINARIES)
        and all(
            item.get("command") == PROCESS_BINARIES[item.get("label")]
            and exact_int(item.get("pid"))
            and exact_int(item.get("ppid"))
            and item.get("pid") > 0
            and item.get("ppid") > 0
            and isinstance(item.get("start_token"), str)
            and "Z" not in item.get("state", "")
            for item in items
        )
    )


def process_identity(items: list[dict[str, Any]]) -> list[list[Any]]:
    return [[item[key] for key in ("label", "pid", "ppid", "start_token", "command")] for item in items]


def control_handoff(document: Any, certificates: Any) -> bool:
    try:
        initial = document["initial_processes"]
        steps = document["steps"]
        expected = {"control-a": 1, "control-b": 1, "control-c": 1}
        if (
            document["schema"] != "inferlab.tls-identity-handoff-controls-sequence.v0.30"
            or document["order"] != ["control-b", "control-c", "control-a"]
            or not exact_processes(initial)
            or len(steps) != 3
        ):
            return False
        baseline = process_identity(initial)
        for index, (step, identity_id) in enumerate(zip(steps, document["order"]), 1):
            expected[identity_id] = 2
            controls = step["controls"]["result"]["controls"]
            if (
                step["step"] != index
                or step["identity_id"] != identity_id
                or not exact_processes(step["processes"])
                or process_identity(step["processes"]) != baseline
                or {item["node_id"] for item in controls} != set(CONTROLS)
            ):
                return False
            for control in controls:
                authentication = control["service_authentication"]
                generation = expected[control["node_id"]]
                if (
                    authentication["trust_policy_generation"] != 1
                    or authentication["trust_policy_last_fetch_tls_bundle_generation"] != generation
                    or authentication["trust_policy_tls_identity"].get("last_error_kind") is not None
                    or not exact_identity(
                        authentication["trust_policy_tls_identity"],
                        control["node_id"],
                        "client",
                        generation,
                        certificates["controls"][control["node_id"]]["A" if generation == 1 else "B"],
                    )
                ):
                    return False
        return True
    except (KeyError, TypeError):
        return False


def process_continuity(document: Any) -> bool:
    try:
        initial = document["initial"]
        final = document["final"]
        return (
            document["schema"] == "inferlab.tls-identity-handoff-process-continuity.v0.30"
            and document["publisher_processes_in_scope"] is False
            and document["unchanged"] is True
            and exact_processes(initial)
            and exact_processes(final)
            and process_identity(initial) == process_identity(final)
            and all(item["ppid"] == document["proof_shell_pid"] for item in initial + final)
        )
    except (KeyError, TypeError):
        return False


def production_tests(document: Any) -> bool:
    try:
        tests = document["tests"]
        if (
            document["schema"] == "inferlab.tls-identity-handoff-production-tests.v0.30"
            and document["test_count"] == len(PRODUCTION_TESTS)
            and len(tests) == len(PRODUCTION_TESTS)
            and {item["test_filter"] for item in tests} == set(PRODUCTION_TESTS)
        ) is False:
            return False
        summary = re.compile(
            r"^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; "
            r"[0-9]+ filtered out; finished in [0-9]+(?:\.[0-9]+)?s$"
        )
        for item in tests:
            test_filter = item["test_filter"]
            package, target = PRODUCTION_TESTS[test_filter]
            lines = item["output_lines"]
            if (
                set(item) != {"package", "target", "test_filter", "exit_code", "output_lines"}
                or item["package"] != package
                or item["target"] != target
                or item["exit_code"] != 0
                or not isinstance(lines, list)
                or len(lines) != 3
                or lines[0] != "running 1 test"
                or lines[1] != f"test {test_filter} ... ok"
                or not summary.fullmatch(lines[2])
            ):
                return False
        return True
    except (KeyError, TypeError):
        return False


def final_json(document: Any) -> bool:
    try:
        observation = document["observation"]
        body = observation["body"]
        generation = body["inferlab"]["generation"]
        return (
            document["schema"] == "inferlab.tls-identity-handoff-json.v0.30"
            and observation["status"] == 200
            and observation["path"] == "/v1/chat/completions"
            and observation["tls_peer_certificate_sha256"] is None
            and body["object"] == "chat.completion"
            and isinstance(body["choices"], list)
            and len(body["choices"]) == 1
            and generation["mode"] == "paged-kv-cache"
            and generation["query_tokens"] > 0
            and generation["decoding"]["greedy_steps"] > 0
        )
    except (KeyError, TypeError):
        return False


def final_sse(document: Any) -> bool:
    try:
        return (
            document["schema"] == "inferlab.tls-identity-handoff-sse.v0.30"
            and document["status"] == 200
            and document["path"] == "/v1/chat/completions"
            and document["done_received"] is True
            and document["eof_after_done"] is True
            and document["event_count"] >= 3
            and document["content_event_count"] >= 1
            and document["finish_reason"] == "stop"
            and "v030-sse-private-proof-prompt" not in document["content"]
            and document["generation"]["mode"] == "paged-kv-cache"
            and document["generation"]["query_tokens"] > 0
            and document["generation"]["decoding"]["greedy_steps"] > 0
            and len(document["offsets_ms"]) == document["event_count"]
            and all(finite_number(value) for value in document["offsets_ms"])
        )
    except (KeyError, TypeError):
        return False


def run(directory: Path, require_manifest: bool) -> dict[str, Any]:
    if not exact_inventory(directory, require_manifest):
        raise EvidenceError("inventory")
    if require_manifest and not exact_manifest(directory, load(directory, "manifest.json")):
        raise EvidenceError("manifest")
    names = sorted(NON_MANIFEST_FILES - {"assertions.json", "tls-identity-handoff-proof.svg"})
    documents = {name: load(directory, name) for name in names if name.endswith(".json")}
    certificates = documents["certificate-identities.json"]
    trust = documents["trust-generations.json"]
    publish_one = documents["publish-g1-publisher-a.json"]
    publish_two = documents["publish-g2-publisher-b.json"]
    live_ok, live_counts = live_matrix(documents["live-rejections.json"])
    checks = [
        assertion("the retained inventory is exact and contains no links", True, {"non_manifest_file_count": len(NON_MANIFEST_FILES)}),
        assertion("the proof contract limits renewal to one CA and excludes publisher process continuity", documents["proof-contract.json"] == {
            "cluster_id": "inferlab-primary",
            "connection_barriers": ["held-server-ready", "server-B-active", "held-server-release"],
            "controls": CONTROLS,
            "identity_generations": [1, 2],
            "policy_generations": [1, 2],
            "processes": ["control-a", "control-b", "control-c", "cpu-worker", "gateway", "trust-distributor"],
            "publisher_semantics": "A and B are separate fresh proof clients; neither is a retained runtime process",
            "schema": "inferlab.tls-identity-handoff-proof-contract.v0.30",
            "server_name": "localhost",
            "tls_protocol": "TLSv1.3",
        }),
        assertion("all public leaf fingerprints are bounded, distinct SHA-256 DER identities under an unchanged issuer", fingerprints(certificates)),
        assertion("all fifteen startup candidates fail before the listener opens", startup_matrix(documents["startup-rejections.json"]), {"case_count": len(documents["startup-rejections.json"].get("cases", []))}),
        assertion("both policies are exact independently root-signed Ed25519 documents", trust_documents(trust)),
        assertion("publisher A uses a fresh proof client to publish policy generation one", publish_document(publish_one, 1, certificates["server"]["A"])),
        assertion("publisher B uses a different fresh proof client to publish policy generation two", publish_document(publish_two, 2, certificates["server"]["B"]) and publish_one.get("probe_pid") != publish_two.get("probe_pid")),
        assertion("publisher freshness is not represented as runtime process continuity", "publisher" not in PROCESS_BINARIES and documents["process-continuity.json"].get("publisher_processes_in_scope") is False),
        assertion("generation one converges on all three watched client-A identities", control_document(documents["generation-1-controls.json"], 1, 1, True, certificates)),
        assertion("generation one retains three cryptographically verified policy receipts over server A", receipt_document(documents["generation-1-receipts.json"], trust, 1, 1, certificates["server"]["A"])),
        assertion("the full live server and client rejection matrices retain their LKG runtime objects", live_ok, live_counts),
        assertion("server activation is observed before a wholly new B connection while the held A connection remains A", server_handoff(documents["server-handoff.json"], certificates)),
        assertion("all three watched controls replace their clients sequentially with fresh B pools", control_handoff(documents["control-handoff.json"], certificates)),
        assertion("generation two converges on all three watched client-B identities and B receipts", control_document(documents["generation-2-controls.json"], 2, 2, True, certificates)),
        assertion("generation two retains three cryptographically verified policy receipts over server B", receipt_document(documents["generation-2-receipts.json"], trust, 2, 2, certificates["server"]["B"])),
        assertion("the final Raft cluster remains single-leader and fully converged", control_document(documents["final-cluster.json"], 2, 2, True, certificates)),
        assertion("six long-lived runtime process identities remain unchanged", process_continuity(documents["process-continuity.json"]), {"process_count": 6}),
        assertion("all twelve exact production regressions pass", production_tests(documents["production-tests.json"]), {"test_count": documents["production-tests.json"].get("test_count")}),
        assertion("a real deterministic CPU JSON completion succeeds after renewal", final_json(documents["final-json.json"])),
        assertion("a real incremental CPU SSE stream reaches DONE and EOF after renewal", final_sse(documents["final-sse.json"])),
        assertion("discarded logs contain no proof-private material", documents["discarded-log-scan.json"].get("passed") is True and documents["discarded-log-scan.json"].get("matches") == []),
        assertion("retained JSON and SVG inputs contain no private markers, paths, or sensitive fields", documents["sanitizer.json"].get("problem_count") == 0 and documents["sanitizer.json"].get("problems") == []),
        assertion("deterministic secret representations are absent from retained evidence", documents["private-material-scan.json"].get("passed") is True and documents["private-material-scan.json"].get("matches") == []),
    ]
    passed = sum(item["passed"] for item in checks)
    return {
        "schema": "inferlab.tls-identity-handoff-assertions.v0.30",
        "passed": passed,
        "total": len(checks),
        "all_passed": passed == len(checks),
        "assertions": checks,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--require-manifest", action="store_true")
    args = parser.parse_args()
    try:
        report = run(args.evidence_dir, args.require_manifest)
    except EvidenceError as error:
        print(f"invalid v0.30 evidence: {error}", file=sys.stderr)
        raise SystemExit(1) from None
    encoded = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.output is None:
        sys.stdout.write(encoded)
    else:
        args.output.write_text(encoded, encoding="utf-8")
    if not report["all_passed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
