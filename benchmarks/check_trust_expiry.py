#!/usr/bin/env python3
"""Deterministically evaluate retained v0.27 trust-expiry evidence."""

from __future__ import annotations

import argparse
import base64
import binascii
import hashlib
import json
import math
import re
from pathlib import Path
from typing import Any


CONTROL_IDS = {"control-a", "control-b", "control-c"}
EXPECTED_RECEIVERS = ["control-a/key-a", "control-b/key-a", "control-c/key-a"]
POLICY_V2 = "inferlab.service-trust-policy.v2"
POLICY_AUTH_V2 = "inferlab.service-trust-authentication.v2"
RECEIPT_SCHEMA = "inferlab.service-trust-receipt.v1"
RECEIPT_AUTH_SCHEMA = "inferlab.service-trust-receipt-authentication.v1"
VALIDITY_TESTS = {
    "service_authentication::tests::signed_policy_expiry_is_exclusive_latched_and_recovers_on_higher_generation",
    "service_trust::tests::post_persist_expiry_advances_floor_without_activation_or_rollback",
    "service_trust::tests::local_future_issued_snapshot_is_retried_when_unchanged_bytes_become_eligible",
    "service_trust::tests::unchanged_304_does_not_renew_expiry_and_valid_higher_generation_recovers",
    "service_trust::tests::unchanged_local_poll_latches_expiry_against_backward_clock",
    "service_trust::tests::remote_post_persist_expiry_advances_floor_without_activation_or_receipt",
    "service_trust::tests::remote_etag_update_failure_and_receipt_paths_keep_last_known_good",
}
EXPECTED_EVIDENCE_FILES = {
    "assertions.json",
    "distributor-outage.json",
    "durable-after-candidate-attacks.json",
    "durable-expired-generation-1.json",
    "durable-generation-1.json",
    "durable-generation-2.json",
    "excessive-lifetime-startup.json",
    "expired-cache-restart.json",
    "expired-controls.json",
    "expiry-tamper.json",
    "final-cluster.json",
    "final-gateway.json",
    "final-request.json",
    "final-stream.json",
    "future-issued-startup.json",
    "gateway-ready.json",
    "generation-1-controls.json",
    "generation-1-receipts.json",
    "generation-1-request.json",
    "generation-2-controls.json",
    "generation-2-receipts.json",
    "initial-cluster.json",
    "legacy-v1-startup.json",
    "malformed-window.json",
    "manifest.json",
    "not-modified-does-not-renew.json",
    "post-candidate-attacks.json",
    "pre-expiry-controls.json",
    "private-material-scan.json",
    "process-continuity.json",
    "production-validity-tests.json",
    "publish-g1.json",
    "publish-g2.json",
    "request-time-cutoff-and-admitted-stream.json",
    "same-generation-deadline-fork.json",
    "sanitizer.json",
    "trust-expiry-proof.svg",
    "write-committed.json",
}
DERIVED_EVIDENCE_FILES = {
    "assertions.json",
    "manifest.json",
    "private-material-scan.json",
    "sanitizer.json",
    "trust-expiry-proof.svg",
}
SANITIZER_INPUT_FILES = sorted(EXPECTED_EVIDENCE_FILES - DERIVED_EVIDENCE_FILES)
PRIVATE_SCAN_PRELIMINARY_FILES = sorted(
    set(SANITIZER_INPUT_FILES) | {"sanitizer.json"}
)
PRIVATE_SCAN_FINAL_FILES = sorted(EXPECTED_EVIDENCE_FILES - {"manifest.json"})
CARGO_ONE_TEST_SUMMARY = re.compile(
    r"^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; "
    r"[0-9]+ filtered out; finished in [0-9]+(?:\.[0-9]+)?s$"
)
HOST_PATH = re.compile(
    r"(?:/Users|/home|/private/var|/var/folders|/tmp|/workspace|/workspaces|"
    r"/github/workspace)/[^\s\"'<>]+"
)
PRIVATE_MARKERS = ("-----BEGIN", "PRIVATE KEY", "CERTIFICATE-----")
KNOWN_ED25519_SEEDS = (
    ("route_seed", "TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs="),
    ("writer_seed", "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A="),
    ("trust_root_seed", "BQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQU="),
    ("control_a_seed", "TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs="),
    ("control_b_seed", "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A="),
    ("control_c_seed", "xaqN9D+fg3vtt0QvMdy3sWbThTUHbwlLhc46LgtEWPc="),
    ("gateway_seed", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="),
)
EXPIRED_STARTUP_DIAGNOSTIC = (
    "service-trust policy validity rejected (expired): "
    "service-trust policy is expired"
)
STARTUP_FAILURE_DIAGNOSTICS = {
    "issued_in_future": (
        'Error: Custom { kind: InvalidData, error: "service-trust policy validity '
        'rejected (issued_in_future): service-trust policy issue time exceeds the '
        'configured future-skew allowance of 250 ms" }'
    ),
    "lifetime_exceeded": (
        'Error: Custom { kind: InvalidData, error: "service-trust policy validity '
        'rejected (lifetime_exceeded): service-trust policy lifetime exceeds the '
        'configured 45000 ms maximum" }'
    ),
    "legacy_v1_disallowed": (
        'Error: Custom { kind: InvalidData, error: "service-trust policy validity '
        'rejected (legacy_v1_disallowed): legacy non-expiring service-trust policy '
        'v1 is disabled" }'
    ),
}
SHA256_HEX = re.compile(r"^[0-9a-f]{64}$")
LSTART = re.compile(
    r"^(?:Mon|Tue|Wed|Thu|Fri|Sat|Sun) "
    r"(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) +"
    r"(?:[1-9]|[12][0-9]|3[01]) (?:[01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9] "
    r"[0-9]{4}$"
)
TEACHING_PIECES = ["InferLab", " turns", " prompts", " into", " real", " tokens", "."]


def load(directory: Path, name: str) -> dict[str, Any]:
    with (directory / name).open(encoding="utf-8") as source:
        document = json.load(source)
    if not isinstance(document, dict):
        raise SystemExit(f"{name} must contain one JSON object")
    return document


def cargo_test_output_fields(test_filter: str, output: Any) -> dict[str, Any]:
    if not isinstance(output, str):
        return {
            "running_one_test": False,
            "exact_test_line": False,
            "exact_summary": False,
            "summary_line": None,
        }
    lines = output.splitlines()
    summaries = [line for line in lines if CARGO_ONE_TEST_SUMMARY.fullmatch(line)]
    return {
        "running_one_test": lines.count("running 1 test") == 1,
        "exact_test_line": lines.count(f"test {test_filter} ... ok") == 1,
        "exact_summary": len(summaries) == 1,
        "summary_line": summaries[0] if len(summaries) == 1 else None,
    }


def exact_cargo_test_item(item: dict[str, Any]) -> bool:
    test_filter = item.get("test_filter")
    if not isinstance(test_filter, str):
        return False
    parsed = cargo_test_output_fields(test_filter, item.get("output"))
    return (
        exact_int(item.get("exit_code"), 0)
        and item.get("environment") == {"CARGO_TERM_COLOR": "never"}
        and item.get("command")
        == [
            "cargo",
            "test",
            "-p",
            "control-plane",
            "--lib",
            test_filter,
            "--",
            "--exact",
        ]
        and item.get("running_one_test") is True
        and item.get("exact_test_line") is True
        and item.get("exact_summary") is True
        and item.get("summary_line") == parsed["summary_line"]
        and all(item.get(field) == parsed[field] for field in parsed)
    )


def startup_diagnostic_observed(kind: str, excerpt: Any) -> bool:
    expected = STARTUP_FAILURE_DIAGNOSTICS.get(kind)
    return (
        isinstance(expected, str)
        and isinstance(excerpt, str)
        and excerpt.splitlines().count(expected) == 1
    )


def exact_sha256(value: Any) -> bool:
    return isinstance(value, str) and SHA256_HEX.fullmatch(value) is not None


def exact_int(value: Any, expected: int | None = None) -> bool:
    return type(value) is int and (expected is None or value == expected)


def positive_pid(value: Any) -> bool:
    return exact_int(value) and value > 0


def unique_positive_pids(values: list[Any], expected_count: int) -> bool:
    return (
        len(values) == expected_count
        and all(positive_pid(value) for value in values)
        and len(set(values)) == expected_count
    )


def exact_process_command(value: Any, binary: str) -> bool:
    return value == f"target/debug/{binary}"


def exact_start_token(value: Any) -> bool:
    return isinstance(value, str) and LSTART.fullmatch(value) is not None


def positive_finite(value: Any) -> bool:
    return (
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(float(value))
        and float(value) > 0
    )


def valid_observed_interval(observation: dict[str, Any]) -> bool:
    started = observation.get("started_at_ms")
    completed = observation.get("completed_at_ms")
    duration = observation.get("duration_ms")
    return (
        type(started) is int
        and started > 0
        and type(completed) is int
        and completed >= started
        and positive_finite(duration)
        and abs((completed - started) - float(duration)) <= 50.0
    )


def exact_teaching_pieces(value: Any) -> bool:
    return value == TEACHING_PIECES


def conditional_etags_are_stable(
    initial_etag: Any, conditional_etag: Any, reported_stable: Any
) -> bool:
    return (
        isinstance(initial_etag, str)
        and bool(initial_etag)
        and (conditional_etag is None or conditional_etag == initial_etag)
        and reported_stable is True
    )


def failed_restart_is_not_current(
    failed_pid: Any,
    restart_evidence_pid: Any,
    current_pids: list[Any],
    reported_not_live: Any,
) -> bool:
    return (
        type(failed_pid) is int
        and failed_pid > 0
        and type(restart_evidence_pid) is int
        and failed_pid == restart_evidence_pid
        and all(type(pid) is int and pid > 0 for pid in current_pids)
        and failed_pid not in current_pids
        and reported_not_live is True
    )


def retained_text_is_safe(text: str) -> bool:
    return not HOST_PATH.search(text) and not any(
        marker in text for marker in PRIVATE_MARKERS
    )


def compact_private_material(value: str) -> str:
    return "".join(value.replace("\\n", "").replace("\\r", "").split())


def known_seed_labels_in_text(text: str) -> list[str]:
    compacted = compact_private_material(text)
    labels = []
    for label, encoded in KNOWN_ED25519_SEEDS:
        candidate = compact_private_material(encoded)
        if candidate in compacted or candidate.rstrip("=") in compacted:
            labels.append(label)
    return labels


def direct_retained_scan(directory: Path) -> dict[str, Any]:
    final_derived_files_exist = (
        (directory / "assertions.json").exists()
        and (directory / "trust-expiry-proof.svg").exists()
    )
    expected_names = (
        EXPECTED_EVIDENCE_FILES - {"manifest.json"}
        if final_derived_files_exist
        else set(SANITIZER_INPUT_FILES)
        | {"sanitizer.json", "private-material-scan.json"}
    )
    observed_names = {
        path.name for path in directory.iterdir() if path.is_file()
    } - {"manifest.json"}
    files = sorted(directory / name for name in observed_names)
    violations = []
    known_seed_violations = []
    for path in files:
        text = path.read_text(encoding="utf-8", errors="replace")
        if not retained_text_is_safe(text):
            violations.append(path.name)
        for label in known_seed_labels_in_text(text):
            known_seed_violations.append({"file": path.name, "seed_label": label})
    return {
        "files_scanned": [path.name for path in files],
        "violations": violations,
        "known_ed25519_seed_violations": known_seed_violations,
        "unexpected_files": sorted(observed_names - expected_names),
        "missing_files": sorted(expected_names - observed_names),
    }


def exact_manifest_shape(manifest: Any) -> bool:
    top_level_keys = {
        "schema",
        "expected_files",
        "file_count",
        "hashed_file_count",
        "files",
    }
    if not isinstance(manifest, dict) or set(manifest) != top_level_keys:
        return False
    files = manifest.get("files")
    if not isinstance(files, list):
        return False
    for item in files:
        if (
            not isinstance(item, dict)
            or set(item) != {"path", "bytes", "sha256"}
            or not isinstance(item.get("path"), str)
            or type(item.get("bytes")) is not int
            or item.get("bytes", -1) < 0
            or not isinstance(item.get("sha256"), str)
            or re.fullmatch(r"[0-9a-f]{64}", item.get("sha256", "")) is None
        ):
            return False
    return True


def manifest_presence_satisfied(present: bool, require_manifest: bool) -> bool:
    return present or not require_manifest


def manifest_valid_if_present(
    directory: Path, *, require_manifest: bool = False
) -> bool:
    path = directory / "manifest.json"
    if not path.exists():
        return manifest_presence_satisfied(False, require_manifest)
    try:
        raw_manifest = path.read_text(encoding="utf-8")
        if (
            not retained_text_is_safe(raw_manifest)
            or known_seed_labels_in_text(raw_manifest)
        ):
            return False
        manifest = json.loads(raw_manifest)
        if not exact_manifest_shape(manifest):
            return False
        files = manifest.get("files")
        if (
            manifest.get("schema") != "inferlab.evidence-manifest.v0.27"
            or manifest.get("expected_files") != sorted(EXPECTED_EVIDENCE_FILES)
            or manifest.get("file_count") != len(EXPECTED_EVIDENCE_FILES)
            or manifest.get("hashed_file_count") != len(EXPECTED_EVIDENCE_FILES) - 1
            or len(files) != len(EXPECTED_EVIDENCE_FILES) - 1
        ):
            return False
        expected_hashed = sorted(EXPECTED_EVIDENCE_FILES - {"manifest.json"})
        if [item.get("path") for item in files] != expected_hashed:
            return False
        for item in files:
            content = (directory / item["path"]).read_bytes()
            if (
                item.get("bytes") != len(content)
                or item.get("sha256") != hashlib.sha256(content).hexdigest()
            ):
                return False
        return True
    except (KeyError, OSError, TypeError, ValueError, json.JSONDecodeError):
        return False


def hardening_negative_cases_rejected() -> bool:
    test_filter = next(iter(sorted(VALIDITY_TESTS)))
    zero_test = (
        "running 0 tests\n\n"
        "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; "
        "47 filtered out; finished in 0.00s\n"
    )
    wrong_filter = (
        "running 1 test\n"
        "test service_trust::tests::different_test ... ok\n\n"
        "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; "
        "46 filtered out; finished in 0.01s\n"
    )
    zero_fields = cargo_test_output_fields(test_filter, zero_test)
    wrong_fields = cargo_test_output_fields(test_filter, wrong_filter)
    manifest_shape = {
        "schema": "inferlab.evidence-manifest.v0.27",
        "expected_files": sorted(EXPECTED_EVIDENCE_FILES),
        "file_count": len(EXPECTED_EVIDENCE_FILES),
        "hashed_file_count": len(EXPECTED_EVIDENCE_FILES) - 1,
        "files": [
            {"path": name, "bytes": 0, "sha256": "0" * 64}
            for name in sorted(EXPECTED_EVIDENCE_FILES - {"manifest.json"})
        ],
    }
    cluster_fixture = {
        "schema": "inferlab.full-stack-leader.v0.13",
        "leader_id": "control-a",
        "statuses": [
            {
                "status": 200,
                "body": {
                    "node_id": node_id,
                    "cluster_id": "inferlab-primary",
                    "leader_id": "control-a",
                    "role": "leader" if node_id == "control-a" else "follower",
                },
            }
            for node_id in sorted(CONTROL_IDS)
        ],
    }
    duplicate_cluster_fixture = json.loads(json.dumps(cluster_fixture))
    duplicate_cluster_fixture["statuses"][2]["body"]["node_id"] = "control-b"
    return (
        zero_fields["running_one_test"] is False
        and zero_fields["exact_test_line"] is False
        and zero_fields["exact_summary"] is False
        and wrong_fields["running_one_test"] is True
        and wrong_fields["exact_test_line"] is False
        and wrong_fields["exact_summary"] is True
        and retained_text_is_safe('/Users/example/private/evidence.json') is False
        and retained_text_is_safe('-----BEGIN PRIVATE KEY-----') is False
        and known_seed_labels_in_text(
            'prefix BQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQU suffix'
        )
        == ["trust_root_seed"]
        and known_seed_labels_in_text(
            'prefix BQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQU= suffix'
        )
        == ["trust_root_seed"]
        and [] != SANITIZER_INPUT_FILES
        and [] != PRIVATE_SCAN_FINAL_FILES
        and EXPIRED_STARTUP_DIAGNOSTIC not in 'expired-cache restart path'
        and startup_diagnostic_observed(
            "issued_in_future",
            'totally unrelated startup crash containing issued_in_future token',
        )
        is False
        and exact_manifest_shape(manifest_shape)
        and exact_manifest_shape({**manifest_shape, "adversarial_host_path": "/Users/example"})
        is False
        and manifest_presence_satisfied(False, True) is False
        and manifest_presence_satisfied(False, False)
        and conditional_etags_are_stable('"etag-a"', '"etag-b"', True) is False
        and failed_restart_is_not_current(17, 17, [17, 18], True) is False
        and exact_three_control_cluster(cluster_fixture)
        and exact_three_control_cluster(duplicate_cluster_fixture) is False
        and exact_sha256("g1") is False
        and exact_sha256("A" * 64) is False
        and exact_int(True, 1) is False
        and exact_int(1, 1)
        and unique_positive_pids([11, 12, 13], 3)
        and unique_positive_pids([11, 11, 13], 3) is False
        and positive_pid(0) is False
        and exact_process_command("forged/control-plane", "control-plane") is False
        and exact_start_token("x") is False
        and valid_observed_interval(
            {"started_at_ms": 100, "completed_at_ms": 101, "duration_ms": 1.0}
        )
        and valid_observed_interval(
            {"started_at_ms": 100, "completed_at_ms": 99, "duration_ms": 1.0}
        )
        is False
        and exact_teaching_pieces(["fabricated"]) is False
    )


def response_error(observation: dict[str, Any]) -> tuple[Any, Any]:
    error = observation.get("body", {}).get("error", {})
    return error.get("code"), error.get("message")


def controls(document: dict[str, Any]) -> list[dict[str, Any]]:
    statuses = document.get("statuses")
    if not isinstance(statuses, list):
        return []
    return [
        status.get("body", {}).get("service_authentication", {})
        for status in statuses
        if isinstance(status, dict) and status.get("status") == 200
    ]


def exact_three_control_cluster(document: dict[str, Any]) -> bool:
    statuses = document.get("statuses")
    if not isinstance(statuses, list) or len(statuses) != 3:
        return False
    bodies = [
        item.get("body", {}) if isinstance(item, dict) else {}
        for item in statuses
    ]
    leader_id = document.get("leader_id")
    leader_nodes = [body.get("node_id") for body in bodies if body.get("role") == "leader"]
    return (
        document.get("schema") == "inferlab.full-stack-leader.v0.13"
        and all(item.get("status") == 200 for item in statuses)
        and {body.get("node_id") for body in bodies} == CONTROL_IDS
        and [body.get("role") for body in bodies].count("leader") == 1
        and [body.get("role") for body in bodies].count("follower") == 2
        and leader_id in CONTROL_IDS
        and leader_nodes == [leader_id]
        and all(body.get("leader_id") == leader_id for body in bodies)
        and all(body.get("cluster_id") == "inferlab-primary" for body in bodies)
    )


def exact_committed_cpu_route(status: dict[str, Any]) -> bool:
    body = status.get("body", {}) if isinstance(status, dict) else {}
    committed = body.get("committed_configuration", {})
    configuration = committed.get("configuration", {}) if isinstance(committed, dict) else {}
    return (
        body.get("commit_index", 0) >= 2
        and body.get("last_applied", 0) >= 2
        and committed.get("cluster_id") == "inferlab-primary"
        and exact_int(committed.get("revision"), 2)
        and configuration.get("routing_policy") == "round-robin"
        and configuration.get("workers")
        == [
            {
                "base_url": "http://127.0.0.1:10084",
                "id": "cpu-trust-expiry",
                "weight": 1,
            }
        ]
    )


def exact_control_nodes(document: dict[str, Any]) -> bool:
    statuses = document.get("statuses")
    return (
        isinstance(statuses, list)
        and len(statuses) == 3
        and {
            status.get("body", {}).get("node_id")
            for status in statuses
            if isinstance(status, dict) and status.get("status") == 200
        }
        == CONTROL_IDS
    )


def valid_v2_controls(
    document: dict[str, Any], generation: int, expires_at_ms: int
) -> bool:
    values = controls(document)
    expected = document.get("expected", {})
    return (
        document.get("schema") == "inferlab.trust-expiry-controls.v0.27"
        and exact_int(expected.get("generation"), generation)
        and expected.get("validity") == "valid"
        and expected.get("expires_at_ms") == expires_at_ms
        and exact_control_nodes(document)
        and len(values) == 3
        and all(item.get("required") is True for item in values)
        and all(item.get("trust_policy_source") == "signed-snapshot" for item in values)
        and all(item.get("trust_policy_distribution_mode") == "remote-http" for item in values)
        and all(item.get("trust_policy_bootstrap_source") == "remote" for item in values)
        and all(item.get("trust_policy_etag_present") is True for item in values)
        and all(
            item.get("trust_policy_signing_key_id") == "service-trust-root-a"
            for item in values
        )
        and all(exact_int(item.get("trust_policy_generation"), generation) for item in values)
        and all(item.get("trust_policy_expires_at_ms") == expires_at_ms for item in values)
        and all(item.get("trust_policy_validity") == "valid" for item in values)
        and all(
            exact_int(item.get("trust_policy_issued_at_ms"))
            and 0 < item.get("trust_policy_issued_at_ms", 0) < expires_at_ms
            for item in values
        )
        and all(
            exact_int(item.get("trust_policy_loaded_at_ms"))
            and 0 < item.get("trust_policy_loaded_at_ms", 0) < expires_at_ms
            for item in values
        )
        and all(exact_int(item.get("trust_policy_remaining_ms")) for item in values)
        and all(item.get("trust_policy_remaining_ms", 0) > 0 for item in values)
        and all(item.get("trust_policy_max_lifetime_ms") == 45_000 for item in values)
        and all(item.get("trust_policy_max_future_skew_ms") == 250 for item in values)
        and all(item.get("trust_policy_allow_legacy_v1") is False for item in values)
        and all(item.get("trust_policy_transport_mode") == "mutual-tls" for item in values)
        and all(item.get("trust_policy_server_authentication") is True for item in values)
        and all(item.get("trust_policy_client_authentication") is True for item in values)
    )


def distributor_body(document: dict[str, Any]) -> dict[str, Any]:
    return document.get("status", {}).get("body", {})


def exact_signature(value: Any) -> bool:
    if not isinstance(value, str):
        return False
    try:
        return len(base64.b64decode(value, validate=True)) == 64
    except (ValueError, binascii.Error):
        return False


def exact_receipts(
    body: dict[str, Any],
    generation: int,
    expires_at_ms: int,
    control_document: dict[str, Any],
) -> bool:
    receipts = body.get("receipts")
    statuses = control_document.get("statuses")
    receipt_items = receipts if isinstance(receipts, list) else []
    status_items = statuses if isinstance(statuses, list) else []
    control_activation_times = {
        status.get("body", {}).get("node_id"): status.get("body", {})
        .get("service_authentication", {})
        .get("trust_policy_loaded_at_ms")
        for status in status_items
        if isinstance(status, dict) and status.get("status") == 200
    }
    receipt_activation_times = {
        item.get("receiver_service_id"): item.get("applied_at_ms")
        for item in receipt_items
        if isinstance(item, dict)
    }
    return (
        body.get("acked_receivers") == EXPECTED_RECEIVERS
        and body.get("pending_receivers") == []
        and exact_int(body.get("receipt_count"), 3)
        and isinstance(receipts, list)
        and len(receipts) == 3
        and {
            f"{item.get('receiver_service_id')}/{item.get('receiver_credential_id')}"
            for item in receipts
        }
        == set(EXPECTED_RECEIVERS)
        and all(item.get("schema") == RECEIPT_SCHEMA for item in receipts)
        and all(exact_int(item.get("generation"), generation) for item in receipts)
        and all(item.get("cluster_id") == "inferlab-primary" for item in receipts)
        and all(item.get("root_key_id") == "service-trust-root-a" for item in receipts)
        and all(exact_signature(item.get("snapshot_signature")) for item in receipts)
        and all(
            exact_int(item.get("applied_at_ms"))
            and 0 < item.get("applied_at_ms", 0) < expires_at_ms
            for item in receipts
        )
        and control_activation_times == receipt_activation_times
        and set(control_activation_times) == CONTROL_IDS
        and all(
            item.get("authentication", {}).get("schema") == RECEIPT_AUTH_SCHEMA
            and item.get("authentication", {}).get("algorithm") == "ed25519"
            and exact_signature(item.get("authentication", {}).get("signature"))
            for item in receipts
        )
    )


def durable_nodes(document: dict[str, Any]) -> dict[str, dict[str, Any]]:
    nodes = document.get("nodes")
    return nodes if isinstance(nodes, dict) else {}


def exact_durable_generation(
    document: dict[str, Any], generation: int, expires_at_ms: int, lifetime_ms: int
) -> bool:
    nodes = durable_nodes(document)
    return (
        document.get("schema") == "inferlab.trust-expiry-durable-state.v0.27"
        and set(nodes) == CONTROL_IDS
        and all(item.get("cache_schema") == "inferlab.service-trust-cache.v1" for item in nodes.values())
        and all(item.get("policy_schema") == POLICY_V2 for item in nodes.values())
        and all(item.get("authentication_schema") == POLICY_AUTH_V2 for item in nodes.values())
        and all(item.get("authentication_algorithm") == "ed25519" for item in nodes.values())
        and all(item.get("root_key_id") == "service-trust-root-a" for item in nodes.values())
        and all(exact_int(item.get("generation"), generation) for item in nodes.values())
        and all(exact_int(item.get("expires_at_ms"), expires_at_ms) for item in nodes.values())
        and all(exact_int(item.get("issued_at_ms")) for item in nodes.values())
        and all(
            item.get("expires_at_ms", 0) - item.get("issued_at_ms", 0) == lifetime_ms
            for item in nodes.values()
        )
        and len({item.get("issued_at_ms") for item in nodes.values()}) == 1
        and all(exact_sha256(item.get("snapshot_signature_sha256")) for item in nodes.values())
        and len({item.get("snapshot_signature_sha256") for item in nodes.values()}) == 1
        and all(item.get("floor_schema") == "inferlab.service-trust-floor.v1" for item in nodes.values())
        and all(exact_int(item.get("floor_generation"), generation) for item in nodes.values())
        and all(item.get("floor_signature_matches_cache") is True for item in nodes.values())
        and all(exact_sha256(item.get("cache_sha256")) for item in nodes.values())
        and all(exact_sha256(item.get("floor_sha256")) for item in nodes.values())
        and len({item.get("cache_sha256") for item in nodes.values()}) == 1
        and len({item.get("floor_sha256") for item in nodes.values()}) == 1
    )


def exact_startup_failure(document: dict[str, Any], scenario: str, kind: str) -> bool:
    excerpt = document.get("log_excerpt")
    return (
        document.get("schema") == "inferlab.trust-expiry-startup-failure.v0.27"
        and document.get("scenario") == scenario
        and document.get("expected_error_kind") == kind
        and exact_int(document.get("exit_code"))
        and document.get("exit_code") != 0
        and document.get("listener_ever_open") is False
        and exact_int(document.get("listener_probe_count"))
        and document.get("listener_probe_count", 0) >= 1
        and document.get("listener_open_after_exit") is False
        and document.get("failed_before_listener") is True
        and document.get("error_kind_observed") is True
        and document.get("durable_floor_created") is False
        and startup_diagnostic_observed(kind, excerpt)
        and exact_int(document.get("failed_pid"))
        and document.get("failed_pid", 0) > 0
    )


def exact_signed_route_body(body: Any) -> bool:
    if not isinstance(body, dict):
        return False
    authentication = body.get("authentication", {})
    return (
        body.get("cluster_id") == "inferlab-primary"
        and exact_int(body.get("revision"), 2)
        and body.get("configuration")
        == {
            "routing_policy": "round-robin",
            "workers": [
                {
                    "base_url": "http://127.0.0.1:10084",
                    "id": "cpu-trust-expiry",
                    "weight": 1,
                }
            ],
        }
        and authentication.get("schema") == "inferlab.control-authentication.v1"
        and authentication.get("algorithm") == "ed25519"
        and authentication.get("key_id") == "route-2026-b"
        and exact_signature(authentication.get("signature"))
    )


def full_stack_json(document: dict[str, Any]) -> bool:
    requests = document.get("requests")
    return (
        document.get("schema") == "inferlab.full-stack-request-set.v0.13"
        and exact_int(document.get("requested"), 1)
        and exact_int(document.get("succeeded"), 1)
        and isinstance(requests, list)
        and len(requests) == 1
        and positive_finite(document.get("duration_ms"))
        and requests[0].get("status") == 200
        and requests[0].get("worker") == "cpu-trust-expiry"
        and exact_int(requests[0].get("attempts"), 1)
        and requests[0].get("content") == "InferLab turns prompts into real tokens."
        and requests[0].get("finish_reason") == "stop"
        and exact_int(requests[0].get("config_revision"), 2)
        and requests[0].get("control_cluster_id") == "inferlab-primary"
        and requests[0].get("control_signing_key_id") == "route-2026-b"
    )


def full_stack_stream(document: dict[str, Any]) -> bool:
    pieces = document.get("pieces")
    return (
        document.get("schema") == "inferlab.full-stack-stream.v0.13"
        and document.get("status") == 200
        and document.get("worker") == "cpu-trust-expiry"
        and exact_int(document.get("attempts"), 1)
        and positive_finite(document.get("duration_ms"))
        and document.get("done_received") is True
        and document.get("finish_reason") == "stop"
        and exact_teaching_pieces(pieces)
        and document.get("content") == "".join(pieces)
        and exact_int(document.get("config_revision"), 2)
        and document.get("control_cluster_id") == "inferlab-primary"
        and document.get("control_signing_key_id") == "route-2026-b"
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--require-manifest", action="store_true")
    args = parser.parse_args()

    evidence = args.evidence_dir
    publish_g1 = load(evidence, "publish-g1.json")
    g1_controls = load(evidence, "generation-1-controls.json")
    g1_receipts = load(evidence, "generation-1-receipts.json")
    durable_g1 = load(evidence, "durable-generation-1.json")
    g1_request = load(evidence, "generation-1-request.json")
    initial_cluster = load(evidence, "initial-cluster.json")
    write = load(evidence, "write-committed.json")
    gateway_ready = load(evidence, "gateway-ready.json")
    tampered = load(evidence, "expiry-tamper.json")
    malformed = load(evidence, "malformed-window.json")
    fork = load(evidence, "same-generation-deadline-fork.json")
    post_attacks = load(evidence, "post-candidate-attacks.json")
    durable_attacks = load(evidence, "durable-after-candidate-attacks.json")
    future = load(evidence, "future-issued-startup.json")
    excessive = load(evidence, "excessive-lifetime-startup.json")
    legacy = load(evidence, "legacy-v1-startup.json")
    conditional = load(evidence, "not-modified-does-not-renew.json")
    pre_controls = load(evidence, "pre-expiry-controls.json")
    cutoff = load(evidence, "request-time-cutoff-and-admitted-stream.json")
    expired_controls = load(evidence, "expired-controls.json")
    durable_expired = load(evidence, "durable-expired-generation-1.json")
    outage = load(evidence, "distributor-outage.json")
    expired_restart = load(evidence, "expired-cache-restart.json")
    publish_g2 = load(evidence, "publish-g2.json")
    g2_controls = load(evidence, "generation-2-controls.json")
    g2_receipts = load(evidence, "generation-2-receipts.json")
    durable_g2 = load(evidence, "durable-generation-2.json")
    continuity = load(evidence, "process-continuity.json")
    production = load(evidence, "production-validity-tests.json")
    final_cluster = load(evidence, "final-cluster.json")
    final_gateway = load(evidence, "final-gateway.json")
    final_request = load(evidence, "final-request.json")
    final_stream = load(evidence, "final-stream.json")
    sanitizer = load(evidence, "sanitizer.json")
    private_scan = load(evidence, "private-material-scan.json")

    assertions: list[dict[str, Any]] = []

    def check(name: str, passed: bool, observation: Any) -> None:
        assertions.append({"name": name, "passed": bool(passed), "observation": observation})

    g1_observation = publish_g1.get("observation", {})
    check(
        "a root-signed policy-v2 generation 1 publication succeeds",
        publish_g1.get("schema") == "inferlab.trust-expiry-http-capture.v0.27"
        and publish_g1.get("method") == "POST"
        and g1_observation.get("status") == 201
        and g1_observation.get("body", {}).get("schema")
        == "inferlab.trust-distributor-publish.v1"
        and exact_int(g1_observation.get("body", {}).get("generation"), 1)
        and g1_observation.get("body", {}).get("root_key_id")
        == "service-trust-root-a"
        and g1_observation.get("body", {}).get("outcome") == "published"
        and isinstance(g1_observation.get("body", {}).get("etag"), str)
        and g1_observation.get("body", {}).get("etag") == g1_observation.get("etag"),
        g1_observation,
    )

    g1_expiry = g1_receipts.get("expected_expires_at_ms")
    check(
        "three exact controls activate valid v2 generation 1 over mutual TLS",
        exact_int(g1_expiry) and valid_v2_controls(g1_controls, 1, g1_expiry),
        controls(g1_controls),
    )
    g1_body = distributor_body(g1_receipts)
    g1_snapshot = g1_body.get("snapshot") or {}
    check(
        "the distributor reports schema and expiry without claiming receiver validity",
        g1_receipts.get("schema") == "inferlab.trust-expiry-distributor.v0.27"
        and exact_int(g1_receipts.get("expected_generation"), 1)
        and g1_receipts.get("expected_policy_schema") == POLICY_V2
        and g1_receipts.get("expected_expires_at_ms") == g1_expiry
        and g1_receipts.get("expected_acked_receivers") == EXPECTED_RECEIVERS
        and g1_receipts.get("status", {}).get("status") == 200
        and g1_snapshot.get("policy_schema") == POLICY_V2
        and exact_int(g1_snapshot.get("generation"), 1)
        and g1_snapshot.get("expires_at_ms") == g1_expiry
        and "validity" not in g1_snapshot
        and "remaining_ms" not in g1_snapshot,
        g1_snapshot,
    )
    check(
        "the distributor retains three structurally signed generation-1 receipts",
        g1_receipts.get("status", {}).get("status") == 200
        and exact_int(g1_expiry)
        and exact_receipts(g1_body, 1, g1_expiry, g1_controls),
        {"acked": g1_body.get("acked_receivers"), "receipts": g1_body.get("receipt_count")},
    )
    check(
        "all three generation-1 caches and rollback floors bind the exact v2 deadline",
        exact_int(g1_expiry)
        and exact_durable_generation(durable_g1, 1, g1_expiry, 45_000)
        and {
            hashlib.sha256(item["snapshot_signature"].encode("utf-8")).hexdigest()
            for item in g1_body.get("receipts", [])
        }
        == {next(iter(durable_nodes(durable_g1).values())).get("snapshot_signature_sha256")},
        durable_nodes(durable_g1),
    )
    check(
        "generation 1 commits routing and the gateway applies revision 2",
        exact_three_control_cluster(initial_cluster)
        and write.get("response", {}).get("status") == 200
        and exact_int(write.get("response", {}).get("body", {}).get("revision"), 2)
        and gateway_ready.get("status", {}).get("status") == 200
        and exact_int(
            gateway_ready.get("status", {}).get("body", {}).get("routing_snapshot", {}).get("control_revision"),
            2,
        ),
        {"write": write, "gateway": gateway_ready},
    )
    check(
        "real CPU JSON inference succeeds while generation 1 is valid",
        full_stack_json(g1_request),
        g1_request,
    )

    tampered_observation = tampered.get("observation", {})
    tampered_code, tampered_message = response_error(tampered_observation)
    check(
        "changing the signed expiry is rejected as an invalid snapshot",
        tampered_observation.get("status") == 400
        and tampered_code == "invalid_snapshot"
        and "signature" in str(tampered_message).lower(),
        tampered_observation,
    )
    malformed_observation = malformed.get("observation", {})
    malformed_code, malformed_message = response_error(malformed_observation)
    check(
        "a v2 expiry not later than issue time is rejected structurally",
        malformed_observation.get("status") == 400
        and malformed_code == "invalid_snapshot"
        and "expiry" in str(malformed_message).lower()
        and "issue" in str(malformed_message).lower(),
        malformed_observation,
    )
    fork_observation = fork.get("observation", {})
    fork_code, _ = response_error(fork_observation)
    check(
        "a different valid deadline at the same generation is a fork, not renewal",
        fork_observation.get("status") == 409 and fork_code == "snapshot_fork",
        fork_observation,
    )
    post_body = post_attacks.get("observation", {}).get("body", {})
    check(
        "candidate attacks leave distributor generation 1 and its receipt set authoritative",
        post_attacks.get("observation", {}).get("status") == 200
        and exact_int(post_body.get("snapshot", {}).get("generation"), 1)
        and post_body.get("snapshot", {}).get("expires_at_ms") == g1_expiry
        and post_body.get("receipts") == g1_body.get("receipts"),
        post_body,
    )
    check(
        "candidate attacks leave every generation-1 cache and floor byte-for-byte unchanged",
        durable_nodes(durable_attacks) == durable_nodes(durable_g1),
        {"before": durable_nodes(durable_g1), "after": durable_nodes(durable_attacks)},
    )

    check(
        "a future-issued authentic v2 policy fails startup before listening",
        exact_startup_failure(future, "future-issued", "issued_in_future"),
        future,
    )
    check(
        "an authentic v2 policy above the receiver lifetime cap fails startup",
        exact_startup_failure(excessive, "excessive-lifetime", "lifetime_exceeded"),
        excessive,
    )
    check(
        "policy v1 is default-rejected as a legacy unbounded downgrade",
        exact_startup_failure(legacy, "legacy-v1-default", "legacy_v1_disallowed"),
        legacy,
    )

    initial_conditional = conditional.get("initial", {})
    second_conditional = conditional.get("conditional", {})
    check(
        "a 304 Not Modified retains the exact generation-1 signed deadline",
        conditional.get("schema") == "inferlab.trust-expiry-conditional-get.v0.27"
        and exact_int(conditional.get("expected_generation"), 1)
        and conditional.get("expected_expires_at_ms") == g1_expiry
        and initial_conditional.get("status") == 200
        and exact_int(initial_conditional.get("body", {}).get("generation"), 1)
        and initial_conditional.get("body", {}).get("expires_at_ms") == g1_expiry
        and second_conditional.get("status") == 304
        and second_conditional.get("body") is None
        and conditional_etags_are_stable(
            initial_conditional.get("etag"),
            second_conditional.get("etag"),
            conditional.get("etag_stable"),
        ),
        conditional,
    )
    pre_values = controls(pre_controls)
    pre_controls_ok = (
        exact_control_nodes(pre_controls)
        and len(pre_values) == 3
        and all(exact_int(item.get("trust_policy_generation"), 1) for item in pre_values)
        and all(item.get("trust_policy_expires_at_ms") == g1_expiry for item in pre_values)
        and all(item.get("trust_policy_validity") == "valid" for item in pre_values)
        and all(item.get("trust_policy_last_fetch_outcome") == "not-modified" for item in pre_values)
    )
    check(
        "all receivers observe not-modified without moving the pre-expiry deadline",
        pre_controls_ok,
        pre_controls,
    )

    cutoff_expiry = cutoff.get("expires_at_ms")
    pre_auth = cutoff.get("pre_expiry_authentication", {})
    post_auth = cutoff.get("post_expiry_authentication", {})
    pre_request = cutoff.get("pre_expiry_signed_request", {})
    post_request = cutoff.get("post_expiry_signed_request", {})
    missing_request = cutoff.get("post_expiry_missing_authentication_request", {})
    cutoff_schedule = cutoff.get("schedule")
    check(
        "a valid signed gateway request beginning before expiry is accepted",
        cutoff.get("schema") == "inferlab.trust-expiry-cutoff.v0.27"
        and cutoff_expiry == g1_expiry
        and cutoff_schedule
        == {
            "stream_start_before_ms": 1_500,
            "pre_request_before_ms": 400,
            "post_request_after_ms": 25,
        }
        and pre_auth.get("schema") == "inferlab.service-authentication.v1"
        and pre_auth.get("algorithm") == "ed25519"
        and pre_auth.get("service_id") == "gateway-primary"
        and pre_auth.get("audience_id") == initial_cluster.get("leader_id")
        and pre_auth.get("issued_at_ms") == cutoff_expiry - 1_600
        and pre_auth.get("nonce") == "expiry-pre-request-0001"
        and pre_auth.get("signature_present") is True
        and exact_int(pre_auth.get("signature_bytes"), 64)
        and exact_sha256(pre_auth.get("signature_sha256"))
        and valid_observed_interval(pre_request)
        and 0 < cutoff_expiry - pre_request.get("started_at_ms", 0) <= 400
        and pre_request.get("completed_at_ms", cutoff_expiry + 1) < cutoff_expiry
        and pre_request.get("status") == 200
        and exact_signed_route_body(pre_request.get("body")),
        {"authentication": pre_auth, "request": pre_request},
    )
    post_code, post_message = response_error(post_request)
    check(
        "the same protected route is rejected at or after the exclusive expiry",
        post_auth.get("schema") == "inferlab.service-authentication.v1"
        and post_auth.get("algorithm") == "ed25519"
        and post_auth.get("service_id") == "gateway-primary"
        and post_auth.get("audience_id") == initial_cluster.get("leader_id")
        and post_auth.get("issued_at_ms") == cutoff_expiry - 1_600
        and post_auth.get("nonce") == "expiry-post-request-0002"
        and post_auth.get("signature_present") is True
        and exact_int(post_auth.get("signature_bytes"), 64)
        and exact_sha256(post_auth.get("signature_sha256"))
        and valid_observed_interval(post_request)
        and 25 <= post_request.get("started_at_ms", 0) - cutoff_expiry <= 1_000
        and post_request.get("completed_at_ms", 0) <= cutoff_expiry + 5_000
        and post_request.get("status") == 401
        and post_code == "unauthorized"
        and post_message == "signed service-trust policy is expired"
        and post_request.get("body") == {
            "error": {
                "code": "unauthorized",
                "message": "signed service-trust policy is expired",
                "leader_id": None,
            }
        },
        {"authentication": post_auth, "request": post_request},
    )
    missing_code, missing_message = response_error(missing_request)
    check(
        "expiry is checked before missing authentication headers",
        valid_observed_interval(missing_request)
        and 25 <= missing_request.get("started_at_ms", 0) - cutoff_expiry <= 1_000
        and missing_request.get("started_at_ms", 0)
        >= post_request.get("completed_at_ms", cutoff_expiry + 5_001)
        and missing_request.get("completed_at_ms", 0) <= cutoff_expiry + 5_000
        and missing_request.get("status") == 401
        and missing_code == "unauthorized"
        and missing_message == "signed service-trust policy is expired"
        and missing_request.get("body") == post_request.get("body")
        and missing_request.get("body") == {
            "error": {
                "code": "unauthorized",
                "message": "signed service-trust policy is expired",
                "leader_id": None,
            }
        },
        missing_request,
    )
    cutoff_status = cutoff.get("post_expiry_control_status", {})
    cutoff_auth = cutoff_status.get("body", {}).get("service_authentication", {})
    check(
        "post-expiry status saturates remaining time and counts both rejection paths",
        cutoff_status.get("status") == 200
        and exact_int(cutoff_auth.get("trust_policy_generation"), 1)
        and cutoff_auth.get("trust_policy_expires_at_ms") == cutoff_expiry
        and cutoff_auth.get("trust_policy_validity") == "expired"
        and exact_int(cutoff_auth.get("trust_policy_remaining_ms"), 0)
        and exact_int(cutoff_auth.get("trust_policy_expiration_rejections"))
        and cutoff_auth.get("trust_policy_expiration_rejections", 0) >= 2,
        cutoff_auth,
    )
    admitted_stream = cutoff.get("pre_expiry_stream", {})
    admitted_pieces = admitted_stream.get("pieces")
    check(
        "a real CPU SSE admitted before expiry deliberately completes after expiry",
        cutoff.get("stream_error") == []
        and admitted_stream.get("status") == 200
        and admitted_stream.get("worker") == "cpu-trust-expiry"
        and exact_int(admitted_stream.get("attempts"), 1)
        and exact_int(admitted_stream.get("config_revision"), 2)
        and valid_observed_interval(admitted_stream)
        and 0 < cutoff_expiry - admitted_stream.get("started_at_ms", 0) <= 1_500
        and cutoff_expiry <= admitted_stream.get("completed_at_ms", 0)
        <= cutoff_expiry + 10_000
        and admitted_stream.get("done_received") is True
        and admitted_stream.get("finish_reason") == "stop"
        and exact_teaching_pieces(admitted_pieces)
        and admitted_stream.get("content") == "".join(admitted_pieces)
        and exact_int(admitted_stream.get("event_count"), 10),
        admitted_stream,
    )

    expired_values = controls(expired_controls)
    check(
        "all three live controls report expired generation 1 with zero remaining time",
        exact_control_nodes(expired_controls)
        and len(expired_values) == 3
        and all(exact_int(item.get("trust_policy_generation"), 1) for item in expired_values)
        and all(item.get("trust_policy_expires_at_ms") == g1_expiry for item in expired_values)
        and all(item.get("trust_policy_validity") == "expired" for item in expired_values)
        and all(exact_int(item.get("trust_policy_remaining_ms"), 0) for item in expired_values)
        and all(
            item.get("trust_policy_last_fetch_outcome") == "not-modified"
            for item in expired_values
        ),
        expired_values,
    )
    check(
        "runtime expiry changes authority but not cached generation-1 bytes or floors",
        durable_nodes(durable_expired) == durable_nodes(durable_g1),
        {"valid": durable_nodes(durable_g1), "expired": durable_nodes(durable_expired)},
    )

    outage_observation = outage.get("observation", {})
    check(
        "the exact mTLS-configured distributor attempt observes a stopped listener",
        outage.get("schema") == "inferlab.trust-expiry-transport-failure.v0.27"
        and outage.get("scenario") == "distributor-withheld-and-stopped"
        and outage.get("failed_before_http_response") is True
        and outage_observation.get("status") is None
        and outage_observation.get("transport_error") == "ConnectionRefusedError",
        outage,
    )
    c_g1 = durable_nodes(durable_g1).get("control-c", {})
    check(
        "a restart with only expired cache fails before listening without changing durable state",
        expired_restart.get("schema") == "inferlab.trust-expiry-cache-restart.v0.27"
        and exact_int(expired_restart.get("exit_code"))
        and expired_restart.get("exit_code") != 0
        and expired_restart.get("listener_ever_open") is False
        and exact_int(expired_restart.get("listener_probe_count"))
        and expired_restart.get("listener_probe_count", 0) >= 1
        and expired_restart.get("listener_open_after_exit") is False
        and expired_restart.get("failed_before_listener") is True
        and expired_restart.get("expired_error_observed") is True
        and EXPIRED_STARTUP_DIAGNOSTIC
        in str(expired_restart.get("log_excerpt", ""))
        and expired_restart.get("cache_sha256") == c_g1.get("cache_sha256")
        and expired_restart.get("floor_sha256") == c_g1.get("floor_sha256"),
        expired_restart,
    )

    g2_observation = publish_g2.get("observation", {})
    check(
        "a newly signed generation 2 publication succeeds after expiry",
        publish_g2.get("schema") == "inferlab.trust-expiry-http-capture.v0.27"
        and publish_g2.get("method") == "POST"
        and g2_observation.get("status") == 201
        and g2_observation.get("body", {}).get("schema")
        == "inferlab.trust-distributor-publish.v1"
        and exact_int(g2_observation.get("body", {}).get("generation"), 2)
        and g2_observation.get("body", {}).get("root_key_id")
        == "service-trust-root-a"
        and g2_observation.get("body", {}).get("outcome") == "published"
        and isinstance(g2_observation.get("body", {}).get("etag"), str)
        and g2_observation.get("body", {}).get("etag") == g2_observation.get("etag"),
        g2_observation,
    )
    g2_expiry = g2_receipts.get("expected_expires_at_ms")
    check(
        "three controls recover to valid higher-generation policy v2",
        exact_int(g2_expiry)
        and g2_expiry > g1_expiry
        and valid_v2_controls(g2_controls, 2, g2_expiry),
        controls(g2_controls),
    )
    g2_body = distributor_body(g2_receipts)
    check(
        "the distributor retains three structurally signed generation-2 receipts",
        g2_receipts.get("schema") == "inferlab.trust-expiry-distributor.v0.27"
        and exact_int(g2_receipts.get("expected_generation"), 2)
        and g2_receipts.get("expected_policy_schema") == POLICY_V2
        and g2_receipts.get("expected_expires_at_ms") == g2_expiry
        and g2_receipts.get("expected_acked_receivers") == EXPECTED_RECEIVERS
        and g2_receipts.get("status", {}).get("status") == 200
        and g2_body.get("snapshot", {}).get("policy_schema") == POLICY_V2
        and g2_body.get("snapshot", {}).get("expires_at_ms") == g2_expiry
        and exact_int(g2_expiry)
        and exact_receipts(g2_body, 2, g2_expiry, g2_controls),
        {"snapshot": g2_body.get("snapshot"), "acked": g2_body.get("acked_receivers")},
    )
    g1_nodes = durable_nodes(durable_g1)
    g2_nodes = durable_nodes(durable_g2)
    check(
        "generation 2 advances every durable cache and rollback floor without deletion",
        exact_int(g2_expiry)
        and exact_durable_generation(durable_g2, 2, g2_expiry, 30_000)
        and {
            hashlib.sha256(item["snapshot_signature"].encode("utf-8")).hexdigest()
            for item in g2_body.get("receipts", [])
        }
        == {next(iter(g2_nodes.values())).get("snapshot_signature_sha256")}
        and all(
            g2_nodes[node].get("cache_sha256") != g1_nodes[node].get("cache_sha256")
            and g2_nodes[node].get("floor_sha256") != g1_nodes[node].get("floor_sha256")
            for node in CONTROL_IDS
        ),
        {"generation_1": g1_nodes, "generation_2": g2_nodes},
    )

    raw_participants = continuity.get("processes")
    participants = raw_participants if isinstance(raw_participants, dict) else {}
    expected_commands = {
        "control-a": "control-plane",
        "control-b": "control-plane",
        "control-c": "control-plane",
        "trust-distributor": "trust-distributor",
        "cpu-worker": "cpu-worker",
        "gateway": "gateway",
    }
    stable = {"control-a", "control-b", "cpu-worker", "gateway"}
    restarted = {"control-c", "trust-distributor"}
    proof_shell_pid = continuity.get("proof_shell_pid")
    initial_pids = [
        participants.get(name, {}).get("initial_pid") for name in expected_commands
    ]
    current_pids = [
        participants.get(name, {}).get("current_pid") for name in expected_commands
    ]
    check(
        "four unaffected exact OS processes retain PID, start token, and command",
        continuity.get("schema") == "inferlab.trust-expiry-process-continuity.v0.27"
        and set(participants) == set(expected_commands)
        and all(
            participants[name].get("same_pid") is True
            and participants[name].get("same_start_token") is True
            and participants[name].get("same_command") is True
            and participants[name].get("initial_pid")
            == participants[name].get("current_pid")
            and participants[name].get("initial_start_token")
            == participants[name].get("current_start_token")
            and participants[name].get("initial_command")
            == participants[name].get("current_command")
            and exact_start_token(participants[name].get("initial_start_token"))
            and exact_process_command(
                participants[name].get("initial_command"), expected_commands[name]
            )
            for name in stable
        ),
        {name: participants.get(name) for name in sorted(stable)},
    )
    check(
        "control C and the distributor are exact deliberate replacements after failure/outage",
        continuity.get("expected_restarts") == ["control-c", "trust-distributor"]
        and failed_restart_is_not_current(
            continuity.get("failed_expired_cache_restart_pid"),
            expired_restart.get("failed_pid"),
            current_pids,
            continuity.get("failed_restart_pid_is_not_live_participant"),
        )
        and all(
            participants[name].get("same_pid") is False
            and participants[name].get("same_start_token") is False
            and participants[name].get("same_command") is True
            and participants[name].get("initial_pid")
            != participants[name].get("current_pid")
            and participants[name].get("initial_start_token")
            != participants[name].get("current_start_token")
            and participants[name].get("initial_command")
            == participants[name].get("current_command")
            and exact_start_token(participants[name].get("initial_start_token"))
            and exact_start_token(participants[name].get("current_start_token"))
            and exact_process_command(
                participants[name].get("initial_command"), expected_commands[name]
            )
            and participants[name].get("current_pid") not in initial_pids
            for name in restarted
        ),
        {
            "restarted": {name: participants.get(name) for name in sorted(restarted)},
            "failed_expired_cache_restart_pid": continuity.get(
                "failed_expired_cache_restart_pid"
            ),
            "expired_restart_evidence_pid": expired_restart.get("failed_pid"),
        },
    )
    check(
        "all six final participants remain owned non-zombie proof children with exact executables",
        positive_pid(proof_shell_pid)
        and unique_positive_pids(initial_pids, 6)
        and unique_positive_pids(current_pids, 6)
        and all(
            positive_pid(item.get("initial_pid"))
            and positive_pid(item.get("current_pid"))
            and item.get("initial_parent_pid") == proof_shell_pid
            and item.get("initial_alive") is True
            and item.get("initial_owned_child") is True
            and item.get("initial_non_zombie") is True
            and isinstance(item.get("initial_process_state"), str)
            and bool(item.get("initial_process_state"))
            and "Z" not in item.get("initial_process_state", "")
            and item.get("parent_pid") == proof_shell_pid
            and item.get("alive") is True
            and item.get("owned_child") is True
            and item.get("non_zombie") is True
            and isinstance(item.get("process_state"), str)
            and bool(item.get("process_state"))
            and "Z" not in item.get("process_state", "")
            and exact_start_token(item.get("initial_start_token"))
            and exact_start_token(item.get("current_start_token"))
            and exact_process_command(item.get("initial_command"), expected_commands[name])
            and exact_process_command(item.get("current_command"), expected_commands[name])
            and (
                (
                    item.get("initial_pid") == item.get("current_pid")
                    and item.get("initial_start_token") == item.get("current_start_token")
                )
                if name in stable
                else (
                    item.get("initial_pid") != item.get("current_pid")
                    and item.get("initial_start_token") != item.get("current_start_token")
                )
            )
            for name, item in participants.items()
        ),
        participants,
    )

    production_tests = production.get("tests")
    production_items = production_tests if isinstance(production_tests, list) else []
    check(
        "seven exact production regressions pass activation, receipt, remote/local clock, retry, and 304 cases",
        production.get("schema") == "inferlab.trust-expiry-production-tests.v0.27"
        and exact_int(production.get("test_count"), 7)
        and isinstance(production_tests, list)
        and len(production_tests) == 7
        and {item.get("test_filter") for item in production_items}
        == VALIDITY_TESTS
        and all(exact_cargo_test_item(item) for item in production_items)
        and hardening_negative_cases_rejected(),
        [
            {"command": item.get("command"), "exit_code": item.get("exit_code")}
            for item in production_items
        ],
    )

    final_statuses = final_cluster.get("statuses")
    check(
        "the healed three-control cluster has exactly one leader and retains revision 2",
        exact_three_control_cluster(final_cluster)
        and all(exact_committed_cpu_route(item) for item in final_statuses),
        final_cluster,
    )
    check(
        "the gateway retains the committed CPU route after generation-2 recovery",
        final_gateway.get("status", {}).get("status") == 200
        and exact_int(
            final_gateway.get("status", {}).get("body", {}).get("routing_snapshot", {}).get("control_revision"),
            2,
        )
        and [
            worker.get("id")
            for worker in final_gateway.get("status", {}).get("body", {}).get("workers", [])
        ]
        == ["cpu-trust-expiry"],
        final_gateway,
    )
    check(
        "real CPU JSON inference succeeds after valid generation-2 recovery",
        full_stack_json(final_request),
        final_request,
    )
    check(
        "real CPU SSE reaches the terminal DONE event after recovery",
        full_stack_stream(final_stream),
        final_stream,
    )

    retained_scan = direct_retained_scan(evidence)
    final_derived_files_exist = (
        (evidence / "assertions.json").exists()
        and (evidence / "trust-expiry-proof.svg").exists()
    )
    expected_private_inventory = (
        PRIVATE_SCAN_FINAL_FILES
        if final_derived_files_exist
        else PRIVATE_SCAN_PRELIMINARY_FILES
    )
    check(
        "the evidence sanitizer removed every proof/project path and certificate marker",
        sanitizer.get("schema") == "inferlab.evidence-sanitization.v0.27"
        and exact_int(sanitizer.get("remaining_sensitive_strings"), 0)
        and sanitizer.get("files_sanitized") == SANITIZER_INPUT_FILES
        and retained_scan["violations"] == []
        and retained_scan["unexpected_files"] == []
        and retained_scan["missing_files"] == []
        and manifest_valid_if_present(
            evidence, require_manifest=args.require_manifest
        )
        and hardening_negative_cases_rejected(),
        {"sanitizer": sanitizer, "direct_scan": retained_scan},
    )
    check(
        "offline scan finds no known Ed25519 seed and the proof-run scan reports no disposable PKI private-key payload",
        private_scan.get("schema") == "inferlab.private-material-scan.v0.27"
        and private_scan.get("files_scanned") == expected_private_inventory
        and private_scan.get("known_ed25519_seed_labels")
        == [
            "route_seed",
            "writer_seed",
            "trust_root_seed",
            "control_a_seed",
            "control_b_seed",
            "control_c_seed",
            "gateway_seed",
        ]
        and private_scan.get("known_ed25519_seed_count") == 7
        and private_scan.get("generated_pki_private_key_files")
        == [
            "ca.key",
            "control-a.key",
            "control-b.key",
            "control-c.key",
            "publisher.key",
            "server.key",
        ]
        and private_scan.get("generated_pki_private_key_count") == 6
        and private_scan.get("normalized_base64_and_escaped_newlines") is True
        and exact_int(private_scan.get("matches"), 0)
        and retained_scan["known_ed25519_seed_violations"] == [],
        {"proof_run_scan": private_scan, "offline_direct_scan": retained_scan},
    )

    passed = sum(item["passed"] for item in assertions)
    report = {
        "schema": "inferlab.trust-expiry-assertions.v0.27",
        "passed": passed,
        "total": len(assertions),
        "all_passed": passed == len(assertions),
        "assertions": assertions,
    }
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    if not report["all_passed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
