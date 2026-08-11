#!/usr/bin/env python3
"""Adversarial offline checker for the v0.29 restart-free signer handoff proof."""

from __future__ import annotations

import argparse
import base64
import copy
import hashlib
import json
import math
import re
import sys
import urllib.parse
from pathlib import Path
from typing import Any


SERVICES = ["control-a", "control-b", "control-c", "gateway-primary"]
CONTROLS = SERVICES[:3]
EXPECTED_ROOT_PUBLIC_KEY = "FH9+6CsZmDVXQb/jupt7zeM5b7qqYAvty0bQB0ww7F4="
EXPECTED_SERVICE_PUBLIC_KEYS = {
    ("control-a", "key-a"): "3EBA9ialtLUO6ubt4q7+okEr9JCx5x6WcXmTPcgbiPs=",
    ("control-a", "key-b"): "ZzJDiiufQZV1OZjjo4vLV/Syd0DSw3MWKSVei9eBiFY=",
    ("control-b", "key-a"): "KIWCCBKvoMM8CUyUk6NOsroF5HxgIzZI/bzLzJznw50=",
    ("control-b", "key-b"): "Fo1ep++emytjJmAykNN5ch3Il6vhYB5u6NxBw1SCQbU=",
    ("control-c", "key-a"): "yWRUfRF/JG02Y+RMvp1LTgLfNQdFVWRFQPPB+x4ybGY=",
    ("control-c", "key-b"): "/jS2tGmp0ErPA3REw+A0YPI7DsrvRWSW00Q3CmKNo9Q=",
    ("gateway-primary", "key-a"): "TUfSqEhbC2wePVqqKGwurSBjpmxBBx0tdQlCe0Lx8L8=",
    ("gateway-primary", "key-b"): "fb3hz/OMchMweZDvuDjrk9RXjNfhC28dnSjvoddndsI=",
}
EXPECTED_FILES = {
    "assertions.json",
    "discarded-log-scan.json",
    "final-cluster.json",
    "final-gateway.json",
    "final-json.json",
    "final-sse.json",
    "generation-1-after-handoff.json",
    "generation-1-controls.json",
    "generation-1-receipts.json",
    "generation-2-controls.json",
    "generation-2-receipts.json",
    "gateway-r2.json",
    "handoff-sequence.json",
    "live-source-rejections.json",
    "manifest.json",
    "private-material-scan.json",
    "process-continuity.json",
    "production-tests.json",
    "proof-contract.json",
    "publish-g1.json",
    "publish-g2.json",
    "r2-write.json",
    "r3-write.json",
    "revoked-a-attacks.json",
    "sanitizer.json",
    "signer-handoff-proof.svg",
    "startup-rejections.json",
    "trust-generations.json",
}
NON_MANIFEST_FILES = EXPECTED_FILES - {"manifest.json"}
PROCESS_BINARIES = {
    "control-a": "control-plane",
    "control-b": "control-plane",
    "control-c": "control-plane",
    "gateway": "gateway",
    "cpu-worker": "cpu-worker",
    "trust-distributor": "trust-distributor",
}
STARTUP_CASES = {
    "missing": "source_unavailable",
    "malformed": "invalid_json",
    "oversize": "bundle_too_large",
    "unsafe-permissions": "unsafe_permissions",
    "non-regular": "not_regular_file",
    "symlink": "not_regular_file",
    "wrong-cluster": "cluster_mismatch",
    "wrong-service": "service_mismatch",
    "unknown-active": "unknown_active_credential",
}
ERROR_MESSAGES = {
    "source_unavailable": "service signing bundle metadata is unavailable",
    "invalid_json": "service signing bundle is not exact valid JSON",
    "bundle_too_large": "service signing bundle exceeds the byte limit",
    "unsafe_permissions": "service signing bundle permissions must be exactly 0600",
    "not_regular_file": "service signing bundle must be a regular file and not a symbolic link",
    "cluster_mismatch": "service signing bundle cluster ID does not match this process",
    "service_mismatch": "service signing bundle service ID does not match this process",
    "unknown_active_credential": "service signing bundle active credential is not configured",
    "stale_generation": "service signing bundle generation is older than the active generation",
    "generation_fork": "service signing bundle reuses the active generation with different contents",
    "candidate_rejected": "service signing bundle candidate was rejected by local policy",
}
LIVE_CASES = {
    **STARTUP_CASES,
    "stale": "stale_generation",
    "fork": "generation_fork",
}
PRODUCTION_TESTS = {
    "signing_bundle::tests::same_millisecond_concurrent_handoff_never_reuses_nonce":
        ("service-auth", ["--lib"]),
    "signing_bundle::tests::rollback_fork_and_policy_rejection_retain_last_known_good":
        ("service-auth", ["--lib"]),
    "signing_bundle::tests::load_requires_exact_0600_regular_file":
        ("service-auth", ["--lib"]),
    "signing_bundle::tests::watched_receipts_are_cluster_bound_while_static_receipts_remain_compatible":
        ("service-auth", ["--lib"]),
    "raft::tests::raft_requests_use_the_current_bundle_signer_without_reopening_the_node":
        ("control-plane", ["--lib"]),
    "service_trust::tests::remote_policy_receipts_follow_the_current_signer_without_false_handoff_receipts":
        ("control-plane", ["--lib"]),
    "tests::watched_service_signer_activates_only_the_exact_policy_key":
        ("control-plane", ["--bin", "control-plane"]),
    "tests::supervisor_fails_when_the_service_signing_watcher_completes_or_panics":
        ("control-plane", ["--bin", "control-plane"]),
    "service_client::tests::in_flight_request_keeps_its_snapshot_and_the_next_request_uses_the_handoff":
        ("gateway", ["--lib"]),
    "tests::signing_watch_loop_retries_transient_source_race_but_dedupes_deterministic_input":
        ("gateway", ["--bin", "gateway"]),
    "service_receiver_mode_survives_credential_handoff_and_revocation":
        ("trust-distributor", ["--test", "distributor"]),
}
PRODUCTION_FILTERS = set(PRODUCTION_TESTS)
HOST_PATH = re.compile(r"(?:/Users/|/home/|/tmp/|/private/var/|/var/folders/|/workspace/|/github/workspace)")
PRIVATE_MARKERS = (
    "-----BEGIN PRIVATE KEY-----",
    "-----BEGIN RSA PRIVATE KEY-----",
    "-----BEGIN EC PRIVATE KEY-----",
    "PRIVATE_KEY_B64",
    "PRIVATE_KEY_BASE64",
)
SEED_LABELS = [
    "v029-service-trust-root",
    "v029-route-signing",
    "v029-control-writer",
    *(f"v029-{service}-{credential}" for service in SERVICES for credential in ("key-a", "key-b")),
]
FORBIDDEN_TEXT = (
    "v029-final-json-private-proof-prompt",
    "v029-final-sse-private-proof-prompt",
    "v029-proof-request-id",
)
SENSITIVE_JSON_FIELDS = {
    "private_key",
    "private_key_base64",
    "private_key_b64",
    "seed",
    "api_key",
    "authorization",
    "nonce",
    "request_id",
    "snapshot_path",
    "bundle_path",
    "base_url",
}
BASE_EVIDENCE_FILES = NON_MANIFEST_FILES - {
    "assertions.json",
    "private-material-scan.json",
    "sanitizer.json",
    "signer-handoff-proof.svg",
}


# Minimal strict Ed25519 verifier. Keeping this checker dependency-free makes the
# retained receipt and root-signature claims reproducible on stock Python.
Q = 2**255 - 19
L = 2**252 + 27742317777372353535851937790883648493
D = (-121665 * pow(121666, Q - 2, Q)) % Q
I = pow(2, (Q - 1) // 4, Q)


def xrecover(y: int) -> int:
    xx = (y * y - 1) * pow(D * y * y + 1, Q - 2, Q) % Q
    x = pow(xx, (Q + 3) // 8, Q)
    if (x * x - xx) % Q != 0:
        x = x * I % Q
    if x & 1:
        x = Q - x
    return x


BY = 4 * pow(5, Q - 2, Q) % Q
BX = xrecover(BY)
B = (BX, BY)


def edwards(point: tuple[int, int], other: tuple[int, int]) -> tuple[int, int]:
    x1, y1 = point
    x2, y2 = other
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
    if x >= Q:
        return None
    point = (x, y)
    canonical = (y | ((x & 1) << 255)).to_bytes(32, "little")
    if (
        canonical != encoded
        or (-x * x + y * y - 1 - D * x * x * y * y) % Q != 0
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
    if (
        public_point is None
        or r_point is None
        or public_point == (0, 1)
        or r_point == (0, 1)
        or scalar >= L
    ):
        return False
    challenge = int.from_bytes(
        hashlib.sha512(signature_bytes[:32] + public_bytes + message).digest(), "little"
    ) % L
    return scalarmult(B, scalar) == edwards(r_point, scalarmult(public_point, challenge))


def valid_ed25519_public(value: Any) -> bool:
    if not isinstance(value, str):
        return False
    try:
        decoded = base64.b64decode(value, validate=True)
    except (ValueError, base64.binascii.Error):
        return False
    point = decodepoint(decoded)
    return point is not None and point != (0, 1)


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
    credentials = snapshot["trusted_credentials"]
    append_count(output, len(credentials))
    for item in credentials:
        append_string(output, item["service_id"])
        append_string(output, item["credential_id"])
        append_string(output, item["public_key_base64"])
    revoked_services = snapshot["revoked_service_ids"]
    append_count(output, len(revoked_services))
    for item in revoked_services:
        append_string(output, item)
    revoked_credentials = snapshot["revoked_credentials"]
    append_count(output, len(revoked_credentials))
    for item in revoked_credentials:
        append_string(output, item["service_id"])
        append_string(output, item["credential_id"])
    gateways = snapshot["gateway_service_ids"]
    append_count(output, len(gateways))
    for item in gateways:
        append_string(output, item)
    append_string(output, snapshot["authentication"]["schema"])
    append_string(output, snapshot["authentication"]["algorithm"])
    append_string(output, snapshot["authentication"]["key_id"])
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


def exact_int(value: Any, expected: int | None = None) -> bool:
    if not isinstance(value, int) or isinstance(value, bool):
        return False
    return expected is None or value == expected


def finite_number(value: Any) -> bool:
    return (
        (exact_int(value) or isinstance(value, float))
        and not isinstance(value, bool)
        and math.isfinite(value)
        and value >= 0
    )


def exact_keys(value: Any, keys: set[str]) -> bool:
    return isinstance(value, dict) and set(value) == keys


class EvidenceError(Exception):
    """Finite, user-safe rejection for an unreadable evidence bundle."""


def reject_duplicate_json_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON key")
        result[key] = value
    return result


def require_exact_inventory(directory: Path, require_manifest: bool) -> None:
    try:
        entries = list(directory.iterdir())
    except OSError:
        raise EvidenceError("inventory") from None
    manifest_present = (directory / "manifest.json").is_file()
    expected = EXPECTED_FILES if require_manifest or manifest_present else NON_MANIFEST_FILES
    if (
        {path.name for path in entries} != expected
        or any(not path.is_file() or path.is_symlink() for path in entries)
    ):
        raise EvidenceError("inventory")


def load(directory: Path, name: str) -> Any:
    try:
        with (directory / name).open(encoding="utf-8") as source:
            return json.load(source, object_pairs_hook=reject_duplicate_json_keys)
    except (OSError, UnicodeError, ValueError, RecursionError):
        if name not in EXPECTED_FILES:
            raise EvidenceError("file") from None
        raise EvidenceError(name) from None


def assertion(name: str, passed: bool, observations: dict[str, Any] | None = None) -> dict[str, Any]:
    return {
        "name": name,
        "passed": bool(passed),
        "observations": observations or {},
    }


def safe_text(text: str) -> bool:
    if HOST_PATH.search(text) or any(marker in text for marker in PRIVATE_MARKERS):
        return False
    values = list(FORBIDDEN_TEXT)
    for label in SEED_LABELS:
        digest = hashlib.sha256(label.encode()).digest()
        padded = base64.b64encode(digest).decode()
        values.extend((
            padded,
            padded.rstrip("="),
            urllib.parse.quote(padded, safe=""),
            hashlib.sha256(padded.encode()).hexdigest(),
        ))
    return not any(value and value in text for value in values)


def sensitive_paths(value: Any, path: str = "$") -> list[str]:
    found: list[str] = []
    if isinstance(value, dict):
        for key, child in value.items():
            child_path = f"{path}.{key}"
            if key.lower().replace("-", "_") in SENSITIVE_JSON_FIELDS:
                found.append(child_path)
            found.extend(sensitive_paths(child, child_path))
    elif isinstance(value, list):
        for index, child in enumerate(value):
            found.extend(sensitive_paths(child, f"{path}[{index}]"))
    return found


def direct_scan(directory: Path) -> dict[str, Any]:
    problems: list[dict[str, Any]] = []
    scanned = []
    for name in sorted(NON_MANIFEST_FILES):
        path = directory / name
        if not path.is_file():
            continue
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            raise EvidenceError(name) from None
        scanned.append(name)
        try:
            parsed = (
                json.loads(text, object_pairs_hook=reject_duplicate_json_keys)
                if path.suffix == ".json"
                else None
            )
        except (ValueError, RecursionError):
            raise EvidenceError(name) from None
        fields = sensitive_paths(parsed) if parsed is not None else []
        scan_text = text
        canonical = (
            json.dumps(parsed, indent=2, sort_keys=True) + "\n"
            if isinstance(parsed, dict)
            else None
        )
        if (
            name == "sanitizer.json"
            and isinstance(parsed, dict)
            and exact_sanitizer(parsed)
            and text == canonical
        ):
            # The sanitizer's exact, checker-bound marker catalog names what it scanned;
            # those literal sentinels are metadata rather than leaked source material.
            projected = dict(parsed)
            projected["private_markers"] = []
            scan_text = json.dumps(projected, sort_keys=True)
        if not safe_text(scan_text) or fields:
            problems.append({"file": name, "sensitive_paths": fields})
    return {"files_scanned": scanned, "problems": problems}


def exact_signing(signing: Any, service: str, credential: str, generation: int, error: str | None = None) -> bool:
    expected = {
        "mode",
        "service_id",
        "active_credential_id",
        "bundle_generation",
        "configured_credential_count",
        "successful_activations",
        "rejected_reloads",
        "last_error_kind",
    }
    return (
        exact_keys(signing, expected)
        and signing["mode"] == "watched-bundle"
        and signing["service_id"] == service
        and signing["active_credential_id"] == credential
        and exact_int(signing["bundle_generation"], generation)
        and exact_int(signing["configured_credential_count"], 2)
        and exact_int(signing["successful_activations"], generation - 1)
        and exact_int(signing["rejected_reloads"])
        and signing["rejected_reloads"] >= 0
        and signing["last_error_kind"] == error
    )


def exact_control_projection(
    item: Any,
    *,
    leader_id: str,
    term: int,
    credential: str,
    bundle_generation: int,
    trust_generation: int,
    revision: int,
) -> bool:
    if not exact_keys(item, {
        "node_id", "cluster_id", "role", "term", "leader_id", "commit_index", "last_applied",
        "last_log_index", "storage_healthy", "local_service_credential_id",
        "committed_configuration", "service_signing", "service_authentication",
    }):
        return False
    committed = item["committed_configuration"]
    authentication = item["service_authentication"]
    if not exact_keys(committed, {"cluster_id", "revision", "term", "routing_policy", "worker_ids"}):
        return False
    if not exact_keys(authentication, {
        "required", "trusted_service_ids", "trusted_service_credentials", "revoked_service_credentials",
        "gateway_service_ids", "verifications", "authentication_rejections",
        "credential_revocation_rejections", "authorized_peer_rpcs", "authorized_gateway_reads",
        "last_verified_service_id", "last_verified_service_credential", "last_rejected_service_id",
        "last_rejected_service_credential", "trust_policy_generation", "trust_policy_validity",
    }):
        return False
    expected_credentials = [f"{service}/{key}" for service in SERVICES for key in ("key-a", "key-b")]
    expected_revoked = [] if trust_generation == 1 else [f"{service}/key-a" for service in SERVICES]
    numeric_auth = (
        "verifications", "authentication_rejections", "credential_revocation_rejections",
        "authorized_peer_rpcs", "authorized_gateway_reads",
    )
    return (
        item["node_id"] in CONTROLS
        and item["cluster_id"] == "inferlab-primary"
        and item["role"] in {"leader", "follower"}
        and exact_int(item["term"], term)
        and item["leader_id"] == leader_id
        and exact_int(item["commit_index"])
        and exact_int(item["last_applied"])
        and exact_int(item["last_log_index"])
        and item["commit_index"] >= revision
        and item["last_applied"] >= revision
        and item["last_log_index"] >= item["commit_index"]
        and item["storage_healthy"] is True
        and item["local_service_credential_id"] == credential
        and committed["cluster_id"] == "inferlab-primary"
        and exact_int(committed["revision"], revision)
        and exact_int(committed["term"])
        and committed["term"] > 0
        and committed["routing_policy"] == ("least-in-flight" if revision == 3 else "round-robin")
        and committed["worker_ids"] == ["cpu-signer-handoff"]
        and exact_signing(item["service_signing"], item["node_id"], credential, bundle_generation)
        and authentication["required"] is True
        and authentication["trusted_service_ids"] == SERVICES
        and authentication["trusted_service_credentials"] == expected_credentials
        and authentication["revoked_service_credentials"] == expected_revoked
        and authentication["gateway_service_ids"] == ["gateway-primary"]
        and all(exact_int(authentication[key]) and authentication[key] >= 0 for key in numeric_auth)
        and authentication["last_verified_service_id"] in {None, *SERVICES}
        and authentication["last_verified_service_credential"] in {None, *expected_credentials}
        and authentication["last_rejected_service_id"] in {None, *SERVICES}
        and authentication["last_rejected_service_credential"] in {None, *expected_credentials}
        and exact_int(authentication["trust_policy_generation"], trust_generation)
        and authentication["trust_policy_validity"] == "valid"
    )


def controls_document(document: Any, credential: str, bundle_generation: int, trust_generation: int, revision: int) -> bool:
    if not exact_keys(document, {"schema", "observed_at_ms", "samples", "result"}):
        return False
    if (
        document["schema"] != "inferlab.signer-handoff-controls.v0.29"
        or not exact_int(document["observed_at_ms"])
        or document["observed_at_ms"] <= 0
        or not exact_int(document["samples"])
        or document["samples"] < 1
    ):
        return False
    result = document["result"]
    if not exact_keys(result, {"leader_id", "term", "revision", "controls"}):
        return False
    controls = result["controls"]
    if not isinstance(controls, list) or len(controls) != 3:
        return False
    if sorted(item.get("node_id") for item in controls) != CONTROLS:
        return False
    roles = [item.get("role") for item in controls]
    if roles.count("leader") != 1 or roles.count("follower") != 2:
        return False
    leader = next(item for item in controls if item.get("role") == "leader")
    if (
        result["leader_id"] != leader["node_id"]
        or not exact_int(result["term"])
        or result["term"] < 1
        or not exact_int(result["revision"], revision)
    ):
        return False
    for item in controls:
        if not exact_control_projection(
            item,
            leader_id=leader["node_id"],
            term=result["term"],
            credential=credential,
            bundle_generation=bundle_generation,
            trust_generation=trust_generation,
            revision=revision,
        ):
            return False
    return True


def mixed_controls_document(
    document: Any,
    expected_credentials: dict[str, str],
    trust_generation: int,
    revision: int,
) -> bool:
    if not exact_keys(document, {"schema", "observed_at_ms", "samples", "result"}):
        return False
    if (
        document["schema"] != "inferlab.signer-handoff-controls.v0.29"
        or not exact_int(document["observed_at_ms"])
        or document["observed_at_ms"] <= 0
        or not exact_int(document["samples"])
        or document["samples"] < 1
    ):
        return False
    result = document["result"]
    if not exact_keys(result, {"leader_id", "term", "revision", "controls"}):
        return False
    controls = result["controls"]
    if not isinstance(controls, list) or len(controls) != 3:
        return False
    if sorted(item.get("node_id") for item in controls) != CONTROLS:
        return False
    roles = [item.get("role") for item in controls]
    if roles.count("leader") != 1 or roles.count("follower") != 2:
        return False
    leader = next(item for item in controls if item.get("role") == "leader")
    if (
        result["leader_id"] != leader["node_id"]
        or not exact_int(result["revision"], revision)
        or not exact_int(result["term"])
        or result["term"] < 1
    ):
        return False
    if set(expected_credentials) != set(CONTROLS):
        return False
    for item in controls:
        service = item["node_id"]
        credential = expected_credentials[service]
        generation = 2 if credential == "key-b" else 1
        if not exact_control_projection(
            item,
            leader_id=leader["node_id"],
            term=result["term"],
            credential=credential,
            bundle_generation=generation,
            trust_generation=trust_generation,
            revision=revision,
        ):
            return False
    return True


def trusted_public_keys(generations: dict[str, Any], generation: int) -> dict[tuple[str, str], str]:
    snapshot = generations["generations"][str(generation)]
    return {
        (item["service_id"], item["credential_id"]): item["public_key_base64"]
        for item in snapshot["trusted_credentials"]
    }


def exact_trust_generations(document: Any) -> bool:
    if not exact_keys(document, {"schema", "root_key_id", "root_public_key_base64", "generations"}):
        return False
    if (
        document["schema"] != "inferlab.signer-handoff-trust-generations.v0.29"
        or document["root_key_id"] != "service-trust-root-v029"
        or document["root_public_key_base64"] != EXPECTED_ROOT_PUBLIC_KEY
        or not valid_ed25519_public(document["root_public_key_base64"])
    ):
        return False
    generations = document["generations"]
    if not exact_keys(generations, {"1", "2"}):
        return False
    expected_pairs = [(service, key) for service in SERVICES for key in ("key-a", "key-b")]
    for number in (1, 2):
        snapshot = generations[str(number)]
        if not exact_keys(snapshot, {
            "schema", "cluster_id", "generation", "issued_at_ms", "expires_at_ms",
            "trusted_credentials", "revoked_service_ids", "revoked_credentials",
            "gateway_service_ids", "authentication",
        }):
            return False
        auth = snapshot.get("authentication")
        credentials = snapshot.get("trusted_credentials")
        revoked = snapshot.get("revoked_credentials")
        if (
            snapshot.get("schema") != "inferlab.service-trust-policy.v2"
            or snapshot.get("cluster_id") != "inferlab-primary"
            or not exact_int(snapshot.get("generation"), number)
            or not exact_int(snapshot.get("issued_at_ms"))
            or not exact_int(snapshot.get("expires_at_ms"))
            or snapshot["issued_at_ms"] <= 0
            or snapshot["issued_at_ms"] >= snapshot["expires_at_ms"]
            or snapshot["expires_at_ms"] - snapshot["issued_at_ms"] != 600_000
            or snapshot.get("revoked_service_ids") != []
            or snapshot.get("gateway_service_ids") != ["gateway-primary"]
            or not isinstance(credentials, list)
            or len(credentials) != len(expected_pairs)
            or not all(exact_keys(item, {"service_id", "credential_id", "public_key_base64"}) for item in credentials)
            or [(item.get("service_id"), item.get("credential_id")) for item in credentials] != expected_pairs
            or any(
                item["public_key_base64"]
                != EXPECTED_SERVICE_PUBLIC_KEYS[(item["service_id"], item["credential_id"])]
                for item in credentials
            )
            or not all(valid_ed25519_public(item["public_key_base64"]) for item in credentials)
            or len({item["public_key_base64"] for item in credentials}) != len(credentials)
            or not exact_keys(auth, {"schema", "algorithm", "key_id", "signature"})
            or auth["schema"] != "inferlab.service-trust-authentication.v2"
            or auth["algorithm"] != "ed25519"
            or auth["key_id"] != document["root_key_id"]
            or not verify_ed25519(document["root_public_key_base64"], canonical_snapshot(snapshot), auth["signature"])
        ):
            return False
        expected_revoked = [] if number == 1 else [
            {"service_id": service, "credential_id": "key-a"} for service in SERVICES
        ]
        if (
            not isinstance(revoked, list)
            or not all(exact_keys(item, {"service_id", "credential_id"}) for item in revoked)
            or revoked != expected_revoked
        ):
            return False
    if generations["1"]["issued_at_ms"] != generations["2"]["issued_at_ms"] or generations["1"]["expires_at_ms"] != generations["2"]["expires_at_ms"]:
        return False
    return True


def receipt_document(document: Any, generations: dict[str, Any], generation: int, credential: str) -> bool:
    if not exact_keys(document, {"schema", "observed_at_ms", "samples", "result"}):
        return False
    if (
        document["schema"] != "inferlab.signer-handoff-distributor.v0.29"
        or not exact_int(document["observed_at_ms"])
        or document["observed_at_ms"] <= 0
        or not exact_int(document["samples"])
        or document["samples"] < 1
    ):
        return False
    observation = document["result"]
    if not exact_capture(observation, "GET", "/v1/service-trust/status", 200) or observation["headers"] != {}:
        return False
    body = observation.get("body")
    receipts = body.get("receipts") if isinstance(body, dict) else None
    snapshot_status = body.get("snapshot") if isinstance(body, dict) else None
    if (
        not exact_keys(body, {
            "schema", "cluster_id", "expected_receiver_mode", "snapshot", "expected_receivers",
            "acked_receivers", "pending_receivers", "receipt_count", "receipts", "storage",
            "transport_security",
        })
        or body["schema"] != "inferlab.trust-distributor-status.v1"
        or body["cluster_id"] != "inferlab-primary"
        or body.get("expected_receiver_mode") != "service-id"
        or body.get("expected_receivers") != CONTROLS
        or body.get("acked_receivers") != CONTROLS
        or body.get("pending_receivers") != []
        or not exact_int(body.get("receipt_count"), 3)
        or not isinstance(receipts, list)
        or len(receipts) != 3
        or not exact_keys(snapshot_status, {
            "policy_schema", "generation", "issued_at_ms", "expires_at_ms", "root_key_id", "etag",
        })
        or snapshot_status["policy_schema"] != "inferlab.service-trust-policy.v2"
        or not exact_int(snapshot_status["generation"], generation)
        or not exact_int(snapshot_status["issued_at_ms"])
        or not exact_int(snapshot_status["expires_at_ms"])
        or snapshot_status["issued_at_ms"] >= snapshot_status["expires_at_ms"]
        or snapshot_status["root_key_id"] != generations["root_key_id"]
        or not isinstance(snapshot_status["etag"], str)
        or len(snapshot_status["etag"]) < 16
        or body["storage"] != {"mutation_poisoned": False, "error_code": None}
        or body["transport_security"] != {
            "mode": "mutual-tls", "client_certificate_required": True, "minimum_protocol": "TLSv1.3",
        }
    ):
        return False
    public = trusted_public_keys(generations, generation)
    snapshot = generations["generations"][str(generation)]
    if (
        snapshot_status["issued_at_ms"] != snapshot["issued_at_ms"]
        or snapshot_status["expires_at_ms"] != snapshot["expires_at_ms"]
    ):
        return False
    seen = set()
    signatures = set()
    for receipt in receipts:
        authentication = receipt.get("authentication")
        receiver = receipt.get("receiver_service_id")
        key_id = receipt.get("receiver_credential_id")
        if (
            not exact_keys(receipt, {
                "schema", "cluster_id", "generation", "root_key_id", "snapshot_signature",
                "receiver_service_id", "receiver_credential_id", "applied_at_ms", "authentication",
            })
            or receipt["schema"] != "inferlab.service-trust-receipt.v1"
            or receipt["cluster_id"] != "inferlab-primary"
            or not exact_int(receipt["generation"], generation)
            or receipt["root_key_id"] != generations["root_key_id"]
            or receipt["snapshot_signature"] != snapshot["authentication"]["signature"]
            or receiver not in CONTROLS
            or key_id != credential
            or not exact_int(receipt["applied_at_ms"])
            or receipt["applied_at_ms"] < snapshot["issued_at_ms"]
            or receipt["applied_at_ms"] >= snapshot["expires_at_ms"]
            or receipt["applied_at_ms"] > document["observed_at_ms"]
            or not exact_keys(authentication, {"schema", "algorithm", "signature"})
            or authentication["schema"] != "inferlab.service-trust-receipt-authentication.v1"
            or authentication["algorithm"] != "ed25519"
            or not verify_ed25519(public[(receiver, key_id)], canonical_receipt(receipt), authentication["signature"])
        ):
            return False
        seen.add(receiver)
        signatures.add(authentication["signature"])
    return seen == set(CONTROLS) and len(signatures) == 3


def exact_process(item: Any, label: str) -> bool:
    return (
        exact_keys(item, {"label", "pid", "ppid", "state", "start_token", "command"})
        and item["label"] == label
        and exact_int(item["pid"])
        and item["pid"] > 1
        and exact_int(item["ppid"])
        and item["ppid"] > 0
        and isinstance(item["state"], str)
        and "Z" not in item["state"]
        and isinstance(item["start_token"], str)
        and re.fullmatch(
            r"(?:Mon|Tue|Wed|Thu|Fri|Sat|Sun) (?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) "
            r"[0-9]{1,2} [0-9]{2}:[0-9]{2}:[0-9]{2} [0-9]{4}",
            item["start_token"],
        ) is not None
        and item["command"] == PROCESS_BINARIES[label]
    )


def exact_process_set(value: Any) -> bool:
    return (
        isinstance(value, list)
        and len(value) == 6
        and {item.get("label") for item in value} == set(PROCESS_BINARIES)
        and all(exact_process(item, item["label"]) for item in value)
        and len({item["pid"] for item in value}) == 6
        and len({item["ppid"] for item in value}) == 1
    )


def process_identity(item: dict[str, Any]) -> tuple[Any, ...]:
    return (item.get("label"), item.get("pid"), item.get("ppid"), item.get("start_token"), item.get("command"))


def exact_capture(observation: Any, method: str, path: str, status: int) -> bool:
    return (
        exact_keys(
            observation,
            {"method", "path", "started_at_ms", "observed_at_ms", "duration_ms", "status", "headers", "body"},
        )
        and observation["method"] == method
        and observation["path"] == path
        and exact_int(observation["started_at_ms"])
        and observation["started_at_ms"] > 0
        and exact_int(observation["observed_at_ms"])
        and observation["observed_at_ms"] >= observation["started_at_ms"]
        and finite_number(observation["duration_ms"])
        and exact_int(observation["status"], status)
        and isinstance(observation["headers"], dict)
    )


def exact_success_headers(headers: Any, revision: int) -> bool:
    return (
        exact_keys(headers, {
            "x-inferlab-attempts", "x-inferlab-config-revision", "x-inferlab-config-term",
            "x-inferlab-control-cluster", "x-inferlab-control-key-id", "x-inferlab-worker",
        })
        and headers["x-inferlab-attempts"] == "1"
        and headers["x-inferlab-config-revision"] == str(revision)
        and isinstance(headers["x-inferlab-config-term"], str)
        and re.fullmatch(r"[1-9][0-9]*", headers["x-inferlab-config-term"]) is not None
        and headers["x-inferlab-control-cluster"] == "inferlab-primary"
        and headers["x-inferlab-control-key-id"] == "route-v029"
        and headers["x-inferlab-worker"] == "cpu-signer-handoff"
    )


def exact_generation_metrics(value: Any) -> bool:
    if not exact_keys(value, {
        "mode", "query_tokens", "kv_tokens", "attention_score_elements", "cache_bytes",
        "peak_cache_bytes", "cache_rebuilds", "cache_pages", "shared_cache_pages",
        "reserved_cache_bytes", "internal_fragmentation_bytes", "prefix_cache_hit",
        "prefix_tokens_reused", "copy_on_write_copies", "decoding", "speculation",
    }):
        return False
    integer_fields = (
        "query_tokens", "kv_tokens", "attention_score_elements", "cache_bytes", "peak_cache_bytes",
        "cache_rebuilds", "cache_pages", "shared_cache_pages", "reserved_cache_bytes",
        "internal_fragmentation_bytes", "prefix_tokens_reused", "copy_on_write_copies",
    )
    if (
        value["mode"] != "paged-kv-cache"
        or not all(exact_int(value[key]) and value[key] >= 0 for key in integer_fields)
        or value["query_tokens"] <= 0
        or value["kv_tokens"] <= 0
        or value["attention_score_elements"] <= 0
        or value["cache_bytes"] <= 0
        or value["peak_cache_bytes"] < value["cache_bytes"]
        or value["cache_pages"] <= 0
        or value["reserved_cache_bytes"] < value["cache_bytes"]
        or not isinstance(value["prefix_cache_hit"], bool)
    ):
        return False
    decoding = value["decoding"]
    speculation = value["speculation"]
    if not exact_keys(decoding, {
        "kind", "schema_name", "temperature", "top_k", "top_p", "repetition_penalty",
        "banned_token_count", "sampled_steps", "greedy_steps",
        "grammar_constrained_steps", "candidate_tokens_total", "masked_tokens_total", "mean_entropy",
    }):
        return False
    decoding_ints = (
        "top_k", "banned_token_count", "sampled_steps", "greedy_steps",
        "grammar_constrained_steps", "candidate_tokens_total", "masked_tokens_total",
    )
    if (
        decoding["kind"] != "text"
        or decoding["schema_name"] is not None
        or not all(exact_int(decoding[key]) and decoding[key] >= 0 for key in decoding_ints)
        or not all(finite_number(decoding[key]) for key in ("temperature", "top_p", "repetition_penalty", "mean_entropy"))
        or decoding["temperature"] != 0
        or decoding["top_k"] != 0
        or decoding["top_p"] != 1
        or decoding["repetition_penalty"] != 1
        or decoding["sampled_steps"] != 0
        or decoding["greedy_steps"] <= 0
        or decoding["candidate_tokens_total"] <= 0
    ):
        return False
    if not exact_keys(speculation, {
        "enabled", "draft_quantization", "draft_tokens_per_cycle", "target_forward_calls",
        "draft_forward_calls", "cycles", "proposed_tokens", "accepted_tokens", "rejected_tokens",
        "discarded_tokens", "correction_tokens", "extra_target_tokens", "acceptance_rate_percent",
    }):
        return False
    speculation_ints = (
        "draft_tokens_per_cycle", "target_forward_calls", "draft_forward_calls", "cycles",
        "proposed_tokens", "accepted_tokens", "rejected_tokens", "discarded_tokens",
        "correction_tokens", "extra_target_tokens",
    )
    return (
        isinstance(speculation["enabled"], bool)
        and (speculation["draft_quantization"] is None or isinstance(speculation["draft_quantization"], str))
        and all(exact_int(speculation[key]) and speculation[key] >= 0 for key in speculation_ints)
        and speculation["target_forward_calls"] > 0
        and finite_number(speculation["acceptance_rate_percent"])
        and speculation["acceptance_rate_percent"] <= 100
    )


def generation_metric_diagnostics(value: Any) -> list[str]:
    expected_keys = {
        "mode", "query_tokens", "kv_tokens", "attention_score_elements", "cache_bytes",
        "peak_cache_bytes", "cache_rebuilds", "cache_pages", "shared_cache_pages",
        "reserved_cache_bytes", "internal_fragmentation_bytes", "prefix_cache_hit",
        "prefix_tokens_reused", "copy_on_write_copies", "decoding", "speculation",
    }
    if not exact_keys(value, expected_keys):
        return ["schema"]
    tags: list[str] = []
    integer_fields = (
        "query_tokens", "kv_tokens", "attention_score_elements", "cache_bytes", "peak_cache_bytes",
        "cache_rebuilds", "cache_pages", "shared_cache_pages", "reserved_cache_bytes",
        "internal_fragmentation_bytes", "prefix_tokens_reused", "copy_on_write_copies",
    )
    if not all(exact_int(value[key]) and value[key] >= 0 for key in integer_fields):
        tags.append("integer-fields")
    if value["mode"] != "paged-kv-cache":
        tags.append("mode")
    if not all(
        exact_int(value[key]) and value[key] > 0
        for key in ("query_tokens", "kv_tokens", "attention_score_elements", "cache_bytes", "cache_pages")
    ):
        tags.append("positive-core")
    if not (
        exact_int(value["peak_cache_bytes"])
        and exact_int(value["reserved_cache_bytes"])
        and exact_int(value["cache_bytes"])
        and value["peak_cache_bytes"] >= value["cache_bytes"]
        and value["reserved_cache_bytes"] >= value["cache_bytes"]
    ):
        tags.append("cache-algebra")
    if not isinstance(value["prefix_cache_hit"], bool):
        tags.append("prefix-flag")
    decoding = value["decoding"]
    decoding_keys = {
        "kind", "schema_name", "temperature", "top_k", "top_p", "repetition_penalty",
        "banned_token_count", "sampled_steps", "greedy_steps", "grammar_constrained_steps",
        "candidate_tokens_total", "masked_tokens_total", "mean_entropy",
    }
    if not exact_keys(decoding, decoding_keys):
        tags.append("decoding-schema")
    else:
        decoding_ints = (
            "top_k", "banned_token_count", "sampled_steps", "greedy_steps",
            "grammar_constrained_steps", "candidate_tokens_total", "masked_tokens_total",
        )
        if not all(exact_int(decoding[key]) and decoding[key] >= 0 for key in decoding_ints):
            tags.append("decoding-integers")
        if not all(finite_number(decoding[key]) for key in ("temperature", "top_p", "repetition_penalty", "mean_entropy")):
            tags.append("decoding-numbers")
        if decoding["kind"] != "text" or decoding["schema_name"] is not None:
            tags.append("decoding-kind")
        if not (
            finite_number(decoding["temperature"])
            and exact_int(decoding["top_k"])
            and finite_number(decoding["top_p"])
            and finite_number(decoding["repetition_penalty"])
            and decoding["temperature"] == 0
            and decoding["top_k"] == 0
            and decoding["top_p"] == 1
            and decoding["repetition_penalty"] == 1
        ):
            tags.append("decoding-policy")
        if not (
            exact_int(decoding["sampled_steps"], 0)
            and exact_int(decoding["greedy_steps"])
            and decoding["greedy_steps"] > 0
            and exact_int(decoding["candidate_tokens_total"])
            and decoding["candidate_tokens_total"] > 0
        ):
            tags.append("decoding-work")
    speculation = value["speculation"]
    speculation_keys = {
        "enabled", "draft_quantization", "draft_tokens_per_cycle", "target_forward_calls",
        "draft_forward_calls", "cycles", "proposed_tokens", "accepted_tokens", "rejected_tokens",
        "discarded_tokens", "correction_tokens", "extra_target_tokens", "acceptance_rate_percent",
    }
    if not exact_keys(speculation, speculation_keys):
        tags.append("speculation-schema")
    else:
        speculation_ints = (
            "draft_tokens_per_cycle", "target_forward_calls", "draft_forward_calls", "cycles",
            "proposed_tokens", "accepted_tokens", "rejected_tokens", "discarded_tokens",
            "correction_tokens", "extra_target_tokens",
        )
        if not all(exact_int(speculation[key]) and speculation[key] >= 0 for key in speculation_ints):
            tags.append("speculation-integers")
        if not isinstance(speculation["enabled"], bool) or not (
            speculation["draft_quantization"] is None or isinstance(speculation["draft_quantization"], str)
        ):
            tags.append("speculation-types")
        if not exact_int(speculation["target_forward_calls"]) or speculation["target_forward_calls"] <= 0:
            tags.append("speculation-work")
        if not finite_number(speculation["acceptance_rate_percent"]) or speculation["acceptance_rate_percent"] > 100:
            tags.append("speculation-rate")
    return sorted(set(tags))


def exact_control_config_body(body: Any, revision: int) -> bool:
    return (
        exact_keys(body, {
            "cluster_id", "revision", "term", "routing_policy", "worker_ids", "authentication_key_id",
        })
        and body["cluster_id"] == "inferlab-primary"
        and exact_int(body["revision"], revision)
        and exact_int(body["term"])
        and body["term"] > 0
        and body["routing_policy"] == ("least-in-flight" if revision == 3 else "round-robin")
        and body["worker_ids"] == ["cpu-signer-handoff"]
        and body["authentication_key_id"] == "route-v029"
    )


def exact_revoked_error(observation: Any, method: str, path: str, service: str) -> bool:
    body = observation.get("body") if isinstance(observation, dict) else None
    error = body.get("error") if isinstance(body, dict) else None
    return (
        exact_capture(observation, method, path, 401)
        and observation["headers"] == {}
        and exact_keys(body, {"error"})
        and exact_keys(error, {"code", "message", "leader_id"})
        and error["code"] == "unauthorized"
        and error["message"] == f"service credential '{service}/key-a' is revoked"
        and error["leader_id"] is None
    )


def exact_json_completion(document: Any) -> bool:
    if not exact_keys(document, {"schema", "observation"}) or document["schema"] != "inferlab.signer-handoff-json.v0.29":
        return False
    observation = document["observation"]
    if (
        not exact_capture(observation, "POST", "/v1/chat/completions", 200)
        or observation["started_at_ms"] <= 0
        or not finite_number(observation["duration_ms"])
        or observation["duration_ms"] <= 0
        or not exact_success_headers(observation["headers"], 3)
    ):
        return False
    body = observation["body"]
    if not exact_keys(body, {"object", "model", "choices", "usage", "inferlab"}):
        return False
    choices = body["choices"]
    usage = body["usage"]
    inferlab = body["inferlab"]
    if not isinstance(choices, list) or len(choices) != 1:
        return False
    choice = choices[0]
    message = choice.get("message") if isinstance(choice, dict) else None
    return (
        body["object"] == "chat.completion"
        and body["model"] == "inferlab-tiny"
        and exact_keys(choice, {"index", "message", "finish_reason"})
        and exact_int(choice["index"], 0)
        and choice["finish_reason"] == "stop"
        and exact_keys(message, {"role", "content"})
        and message["role"] == "assistant"
        and message["content"] == "InferLab turns prompts into real tokens."
        and exact_keys(usage, {"prompt_tokens", "completion_tokens", "total_tokens"})
        and all(exact_int(usage[key]) and usage[key] > 0 for key in usage)
        and usage["completion_tokens"] <= 8
        and usage["total_tokens"] == usage["prompt_tokens"] + usage["completion_tokens"]
        and exact_keys(inferlab, {"generation"})
        and exact_generation_metrics(inferlab["generation"])
    )


def exact_sse_completion(document: Any) -> bool:
    if not exact_keys(document, {
        "schema", "method", "path", "started_at_ms", "observed_at_ms", "duration_ms", "status",
        "headers", "event_count", "content_event_count", "offsets_ms", "pieces", "content",
        "finish_reason", "generation", "done_received", "eof_after_done",
    }):
        return False
    expected_pieces = ["InferLab", " turns", " prompts", " into", " real", " tokens", "."]
    offsets = document["offsets_ms"]
    return (
        document["schema"] == "inferlab.signer-handoff-sse.v0.29"
        and document["method"] == "POST"
        and document["path"] == "/v1/chat/completions"
        and exact_int(document["started_at_ms"])
        and document["started_at_ms"] > 0
        and exact_int(document["observed_at_ms"])
        and document["observed_at_ms"] >= document["started_at_ms"]
        and finite_number(document["duration_ms"])
        and document["duration_ms"] > 0
        and exact_int(document["status"], 200)
        and exact_success_headers(document["headers"], 3)
        and document["pieces"] == expected_pieces
        and exact_int(document["content_event_count"], len(expected_pieces))
        and exact_int(document["event_count"], len(expected_pieces) + 3)
        and document["content"] == "".join(expected_pieces)
        and document["finish_reason"] == "stop"
        and exact_generation_metrics(document["generation"])
        and document["done_received"] is True
        and document["eof_after_done"] is True
        and isinstance(offsets, list)
        and len(offsets) == document["event_count"]
        and all(finite_number(value) for value in offsets)
        and offsets == sorted(offsets)
        and len(set(offsets)) == len(offsets)
        and offsets[0] >= 0
        and offsets[-1] <= document["duration_ms"]
        and offsets[-2] - offsets[1] >= 400
    )


def json_completion_diagnostics(document: Any) -> list[str]:
    if not exact_keys(document, {"schema", "observation"}):
        return ["top-schema"]
    if document["schema"] != "inferlab.signer-handoff-json.v0.29":
        return ["schema-value"]
    observation = document["observation"]
    tags: list[str] = []
    if not exact_capture(observation, "POST", "/v1/chat/completions", 200):
        return ["capture"]
    if observation["started_at_ms"] <= 0 or not finite_number(observation["duration_ms"]) or observation["duration_ms"] <= 0:
        tags.append("timing")
    if not exact_success_headers(observation["headers"], 3):
        tags.append("headers")
    body = observation["body"]
    if not exact_keys(body, {"object", "model", "choices", "usage", "inferlab"}):
        return sorted(set([*tags, "body-schema"]))
    if body["object"] != "chat.completion" or body["model"] != "inferlab-tiny":
        tags.append("body-identity")
    choices = body["choices"]
    if not isinstance(choices, list) or len(choices) != 1:
        return sorted(set([*tags, "choice-count"]))
    choice = choices[0]
    if not exact_keys(choice, {"index", "message", "finish_reason"}):
        tags.append("choice-schema")
    else:
        message = choice["message"]
        if not exact_int(choice["index"], 0) or choice["finish_reason"] != "stop":
            tags.append("choice-result")
        if not exact_keys(message, {"role", "content"}):
            tags.append("message-schema")
        elif message["role"] != "assistant" or message["content"] != "InferLab turns prompts into real tokens.":
            tags.append("message-result")
    usage = body["usage"]
    if not exact_keys(usage, {"prompt_tokens", "completion_tokens", "total_tokens"}):
        tags.append("usage-schema")
    elif not (
        all(exact_int(usage[key]) and usage[key] > 0 for key in usage)
        and usage["completion_tokens"] <= 8
        and usage["total_tokens"] == usage["prompt_tokens"] + usage["completion_tokens"]
    ):
        tags.append("usage-algebra")
    inferlab = body["inferlab"]
    if not exact_keys(inferlab, {"generation"}):
        tags.append("inferlab-schema")
    elif not exact_generation_metrics(inferlab["generation"]):
        tags.extend(
            f"generation-{tag}" for tag in generation_metric_diagnostics(inferlab["generation"])
        )
    return sorted(set(tags))


def sse_completion_diagnostics(document: Any) -> list[str]:
    expected_keys = {
        "schema", "method", "path", "started_at_ms", "observed_at_ms", "duration_ms", "status",
        "headers", "event_count", "content_event_count", "offsets_ms", "pieces", "content",
        "finish_reason", "generation", "done_received", "eof_after_done",
    }
    if not exact_keys(document, expected_keys):
        return ["top-schema"]
    tags: list[str] = []
    if document["schema"] != "inferlab.signer-handoff-sse.v0.29" or document["method"] != "POST" or document["path"] != "/v1/chat/completions":
        tags.append("identity")
    if not (
        exact_int(document["started_at_ms"])
        and document["started_at_ms"] > 0
        and exact_int(document["observed_at_ms"])
        and document["observed_at_ms"] >= document["started_at_ms"]
        and finite_number(document["duration_ms"])
        and document["duration_ms"] > 0
        and exact_int(document["status"], 200)
    ):
        tags.append("capture")
    if not exact_success_headers(document["headers"], 3):
        tags.append("headers")
    expected_pieces = ["InferLab", " turns", " prompts", " into", " real", " tokens", "."]
    if document["pieces"] != expected_pieces or document["content"] != "".join(expected_pieces):
        tags.append("content")
    if not exact_int(document["content_event_count"], len(expected_pieces)) or not exact_int(document["event_count"], len(expected_pieces) + 3):
        tags.append("counts")
    if document["finish_reason"] != "stop" or document["done_received"] is not True or document["eof_after_done"] is not True:
        tags.append("terminal")
    if not exact_generation_metrics(document["generation"]):
        tags.extend(
            f"generation-{tag}" for tag in generation_metric_diagnostics(document["generation"])
        )
    offsets = document["offsets_ms"]
    if (
        not isinstance(offsets, list)
        or len(offsets) < 3
        or not exact_int(document["event_count"])
        or len(offsets) != document["event_count"]
        or not all(finite_number(value) for value in offsets)
    ):
        tags.append("offset-schema")
    elif not (
        offsets == sorted(offsets)
        and len(set(offsets)) == len(offsets)
        and offsets[0] >= 0
        and offsets[-1] <= document["duration_ms"]
    ):
        tags.append("offset-order")
    elif offsets[-2] - offsets[1] < 400:
        tags.append("offset-span")
    return sorted(set(tags))


def exact_gateway_document(document: Any, credential: str, generation: int, revision: int) -> bool:
    if not exact_keys(document, {"schema", "observed_at_ms", "samples", "result"}):
        return False
    if (
        document["schema"] != "inferlab.signer-handoff-gateway.v0.29"
        or not exact_int(document["observed_at_ms"])
        or document["observed_at_ms"] <= 0
        or not exact_int(document["samples"])
        or document["samples"] < 1
    ):
        return False
    observation = document["result"]
    if not exact_capture(observation, "GET", "/internal/workers", 200):
        return False
    body = observation["body"]
    if not exact_keys(body, {"routing_policy", "worker_ids", "routing_snapshot", "control_plane", "service_signing"}):
        return False
    signing = body["service_signing"]
    control = body["control_plane"]
    routing = body["routing_snapshot"]
    return (
        exact_keys(routing, {"control_cluster_id", "control_revision", "control_term", "control_signing_key_id"})
        and exact_keys(control, {
            "enabled", "service_authentication_enabled", "service_id", "service_credential_id",
            "revision", "term", "last_error",
        })
        and body["routing_policy"] == ("least-in-flight" if revision == 3 else "round-robin")
        and body["worker_ids"] == ["cpu-signer-handoff"]
        and routing.get("control_cluster_id") == "inferlab-primary"
        and exact_int(routing.get("control_revision"), revision)
        and exact_int(routing.get("control_term"))
        and routing.get("control_term") > 0
        and routing.get("control_signing_key_id") == "route-v029"
        and control.get("enabled") is True
        and control.get("service_authentication_enabled") is True
        and control.get("service_id") == "gateway-primary"
        and control.get("service_credential_id") == credential
        and exact_int(control.get("revision"), revision)
        and exact_int(control.get("term"), routing.get("control_term"))
        and control.get("last_error") is None
        and exact_keys(signing, {
            "mode", "active_credential_id", "bundle_generation", "configured_credential_count",
            "successful_activations", "rejected_reloads", "last_error_kind",
        })
        and signing["mode"] == "watched-bundle"
        and signing["active_credential_id"] == credential
        and exact_int(signing["bundle_generation"], generation)
        and exact_int(signing["configured_credential_count"], 2)
        and exact_int(signing["successful_activations"], generation - 1)
        and exact_int(signing["rejected_reloads"])
        and signing["last_error_kind"] is None
    )


def exact_auth_guard(value: Any) -> bool:
    return (
        exact_keys(value, {
            "term", "revision", "commit_index", "last_applied", "last_log_index",
            "authentication_rejections", "credential_revocation_rejections",
        })
        and all(exact_int(value[key]) for key in value)
        and value["term"] > 0
        and value["revision"] >= 2
        and value["commit_index"] >= value["revision"]
        and value["last_applied"] >= value["revision"]
        and value["last_log_index"] >= value["commit_index"]
        and value["authentication_rejections"] >= 0
        and value["credential_revocation_rejections"] >= 0
    )


def exact_publish(document: Any, generation: int) -> bool:
    if not exact_keys(document, {"schema", "observation"}) or document["schema"] != "inferlab.signer-handoff-capture.v0.29":
        return False
    observation = document["observation"]
    body = observation.get("body") if isinstance(observation, dict) else None
    return (
        exact_capture(observation, "POST", "/v1/service-trust/snapshot", 201)
        and observation["headers"] == {}
        and exact_keys(body, {"schema", "outcome", "generation", "root_key_id", "etag"})
        and body["schema"] == "inferlab.trust-distributor-publish.v1"
        and body["outcome"] == "published"
        and exact_int(body["generation"], generation)
        and body["root_key_id"] == "service-trust-root-v029"
        and isinstance(body["etag"], str)
        and re.fullmatch(
            rf'"inferlab-primary:{generation}:service-trust-root-v029:[A-Za-z0-9+/]+={{0,2}}"',
            body["etag"],
        ) is not None
    )


def exact_write(document: Any, label: str, revision: int, policy: str) -> bool:
    if not exact_keys(document, {"schema", "label", "started_at_ms", "observed_at_ms", "status", "committed"}):
        return False
    committed = document["committed"]
    return (
        document["schema"] == "inferlab.signer-handoff-write.v0.29"
        and document["label"] == label
        and exact_int(document["started_at_ms"])
        and document["started_at_ms"] > 0
        and exact_int(document["observed_at_ms"])
        and document["observed_at_ms"] >= document["started_at_ms"]
        and exact_int(document["status"], 200)
        and exact_keys(committed, {
            "cluster_id", "revision", "term", "routing_policy", "worker_ids", "writer_id", "authentication_key_id",
        })
        and committed["cluster_id"] == "inferlab-primary"
        and exact_int(committed["revision"], revision)
        and exact_int(committed["term"])
        and committed["term"] > 0
        and committed["routing_policy"] == policy
        and committed["worker_ids"] == ["cpu-signer-handoff"]
        and committed["writer_id"] == "v029-deployer"
        and committed["authentication_key_id"] == "route-v029"
    )


def published_etag(document: Any) -> Any:
    if not isinstance(document, dict):
        return None
    observation = document.get("observation")
    body = observation.get("body") if isinstance(observation, dict) else None
    return body.get("etag") if isinstance(body, dict) else None


def distributor_etag(document: Any) -> Any:
    if not isinstance(document, dict):
        return None
    observation = document.get("result")
    body = observation.get("body") if isinstance(observation, dict) else None
    snapshot = body.get("snapshot") if isinstance(body, dict) else None
    return snapshot.get("etag") if isinstance(snapshot, dict) else None


def exact_log_scan(document: Any) -> bool:
    expected_files = sorted([
        "control-a.log", "control-b.log", "control-c.log", "cpu-worker.log", "gateway.log", "trust-distributor.log",
        "startup-missing.log", "startup-malformed.log", "startup-oversize.log",
        "startup-unsafe-permissions.log", "startup-non-regular.log", "startup-symlink.log",
        "startup-wrong-cluster.log", "startup-wrong-service.log", "startup-unknown-active.log",
        *(f"production-{index}.log" for index in range(11)),
    ])
    return (
        exact_keys(document, {"schema", "files_scanned", "checks", "matches", "passed"})
        and document["schema"] == "inferlab.signer-handoff-discarded-log-scan.v0.29"
        and document["files_scanned"] == expected_files
        and document["checks"] == [
            "deterministic-seeds", "fixed-prompts", "sensitive-source-paths", "project-paths",
            "unexpected-host-paths", "private-markers",
        ]
        and document["matches"] == []
        and document["passed"] is True
    )


def exact_sanitizer(document: Any) -> bool:
    return (
        exact_keys(document, {"schema", "files_scanned", "private_markers", "sensitive_fields", "problem_count", "problems"})
        and document["schema"] == "inferlab.signer-handoff-sanitizer.v0.29"
        and document["files_scanned"] == sorted(BASE_EVIDENCE_FILES)
        and document["private_markers"] == list(PRIVATE_MARKERS)
        and document["sensitive_fields"] == sorted(SENSITIVE_JSON_FIELDS)
        and exact_int(document["problem_count"], 0)
        and document["problems"] == []
    )


def exact_private_scan(document: Any) -> bool:
    return (
        exact_keys(document, {"schema", "algorithm", "files_scanned", "seed_labels_scanned", "representations_per_seed", "matches", "passed"})
        and document["schema"] == "inferlab.signer-handoff-private-material-scan.v0.29"
        and document["algorithm"] == "sha256-label-to-ed25519-seed"
        and document["files_scanned"] == sorted(NON_MANIFEST_FILES - {"private-material-scan.json"})
        and document["seed_labels_scanned"] == SEED_LABELS
        and exact_int(document["representations_per_seed"], 4)
        and document["matches"] == []
        and document["passed"] is True
    )


def startup_matrix(document: Any) -> bool:
    if not exact_keys(document, {"schema", "cases"}) or document["schema"] != "inferlab.signer-handoff-startup-rejections.v0.29":
        return False
    cases = document["cases"]
    if not isinstance(cases, list) or len(cases) != len(STARTUP_CASES):
        return False
    by_name = {item.get("scenario"): item for item in cases}
    if set(by_name) != set(STARTUP_CASES):
        return False
    for scenario, kind in STARTUP_CASES.items():
        item = by_name[scenario]
        if (
            not exact_keys(item, {
                "scenario", "expected_error_kind", "port", "exit_code", "pid", "listener_ever_open",
                "listener_probe_count", "state_files_created", "diagnostic",
            })
            or item["expected_error_kind"] != kind
            or not exact_int(item["port"], 12180 + list(STARTUP_CASES).index(scenario))
            or not exact_int(item["exit_code"], 1)
            or not exact_int(item["pid"])
            or item["pid"] <= 1
            or item["listener_ever_open"] is not False
            or not exact_int(item["listener_probe_count"])
            or item["listener_probe_count"] < 1
            or item["state_files_created"] != []
            or item["diagnostic"] != ERROR_MESSAGES[kind]
        ):
            return False
    return True


def live_rejections(document: Any, process_map: dict[str, dict[str, Any]]) -> bool:
    if not exact_keys(document, {"schema", "cases"}) or document["schema"] != "inferlab.signer-handoff-live-rejections.v0.29":
        return False
    cases = document["cases"]
    if (
        not isinstance(cases, list)
        or len(cases) != len(LIVE_CASES)
        or [item.get("scenario") for item in cases] != list(LIVE_CASES)
    ):
        return False
    common_service = cases[0].get("service_id") if isinstance(cases[0], dict) else None
    if common_service not in CONTROLS:
        return False
    common_process = process_map.get(common_service, {})
    for index, (scenario, kind) in enumerate(LIVE_CASES.items()):
        item = cases[index]
        if (
            not exact_keys(item, {
                "scenario", "expected_error_kind", "service_id", "pid", "start_token",
                "before", "rejected", "recovered",
            })
            or item["expected_error_kind"] != kind
            or item["service_id"] != common_service
            or not exact_int(item["pid"])
            or item["pid"] != common_process.get("pid")
            or item["start_token"] != common_process.get("start_token")
        ):
            return False
        before, rejected, recovered = item["before"], item["rejected"], item["recovered"]
        expected_credential = "key-b" if scenario in {"stale", "fork"} else "key-a"
        expected_generation = 2 if scenario in {"stale", "fork"} else 1
        if not exact_signing(before, item["service_id"], expected_credential, expected_generation):
            return False
        if (
            not exact_signing(
                rejected, item["service_id"], expected_credential, expected_generation, kind
            )
            or not exact_signing(
                recovered, item["service_id"], expected_credential, expected_generation
            )
            or before.get("rejected_reloads") != index
            or rejected.get("rejected_reloads") != index + 1
            or recovered.get("rejected_reloads") != index + 1
        ):
            return False
    return True


def live_rejection_diagnostics(
    document: Any, process_map: dict[str, dict[str, Any]]
) -> list[str]:
    tags: list[str] = []
    if not exact_keys(document, {"schema", "cases"}):
        return ["top-schema"]
    if document["schema"] != "inferlab.signer-handoff-live-rejections.v0.29":
        return ["schema-value"]
    cases = document["cases"]
    if not isinstance(cases, list) or len(cases) != len(LIVE_CASES):
        return ["case-count"]
    if [item.get("scenario") for item in cases if isinstance(item, dict)] != list(LIVE_CASES):
        tags.append("scenario-order")
    common_service = cases[0].get("service_id") if isinstance(cases[0], dict) else None
    if common_service not in CONTROLS:
        tags.append("common-service")
    common_process = process_map.get(common_service, {})
    for index, (scenario, kind) in enumerate(LIVE_CASES.items()):
        item = cases[index]
        prefix = f"case-{index}"
        if not exact_keys(item, {
            "scenario", "expected_error_kind", "service_id", "pid", "start_token",
            "before", "rejected", "recovered",
        }):
            tags.append(f"{prefix}-schema")
            continue
        if item["scenario"] != scenario or item["expected_error_kind"] != kind:
            tags.append(f"{prefix}-identity")
        if item["service_id"] != common_service:
            tags.append(f"{prefix}-service")
        if not exact_int(item["pid"]) or item["pid"] != common_process.get("pid"):
            tags.append(f"{prefix}-pid")
        if item["start_token"] != common_process.get("start_token"):
            tags.append(f"{prefix}-start")
        expected_credential = "key-b" if scenario in {"stale", "fork"} else "key-a"
        expected_generation = 2 if scenario in {"stale", "fork"} else 1
        if not exact_signing(item["before"], item["service_id"], expected_credential, expected_generation):
            tags.append(f"{prefix}-before")
        if not exact_signing(item["rejected"], item["service_id"], expected_credential, expected_generation, kind):
            tags.append(f"{prefix}-rejected")
        if not exact_signing(item["recovered"], item["service_id"], expected_credential, expected_generation):
            tags.append(f"{prefix}-recovered")
        states = (item["before"], item["rejected"], item["recovered"])
        if not all(isinstance(state, dict) for state in states) or (
            states[0].get("rejected_reloads") != index
            or states[1].get("rejected_reloads") != index + 1
            or states[2].get("rejected_reloads") != index + 1
        ):
            tags.append(f"{prefix}-counter")
    return sorted(set(tags))


def production_tests(document: Any) -> bool:
    if not exact_keys(document, {"schema", "test_count", "tests"}) or document["schema"] != "inferlab.signer-handoff-production-tests.v0.29":
        return False
    tests = document["tests"]
    if not exact_int(document["test_count"], len(PRODUCTION_FILTERS)) or not isinstance(tests, list) or len(tests) != len(PRODUCTION_FILTERS):
        return False
    if (
        {item.get("test_filter") for item in tests} != PRODUCTION_FILTERS
        or [item.get("test_filter") for item in tests] != list(PRODUCTION_TESTS)
    ):
        return False
    for item in tests:
        if not exact_keys(item, {
            "package", "test_filter", "command", "environment", "exit_code",
            "summary_line", "output_lines",
        }):
            return False
        package = item.get("package")
        test_filter = item.get("test_filter")
        expected_package, target_arguments = PRODUCTION_TESTS.get(test_filter, (None, []))
        expected = ["cargo", "test", "--locked", "-p", package, *target_arguments]
        expected.extend([test_filter, "--", "--exact"])
        summary = item.get("summary_line")
        lines = item.get("output_lines")
        if (
            package != expected_package
            or item.get("command") != expected
            or item.get("environment") != {"CARGO_TERM_COLOR": "never"}
            or not exact_int(item.get("exit_code"), 0)
            or lines != ["running 1 test", f"test {test_filter} ... ok", summary]
            or not isinstance(summary, str)
            or not re.fullmatch(r"test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; [0-9]+ filtered out; finished in [0-9]+(?:\.[0-9]+)?s", summary)
        ):
            return False
    return True


def exact_contract(document: Any, expected: dict[str, Any]) -> bool:
    return (
        document == expected
        and exact_keys(document, set(expected))
        and all(exact_int(value) for value in document["bundle_generations"])
        and all(exact_int(value) for value in document["trust_generations"])
        and all(exact_int(value, expected["ports"][key]) for key, value in document["ports"].items())
        and all(
            exact_int(value, expected["startup_ports"][key])
            for key, value in document["startup_ports"].items()
        )
    )


def exact_manifest(manifest: Any, directory: Path) -> bool:
    if not exact_keys(manifest, {"schema", "file_count", "files"}) or manifest["schema"] != "inferlab.signer-handoff-manifest.v0.29":
        return False
    entries = list(directory.iterdir())
    if (
        {path.name for path in entries} != EXPECTED_FILES
        or any(not path.is_file() or path.is_symlink() for path in entries)
    ):
        return False
    files = manifest["files"]
    expected_names = sorted(NON_MANIFEST_FILES)
    if not exact_int(manifest["file_count"], len(expected_names)) or not isinstance(files, list) or len(files) != len(expected_names):
        return False
    if [item.get("name") for item in files] != expected_names:
        return False
    for item in files:
        if not exact_keys(item, {"name", "bytes", "sha256"}) or not exact_int(item["bytes"]):
            return False
        path = directory / item["name"]
        if not path.is_file() or path.stat().st_size != item["bytes"]:
            return False
        if hashlib.sha256(path.read_bytes()).hexdigest() != item["sha256"]:
            return False
    return safe_text((directory / "manifest.json").read_text(encoding="utf-8"))


def hardening_self_test() -> bool:
    # Critical Python equality pitfalls and crypto tampering are covered without
    # depending on a retained bundle being present.
    if exact_int(True) or verify_ed25519("x", b"x", "x"):
        return False
    try:
        json.loads(
            '{"cluster_id":"shadow","cluster_id":"inferlab-primary"}',
            object_pairs_hook=reject_duplicate_json_keys,
        )
    except ValueError:
        pass
    else:
        return False
    identity = base64.b64encode(bytes([1]) + bytes(31)).decode()
    forged = base64.b64encode(bytes([1]) + bytes(63)).decode()
    if verify_ed25519(identity, b"forged", forged):
        return False
    noncanonical_identity = base64.b64encode(bytes([1]) + bytes(30) + bytes([128])).decode()
    noncanonical_forged = base64.b64encode(
        bytes([1]) + bytes(30) + bytes([128]) + bytes(32)
    ).decode()
    if verify_ed25519(noncanonical_identity, b"forged", noncanonical_forged):
        return False
    rfc_public = base64.b64encode(bytes.fromhex(
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
    )).decode()
    rfc_signature = base64.b64encode(bytes.fromhex(
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155"
        "5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
    )).decode()
    if not verify_ed25519(rfc_public, b"", rfc_signature) or verify_ed25519(rfc_public, b"x", rfc_signature):
        return False
    fixture = {
        "label": "control-a", "pid": 7, "ppid": 1, "state": "S",
        "start_token": "Mon Aug 10 10:20:30 2026", "command": "control-plane",
    }
    if not exact_process(fixture, "control-a"):
        return False
    fixture["pid"] = False
    if exact_process(fixture, "control-a"):
        return False
    return not safe_text("-----BEGIN PRIVATE KEY-----") and not safe_text("/Users/example")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--require-manifest", action="store_true")
    args = parser.parse_args()
    directory = args.evidence_dir
    require_exact_inventory(directory, args.require_manifest)

    contract = load(directory, "proof-contract.json")
    generations = load(directory, "trust-generations.json")
    publish_one = load(directory, "publish-g1.json")
    publish_two = load(directory, "publish-g2.json")
    write_two = load(directory, "r2-write.json")
    write_three = load(directory, "r3-write.json")
    startup = load(directory, "startup-rejections.json")
    initial = load(directory, "generation-1-controls.json")
    receipts_one = load(directory, "generation-1-receipts.json")
    handoff = load(directory, "handoff-sequence.json")
    post_handoff = load(directory, "generation-1-after-handoff.json")
    live = load(directory, "live-source-rejections.json")
    controls_two = load(directory, "generation-2-controls.json")
    receipts_two = load(directory, "generation-2-receipts.json")
    attacks = load(directory, "revoked-a-attacks.json")
    final_controls = load(directory, "final-cluster.json")
    final_gateway = load(directory, "final-gateway.json")
    gateway_two = load(directory, "gateway-r2.json")
    final_json = load(directory, "final-json.json")
    final_sse = load(directory, "final-sse.json")
    continuity = load(directory, "process-continuity.json")
    tests = load(directory, "production-tests.json")
    logs = load(directory, "discarded-log-scan.json")
    sanitizer = load(directory, "sanitizer.json")
    private_scan = load(directory, "private-material-scan.json")
    direct = direct_scan(directory)

    expected_contract = {
        "schema": "inferlab.signer-handoff-proof-contract.v0.29",
        "cluster_id": "inferlab-primary",
        "services": SERVICES,
        "controls": CONTROLS,
        "credentials": ["key-a", "key-b"],
        "bundle_generations": [1, 2],
        "trust_generations": [1, 2],
        "expected_receiver_mode": "service-id",
        "processes": sorted(PROCESS_BINARIES),
        "ports": {
            "gateway": 12080, "control-a": 12081, "control-b": 12082,
            "control-c": 12083, "cpu-worker": 12084, "trust-distributor": 12085,
        },
        "startup_ports": {
            scenario: 12180 + index for index, scenario in enumerate(STARTUP_CASES)
        },
        "handoff_order": "follower,follower,leader,gateway",
    }

    handoff_ok = False
    handoff_traffic_ok = False
    validated_handoff_steps: list[dict[str, Any]] = []
    if exact_keys(handoff, {"schema", "initial_processes", "steps"}) and handoff["schema"] == "inferlab.signer-handoff-sequence.v0.29":
        steps = handoff["steps"]
        initial_processes = handoff["initial_processes"]
        if (
            exact_process_set(initial_processes)
            and isinstance(steps, list)
            and len(steps) == 4
            and controls_document(initial, "key-a", 1, 1, 2)
        ):
            initial_map = {item["label"]: item for item in initial_processes}
            baseline_cluster = initial["result"]
            previous_control_state = {
                item["node_id"]: item for item in baseline_cluster["controls"]
            }
            continuity_initial_map = {
                item.get("label"): item for item in continuity.get("initial", [])
                if isinstance(item, dict)
            }
            if (
                set(continuity_initial_map) != set(initial_map)
                or any(
                    process_identity(initial_map[label]) != process_identity(continuity_initial_map[label])
                    for label in initial_map
                )
            ):
                steps = []
            expected_order = ["follower", "follower", "leader", "gateway"]
            seen_controls = set()
            valid = True
            for index, step in enumerate(steps):
                if not exact_keys(step, {"index", "role_at_handoff", "service_id", "cluster", "gateway", "processes"}):
                    valid = False
                    break
                if not exact_int(step["index"], index + 1) or step["role_at_handoff"] != expected_order[index]:
                    valid = False
                    break
                service = step["service_id"]
                if index < 3:
                    if service not in CONTROLS or service in seen_controls:
                        valid = False
                        break
                    cluster_controls = step["cluster"].get("result", {}).get("controls", [])
                    selected = next(
                        (item for item in cluster_controls if item.get("node_id") == service),
                        None,
                    )
                    if selected is None or selected.get("role") != step["role_at_handoff"]:
                        valid = False
                        break
                    seen_controls.add(service)
                elif service != "gateway-primary":
                    valid = False
                if not exact_process_set(step["processes"]):
                    valid = False
                    break
                current_map = {item["label"]: item for item in step["processes"]}
                if any(process_identity(current_map[label]) != process_identity(initial_map[label]) for label in initial_map):
                    valid = False
                    break
                cluster_controls = step["cluster"].get("result", {}).get("controls", [])
                active = {item.get("node_id"): item.get("service_signing", {}).get("active_credential_id") for item in cluster_controls}
                expected_b = seen_controls
                if {node for node, credential in active.items() if credential == "key-b"} != expected_b:
                    valid = False
                    break
                expected_credentials = {
                    node: "key-b" if node in expected_b else "key-a"
                    for node in CONTROLS
                }
                if not mixed_controls_document(step["cluster"], expected_credentials, 1, 2):
                    valid = False
                    break
                step_result = step["cluster"]["result"]
                if (
                    step_result["leader_id"] != baseline_cluster["leader_id"]
                    or step_result["term"] != baseline_cluster["term"]
                ):
                    valid = False
                    break
                current_control_state = {
                    item["node_id"]: item for item in step_result["controls"]
                }
                for node in CONTROLS:
                    if any(
                        current_control_state[node][field] < previous_control_state[node][field]
                        for field in ("commit_index", "last_applied", "last_log_index")
                    ):
                        valid = False
                        break
                if not valid:
                    break
                previous_control_state = current_control_state
                expected_gateway_credential = "key-b" if index == 3 else "key-a"
                expected_gateway_generation = 2 if index == 3 else 1
                if not exact_gateway_document(
                    step["gateway"], expected_gateway_credential, expected_gateway_generation, 2
                ):
                    valid = False
                    break
                if step["gateway"]["result"]["body"]["routing_snapshot"]["control_term"] != baseline_cluster["term"]:
                    valid = False
                    break
            handoff_ok = valid and seen_controls == set(CONTROLS)
            if handoff_ok:
                validated_handoff_steps = steps
            if handoff_ok and controls_document(initial, "key-a", 1, 1, 2):
                before_controls = {
                    item["node_id"]: item["service_authentication"]
                    for item in initial["result"]["controls"]
                }
                after_controls = {
                    item["node_id"]: item["service_authentication"]
                    for item in steps[-1]["cluster"]["result"]["controls"]
                }
                counters = ("verifications", "authorized_peer_rpcs", "authorized_gateway_reads")
                handoff_traffic_ok = (
                    set(before_controls) == set(CONTROLS)
                    and set(after_controls) == set(CONTROLS)
                    and all(
                        after_controls[service][counter] >= before_controls[service][counter]
                        for service in CONTROLS for counter in counters
                    )
                    and sum(after_controls[service]["authorized_peer_rpcs"] for service in CONTROLS)
                    > sum(before_controls[service]["authorized_peer_rpcs"] for service in CONTROLS)
                    and sum(after_controls[service]["authorized_gateway_reads"] for service in CONTROLS)
                    > sum(before_controls[service]["authorized_gateway_reads"] for service in CONTROLS)
                    and sum(after_controls[service]["verifications"] for service in CONTROLS)
                    > sum(before_controls[service]["verifications"] for service in CONTROLS)
                )

    g1_after_ok = receipt_document(post_handoff, generations, 1, "key-a")
    if g1_after_ok:
        before_receipts = receipts_one["result"]["body"]["receipts"]
        after_receipts = post_handoff["result"]["body"]["receipts"]
        step_times = [
            (step["cluster"]["observed_at_ms"], step["gateway"]["observed_at_ms"])
            for step in validated_handoff_steps
        ]
        g1_after_ok = (
            before_receipts == after_receipts
            and len(step_times) == 4
            and receipts_one["observed_at_ms"] < step_times[0][0]
            and all(cluster_time <= gateway_time for cluster_time, gateway_time in step_times)
            and all(step_times[index][1] < step_times[index + 1][0] for index in range(3))
            and step_times[-1][1] < post_handoff["observed_at_ms"]
        )

    continuity_process_map = {
        item["label"]: item for item in continuity.get("initial", []) if isinstance(item, dict)
    }
    live_ok = live_rejections(live, continuity_process_map)
    live_diagnostics = live_rejection_diagnostics(live, continuity_process_map)
    if live_ok:
        live_ok = (
            len(validated_handoff_steps) == 4
            and live["cases"][0]["service_id"] == validated_handoff_steps[0]["service_id"]
        )
        if not live_ok:
            live_diagnostics = ["handoff-service"]

    quorum_continuity_ok = (
        controls_document(initial, "key-a", 1, 1, 2)
        and controls_document(controls_two, "key-b", 2, 2, 2)
        and controls_document(final_controls, "key-b", 2, 2, 3)
        and exact_gateway_document(gateway_two, "key-a", 1, 2)
        and exact_gateway_document(final_gateway, "key-b", 2, 3)
    )
    if quorum_continuity_ok:
        cluster_documents = [initial, controls_two, final_controls]
        leader_ids = [document["result"]["leader_id"] for document in cluster_documents]
        terms = [document["result"]["term"] for document in cluster_documents]
        by_generation = [
            {item["node_id"]: item for item in document["result"]["controls"]}
            for document in cluster_documents
        ]
        quorum_continuity_ok = (
            len(set(leader_ids)) == 1
            and len(set(terms)) == 1
            and gateway_two["result"]["body"]["routing_snapshot"]["control_term"] == terms[0]
            and final_gateway["result"]["body"]["routing_snapshot"]["control_term"] == terms[0]
            and all(
                by_generation[index + 1][node][field] >= by_generation[index][node][field]
                for index in range(2)
                for node in CONTROLS
                for field in ("commit_index", "last_applied", "last_log_index")
            )
        )

    attack_ok = False
    if exact_keys(attacks, {"schema", "gateway_old_a", "peer_old_a", "revoked_bundle", "valid_b_read"}) and attacks["schema"] == "inferlab.signer-handoff-revoked-a.v0.29":
        gateway_attack = attacks["gateway_old_a"]
        peer_attack = attacks["peer_old_a"]
        rejected_bundle = attacks["revoked_bundle"]
        positive = attacks["valid_b_read"]
        gateway_before = gateway_attack.get("before")
        gateway_after = gateway_attack.get("after")
        peer_before = peer_attack.get("before_mutation")
        peer_after = peer_attack.get("after_mutation")
        gateway_response = gateway_attack.get("response")
        peer_response = peer_attack.get("response")
        attack_ok = (
            exact_keys(gateway_attack, {"service_id", "credential_id", "before", "response", "after"})
            and gateway_attack["service_id"] == "gateway-primary"
            and gateway_attack["credential_id"] == "key-a"
            and exact_auth_guard(gateway_before)
            and exact_auth_guard(gateway_after)
            and exact_revoked_error(gateway_response, "GET", "/v1/control/config", "gateway-primary")
            and {key: gateway_before[key] for key in ("term", "revision", "commit_index", "last_applied", "last_log_index")}
            == {key: gateway_after[key] for key in ("term", "revision", "commit_index", "last_applied", "last_log_index")}
            and gateway_after["authentication_rejections"] == gateway_before["authentication_rejections"] + 1
            and gateway_after["credential_revocation_rejections"] == gateway_before["credential_revocation_rejections"] + 1
            and exact_keys(peer_attack, {
                "service_id", "credential_id", "candidate_id", "high_term", "request",
                "before_mutation", "response", "after_mutation",
            })
            and peer_attack["service_id"] in CONTROLS
            and peer_attack["candidate_id"] == peer_attack["service_id"]
            and peer_attack["credential_id"] == "key-a"
            and exact_int(peer_attack["high_term"])
            and peer_attack["request"] == {
                "cluster_id": "inferlab-primary",
                "term": peer_attack["high_term"],
                "candidate_id": peer_attack["service_id"],
                "last_log_index": 0,
                "last_log_term": 0,
            }
            and exact_auth_guard(peer_before)
            and exact_auth_guard(peer_after)
            and peer_attack["high_term"] > peer_before["term"]
            and exact_revoked_error(peer_response, "POST", "/raft/request-vote", peer_attack["service_id"])
            and {key: peer_before[key] for key in ("term", "revision", "commit_index", "last_applied", "last_log_index")}
            == {key: peer_after[key] for key in ("term", "revision", "commit_index", "last_applied", "last_log_index")}
            and peer_after["authentication_rejections"] == peer_before["authentication_rejections"] + 1
            and peer_after["credential_revocation_rejections"] == peer_before["credential_revocation_rejections"] + 1
            and exact_keys(rejected_bundle, {
                "service_id", "pid", "start_token", "expected_error_kind", "candidate",
                "before", "after", "recovered",
            })
            and rejected_bundle["service_id"] in CONTROLS
            and exact_int(rejected_bundle["pid"])
            and rejected_bundle["pid"] == next(
                (item.get("pid") for item in continuity.get("initial", []) if item.get("label") == rejected_bundle["service_id"]),
                None,
            )
            and rejected_bundle["start_token"] == next(
                (item.get("start_token") for item in continuity.get("initial", []) if item.get("label") == rejected_bundle["service_id"]),
                None,
            )
            and rejected_bundle.get("expected_error_kind") == "candidate_rejected"
            and rejected_bundle.get("candidate") == {
                "schema": "inferlab.service-signing-bundle.v1",
                "cluster_id": "inferlab-primary",
                "service_id": rejected_bundle["service_id"],
                "bundle_generation": 3,
                "active_credential_id": "key-a",
                "configured_credential_ids": ["key-a", "key-b"],
                "configured_credential_count": 2,
            }
            and exact_signing(rejected_bundle.get("before"), rejected_bundle["service_id"], "key-b", 2)
            and exact_signing(
                rejected_bundle.get("after"), rejected_bundle["service_id"], "key-b", 2, "candidate_rejected"
            )
            and rejected_bundle.get("after", {}).get("rejected_reloads") == rejected_bundle.get("before", {}).get("rejected_reloads") + 1
            and exact_signing(rejected_bundle.get("recovered"), rejected_bundle["service_id"], "key-b", 2)
            and rejected_bundle.get("recovered", {}).get("rejected_reloads") == rejected_bundle.get("after", {}).get("rejected_reloads")
            and exact_capture(positive, "GET", "/v1/control/config", 200)
            and positive["headers"] == {}
            and exact_control_config_body(positive["body"], 2)
        )

    json_observation = final_json.get("observation", {})
    json_ok = exact_json_completion(final_json)
    json_diagnostics = json_completion_diagnostics(final_json)
    pieces = final_sse.get("pieces")
    sse_ok = exact_sse_completion(final_sse)
    sse_diagnostics = sse_completion_diagnostics(final_sse)
    direct_diagnostics = []
    for problem in direct["problems"]:
        name = problem.get("file")
        if name not in NON_MANIFEST_FILES:
            direct_diagnostics.append("inventory")
            continue
        kind = "field" if problem.get("sensitive_paths") else "text"
        direct_diagnostics.append(f"{name}:{kind}")
    direct_diagnostics = sorted(set(direct_diagnostics))

    continuity_ok = False
    if exact_keys(continuity, {"schema", "proof_shell_pid", "initial", "final", "unchanged"}) and continuity["schema"] == "inferlab.signer-handoff-process-continuity.v0.29":
        continuity_ok = (
            exact_int(continuity["proof_shell_pid"])
            and continuity["proof_shell_pid"] > 1
            and exact_process_set(continuity["initial"])
            and exact_process_set(continuity["final"])
            and {item["ppid"] for item in continuity["initial"]} == {continuity["proof_shell_pid"]}
            and {item["ppid"] for item in continuity["final"]} == {continuity["proof_shell_pid"]}
            and continuity["unchanged"] is True
            and [process_identity(item) for item in continuity["initial"]]
            == [process_identity(item) for item in continuity["final"]]
        )

    manifest_present = (directory / "manifest.json").is_file()
    manifest_ok = exact_manifest(load(directory, "manifest.json"), directory) if manifest_present else not args.require_manifest

    checks = [
        assertion("proof contract fixes six processes, four services and isolated ports", exact_contract(contract, expected_contract)),
        assertion("g1 and g2 are exact root-signed overlap and revocation policies", exact_trust_generations(generations)),
        assertion(
            "distributor publishes exact trust generations one and two",
            exact_publish(publish_one, 1)
            and exact_publish(publish_two, 2)
            and published_etag(publish_one) == distributor_etag(receipts_one)
            and published_etag(publish_two) == distributor_etag(receipts_two),
        ),
        assertion("invalid startup bundles fail closed before a listener or state file", startup_matrix(startup)),
        assertion("authorized writes commit exact routing revisions two and three", exact_write(write_two, "r2", 2, "round-robin") and exact_write(write_three, "r3", 3, "least-in-flight")),
        assertion("generation one starts on A with one healthy Raft leader and revision two", controls_document(initial, "key-a", 1, 1, 2)),
        assertion("generation one converges by service ID with three cryptographic A receipts", receipt_document(receipts_one, generations, 1, "key-a")),
        assertion("gateway begins at bundle generation one on A and revision two", exact_gateway_document(gateway_two, "key-a", 1, 2)),
        assertion("every deterministic live source failure retains and recovers the exact LKG", live_ok),
        assertion("sequential follower follower leader gateway handoff keeps exact process identities", handoff_ok),
        assertion("authenticated peer and gateway traffic continues through the sequential handoff", handoff_traffic_ok),
        assertion("signer-only handoff leaves all generation-one A receipts byte-identical", g1_after_ok),
        assertion("generation two activates B and preserves revision-two quorum", controls_document(controls_two, "key-b", 2, 2, 2)),
        assertion("generation two converges by service ID with three cryptographic B receipts", receipt_document(receipts_two, generations, 2, "key-b")),
        assertion("revoked A cannot read vote or reactivate while valid B still reads", attack_ok),
        assertion("revision three remains a healthy B-authenticated three-control commit", controls_document(final_controls, "key-b", 2, 2, 3)),
        assertion("one leader term and monotonic quorum state span g1 handoff g2 and r3", quorum_continuity_ok),
        assertion("final gateway is bundle generation two on B and routing revision three", exact_gateway_document(final_gateway, "key-b", 2, 3)),
        assertion("real CPU JSON completes through revision three with one attempt", json_ok, {"duration_ms": json_observation.get("duration_ms")}),
        assertion("real CPU SSE is incremental and ends with DONE followed by EOF", sse_ok, {"duration_ms": final_sse.get("duration_ms"), "piece_count": len(pieces) if isinstance(pieces, list) else None}),
        assertion("all exact nonce handoff LKG watcher and convergence regressions run one test", production_tests(tests)),
        assertion("six owned process identities are unchanged from A through B and r3", continuity_ok),
        assertion("discarded runtime logs passed the exact secret path and private scan", exact_log_scan(logs)),
        assertion("sanitizer scanned the exact non-derived evidence inventory", exact_sanitizer(sanitizer)),
        assertion("offline private-material scan found no deterministic seed representation", exact_private_scan(private_scan)),
        assertion(
            "checker directly finds no host path private value prompt nonce or sensitive field",
            direct["files_scanned"] == sorted(NON_MANIFEST_FILES) and direct["problems"] == [],
            {"files_scanned": direct["files_scanned"]},
        ),
        assertion("hardening self-tests reject bool PID secret path and invalid signatures", hardening_self_test()),
        assertion("manifest is exact hash size schema bound when required", manifest_ok),
    ]
    report = {
        "schema": "inferlab.signer-handoff-assertions.v0.29",
        "assertions": checks,
        "passed": sum(item["passed"] for item in checks),
        "failed": sum(not item["passed"] for item in checks),
        "total": len(checks),
    }
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    if report["failed"]:
        if not live_ok:
            print("DIAGNOSTIC live=" + ",".join(live_diagnostics), file=sys.stderr)
        if not json_ok:
            print("DIAGNOSTIC json=" + ",".join(json_diagnostics), file=sys.stderr)
        if not sse_ok:
            print("DIAGNOSTIC sse=" + ",".join(sse_diagnostics), file=sys.stderr)
        if direct["files_scanned"] != sorted(NON_MANIFEST_FILES) or direct["problems"]:
            print("DIAGNOSTIC direct=" + ",".join(direct_diagnostics or ["inventory"]), file=sys.stderr)
        for item in checks:
            if not item["passed"]:
                print(f"FAILED: {item['name']}", file=sys.stderr)
        raise SystemExit(1)


if __name__ == "__main__":
    try:
        main()
    except EvidenceError as error:
        print(f"invalid v0.29 evidence: {error}", file=sys.stderr)
        raise SystemExit(1) from None
