#!/usr/bin/env python3
"""Adversarial offline checker for retained v0.31 policy-renewal evidence."""

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
EXPECTED_RECEIVERS = CONTROLS
GENERATION_LABELS = ["cold-start", "normal", "ambiguous-reconcile", "late-recovery"]
EXPECTED_FILES = {
    "ambiguous-retry.json",
    "assertions.json",
    "authority.json",
    "automatic-generations.json",
    "discarded-log-scan.json",
    "expiry-outage-recovery.json",
    "fault-gate.json",
    "final-cluster.json",
    "final-json.json",
    "final-sse.json",
    "manifest.json",
    "normal-renewal.json",
    "private-material-scan.json",
    "process-continuity.json",
    "production-tests.json",
    "proof-contract.json",
    "protected-request-continuity.json",
    "renewer-startup-rejections.json",
    "sanitizer.json",
    "secret-boundaries.json",
    "state-projections.json",
    "trust-policy-renewal-proof.svg",
}
NON_MANIFEST_FILES = EXPECTED_FILES - {"manifest.json"}
DERIVED_FILES = {
    "assertions.json",
    "manifest.json",
    "private-material-scan.json",
    "sanitizer.json",
    "trust-policy-renewal-proof.svg",
}
PROCESS_BINARIES = {
    "control-a": "control-plane",
    "control-b": "control-plane",
    "control-c": "control-plane",
    "cpu-worker": "cpu-worker",
    "gateway": "gateway",
    "renewal-fault-gate": "python3",
    "trust-distributor": "trust-distributor",
    "trust-renewer": "trust-renewer",
}
STABLE_PROCESSES = {
    "control-a",
    "control-b",
    "control-c",
    "cpu-worker",
    "gateway",
    "trust-distributor",
}
STARTUP_CASES = {
    "corrupt-template",
    "oversize-template",
    "template-symlink",
    "unsafe-template-permissions",
    "corrupt-state",
    "state-symlink",
    "unsafe-state-permissions",
    "writer-already-running",
}
STARTUP_ERROR_KINDS = {
    "corrupt-template": "template",
    "oversize-template": "template",
    "template-symlink": "template",
    "unsafe-template-permissions": "template",
    "corrupt-state": "state",
    "state-symlink": "state",
    "unsafe-state-permissions": "state",
    "writer-already-running": "writer_already_running",
}
PRODUCTION_TESTS = [
    (
        "service-auth",
        ("--lib",),
        "renewal::tests::strict_decode_rejects_unknown_duplicate_wrong_schema_and_cluster",
    ),
    (
        "service-auth",
        ("--lib",),
        "renewal::tests::fingerprint_is_json_canonical_but_array_order_is_semantic",
    ),
    (
        "service-auth",
        ("--lib",),
        "renewal::tests::authority_fingerprint_binds_template_root_id_and_public_key",
    ),
    (
        "service-auth",
        ("--lib",),
        "renewal::tests::semantic_validation_detects_every_fixed_field_drift",
    ),
    (
        "service-auth",
        ("--lib",),
        "renewal::tests::timing_bounds_deadline_and_clock_edges_are_exact",
    ),
    (
        "service-auth",
        ("--lib",),
        "renewal::tests::generation_expiry_signing_and_signature_verification_are_exact",
    ),
    (
        "trust-renewer",
        ("--lib",),
        "engine::tests::cold_start_stages_and_publishes_generation_one",
    ),
    (
        "trust-renewer",
        ("--lib",),
        "engine::tests::ambiguous_retry_reuses_exact_pending_bytes",
    ),
    (
        "trust-renewer",
        ("--lib",),
        "engine::tests::restart_reconciles_exact_pending_without_republishing",
    ),
    (
        "trust-renewer",
        ("--lib",),
        "engine::tests::same_generation_fork_fails_closed",
    ),
    (
        "trust-renewer",
        ("--lib",),
        "engine::tests::retry_crossing_expiry_records_late_recovery",
    ),
    (
        "trust-renewer",
        ("--lib",),
        "engine::tests::backward_clock_step_does_not_postpone_due_work",
    ),
    (
        "trust-renewer",
        ("--lib",),
        "engine::tests::future_issued_current_fails_closed",
    ),
    (
        "trust-renewer",
        ("--lib",),
        "engine::tests::overlong_current_lifetime_fails_closed",
    ),
    (
        "trust-renewer",
        ("--lib",),
        "engine::tests::expired_pending_fails_closed_without_post",
    ),
    (
        "trust-renewer",
        ("--lib",),
        "engine::tests::bootstrap_rejects_pending_with_invalid_signature",
    ),
    (
        "trust-renewer",
        ("--bin", "trust-renewer"),
        "tests::clean_loop_exit_is_fatal_to_supervision",
    ),
    (
        "trust-renewer",
        ("--lib",),
        "engine::tests::post_rename_directory_sync_uncertainty_retains_next_state_and_stops_without_second_mutation",
    ),
]
POLICY_SCHEMA = "inferlab.service-trust-policy.v2"
POLICY_AUTH_SCHEMA = "inferlab.service-trust-authentication.v2"
RECEIPT_SCHEMA = "inferlab.service-trust-receipt.v1"
RECEIPT_AUTH_SCHEMA = "inferlab.service-trust-receipt-authentication.v1"
ROOT_KEY_ID = "service-trust-root-v031"
TEACHING_PIECES = ["InferLab", " turns", " prompts", " into", " real", " tokens", "."]
RENEWER_STATUS_FIELDS = {
    "schema",
    "service",
    "mode",
    "phase",
    "ready",
    "transport",
    "template_fingerprint",
    "authority_fingerprint",
    "distributor_generation",
    "committed_generation",
    "pending_generation",
    "current_expires_at_ms",
    "renewal_deadline_ms",
    "remaining_margin_ms",
    "attempts",
    "successful_renewals",
    "transient_failures",
    "rejected_states",
    "late_recoveries",
    "last_error_kind",
}
SHA256 = re.compile(r"^[0-9a-f]{64}$")
LSTART = re.compile(
    r"^(?:Mon|Tue|Wed|Thu|Fri|Sat|Sun) "
    r"(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) +"
    r"(?:[1-9]|[12][0-9]|3[01]) (?:[01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9] "
    r"[0-9]{4}$"
)
TEST_SUMMARY = re.compile(
    r"^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; "
    r"[0-9]+ filtered out; finished in [0-9]+(?:\.[0-9]+)?s$"
)


# Dependency-free strict Ed25519 verification, including canonical point and
# prime-order subgroup checks. The retained checker has no package dependency.
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


def encodepoint(point: tuple[int, int]) -> bytes:
    x, y = point
    return (y | ((x & 1) << 255)).to_bytes(32, "little")


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
    if (
        encodepoint(point) != encoded
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


def deterministic_public_key(label: str) -> str:
    seed = hashlib.sha256(label.encode()).digest()
    digest = hashlib.sha512(seed).digest()
    scalar_bytes = bytearray(digest[:32])
    scalar_bytes[0] &= 248
    scalar_bytes[31] &= 63
    scalar_bytes[31] |= 64
    point = scalarmult(B, int.from_bytes(scalar_bytes, "little"))
    return base64.b64encode(encodepoint(point)).decode()


EXPECTED_ROOT_PUBLIC_KEY = deterministic_public_key("v031-service-trust-root")
EXPECTED_SERVICE_PUBLIC_KEYS = {
    service: deterministic_public_key(f"v031-service-{service}") for service in SERVICES
}


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
    return isinstance(value, int) and not isinstance(value, bool) and (
        expected is None or value == expected
    )


def finite_number(value: Any, *, positive: bool = False) -> bool:
    valid = (
        (exact_int(value) or isinstance(value, float))
        and not isinstance(value, bool)
        and math.isfinite(float(value))
    )
    return bool(valid and (float(value) > 0 if positive else float(value) >= 0))


def exact_sha256(value: Any) -> bool:
    return isinstance(value, str) and SHA256.fullmatch(value) is not None


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
    files = document.get("files")
    if (
        document.get("schema") != "inferlab.trust-policy-renewal-manifest.v0.31"
        or document.get("file_count") != len(NON_MANIFEST_FILES)
        or not isinstance(files, list)
        or [item.get("name") for item in files] != sorted(NON_MANIFEST_FILES)
    ):
        return False
    for item in files:
        if not isinstance(item, dict) or set(item) != {"name", "bytes", "sha256"}:
            return False
        path = directory / item["name"]
        try:
            raw = path.read_bytes()
        except OSError:
            return False
        if (
            not exact_int(item.get("bytes"), len(raw))
            or item.get("sha256") != hashlib.sha256(raw).hexdigest()
        ):
            return False
    return True


def expected_semantics() -> dict[str, Any]:
    return {
        "cluster_id": "inferlab-primary",
        "policy_schema": POLICY_SCHEMA,
        "trusted_credentials": [
            {
                "service_id": service,
                "credential_id": "key-a",
                "public_key_base64": EXPECTED_SERVICE_PUBLIC_KEYS[service],
            }
            for service in SERVICES
        ],
        "revoked_service_ids": [],
        "revoked_credentials": [],
        "gateway_service_ids": ["gateway-primary"],
    }


def snapshot_semantics(snapshot: dict[str, Any]) -> dict[str, Any]:
    return {
        "cluster_id": snapshot.get("cluster_id"),
        "policy_schema": snapshot.get("schema"),
        "trusted_credentials": snapshot.get("trusted_credentials"),
        "revoked_service_ids": snapshot.get("revoked_service_ids"),
        "revoked_credentials": snapshot.get("revoked_credentials"),
        "gateway_service_ids": snapshot.get("gateway_service_ids"),
    }


def exact_authority(document: Any) -> bool:
    try:
        return (
            document["schema"] == "inferlab.trust-policy-renewal-authority.v0.31"
            and document["cluster_id"] == "inferlab-primary"
            and document["template_schema"]
            == "inferlab.service-trust-renewal-template.v1"
            and document["policy_schema"] == POLICY_SCHEMA
            and document["root_key_id"] == ROOT_KEY_ID
            and document["root_public_key_base64"] == EXPECTED_ROOT_PUBLIC_KEY
            and document["service_public_keys"] == EXPECTED_SERVICE_PUBLIC_KEYS
            and document["semantic_template"] == expected_semantics()
            and exact_sha256(document["template_fingerprint"])
            and exact_sha256(document["authority_fingerprint"])
            and document["template_fingerprint"] != document["authority_fingerprint"]
        )
    except (KeyError, TypeError):
        return False


def exact_etag(snapshot: dict[str, Any]) -> str:
    return (
        f'"{snapshot["cluster_id"]}:{snapshot["generation"]}:'
        f'{snapshot["authentication"]["key_id"]}:'
        f'{snapshot["authentication"]["signature"]}"'
    )


def exact_snapshot(snapshot: Any, generation: int, lifetime_ms: int) -> bool:
    try:
        authentication = snapshot["authentication"]
        return (
            snapshot_semantics(snapshot) == expected_semantics()
            and snapshot["generation"] == generation
            and exact_int(snapshot["issued_at_ms"])
            and exact_int(snapshot["expires_at_ms"])
            and snapshot["issued_at_ms"] > 0
            and snapshot["expires_at_ms"] - snapshot["issued_at_ms"] == lifetime_ms
            and authentication["schema"] == POLICY_AUTH_SCHEMA
            and authentication["algorithm"] == "ed25519"
            and authentication["key_id"] == ROOT_KEY_ID
            and verify_ed25519(
                EXPECTED_ROOT_PUBLIC_KEY,
                canonical_snapshot(snapshot),
                authentication["signature"],
            )
        )
    except (KeyError, TypeError, OverflowError):
        return False


def controls_from_capture(capture: Any) -> list[dict[str, Any]] | None:
    try:
        values = capture["result"]["controls"]
    except (KeyError, TypeError):
        return None
    return values if isinstance(values, list) else None


def exact_controls(capture: Any, generation: int, expires_at_ms: int, validity: str) -> bool:
    controls = controls_from_capture(capture)
    if controls is None:
        return False
    try:
        leaders = [item for item in controls if item["role"] == "leader"]
        return (
            capture["schema"] == "inferlab.trust-policy-renewal-controls.v0.31"
            and exact_int(capture["samples"])
            and capture["samples"] >= 1
            and len(controls) == 3
            and {item["node_id"] for item in controls} == set(CONTROLS)
            and len(leaders) == 1
            and all(item["leader_id"] == leaders[0]["node_id"] for item in controls)
            and len({item["term"] for item in controls}) == 1
            and all(item["storage_healthy"] is True for item in controls)
            and all(item["committed_configuration"]["revision"] == 2 for item in controls)
            and all(
                item["service_authentication"]["trust_policy_generation"] == generation
                and item["service_authentication"]["trust_policy_schema"] is None
                and item["service_authentication"]["trust_policy_root_key_id"] is None
                and item["service_authentication"]["trust_policy_expires_at_ms"]
                == expires_at_ms
                and item["service_authentication"]["trust_policy_validity"] == validity
                and item["service_authentication"]["trust_policy_transport_mode"]
                == "mutual-tls"
                and item["service_authentication"]["trust_policy_server_authentication"]
                is True
                and item["service_authentication"]["trust_policy_client_authentication"]
                is True
                for item in controls
            )
        )
    except (KeyError, TypeError):
        return False


def exact_receipts(capture: Any, snapshot: dict[str, Any]) -> bool:
    try:
        observation = capture["result"]
        body = observation["body"]
        status = body["snapshot"]
        receipts = body["receipts"]
        generation = snapshot["generation"]
        if (
            capture["schema"] != "inferlab.trust-policy-renewal-distributor.v0.31"
            or capture["samples"] < 1
            or observation["status"] != 200
            or body["cluster_id"] != "inferlab-primary"
            or body["expected_receiver_mode"] != "service-id"
            or body["expected_receivers"] != EXPECTED_RECEIVERS
            or body["acked_receivers"] != EXPECTED_RECEIVERS
            or body["pending_receivers"] != []
            or body["receipt_count"] != 3
            or status["policy_schema"] != POLICY_SCHEMA
            or status["generation"] != generation
            or status["issued_at_ms"] != snapshot["issued_at_ms"]
            or status["expires_at_ms"] != snapshot["expires_at_ms"]
            or status["root_key_id"] != ROOT_KEY_ID
            or status["etag"] != exact_etag(snapshot)
            or body["storage"]
            != {"mutation_poisoned": False, "error_code": None}
            or len(receipts) != 3
        ):
            return False
        seen = set()
        signatures = set()
        for receipt in receipts:
            authentication = receipt["authentication"]
            receiver = receipt["receiver_service_id"]
            if (
                receipt["schema"] != RECEIPT_SCHEMA
                or receipt["cluster_id"] != "inferlab-primary"
                or receipt["generation"] != generation
                or receipt["root_key_id"] != ROOT_KEY_ID
                or receipt["snapshot_signature"]
                != snapshot["authentication"]["signature"]
                or receiver not in CONTROLS
                or receipt["receiver_credential_id"] != "key-a"
                or authentication["schema"] != RECEIPT_AUTH_SCHEMA
                or authentication["algorithm"] != "ed25519"
                or not verify_ed25519(
                    EXPECTED_SERVICE_PUBLIC_KEYS[receiver],
                    canonical_receipt(receipt),
                    authentication["signature"],
                )
            ):
                return False
            seen.add(receiver)
            signatures.add(authentication["signature"])
        return seen == set(CONTROLS) and len(signatures) == 3
    except (KeyError, TypeError, OverflowError):
        return False


def renewer_body(capture: Any) -> dict[str, Any]:
    return capture["result"]["body"]


def exact_renewer_status(
    capture: Any,
    *,
    generation: int,
    pending: int | None,
    authority: dict[str, Any],
    clean: bool,
) -> bool:
    try:
        body = renewer_body(capture)
        return (
            capture["schema"] == "inferlab.trust-policy-renewal-renewer-status.v0.31"
            and capture["samples"] >= 1
            and capture["result"]["status"] == 200
            and set(body) == RENEWER_STATUS_FIELDS
            and body["schema"] == "inferlab.trust-renewer-status.v1"
            and body["service"] == "inferlab-trust-renewer"
            and body["mode"] == "automatic-renewal"
            and body["phase"] == "waiting"
            and body["ready"] is True
            and body["transport"] == "mutual-tls"
            and body["template_fingerprint"] == authority["template_fingerprint"]
            and body["authority_fingerprint"] == authority["authority_fingerprint"]
            and body["distributor_generation"] == generation
            and body["committed_generation"] == generation
            and body["pending_generation"] == pending
            and exact_int(body["current_expires_at_ms"])
            and exact_int(body["renewal_deadline_ms"])
            and body["renewal_deadline_ms"] < body["current_expires_at_ms"]
            and exact_int(body["remaining_margin_ms"])
            and all(
                exact_int(body[key]) and body[key] >= 0
                for key in (
                    "attempts",
                    "successful_renewals",
                    "transient_failures",
                    "rejected_states",
                    "late_recoveries",
                )
            )
            and (not clean or body["last_error_kind"] is None)
        )
    except (KeyError, TypeError):
        return False


def exact_generations(document: Any, authority: Any, lifetime_ms: int) -> tuple[bool, list[dict[str, Any]]]:
    try:
        values = document["generations"]
        if (
            document["schema"] != "inferlab.trust-policy-renewal-generations.v0.31"
            or document["generation_count"] != 4
            or len(values) != 4
            or [item["label"] for item in values] != GENERATION_LABELS
            or [item["generation"] for item in values] != [1, 2, 3, 4]
        ):
            return False, []
        snapshots = [item["snapshot"] for item in values]
        signatures = set()
        for generation, item, snapshot in zip(range(1, 5), values, snapshots):
            snapshot_observation = item["snapshot_capture"]["observation"]
            if (
                not exact_snapshot(snapshot, generation, lifetime_ms)
                or item["snapshot_capture"]["schema"]
                != "inferlab.trust-policy-renewal-capture.v0.31"
                or snapshot_observation["method"] != "GET"
                or snapshot_observation["path"] != "/v1/service-trust/snapshot"
                or snapshot_observation["status"] != 200
                or snapshot_observation["body"] != snapshot
                or not exact_sha256(item["snapshot_sha256"])
                or snapshot_observation["body_sha256"] != item["snapshot_sha256"]
                or item["etag"] != exact_etag(snapshot)
                or snapshot_observation["etag"] != item["etag"]
                or not exact_controls(
                    item["controls"], generation, snapshot["expires_at_ms"], "valid"
                )
                or not exact_receipts(item["distributor"], snapshot)
                or not exact_renewer_status(
                    item["renewer"],
                    generation=generation,
                    pending=None,
                    authority=authority,
                    clean=True,
                )
                or renewer_body(item["renewer"])["current_expires_at_ms"]
                != snapshot["expires_at_ms"]
            ):
                return False, []
            signatures.add(snapshot["authentication"]["signature"])
        return (
            len(signatures) == 4
            and all(
                snapshots[index]["issued_at_ms"] < snapshots[index]["expires_at_ms"]
                and snapshots[index]["generation"] + 1
                == snapshots[index + 1]["generation"]
                for index in range(3)
            ),
            values,
        )
    except (KeyError, TypeError):
        return False, []


def exact_normal(document: Any, generations: list[dict[str, Any]]) -> bool:
    if len(generations) != 4:
        return False
    try:
        one = generations[0]["snapshot"]
        two = generations[1]["snapshot"]
        receipts = generations[1]["distributor"]["result"]["body"]["receipts"]
        before = document["expiration_rejections_before"]
        after = document["expiration_rejections_after"]
        return (
            document["schema"] == "inferlab.trust-policy-renewal-normal.v0.31"
            and document["from_generation"] == 1
            and document["to_generation"] == 2
            and document["from_snapshot_sha256"] == generations[0]["snapshot_sha256"]
            and document["to_snapshot_sha256"] == generations[1]["snapshot_sha256"]
            and two["issued_at_ms"] < one["expires_at_ms"]
            and all(receipt["applied_at_ms"] < one["expires_at_ms"] for receipt in receipts)
            and before == after
            and set(before) == set(CONTROLS)
            and all(exact_int(value) and value >= 0 for value in before.values())
            and document["authorization_gap_observed"] is False
        )
    except (KeyError, TypeError):
        return False


def exact_state_projection(value: Any, generation: int, pending: int | None) -> bool:
    try:
        return (
            value["schema"] == "inferlab.trust-policy-renewal-state-projection.v0.31"
            and value["state_schema"] == "inferlab.trust-renewer-state.v1"
            and exact_sha256(value["state_file_sha256"])
            and exact_sha256(value["authority_fingerprint"])
            and exact_sha256(value["template_fingerprint"])
            and value["committed_generation"] == generation
            and value["pending_generation"] == pending
            and (
                (pending is None and value["pending_snapshot_sha256"] is None)
                or (pending is not None and exact_sha256(value["pending_snapshot_sha256"]))
            )
            and value["file_mode"] == "0600"
            and value["regular_file"] is True
            and value["symlink"] is False
        )
    except (KeyError, TypeError):
        return False


def exact_ambiguous(document: Any, states: Any, generations: list[dict[str, Any]]) -> bool:
    if len(generations) != 4:
        return False
    try:
        g3 = generations[2]["snapshot"]
        pending = states["ambiguous_pending"]
        restart_pending = states["ambiguous_restart_pending"]
        committed = states["ambiguous_committed"]
        drop = document["drop_event"]
        return (
            document["schema"] == "inferlab.trust-policy-renewal-ambiguous-retry.v0.31"
            and document["target_generation"] == 3
            and drop["schema"] == "inferlab.trust-policy-renewal-fault-gate-drop.v0.31"
            and drop["method"] == "POST"
            and drop["path"] == "/v1/service-trust/snapshot"
            and drop["upstream_status"] in {200, 201}
            and drop["upstream_body"]["generation"] == 3
            and drop["response_forwarded"] is False
            and exact_state_projection(pending, 2, 3)
            and pending["pending_snapshot_sha256"] == generations[2]["snapshot_sha256"]
            and exact_state_projection(restart_pending, 2, 3)
            and restart_pending["pending_snapshot_sha256"]
            == pending["pending_snapshot_sha256"]
            and exact_state_projection(committed, 3, None)
            and document["same_pending_snapshot_sha256_before_after_restart"] is True
            and document["reconciled_exact_distributor_bytes"] is True
            and document["duplicate_generation_observed"] is False
            and document["fork_observed"] is False
            and document["generation_skipped"] is False
        )
    except (KeyError, TypeError):
        return False


def exact_expiry_recovery(document: Any, states: Any, generations: list[dict[str, Any]]) -> bool:
    if len(generations) != 4:
        return False
    try:
        g3 = generations[2]["snapshot"]
        g4 = generations[3]["snapshot"]
        expired = document["expired_controls"]
        pending = states["outage_pending"]
        recovered = states["final_committed"]
        return (
            document["schema"] == "inferlab.trust-policy-renewal-expiry-recovery.v0.31"
            and document["expired_generation"] == 3
            and document["recovery_generation"] == 4
            and document["outage_started_at_ms"] < g3["expires_at_ms"]
            and document["outage_released_at_ms"] >= g3["expires_at_ms"]
            and exact_controls(expired, 3, g3["expires_at_ms"], "expired")
            and exact_state_projection(pending, 3, 4)
            and pending["pending_snapshot_sha256"] == generations[3]["snapshot_sha256"]
            and pending["pending_late_recovery"] is True
            and exact_state_projection(recovered, 4, None)
            and g4["issued_at_ms"] >= g3["expires_at_ms"]
            and document["hidden_grace_observed"] is False
            and document["late_recovery_count_delta"] >= 1
        )
    except (KeyError, TypeError):
        return False


def exact_protected_requests(document: Any, generations: list[dict[str, Any]]) -> bool:
    if len(generations) != 4:
        return False
    try:
        expiry = generations[2]["snapshot"]["expires_at_ms"]
        before = document["before_expiry"]
        signed = document["after_expiry_signed"]
        signed_repeat = document["after_expiry_signed_repeat"]
        missing = document["after_expiry_missing"]
        exact_error = {
            "error": {
                "code": "unauthorized",
                "leader_id": None,
                "message": "signed service-trust policy is expired",
            }
        }
        return (
            document["schema"] == "inferlab.trust-policy-renewal-protected-requests.v0.31"
            and document["generation"] == 3
            and document["expires_at_ms"] == expiry
            and before["status"] == 200
            and before["body"] == {"cluster_id": "inferlab-primary", "revision": 2}
            and 0 < expiry - before["started_at_ms"] <= 1_000
            and signed["status"] == 401
            and signed_repeat["status"] == 401
            and missing["status"] == 401
            and signed["body"] == signed_repeat["body"] == missing["body"] == exact_error
            and signed["started_at_ms"] >= expiry
            and signed_repeat["started_at_ms"] >= signed["observed_at_ms"]
            and missing["started_at_ms"] >= signed_repeat["observed_at_ms"]
            and exact_int(document["expiration_rejections_before"])
            and exact_int(document["expiration_rejections_after"])
            and document["expiration_rejection_delta"]
            == document["expiration_rejections_after"]
            - document["expiration_rejections_before"]
            and document["expiration_rejection_delta"] >= 2
        )
    except (KeyError, TypeError):
        return False


def process_identity(items: list[dict[str, Any]], label: str) -> tuple[Any, ...]:
    item = next(value for value in items if value["label"] == label)
    return (
        item["pid"],
        item["ppid"],
        item["start_token"],
        item["executable"],
        item["executable_sha256"],
    )


def exact_process_items(items: Any, proof_pid: int) -> bool:
    try:
        return (
            isinstance(items, list)
            and len(items) == len(PROCESS_BINARIES)
            and {item["label"] for item in items} == set(PROCESS_BINARIES)
            and all(
                exact_int(item["pid"])
                and item["pid"] > 0
                and item["ppid"] == proof_pid
                and isinstance(item["state"], str)
                and "Z" not in item["state"]
                and LSTART.fullmatch(item["start_token"]) is not None
                and (
                    item["executable"] == PROCESS_BINARIES[item["label"]]
                    or (
                        item["label"] == "renewal-fault-gate"
                        and re.fullmatch(r"(?:python3(?:\.[0-9]+)?|Python)", item["executable"])
                        is not None
                    )
                )
                and exact_sha256(item["executable_sha256"])
                for item in items
            )
        )
    except (KeyError, TypeError, StopIteration):
        return False


def exact_process_continuity(document: Any) -> bool:
    try:
        proof_pid = document["proof_shell_pid"]
        initial = document["initial"]
        restarted = document["after_renewer_restart"]
        final = document["final"]
        if not all(exact_process_items(items, proof_pid) for items in (initial, restarted, final)):
            return False
        stable = STABLE_PROCESSES | {"renewal-fault-gate"}
        return (
            document["schema"] == "inferlab.trust-policy-renewal-process-continuity.v0.31"
            and document["stable_runtime_processes"] == sorted(STABLE_PROCESSES)
            and document["replaced_runtime_processes"] == ["trust-renewer"]
            and document["proof_only_processes"] == ["renewal-fault-gate"]
            and all(
                process_identity(initial, label)
                == process_identity(restarted, label)
                == process_identity(final, label)
                for label in stable
            )
            and process_identity(initial, "trust-renewer")
            != process_identity(restarted, "trust-renewer")
            and process_identity(restarted, "trust-renewer")
            == process_identity(final, "trust-renewer")
        )
    except (KeyError, TypeError, StopIteration):
        return False


def exact_fault_gate(document: Any) -> bool:
    try:
        ready = document["ready"]
        drop = document["drop"]
        outage = document["outage"]
        return (
            document["schema"] == "inferlab.trust-policy-renewal-fault-gate.v0.31"
            and document["process_role"] == "proof-only-application-fault-gate"
            and document["runtime_process"] is False
            and document["ha_component"] is False
            and document["authority_component"] is False
            and ready["schema"] == "inferlab.trust-policy-renewal-fault-gate-ready.v0.31"
            and ready["tls_protocol"] == "TLSv1.3"
            and exact_int(ready["pid"])
            and drop["response_forwarded"] is False
            and outage["mode"] == "unavailable"
            and outage["request_forwarded"] is False
        )
    except (KeyError, TypeError):
        return False


def exact_startup_rejections(document: Any) -> bool:
    try:
        cases = document["cases"]
        return (
            document["schema"] == "inferlab.trust-policy-renewal-startup-rejections.v0.31"
            and len(cases) == len(STARTUP_CASES)
            and {item["scenario"] for item in cases} == STARTUP_CASES
            and all(
                set(item)
                == {
                    "scenario",
                    "error_kind",
                    "diagnostic",
                    "exit_code",
                    "listener_ever_open",
                    "listener_probe_count",
                }
                and item["error_kind"] == STARTUP_ERROR_KINDS[item["scenario"]]
                and item["diagnostic"] == item["error_kind"]
                and exact_int(item["exit_code"])
                and item["exit_code"] != 0
                and item["listener_ever_open"] is False
                and exact_int(item["listener_probe_count"])
                and item["listener_probe_count"] >= 1
                and isinstance(item["error_kind"], str)
                and item["error_kind"]
                and isinstance(item["diagnostic"], str)
                and 0 < len(item["diagnostic"]) <= 512
                for item in cases
            )
        )
    except (KeyError, TypeError):
        return False


def exact_production_tests(document: Any) -> bool:
    try:
        tests = document["tests"]
        expected = set(PRODUCTION_TESTS)
        if (
            document["schema"] != "inferlab.trust-policy-renewal-production-tests.v0.31"
            or document["test_count"] != len(tests)
            or len(tests) != len(PRODUCTION_TESTS)
        ):
            return False
        identities = set()
        for item in tests:
            identity = (item["package"], tuple(item["target"]), item["test_filter"])
            if identity in identities:
                return False
            identities.add(identity)
            lines = item["output_lines"]
            if (
                set(item)
                != {"package", "target", "test_filter", "exit_code", "output_lines"}
                or item["package"] not in {"service-auth", "trust-renewer", "trust-distributor"}
                or item["target"] not in (["--lib"], ["--bin", "trust-renewer"])
                or not isinstance(item["test_filter"], str)
                or "::" not in item["test_filter"]
                or item["exit_code"] != 0
                or lines[0:2] != ["running 1 test", f'test {item["test_filter"]} ... ok']
                or len(lines) != 3
                or TEST_SUMMARY.fullmatch(lines[2]) is None
            ):
                return False
        return identities == expected
    except (KeyError, TypeError, IndexError):
        return False


def exact_secret_boundaries(document: Any) -> bool:
    try:
        processes = document["processes"]
        return (
            document["schema"] == "inferlab.trust-policy-renewal-secret-boundaries.v0.31"
            and set(processes) == set(PROCESS_BINARIES)
            and processes["trust-renewer"]["root_private_key_env_present"] is True
            and all(
                item["root_private_key_env_present"] is False
                for name, item in processes.items()
                if name != "trust-renewer"
            )
            and processes["trust-distributor"]["public_root_env_present"] is True
            and all(item["values_retained"] is False for item in processes.values())
            and document["root_seed_values_retained"] is False
        )
    except (KeyError, TypeError):
        return False


def exact_final_cluster(document: Any) -> bool:
    try:
        return exact_controls(document, 4, document["expected_expires_at_ms"], "valid")
    except (KeyError, TypeError):
        return False


def exact_final_json(document: Any) -> bool:
    try:
        observation = document["observation"]
        body = observation["body"]
        inferlab = body["inferlab"]
        return (
            document["schema"] == "inferlab.trust-policy-renewal-json.v0.31"
            and observation["status"] == 200
            and observation["method"] == "POST"
            and observation["path"] == "/v1/chat/completions"
            and finite_number(observation["duration_ms"], positive=True)
            and body["object"] == "chat.completion"
            and body["model"] == "inferlab-tiny"
            and body["choices"][0]["message"]["content"] == "".join(TEACHING_PIECES)
            and body["choices"][0]["finish_reason"] == "stop"
            and observation["worker"] == "cpu-trust-renewal"
            and observation["attempts"] == 1
            and observation["config_revision"] == 2
        )
    except (KeyError, TypeError, IndexError):
        return False


def exact_final_sse(document: Any) -> bool:
    try:
        offsets = document["offsets_ms"]
        return (
            document["schema"] == "inferlab.trust-policy-renewal-sse.v0.31"
            and document["status"] == 200
            and document["method"] == "POST"
            and document["path"] == "/v1/chat/completions"
            and finite_number(document["duration_ms"], positive=True)
            and document["event_count"] == 10
            and document["content_event_count"] == 7
            and document["pieces"] == TEACHING_PIECES
            and document["content"] == "".join(TEACHING_PIECES)
            and document["finish_reason"] == "stop"
            and document["done_received"] is True
            and document["eof_after_done"] is True
            and len(offsets) == 10
            and all(finite_number(value) for value in offsets)
            and offsets == sorted(offsets)
        )
    except (KeyError, TypeError):
        return False


def exact_proof_contract(document: Any) -> bool:
    return document == {
        "cluster_id": "inferlab-primary",
        "automatic_generations": [1, 2, 3, 4],
        "normal_renewal": [1, 2],
        "ambiguous_response_generation": 3,
        "expiry_recovery": [3, 4],
        "policy_lifetime_ms": 20_000,
        "renew_before_ms": 10_000,
        "poll_interval_ms": 50,
        "retry_interval_ms": 200,
        "request_timeout_ms": 500,
        "runtime_processes": sorted(STABLE_PROCESSES | {"trust-renewer"}),
        "proof_only_processes": ["renewal-fault-gate"],
        "replaced_processes": ["trust-renewer"],
        "static_tls_identity": True,
        "scope": "deadline-safe-automated-signed-service-trust-renewal",
        "schema": "inferlab.trust-policy-renewal-proof-contract.v0.31",
    }


def run(directory: Path, require_manifest: bool) -> dict[str, Any]:
    if not exact_inventory(directory, require_manifest):
        raise EvidenceError("inventory")
    if require_manifest and not exact_manifest(directory, load(directory, "manifest.json")):
        raise EvidenceError("manifest")
    names = NON_MANIFEST_FILES - {"assertions.json", "trust-policy-renewal-proof.svg"}
    documents = {name: load(directory, name) for name in names if name.endswith(".json")}
    authority = documents["authority.json"]
    contract = documents["proof-contract.json"]
    generations_ok, generations = exact_generations(
        documents["automatic-generations.json"], authority, contract.get("policy_lifetime_ms", -1)
    )
    states = documents["state-projections.json"]
    checks = [
        assertion(
            "the retained inventory is exact and contains no links",
            True,
            {"non_manifest_file_count": len(NON_MANIFEST_FILES)},
        ),
        assertion(
            "the proof contract isolates automated policy renewal and accounts for every process",
            exact_proof_contract(contract),
        ),
        assertion(
            "the fixed renewal authority and semantic template are exactly pinned",
            exact_authority(authority),
        ),
        assertion(
            "unsafe, corrupt, linked and concurrently locked startup sources fail before listening",
            exact_startup_rejections(documents["renewer-startup-rejections.json"]),
            {"case_count": len(documents["renewer-startup-rejections.json"].get("cases", []))},
        ),
        assertion(
            "four automatic generations preserve exact meaning and carry valid root signatures and receipts",
            generations_ok,
            {"generation_count": len(generations)},
        ),
        assertion(
            "normal generation one to two renewal converges before expiry without an authorization gap",
            exact_normal(documents["normal-renewal.json"], generations),
        ),
        assertion(
            "a lost committed POST response and renewer restart reconcile exact pending generation three bytes",
            exact_ambiguous(documents["ambiguous-retry.json"], states, generations),
        ),
        assertion(
            "renewer-only transport outage crosses generation-three expiry without grace and recovers at four",
            exact_expiry_recovery(documents["expiry-outage-recovery.json"], states, generations),
        ),
        assertion(
            "protected requests accept before and fail uniformly after the exclusive outage deadline",
            exact_protected_requests(documents["protected-request-continuity.json"], generations),
        ),
        assertion(
            "the fault gate is explicit proof-only TLS instrumentation, not runtime authority or HA",
            exact_fault_gate(documents["fault-gate.json"]),
        ),
        assertion(
            "six runtime services and the proof gate remain exact while only the renewer is replaced",
            exact_process_continuity(documents["process-continuity.json"]),
            {"stable_runtime_count": 6, "expected_replacements": 1, "proof_only_count": 1},
        ),
        assertion(
            "only the renewer process receives the private root environment variable",
            exact_secret_boundaries(documents["secret-boundaries.json"]),
        ),
        assertion(
            "exact production regressions cover semantic, timing, persistence, ambiguity, clock and supervision edges",
            exact_production_tests(documents["production-tests.json"]),
            {"test_count": documents["production-tests.json"].get("test_count")},
        ),
        assertion(
            "the final Raft cluster remains converged on policy generation four",
            exact_final_cluster(documents["final-cluster.json"]),
        ),
        assertion(
            "real deterministic CPU JSON succeeds after late renewal recovery",
            exact_final_json(documents["final-json.json"]),
        ),
        assertion(
            "real incremental CPU SSE reaches DONE and EOF after late renewal recovery",
            exact_final_sse(documents["final-sse.json"]),
        ),
        assertion(
            "discarded logs contain no deterministic secret or private prompt",
            documents["discarded-log-scan.json"].get("passed") is True
            and documents["discarded-log-scan.json"].get("matches") == [],
        ),
        assertion(
            "retained JSON and SVG inputs contain no host path, private marker or sensitive field",
            documents["sanitizer.json"].get("problem_count") == 0
            and documents["sanitizer.json"].get("problems") == [],
        ),
        assertion(
            "all deterministic root, route, writer and service seed representations are absent",
            documents["private-material-scan.json"].get("passed") is True
            and documents["private-material-scan.json"].get("matches") == [],
        ),
    ]
    passed = sum(item["passed"] for item in checks)
    return {
        "schema": "inferlab.trust-policy-renewal-assertions.v0.31",
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
        print(f"invalid v0.31 evidence: {error}", file=sys.stderr)
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
