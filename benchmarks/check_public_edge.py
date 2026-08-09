#!/usr/bin/env python3
"""Deterministically evaluate retained v0.28 public-edge evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import re
import sys
import urllib.parse
from pathlib import Path
from typing import Any


EXPECTED_FILES = {
    "assertions.json",
    "authentication-rejections.json",
    "attempts-after-rejections-gateway.prom",
    "attempts-after-rejections-worker.prom",
    "attempts-before-rejections-gateway.prom",
    "attempts-before-rejections-worker.prom",
    "discarded-log-scan.json",
    "final-gateway.prom",
    "final-worker.prom",
    "input-rejections.json",
    "json-completion.json",
    "manifest.json",
    "operator-status-final.json",
    "private-material-scan.json",
    "process-continuity.json",
    "production-tests.json",
    "proof-contract.json",
    "public-edge-proof.svg",
    "rate-limit.json",
    "request-boundary.json",
    "route-isolation.json",
    "sanitizer.json",
    "sse-completion.json",
    "sse-disconnect-after-status.json",
    "sse-disconnect-during-status.json",
    "sse-disconnect.json",
    "startup-contract.json",
}
DERIVED_FILES = {
    "assertions.json",
    "manifest.json",
    "private-material-scan.json",
    "public-edge-proof.svg",
    "sanitizer.json",
}
REJECTION_REASONS = {
    "authentication",
    "body_too_large",
    "malformed_json",
    "invalid_messages",
    "too_many_messages",
    "prompt_too_large",
    "invalid_max_tokens",
    "max_output_tokens_exceeded",
    "rate_limited",
    "admission_full",
}
EXPECTED_REJECTION_COUNTS = {
    "authentication": 5,
    "body_too_large": 2,
    "malformed_json": 1,
    "invalid_messages": 2,
    "too_many_messages": 1,
    "prompt_too_large": 1,
    "invalid_max_tokens": 2,
    "max_output_tokens_exceeded": 1,
    "rate_limited": 2,
    "admission_full": 1,
}
PRODUCTION_TESTS = {
    "public_edge::tests::config_bounds_and_mode_are_explicit": "lib",
    "public_edge::tests::exact_burst_refill_and_slot_isolation_use_a_deterministic_clock": "lib",
    "public_edge::tests::input_policy_distinguishes_json_messages_prompt_and_output_limits": "lib",
    "metrics::tests::gateway_theoretical_series_stay_within_the_hard_target_budget": "lib",
    "worker_execution_admission_rejection_is_counted_as_zero_attempt_public_edge_work": "public_edge",
}
CARGO_ONE_TEST_SUMMARY = re.compile(
    r"^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; "
    r"[0-9]+ filtered out; finished in [0-9]+(?:\.[0-9]+)?s$"
)
SHA256 = re.compile(r"^[0-9a-f]{64}$")
LSTART = re.compile(
    r"^(?:Mon|Tue|Wed|Thu|Fri|Sat|Sun) "
    r"(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) +"
    r"(?:[1-9]|[12][0-9]|3[01]) (?:[01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9] "
    r"[0-9]{4}$"
)
TEACHING_PIECES = ["InferLab", " turns", " prompts", " into", " real", " tokens", "."]
HTTP_OBSERVATION_KEYS = {
    "schema",
    "kind",
    "method",
    "path",
    "status",
    "started_at_ms",
    "duration_ms",
    "headers",
    "body",
    "raw_response_scan",
}
SSE_OBSERVATION_KEYS = {
    "schema",
    "path",
    "status",
    "headers",
    "started_at_ms",
    "duration_ms",
    "event_count",
    "content_event_count",
    "distinct_read_offsets",
    "content_read_span_ms",
    "done",
    "eof_after_done",
    "disconnected_after_content",
    "release_handshake",
    "release_wait_ms",
    "events",
    "raw_response_scan",
}
SENSITIVE_JSON_FIELDS = {
    "id",
    "created",
    "system_fingerprint",
    "authorization",
    "api_key",
    "api_keys",
    "operator_api_key",
    "public_api_keys",
    "request_id",
    "x-inferlab-request-id",
    "credential_slot",
    "credential_slots",
    "credential_identity",
    "credential_id",
    "credential_hash",
    "credential_index",
    "credential_position",
    "credential_fingerprint",
    "key_hash",
    "key_fingerprint",
    "matched_credential",
    "matched_slot",
    "base_url",
    "worker_url",
}
HOST_PATH = re.compile(
    r"(?:/Users|/home|/private/var|/var/folders|/tmp|/workspace|/workspaces|"
    r"/github/workspace)/[^\s\"'<>]+"
)
PRIVATE_MARKERS = ("-----BEGIN", "PRIVATE KEY", "CERTIFICATE-----")
FIXED_FORBIDDEN_VALUES = {
    "operator_key": "edge-operator-admin-00000003",
    "prompt": "teach me streaming",
    "public_key_a": "edge-public-alpha-00000001",
    "public_key_b": "edge-public-bravo-00000002",
    "request_id_marker": "edge-proof-request-id-00000005",
    "wrong_key": "edge-wrong-credential-00000004",
}
FIXED_CREDENTIAL_HASHES = {
    f"{label}_sha256": hashlib.sha256(value.encode()).hexdigest()
    for label, value in FIXED_FORBIDDEN_VALUES.items()
    if label in {"operator_key", "public_key_a", "public_key_b", "wrong_key"}
}


def load(directory: Path, name: str) -> dict[str, Any]:
    with (directory / name).open(encoding="utf-8") as source:
        value = json.load(source)
    if not isinstance(value, dict):
        raise SystemExit(f"{name} must contain one JSON object")
    return value


def exact_int(value: Any, expected: int | None = None) -> bool:
    if isinstance(value, bool) or not isinstance(value, int):
        return False
    return expected is None or value == expected


def finite_number(value: Any) -> bool:
    return (
        not isinstance(value, bool)
        and isinstance(value, (int, float))
        and math.isfinite(float(value))
        and float(value) >= 0
    )


def exact_keys(value: Any, keys: set[str]) -> bool:
    return isinstance(value, dict) and set(value) == keys


def exact_int_map(value: Any, expected: dict[str, int]) -> bool:
    return (
        isinstance(value, dict)
        and set(value) == set(expected)
        and all(exact_int(value[name], count) for name, count in expected.items())
    )


def exact_projected_worker(value: Any, *, in_flight: int, executing: int) -> bool:
    return (
        exact_keys(value, {"concurrency_limit", "executing", "in_flight"})
        and exact_int(value.get("concurrency_limit"), 1)
        and exact_int(value.get("executing"), executing)
        and exact_int(value.get("in_flight"), in_flight)
    )


def exact_projection_attestation(attestation: Any) -> bool:
    return (
        exact_keys(
            attestation,
            {
                "full_status_scanned",
                "raw_top_level_schema_validated",
                "forbidden_field_matches",
                "forbidden_value_matches",
            },
        )
        and attestation.get("full_status_scanned") is True
        and attestation.get("raw_top_level_schema_validated") is True
        and exact_int(attestation.get("forbidden_field_matches"), 0)
        and exact_int(attestation.get("forbidden_value_matches"), 0)
    )


def exact_operator_projection(value: Any) -> bool:
    return (
        exact_keys(
            value, {"admission", "projection_attestation", "public_edge", "workers"}
        )
        and exact_projection_attestation(value.get("projection_attestation"))
    )


def exact_raw_response_scan(
    value: Any,
    *,
    completion_fields_projected: int,
    full_body_scanned: bool = True,
) -> bool:
    return (
        exact_keys(
            value,
            {
                "schema",
                "full_headers_scanned",
                "observed_body_scanned",
                "full_body_scanned",
                "canonical_request_id_headers_omitted",
                "completion_fields_projected",
                "forbidden_header_matches",
                "forbidden_body_field_matches",
                "forbidden_value_matches",
            },
        )
        and value.get("schema") == "inferlab.public-edge-raw-response-scan.v0.28"
        and value.get("full_headers_scanned") is True
        and value.get("observed_body_scanned") is True
        and value.get("full_body_scanned") is full_body_scanned
        and exact_int(value.get("canonical_request_id_headers_omitted"))
        and exact_int(value.get("canonical_request_id_headers_omitted"), 1)
        and exact_int(
            value.get("completion_fields_projected"), completion_fields_projected
        )
        and exact_int(value.get("forbidden_header_matches"), 0)
        and exact_int(value.get("forbidden_body_field_matches"), 0)
        and exact_int(value.get("forbidden_value_matches"), 0)
    )


def header(observation: Any, name: str) -> Any:
    if not isinstance(observation, dict):
        return None
    headers = observation.get("headers")
    return headers.get(name) if isinstance(headers, dict) else None


def attempts_zero(observation: Any) -> bool:
    return header(observation, "x-inferlab-attempts") == "0"


def error_body(observation: Any) -> dict[str, Any] | None:
    if not isinstance(observation, dict):
        return None
    body = observation.get("body")
    if not isinstance(body, dict) or set(body) != {"error"}:
        return None
    error = body.get("error")
    return error if isinstance(error, dict) else None


def exact_http_observation(observation: Any) -> bool:
    return (
        exact_keys(observation, HTTP_OBSERVATION_KEYS)
        and observation.get("schema") == "inferlab.public-edge-http-observation.v0.28"
        and isinstance(observation.get("kind"), str)
        and isinstance(observation.get("method"), str)
        and isinstance(observation.get("path"), str)
        and exact_int(observation.get("status"))
        and exact_int(observation.get("started_at_ms"))
        and observation["started_at_ms"] > 0
        and finite_number(observation.get("duration_ms"))
        and isinstance(observation.get("headers"), dict)
        and exact_raw_response_scan(
            observation.get("raw_response_scan"),
            completion_fields_projected=(
                3
                if observation.get("status") == 200
                and observation.get("method") == "POST"
                and observation.get("path") == "/v1/chat/completions"
                else 0
            ),
        )
    )


def exact_sse_observation(observation: Any) -> bool:
    return (
        exact_keys(observation, SSE_OBSERVATION_KEYS)
        and observation.get("schema") == "inferlab.public-edge-sse-observation.v0.28"
        and observation.get("path") == "/v1/chat/completions"
        and exact_int(observation.get("status"))
        and exact_int(observation.get("started_at_ms"))
        and observation["started_at_ms"] > 0
        and finite_number(observation.get("duration_ms"))
        and isinstance(observation.get("headers"), dict)
        and exact_int(observation.get("event_count"))
        and exact_int(observation.get("content_event_count"))
        and exact_int(observation.get("distinct_read_offsets"))
        and finite_number(observation.get("content_read_span_ms"))
        and isinstance(observation.get("done"), bool)
        and isinstance(observation.get("eof_after_done"), bool)
        and isinstance(observation.get("disconnected_after_content"), bool)
        and isinstance(observation.get("release_handshake"), bool)
        and finite_number(observation.get("release_wait_ms"))
        and isinstance(observation.get("events"), list)
        and exact_raw_response_scan(
            observation.get("raw_response_scan"),
            completion_fields_projected=(
                2
                * (
                    observation["event_count"]
                    - (1 if observation.get("done") is True else 0)
                )
                + (1 if observation.get("done") is True else 0)
            ),
            full_body_scanned=observation.get("disconnected_after_content") is False,
        )
    )


AUTH_ERROR = {
    "type": "authentication_error",
    "code": "invalid_api_key",
    "message": "A valid bearer API key is required.",
}
INPUT_ERRORS = {
    "fixed_oversize": (
        413,
        "body_too_large",
        "request body exceeds the 65536-byte limit",
    ),
    "chunked_oversize": (
        413,
        "body_too_large",
        "request body exceeds the 65536-byte limit",
    ),
    "malformed_json": (400, "malformed_json", "request body must be valid JSON"),
    "missing_messages": (
        400,
        "invalid_messages",
        "messages must be a nonempty array of string role/content objects",
    ),
    "invalid_message_content": (
        400,
        "invalid_messages",
        "messages must be a nonempty array of string role/content objects",
    ),
    "too_many_messages": (
        400,
        "too_many_messages",
        "messages exceed the configured limit",
    ),
    "prompt_too_large": (
        413,
        "prompt_too_large",
        "aggregate UTF-8 message content exceeds the configured limit",
    ),
    "invalid_max_tokens_zero": (
        400,
        "invalid_max_tokens",
        "max_tokens must be a positive integer",
    ),
    "invalid_max_tokens_string": (
        400,
        "invalid_max_tokens",
        "max_tokens must be a positive integer",
    ),
    "max_output_tokens_exceeded": (
        400,
        "max_output_tokens_exceeded",
        "max_tokens exceeds the configured output-token limit",
    ),
}


def exact_auth_rejection(observation: Any, *, require_attempts: bool = True) -> bool:
    return (
        exact_http_observation(observation)
        and observation.get("status") == 401
        and error_body(observation) == AUTH_ERROR
        and header(observation, "www-authenticate") == 'Bearer realm="inferlab"'
        and (
            attempts_zero(observation)
            if require_attempts
            else header(observation, "x-inferlab-attempts") is None
        )
    )


def exact_input_rejection(observation: Any, expected: tuple[int, str, str]) -> bool:
    status, code, message = expected
    return (
        exact_http_observation(observation)
        and observation.get("method") == "POST"
        and observation.get("path") == "/v1/chat/completions"
        and observation.get("status") == status
        and error_body(observation)
        == {"type": "invalid_request_error", "code": code, "message": message}
        and attempts_zero(observation)
    )


def parse_prometheus(text: str) -> dict[str, list[tuple[str, float]]]:
    samples: dict[str, list[tuple[str, float]]] = {}
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        fields = line.split()
        if len(fields) not in {2, 3}:
            raise ValueError(f"invalid sample line: {line}")
        token, raw_value = fields[0], fields[1]
        name = token.split("{", 1)[0]
        labels = token[len(name) :]
        value = float(raw_value)
        if not math.isfinite(value):
            raise ValueError(f"non-finite sample: {line}")
        samples.setdefault(name, []).append((labels, value))
    return samples


def scalar(samples: dict[str, list[tuple[str, float]]], name: str) -> float | None:
    values = samples.get(name)
    if values is None or len(values) != 1 or values[0][0] != "":
        return None
    return values[0][1]


def labeled(
    samples: dict[str, list[tuple[str, float]]], name: str, labels: str
) -> float | None:
    values = samples.get(name, [])
    matches = [value for observed_labels, value in values if observed_labels == labels]
    return matches[0] if len(matches) == 1 else None


def exact_production_test(item: Any) -> bool:
    if not isinstance(item, dict):
        return False
    test_filter = item.get("test_filter")
    output = item.get("output")
    if not isinstance(test_filter, str) or not isinstance(output, str):
        return False
    lines = output.splitlines()
    summaries = [line for line in lines if CARGO_ONE_TEST_SUMMARY.fullmatch(line)]
    target = PRODUCTION_TESTS.get(test_filter)
    target_args = ["--lib"] if target == "lib" else ["--test", str(target)]
    return (
        exact_keys(
            item,
            {
                "command",
                "environment",
                "test_filter",
                "exit_code",
                "running_one_test",
                "exact_test_line",
                "exact_summary",
                "summary_line",
                "output",
            },
        )
        and target is not None
        and item.get("command")
        == [
            "cargo",
            "test",
            "--locked",
            "-p",
            "gateway",
            *target_args,
            test_filter,
            "--",
            "--exact",
        ]
        and item.get("environment") == {"CARGO_TERM_COLOR": "never"}
        and exact_int(item.get("exit_code"), 0)
        and len(summaries) == 1
        and lines == ["running 1 test", f"test {test_filter} ... ok", summaries[0]]
        and item.get("running_one_test") is True
        and item.get("exact_test_line") is True
        and item.get("exact_summary") is True
        and item.get("summary_line") == summaries[0]
    )


def validate_manifest(directory: Path) -> tuple[bool, dict[str, Any]]:
    path = directory / "manifest.json"
    if not path.is_file():
        return False, {"error": "manifest missing"}
    raw = path.read_text(encoding="utf-8")
    try:
        manifest = json.loads(raw)
    except json.JSONDecodeError:
        return False, {"error": "manifest is not JSON"}
    if not exact_keys(
        manifest,
        {"schema", "expected_files", "file_count", "hashed_file_count", "files"},
    ):
        return False, {"error": "manifest top-level schema"}
    expected = sorted(EXPECTED_FILES)
    if (
        manifest.get("schema") != "inferlab.evidence-manifest.v0.28"
        or manifest.get("expected_files") != expected
        or manifest.get("file_count") != len(expected)
        or manifest.get("hashed_file_count") != len(expected) - 1
        or sorted(path.name for path in directory.iterdir()) != expected
    ):
        return False, {"error": "manifest inventory"}
    entries = manifest.get("files")
    if not isinstance(entries, list) or len(entries) != len(expected) - 1:
        return False, {"error": "manifest entries"}
    expected_hashed = [name for name in expected if name != "manifest.json"]
    if [item.get("path") for item in entries if isinstance(item, dict)] != expected_hashed:
        return False, {"error": "manifest entry order"}
    for entry in entries:
        if not exact_keys(entry, {"path", "sha256", "bytes"}):
            return False, {"error": "manifest entry schema"}
        name = entry["path"]
        content = (directory / name).read_bytes()
        if (
            not exact_int(entry["bytes"], len(content))
            or not isinstance(entry["sha256"], str)
            or not SHA256.fullmatch(entry["sha256"])
            or hashlib.sha256(content).hexdigest() != entry["sha256"]
        ):
            return False, {"error": f"manifest hash: {name}"}
    return True, {
        "file_count": len(expected),
        "hashed_file_count": len(expected) - 1,
        "manifest_bytes": len(raw.encode()),
    }


def sensitive_field_paths(value: Any, path: str = "$") -> list[str]:
    matches: list[str] = []
    if isinstance(value, dict):
        for key, item in value.items():
            child = f"{path}.{key}"
            if key.lower() in SENSITIVE_JSON_FIELDS:
                matches.append(child)
            matches.extend(sensitive_field_paths(item, child))
    elif isinstance(value, list):
        for index, item in enumerate(value):
            matches.extend(sensitive_field_paths(item, f"{path}[{index}]"))
    return matches


def direct_leak_scan(directory: Path) -> tuple[bool, dict[str, Any]]:
    violations: list[str] = []
    files = sorted(
        path for path in directory.iterdir() if path.is_file() and path.name != "manifest.json"
    )
    for path in files:
        raw = path.read_text(encoding="utf-8", errors="replace")
        if HOST_PATH.search(raw):
            violations.append(f"{path.name}:host-path")
        if any(marker in raw.upper() for marker in PRIVATE_MARKERS):
            violations.append(f"{path.name}:private-marker")
        for label, value in FIXED_FORBIDDEN_VALUES.items():
            if value in raw or urllib.parse.quote(value, safe="") in raw:
                violations.append(f"{path.name}:forbidden:{label}")
        for label, value in FIXED_CREDENTIAL_HASHES.items():
            if value in raw.lower():
                violations.append(f"{path.name}:forbidden:{label}")
        if path.suffix == ".json":
            try:
                value = json.loads(raw)
            except json.JSONDecodeError:
                violations.append(f"{path.name}:invalid-json")
            else:
                violations.extend(
                    f"{path.name}:sensitive-field:{field}"
                    for field in sensitive_field_paths(value)
                )
    return not violations, {"files_scanned": len(files), "violations": violations}


def evaluate(directory: Path, require_manifest: bool) -> dict[str, Any]:
    contract = load(directory, "proof-contract.json")
    startup = load(directory, "startup-contract.json")
    routes = load(directory, "route-isolation.json")
    authentication = load(directory, "authentication-rejections.json")
    inputs = load(directory, "input-rejections.json")
    rate = load(directory, "rate-limit.json")
    request_boundary = load(directory, "request-boundary.json")
    json_completion = load(directory, "json-completion.json")
    sse = load(directory, "sse-completion.json")
    disconnect = load(directory, "sse-disconnect.json")
    disconnect_during = load(directory, "sse-disconnect-during-status.json")
    disconnect_after = load(directory, "sse-disconnect-after-status.json")
    final_status = load(directory, "operator-status-final.json")
    processes = load(directory, "process-continuity.json")
    production = load(directory, "production-tests.json")
    discarded_log_scan = load(directory, "discarded-log-scan.json")
    sanitizer = load(directory, "sanitizer.json")
    private_scan = load(directory, "private-material-scan.json")

    prom_names = [
        "attempts-before-rejections-gateway.prom",
        "attempts-after-rejections-gateway.prom",
        "attempts-before-rejections-worker.prom",
        "attempts-after-rejections-worker.prom",
        "final-gateway.prom",
        "final-worker.prom",
    ]
    parsed_prom: dict[str, dict[str, list[tuple[str, float]]]] = {}
    prom_errors: list[str] = []
    for name in prom_names:
        try:
            parsed_prom[name] = parse_prometheus((directory / name).read_text(encoding="utf-8"))
        except (OSError, UnicodeError, ValueError) as error:
            prom_errors.append(f"{name}:{type(error).__name__}")

    assertions: list[dict[str, Any]] = []

    def check(name: str, passed: bool, observations: Any = None) -> None:
        item: dict[str, Any] = {"name": name, "passed": bool(passed)}
        if observations is not None:
            item["observations"] = observations
        assertions.append(item)

    config = contract.get("config")
    check(
        "the proof locks one hosted gateway with two public credentials and one operator credential",
        exact_keys(contract, {"schema", "version", "processes", "config"})
        and contract.get("schema") == "inferlab.public-edge-proof-contract.v0.28"
        and contract.get("version") == "0.28.0"
        and contract.get("processes") == ["cpu-worker", "gateway"]
        and config
        == {
            "credential_count": 2,
            "max_messages": 3,
            "max_output_tokens": 8,
            "max_prompt_bytes": 64,
            "max_request_bytes": 65536,
            "rate_burst": 2,
            "rate_requests_per_minute": 60,
        },
        config,
    )

    startup_cases = startup.get("cases")
    expected_startup = {
        "missing_public_keys": ([11180, 11181], 'Error: Custom { kind: InvalidInput, error: "hosted public edge requires explicit nonempty INFERLAB_PUBLIC_API_KEYS" }'),
        "bind_collision": ([11181], 'Error: Custom { kind: InvalidInput, error: "INFERLAB_BIND and INFERLAB_OPERATOR_BIND must not overlap" }'),
        "credential_overlap": ([11182, 11183], 'Error: Custom { kind: InvalidInput, error: "INFERLAB_OPERATOR_API_KEY must not match any INFERLAB_PUBLIC_API_KEYS entry" }'),
    }
    startup_ok = (
        exact_keys(startup, {"schema", "cases"})
        and startup.get("schema") == "inferlab.public-edge-startup-contract.v0.28"
        and isinstance(startup_cases, list)
        and [item.get("name") for item in startup_cases if isinstance(item, dict)]
        == list(expected_startup)
    )
    for item in startup_cases if isinstance(startup_cases, list) else []:
        name = item.get("name")
        expected_ports, expected_diagnostic = expected_startup.get(name, (None, None))
        startup_ok = startup_ok and (
            exact_keys(
                item,
                {
                    "name",
                    "exit_code",
                    "listener_ports",
                    "listener_ever_open_by_port",
                    "listener_ever_open",
                    "listener_poll_samples",
                    "process_exited",
                    "diagnostic",
                },
            )
            and name in expected_startup
            and exact_int(item.get("exit_code"))
            and item.get("exit_code") != 0
            and item.get("listener_ports") == expected_ports
            and item.get("listener_ever_open_by_port")
            == {str(port): False for port in expected_ports}
            and item.get("listener_ever_open") is False
            and exact_int(item.get("listener_poll_samples"))
            and item["listener_poll_samples"] >= 2
            and item.get("process_exited") is True
            and item.get("diagnostic") == expected_diagnostic
        )
    check(
        "hosted security misconfiguration fails before a listener is usable",
        startup_ok,
        {"cases": list(expected_startup)},
    )

    public_internal = routes.get("public_internal")
    public_internal_values = (
        list(public_internal.values()) if isinstance(public_internal, dict) else []
    )
    public_internal_surfaces = [
        (item.get("headers"), item.get("body"))
        for item in public_internal_values
        if isinstance(item, dict)
    ]
    expected_public_internal_kinds = {
        "missing": "public-internal-missing",
        "public": "public-internal-public",
        "operator": "public-internal-operator",
    }
    check(
        "the hosted public router returns exact route absence under missing authentication and both credential classes",
        exact_keys(
            routes,
            {
                "schema",
                "public_internal",
                "operator_internal",
                "public_open_routes",
                "public_showcase",
            },
        )
        and routes.get("schema") == "inferlab.public-edge-route-isolation.v0.28"
        and isinstance(public_internal, dict)
        and set(public_internal) == {"missing", "public", "operator"}
        and all(
            exact_http_observation(item)
            and item.get("status") == 404
            and item.get("path") == "/internal/workers"
            and item.get("method") == "GET"
            and item.get("kind") == expected_public_internal_kinds[name]
            and item.get("body") is None
            and item.get("headers") == {}
            for name, item in public_internal.items()
        )
        and len(public_internal_surfaces) == 3
        and public_internal_surfaces.count(public_internal_surfaces[0]) == 3,
        {key: value.get("status") for key, value in public_internal.items()}
        if isinstance(public_internal, dict)
        else None,
    )
    operator_internal = routes.get("operator_internal")
    check(
        "the operator listener accepts only the operator credential",
        isinstance(operator_internal, dict)
        and set(operator_internal) == {"missing", "public", "operator"}
        and exact_auth_rejection(operator_internal.get("missing"), require_attempts=False)
        and operator_internal["missing"].get("kind") == "operator-missing"
        and operator_internal["missing"].get("method") == "GET"
        and operator_internal["missing"].get("path") == "/internal/workers"
        and exact_auth_rejection(operator_internal.get("public"), require_attempts=False)
        and operator_internal["public"].get("kind") == "operator-public"
        and operator_internal["public"].get("method") == "GET"
        and operator_internal["public"].get("path") == "/internal/workers"
        and exact_http_observation(operator_internal.get("operator"))
        and operator_internal["operator"].get("status") == 200
        and operator_internal["operator"].get("kind") == "operator-authorized"
        and operator_internal["operator"].get("method") == "GET"
        and operator_internal["operator"].get("path") == "/internal/workers"
        and exact_operator_projection(operator_internal["operator"].get("body")),
        {key: value.get("status") for key, value in operator_internal.items()}
        if isinstance(operator_internal, dict)
        else None,
    )
    open_routes = routes.get("public_open_routes")
    check(
        "the exact hosted public non-completion route inventory remains reachable without internal diagnostics",
        isinstance(open_routes, list)
        and [item.get("path") for item in open_routes if isinstance(item, dict)]
        == ["/", "/assets/og-inferlab.png", "/health", "/readyz"]
        and all(
            exact_http_observation(item)
            and item.get("method") == "GET"
            and item.get("status") == 200
            and item.get("body") is None
            for item in open_routes
        ),
    )
    showcase = routes.get("public_showcase")
    showcase_public = showcase.get("public") if isinstance(showcase, dict) else None
    showcase_body = (
        showcase_public.get("body") if isinstance(showcase_public, dict) else None
    )
    check(
        "public showcase status requires a public credential and exposes only hosted mode from the edge policy",
        isinstance(showcase, dict)
        and set(showcase) == {"missing", "operator", "public"}
        and exact_auth_rejection(showcase.get("missing"), require_attempts=False)
        and showcase["missing"].get("kind") == "showcase-missing"
        and showcase["missing"].get("method") == "GET"
        and showcase["missing"].get("path") == "/showcase/status"
        and exact_auth_rejection(showcase.get("operator"), require_attempts=False)
        and showcase["operator"].get("kind") == "showcase-operator"
        and showcase["operator"].get("method") == "GET"
        and showcase["operator"].get("path") == "/showcase/status"
        and exact_http_observation(showcase_public)
        and showcase_public.get("status") == 200
        and showcase_public.get("kind") == "showcase-public"
        and showcase_public.get("method") == "GET"
        and showcase_public.get("path") == "/showcase/status"
        and exact_keys(
            showcase_body,
            {
                "projection_attestation",
                "public_api_authentication",
                "public_edge",
                "release",
                "routing_policy",
                "routing_snapshot",
                "worker_count",
            },
        )
        and exact_projection_attestation(showcase_body.get("projection_attestation"))
        and showcase_body.get("public_api_authentication")
        == {"enabled": True, "key_count": 2}
        and showcase_body.get("public_edge") == {"mode": "hosted"}
        and showcase_body.get("release") == {"version": "0.28.0"}
        and showcase_body.get("routing_policy") == "round-robin"
        and exact_int(showcase_body.get("worker_count"), 1)
        and showcase_body.get("routing_snapshot")
        == {
            "control_cluster_id": None,
            "control_revision": None,
            "control_signing_key_id": None,
            "control_term": None,
        },
    )

    auth_cases = authentication.get("cases")
    expected_auth_kinds = {
        "missing": "auth-missing",
        "missing_oversize": "auth-missing-oversize",
        "wrong": "auth-wrong",
        "wrong_scheme": "auth-wrong-scheme",
        "duplicate": "duplicate-authorization",
    }
    check(
        "missing wrong-scheme wrong and duplicate public authentication share one exact redacted 401",
        exact_keys(authentication, {"schema", "cases"})
        and authentication.get("schema")
        == "inferlab.public-edge-authentication-rejections.v0.28"
        and isinstance(auth_cases, dict)
        and set(auth_cases)
        == {"missing", "missing_oversize", "wrong", "wrong_scheme", "duplicate"}
        and all(
            exact_auth_rejection(item)
            and item.get("kind") == expected_auth_kinds[name]
            and item.get("method") == "POST"
            and item.get("path") == "/v1/chat/completions"
            for name, item in auth_cases.items()
        ),
        {key: value.get("status") for key, value in auth_cases.items()}
        if isinstance(auth_cases, dict)
        else None,
    )

    input_cases = inputs.get("cases")
    expected_input_kinds = {
        name: ("chunked-oversize" if name == "chunked_oversize" else f"input-{name}")
        for name in INPUT_ERRORS
    }
    check(
        "fixed-length and chunked bodies share the exact 65536-byte enforcement boundary",
        exact_keys(inputs, {"schema", "cases"})
        and inputs.get("schema") == "inferlab.public-edge-input-rejections.v0.28"
        and isinstance(input_cases, dict)
        and exact_input_rejection(input_cases.get("fixed_oversize"), INPUT_ERRORS["fixed_oversize"])
        and input_cases["fixed_oversize"].get("kind")
        == expected_input_kinds["fixed_oversize"]
        and exact_input_rejection(
            input_cases.get("chunked_oversize"), INPUT_ERRORS["chunked_oversize"]
        )
        and input_cases["chunked_oversize"].get("kind")
        == expected_input_kinds["chunked_oversize"],
    )
    check(
        "every edge-owned JSON message prompt and output-token violation has its exact finite response",
        isinstance(input_cases, dict)
        and set(input_cases) == set(INPUT_ERRORS)
        and all(
            exact_input_rejection(input_cases.get(name), expected)
            and input_cases[name].get("kind") == expected_input_kinds[name]
            for name, expected in INPUT_ERRORS.items()
        ),
        {name: input_cases.get(name, {}).get("status") for name in INPUT_ERRORS}
        if isinstance(input_cases, dict)
        else None,
    )

    before_gateway = parsed_prom.get("attempts-before-rejections-gateway.prom", {})
    after_gateway = parsed_prom.get("attempts-after-rejections-gateway.prom", {})
    before_worker = parsed_prom.get("attempts-before-rejections-worker.prom", {})
    after_worker = parsed_prom.get("attempts-after-rejections-worker.prom", {})
    before_gateway_attempts = scalar(before_gateway, "inferlab_gateway_attempts_total")
    after_gateway_attempts = scalar(after_gateway, "inferlab_gateway_attempts_total")
    before_worker_requests = scalar(before_worker, "inferlab_worker_requests_total")
    after_worker_requests = scalar(after_worker, "inferlab_worker_requests_total")
    check(
        "all finite authentication body and input rejections produce zero compute-boundary delta",
        not prom_errors
        and before_gateway_attempts is not None
        and before_gateway_attempts == 0.0
        and before_gateway_attempts == after_gateway_attempts
        and before_worker_requests is not None
        and before_worker_requests == 0.0
        and before_worker_requests == after_worker_requests,
        {
            "gateway_before": before_gateway_attempts,
            "gateway_after": after_gateway_attempts,
            "worker_before": before_worker_requests,
            "worker_after": after_worker_requests,
            "parse_errors": prom_errors,
        },
    )

    check(
        "an authenticated JSON body of exactly 65536 decoded bytes is accepted at the inclusive wire boundary",
        exact_keys(request_boundary, {"schema", "request_body_bytes", "observation"})
        and request_boundary.get("schema")
        == "inferlab.public-edge-request-boundary.v0.28"
        and exact_int(request_boundary.get("request_body_bytes"), 65536)
        and exact_http_observation(request_boundary.get("observation"))
        and request_boundary["observation"].get("kind") == "exact-request-boundary"
        and request_boundary["observation"].get("method") == "POST"
        and request_boundary["observation"].get("path") == "/v1/chat/completions"
        and request_boundary["observation"].get("status") == 200
        and header(request_boundary["observation"], "x-inferlab-attempts") == "1"
        and header(request_boundary["observation"], "x-inferlab-worker")
        == "cpu-worker-edge",
    )

    rate_cases = rate.get("cases")
    limited = rate_cases.get("a_limited") if isinstance(rate_cases, dict) else None
    limited_error = error_body(limited)
    check(
        "credential A spends exactly the configured two-request burst before a finite 429",
        exact_keys(
            rate,
            {
                "schema",
                "rate_requests_per_minute",
                "rate_burst",
                "limited_completed_offset_ms",
                "refilled_started_offset_ms",
                "refill_wait_ms",
                "case_start_offsets_ms",
                "cases",
            },
        )
        and rate.get("schema") == "inferlab.public-edge-rate-limit.v0.28"
        and exact_int(rate.get("rate_requests_per_minute"), 60)
        and exact_int(rate.get("rate_burst"), 2)
        and isinstance(rate_cases, dict)
        and set(rate_cases)
        == {"a_first", "a_second", "a_limited", "b_independent", "a_refilled"}
        and isinstance(rate.get("case_start_offsets_ms"), dict)
        and set(rate["case_start_offsets_ms"])
        == {"a_first", "a_second", "a_limited", "b_independent", "a_refilled"}
        and all(finite_number(value) for value in rate["case_start_offsets_ms"].values())
        and [
            rate["case_start_offsets_ms"][name]
            for name in ["a_first", "a_second", "a_limited", "b_independent", "a_refilled"]
        ]
        == sorted(rate["case_start_offsets_ms"].values())
        and exact_http_observation(rate_cases["a_first"])
        and exact_http_observation(rate_cases["a_second"])
        and all(
            item.get("method") == "POST"
            and item.get("path") == "/v1/chat/completions"
            and item.get("kind") == "rate-sequence"
            for item in rate_cases.values()
        )
        and rate_cases["a_first"].get("status") == 200
        and rate_cases["a_second"].get("status") == 200
        and header(rate_cases["a_first"], "x-inferlab-attempts") == "1"
        and header(rate_cases["a_second"], "x-inferlab-attempts") == "1"
        and exact_http_observation(limited)
        and limited.get("status") == 429
        and limited_error
        == {
            "type": "invalid_request_error",
            "code": "rate_limited",
            "message": "public credential request rate exceeded",
        }
        and attempts_zero(limited)
        and header(limited, "retry-after") == "1",
        {key: value.get("status") for key, value in rate_cases.items()}
        if isinstance(rate_cases, dict)
        else None,
    )
    check(
        "the second public credential has an isolated bucket",
        isinstance(rate_cases, dict)
        and exact_http_observation(rate_cases.get("b_independent"))
        and rate_cases.get("b_independent", {}).get("status") == 200
        and header(rate_cases.get("b_independent"), "x-inferlab-attempts") == "1",
    )
    check(
        "credential A succeeds after one observed refill interval",
        isinstance(rate_cases, dict)
        and finite_number(rate.get("refill_wait_ms"))
        and 1_000 <= rate["refill_wait_ms"] <= 3_000
        and exact_http_observation(rate_cases.get("a_refilled"))
        and rate_cases.get("a_refilled", {}).get("status") == 200
        and header(rate_cases.get("a_refilled"), "x-inferlab-attempts") == "1"
        and finite_number(rate.get("limited_completed_offset_ms"))
        and finite_number(rate.get("refilled_started_offset_ms"))
        and abs(
            rate["refill_wait_ms"]
            - (
                rate["refilled_started_offset_ms"]
                - rate["limited_completed_offset_ms"]
            )
        )
        < 0.002,
        {"refill_wait_ms": rate.get("refill_wait_ms")},
    )

    check(
        "a real CPU JSON completion crosses the hosted edge",
        exact_http_observation(json_completion)
        and json_completion.get("method") == "POST"
        and json_completion.get("path") == "/v1/chat/completions"
        and json_completion.get("status") == 200
        and header(json_completion, "x-inferlab-attempts") == "1"
        and header(json_completion, "x-inferlab-worker") == "cpu-worker-edge"
        and isinstance(json_completion.get("body"), dict)
        and json_completion["body"].get("object") == "chat.completion"
        and isinstance(json_completion["body"].get("choices"), list)
        and len(json_completion["body"]["choices"]) == 1
        and json_completion["body"]["choices"][0].get("message", {}).get("content")
        == "InferLab turns prompts into real tokens."
        and json_completion["body"]["choices"][0].get("finish_reason") == "stop"
        and "id" not in json_completion["body"]
        and finite_number(json_completion.get("duration_ms")),
        {"duration_ms": json_completion.get("duration_ms")},
    )

    sse_events = sse.get("events")
    sse_event_list = sse_events if isinstance(sse_events, list) else []
    pieces = [
        item.get("content")
        for item in sse_event_list
        if isinstance(item, dict) and isinstance(item.get("content"), str) and item["content"]
    ]
    content_events = [
        item
        for item in sse_event_list
        if isinstance(item, dict) and isinstance(item.get("content"), str) and item["content"]
    ]
    noncontent_events = [
        item
        for item in sse_event_list
        if isinstance(item, dict)
        and set(item) == {"offset_ms", "content", "finish_reason"}
        and item.get("content") is None
    ]
    done_events = [
        item
        for item in sse_event_list
        if isinstance(item, dict) and set(item) == {"offset_ms", "done"}
    ]
    check(
        "a real CPU SSE is observed incrementally and reaches DONE",
        exact_sse_observation(sse)
        and sse.get("status") == 200
        and header(sse, "x-inferlab-attempts") == "1"
        and header(sse, "x-inferlab-worker") == "cpu-worker-edge"
        and sse.get("done") is True
        and sse.get("eof_after_done") is True
        and sse.get("disconnected_after_content") is False
        and sse.get("release_handshake") is False
        and sse.get("release_wait_ms") == 0
        and sse.get("content_event_count") == len(TEACHING_PIECES)
        and sse.get("event_count") == len(sse_event_list)
        and isinstance(sse_events, list)
        and sse.get("distinct_read_offsets") == len(sse_event_list)
        and finite_number(sse.get("content_read_span_ms"))
        and sse["content_read_span_ms"] >= 300
        and pieces == TEACHING_PIECES
        and "".join(pieces) == "InferLab turns prompts into real tokens."
        and len(content_events) == len(TEACHING_PIECES)
        and all(
            set(item) == {"offset_ms", "content", "finish_reason"}
            and item.get("finish_reason") is None
            for item in content_events
        )
        and len(noncontent_events) == 2
        and [item.get("finish_reason") for item in noncontent_events] == [None, "stop"]
        and sse_event_list[0] is noncontent_events[0]
        and len(done_events) == 1
        and done_events[0].get("done") is True
        and all(isinstance(item, dict) for item in sse_event_list)
        and all(finite_number(item.get("offset_ms")) for item in sse_event_list)
        and [item["offset_ms"] for item in sse_event_list]
        == sorted(item["offset_ms"] for item in sse_event_list)
        and len({item["offset_ms"] for item in sse_event_list}) == len(sse_event_list)
        and abs(
            sse["content_read_span_ms"]
            - (
                [item["offset_ms"] for item in sse_event_list if item.get("content")][-1]
                - [item["offset_ms"] for item in sse_event_list if item.get("content")][0]
            )
        )
        < 0.002
        and sse_event_list[-1].get("done") is True,
        {
            "duration_ms": sse.get("duration_ms"),
            "content_event_count": sse.get("content_event_count"),
            "distinct_read_offsets": sse.get("distinct_read_offsets"),
            "content_read_span_ms": sse.get("content_read_span_ms"),
        },
    )

    during_body = disconnect_during.get("body")
    after_body = disconnect_after.get("body")
    during_admission = during_body.get("admission") if isinstance(during_body, dict) else None
    after_admission = after_body.get("admission") if isinstance(after_body, dict) else None
    during_workers = during_body.get("workers") if isinstance(during_body, dict) else None
    after_workers = after_body.get("workers") if isinstance(after_body, dict) else None
    disconnect_stream = disconnect.get("stream")
    admission_case = disconnect.get("admission_full")
    timeline = disconnect.get("timeline")
    timeline_names = [
        "stream_probe_started_ns",
        "content_ready_ns",
        "during_status_started_ns",
        "during_status_completed_ns",
        "admission_started_ns",
        "admission_completed_ns",
        "release_signaled_ns",
        "stream_probe_completed_ns",
        "after_status_started_ns",
        "after_status_completed_ns",
        "first_started_ns",
        "first_completed_ns",
        "limited_started_ns",
        "limited_completed_ns",
    ]
    exact_timeline = (
        isinstance(timeline, dict)
        and set(timeline) == set(timeline_names)
        and all(exact_int(timeline.get(name)) and timeline[name] > 0 for name in timeline_names)
        and [timeline[name] for name in timeline_names]
        == sorted(timeline[name] for name in timeline_names)
        and len({timeline[name] for name in timeline_names}) == len(timeline_names)
    )
    check(
        "an open SSE owns the bounded permits and a concurrent valid request is rejected before a worker attempt",
        exact_keys(
            disconnect,
            {
                "schema",
                "admission_to_second_request_ms",
                "timeline",
                "stream",
                "admission_full",
                "after_release_first",
                "after_release_limited",
            },
        )
        and disconnect.get("schema") == "inferlab.public-edge-disconnect.v0.28"
        and exact_sse_observation(disconnect_stream)
        and disconnect_stream.get("status") == 200
        and header(disconnect_stream, "x-inferlab-attempts") == "1"
        and header(disconnect_stream, "x-inferlab-worker") == "cpu-worker-edge"
        and disconnect_stream.get("disconnected_after_content") is True
        and disconnect_stream.get("done") is False
        and disconnect_stream.get("eof_after_done") is False
        and disconnect_stream.get("release_handshake") is True
        and disconnect_stream.get("release_wait_ms", 0) > 0
        and disconnect_stream.get("content_event_count") == 1
        and disconnect_stream.get("event_count") == 2
        and isinstance(disconnect_stream.get("events"), list)
        and len(disconnect_stream["events"]) == 2
        and disconnect_stream["events"][0]
        == {
            "offset_ms": disconnect_stream["events"][0].get("offset_ms"),
            "content": None,
            "finish_reason": None,
        }
        and disconnect_stream["events"][1]
        == {
            "offset_ms": disconnect_stream["events"][1].get("offset_ms"),
            "content": "InferLab",
            "finish_reason": None,
        }
        and all(
            finite_number(item.get("offset_ms"))
            for item in disconnect_stream["events"]
        )
        and disconnect_stream["events"][0]["offset_ms"]
        < disconnect_stream["events"][1]["offset_ms"]
        and disconnect_stream.get("content_read_span_ms") == 0
        and disconnect_stream.get("duration_ms", 0) + 0.01
        >= disconnect_stream["events"][1]["offset_ms"]
        + disconnect_stream["release_wait_ms"]
        and exact_timeline
        and exact_http_observation(disconnect_during)
        and disconnect_during.get("status") == 200
        and disconnect_during.get("kind") == "disconnect-during"
        and disconnect_during.get("method") == "GET"
        and disconnect_during.get("path") == "/internal/workers"
        and exact_operator_projection(during_body)
        and isinstance(during_admission, dict)
        and exact_int(during_admission.get("outstanding"), 1)
        and exact_int(during_admission.get("executing"), 1)
        and exact_int(during_admission.get("queued"), 0)
        and isinstance(during_workers, list)
        and len(during_workers) == 1
        and exact_projected_worker(during_workers[0], in_flight=1, executing=1)
        and exact_http_observation(admission_case)
        and admission_case.get("status") == 429
        and admission_case.get("kind") == "admission-full"
        and admission_case.get("method") == "POST"
        and admission_case.get("path") == "/v1/chat/completions"
        and error_body(admission_case)
        == {
            "type": "gateway_overloaded",
            "reason": "admission_queue_full",
            "message": "gateway execution and waiting capacity are full",
            "retryable": True,
        }
        and header(admission_case, "retry-after") == "1"
        and attempts_zero(admission_case),
        {
            "during_outstanding": during_admission.get("outstanding")
            if isinstance(during_admission, dict)
            else None,
            "during_executing": during_admission.get("executing")
            if isinstance(during_admission, dict)
            else None,
        },
    )
    check(
        "disconnect drops SSE ownership back to idle without restarting the gateway",
        exact_http_observation(disconnect_after)
        and disconnect_after.get("status") == 200
        and disconnect_after.get("kind") == "disconnect-after"
        and disconnect_after.get("method") == "GET"
        and disconnect_after.get("path") == "/internal/workers"
        and exact_operator_projection(after_body)
        and isinstance(after_admission, dict)
        and exact_int(after_admission.get("outstanding"), 0)
        and exact_int(after_admission.get("executing"), 0)
        and exact_int(after_admission.get("queued"), 0)
        and isinstance(after_workers, list)
        and len(after_workers) == 1
        and exact_projected_worker(after_workers[0], in_flight=0, executing=0),
    )
    after_release_first = disconnect.get("after_release_first")
    after_release_limited = disconnect.get("after_release_limited")
    check(
        "an admission-full request is charged once and is not refunded after the stream releases",
        exact_http_observation(after_release_first)
        and after_release_first.get("status") == 200
        and after_release_first.get("kind") == "no-refund-first"
        and after_release_first.get("method") == "POST"
        and after_release_first.get("path") == "/v1/chat/completions"
        and header(after_release_first, "x-inferlab-attempts") == "1"
        and exact_input_rejection(
            after_release_limited,
            (429, "rate_limited", "public credential request rate exceeded"),
        )
        and after_release_limited.get("kind") == "no-refund-limited"
        and after_release_limited.get("method") == "POST"
        and after_release_limited.get("path") == "/v1/chat/completions"
        and header(after_release_limited, "retry-after") == "1"
        and finite_number(disconnect.get("admission_to_second_request_ms"))
        and 0 < disconnect["admission_to_second_request_ms"] < 1_000
        and exact_timeline
        and abs(
            disconnect["admission_to_second_request_ms"]
            - (
                timeline["limited_started_ns"] - timeline["admission_completed_ns"]
            )
            / 1_000_000
        )
        < 0.001,
    )

    status_body = final_status.get("body")
    public_edge = status_body.get("public_edge") if isinstance(status_body, dict) else None
    rejection_counts = public_edge.get("rejections") if isinstance(public_edge, dict) else None
    check(
        "operator status exposes only the aggregate credential count exact bounds and finite rejection reasons",
        exact_http_observation(final_status)
        and final_status.get("status") == 200
        and final_status.get("kind") == "final-operator-status"
        and final_status.get("method") == "GET"
        and final_status.get("path") == "/internal/workers"
        and exact_operator_projection(status_body)
        and isinstance(public_edge, dict)
        and set(public_edge)
        == {
            "mode",
            "enforced",
            "max_request_bytes",
            "max_messages",
            "max_prompt_bytes",
            "max_output_tokens",
            "rate_requests_per_minute",
            "rate_burst",
            "credential_count",
            "rejections",
        }
        and public_edge["mode"] == "hosted"
        and public_edge["enforced"] is True
        and exact_int(public_edge["max_request_bytes"], 65536)
        and exact_int(public_edge["max_messages"], 3)
        and exact_int(public_edge["max_prompt_bytes"], 64)
        and exact_int(public_edge["max_output_tokens"], 8)
        and exact_int(public_edge["rate_requests_per_minute"], 60)
        and exact_int(public_edge["rate_burst"], 2)
        and exact_int(public_edge["credential_count"], 2)
        and exact_int_map(rejection_counts, EXPECTED_REJECTION_COUNTS),
        public_edge,
    )

    final_gateway = parsed_prom.get("final-gateway.prom", {})
    final_scalar = scalar(final_gateway, "inferlab_gateway_public_edge_rejections_total")
    initial_scalar = scalar(before_gateway, "inferlab_gateway_public_edge_rejections_total")
    check(
        "the hosted scalar rejection metric equals the fixed status-counter sum without labels",
        final_scalar == float(sum(EXPECTED_REJECTION_COUNTS.values()))
        and isinstance(rejection_counts, dict)
        and final_scalar == float(sum(rejection_counts.values()))
        and initial_scalar == 0.0,
        {
            "initial_metric": initial_scalar,
            "final_metric": final_scalar,
            "status_sum": sum(rejection_counts.values())
            if isinstance(rejection_counts, dict)
            else None,
        },
    )

    final_worker = parsed_prom.get("final-worker.prom", {})
    final_gateway_attempts = scalar(final_gateway, "inferlab_gateway_attempts_total")
    final_worker_requests = scalar(final_worker, "inferlab_worker_requests_total")
    check(
        "every rate and admission rejection remains before compute while nine accepted requests reach the real worker",
        final_gateway_attempts == 9.0 and final_worker_requests == 9.0,
        {
            "gateway_attempts": final_gateway_attempts,
            "worker_requests": final_worker_requests,
        },
    )
    completion_counts = {
        outcome: labeled(
            final_gateway,
            "inferlab_gateway_completion_duration_seconds_count",
            f'{{outcome="{outcome}"}}',
        )
        for outcome in ("success", "cancelled", "error", "deadline")
    }
    check(
        "eight drained response bodies complete successfully while the one deliberate early SSE close is cancelled",
        completion_counts
        == {"success": 8.0, "cancelled": 1.0, "error": 0.0, "deadline": 0.0},
        completion_counts,
    )

    production_items = production.get("tests")
    check(
        "five exact production tests prove mode bounds token arithmetic edge validation admission wrapping and the 256-series ceiling",
        exact_keys(production, {"schema", "test_count", "tests"})
        and production.get("schema") == "inferlab.public-edge-production-tests.v0.28"
        and exact_int(production.get("test_count"), len(PRODUCTION_TESTS))
        and isinstance(production_items, list)
        and len(production_items) == len(PRODUCTION_TESTS)
        and {item.get("test_filter") for item in production_items if isinstance(item, dict)}
        == set(PRODUCTION_TESTS)
        and all(exact_production_test(item) for item in production_items),
        {"tests": sorted(PRODUCTION_TESTS)},
    )

    process_items = processes.get("processes")
    process_ok = (
        exact_keys(processes, {"schema", "proof_shell_pid", "processes"})
        and processes.get("schema") == "inferlab.public-edge-process-continuity.v0.28"
        and exact_int(processes.get("proof_shell_pid"))
        and processes["proof_shell_pid"] > 1
        and isinstance(process_items, dict)
        and set(process_items) == {"cpu-worker", "gateway"}
    )
    observed_pids: list[int] = []
    for name, item in process_items.items() if isinstance(process_items, dict) else []:
        process_ok = process_ok and (
            exact_keys(
                item,
                {
                    "initial_pid",
                    "current_pid",
                    "same_pid",
                    "initial_start_token",
                    "current_start_token",
                    "same_start_token",
                    "initial_command",
                    "current_command",
                    "same_command",
                    "initial_parent_pid",
                    "current_parent_pid",
                    "alive",
                    "owned_child",
                    "non_zombie",
                },
            )
            and exact_int(item.get("initial_pid"))
            and item["initial_pid"] > 1
            and exact_int(item.get("current_pid"), item["initial_pid"])
            and item.get("same_pid") is True
            and isinstance(item.get("initial_start_token"), str)
            and LSTART.fullmatch(item["initial_start_token"]) is not None
            and item.get("current_start_token") == item["initial_start_token"]
            and item.get("same_start_token") is True
            and isinstance(item.get("initial_command"), str)
            and item.get("current_command") == item["initial_command"]
            and item.get("same_command") is True
            and item.get("initial_parent_pid") == processes["proof_shell_pid"]
            and item.get("current_parent_pid") == processes["proof_shell_pid"]
            and item.get("alive") is True
            and item.get("owned_child") is True
            and item.get("non_zombie") is True
            and item["initial_command"].split()[0].endswith(
                "/cpu-worker" if name == "cpu-worker" else "/gateway"
            )
        )
        if isinstance(item, dict) and exact_int(item.get("initial_pid")):
            observed_pids.append(item["initial_pid"])
    process_ok = process_ok and len(set(observed_pids)) == 2
    check(
        "the same exact gateway and real CPU worker processes remain owned live and non-zombie",
        process_ok,
        {"process_count": len(observed_pids), "unique_pids": len(set(observed_pids))},
    )

    check(
        "discarded startup and runtime logs are scanned before deletion without treating canonical request IDs as secrets",
        exact_keys(
            discarded_log_scan,
            {
                "schema",
                "startup_files_scanned",
                "runtime_files_scanned",
                "credential_count",
                "credential_encodings_checked",
                "prompt_checked_in_runtime_logs",
                "credential_position_checked_in_all_logs",
                "host_path_checked_in_startup_logs",
                "private_marker_checks",
                "request_ids_allowed_in_runtime_logs",
                "violations",
            },
        )
        and discarded_log_scan.get("schema")
        == "inferlab.public-edge-discarded-log-scan.v0.28"
        and discarded_log_scan.get("startup_files_scanned")
        == [
            "startup-bind_collision.log",
            "startup-credential_overlap.log",
            "startup-missing_public_keys.log",
        ]
        and discarded_log_scan.get("runtime_files_scanned")
        == ["cpu-worker.log", "gateway.log"]
        and exact_int(discarded_log_scan.get("credential_count"), 4)
        and discarded_log_scan.get("credential_encodings_checked")
        == ["literal", "percent-encoded", "sha256"]
        and discarded_log_scan.get("prompt_checked_in_runtime_logs") is True
        and discarded_log_scan.get("credential_position_checked_in_all_logs") is True
        and discarded_log_scan.get("host_path_checked_in_startup_logs") is True
        and exact_int(discarded_log_scan.get("private_marker_checks"), 3)
        and discarded_log_scan.get("request_ids_allowed_in_runtime_logs") is True
        and exact_int(discarded_log_scan.get("violations"), 0),
    )

    check(
        "sanitized retained evidence contains no secret prompt request identifier private marker or host path",
        exact_keys(
            sanitizer,
            {
                "schema",
                "files_scanned",
                "forbidden_value_labels",
                "forbidden_value_count",
                "host_path_patterns_checked",
                "private_marker_checks",
                "sensitive_json_fields_checked",
                "violations",
            },
        )
        and sanitizer.get("schema") == "inferlab.public-edge-sanitizer.v0.28"
        and sanitizer.get("violations") == 0
        and sanitizer.get("forbidden_value_count") == 6
        and sanitizer.get("forbidden_value_labels")
        == [
            "operator_key",
            "prompt",
            "public_key_a",
            "public_key_b",
            "request_id_marker",
            "wrong_key",
        ]
        and sanitizer.get("host_path_patterns_checked") is True
        and sanitizer.get("private_marker_checks") == 3
        and sanitizer.get("sensitive_json_fields_checked")
        == sorted(SENSITIVE_JSON_FIELDS)
        and sanitizer.get("files_scanned")
        == sorted(EXPECTED_FILES - {"manifest.json", "sanitizer.json"})
        and exact_keys(
            private_scan,
            {
                "schema",
                "files_scanned",
                "credential_count",
                "encodings_checked",
                "private_marker_checks",
                "path_patterns_checked",
                "matches",
            },
        )
        and private_scan.get("schema") == "inferlab.private-material-scan.v0.28"
        and private_scan.get("matches") == 0
        and private_scan.get("credential_count") == 3
        and private_scan.get("encodings_checked")
        == ["literal", "percent-encoded", "sha256"]
        and private_scan.get("private_marker_checks") == 3
        and private_scan.get("path_patterns_checked") is True
        and private_scan.get("files_scanned")
        == sorted(EXPECTED_FILES - {"manifest.json", "private-material-scan.json"}),
        {
            "sanitizer_files": len(sanitizer.get("files_scanned", [])),
            "private_scan_files": len(private_scan.get("files_scanned", [])),
        },
    )

    direct_scan_ok, direct_scan_observations = direct_leak_scan(directory)
    check(
        "the checker independently rejects sensitive fields private markers and host paths in final retained bytes",
        direct_scan_ok,
        direct_scan_observations,
    )

    manifest_ok, _manifest_observations = validate_manifest(directory)
    check(
        "the exact evidence inventory is hash-bound by a completion manifest written last",
        manifest_ok
        if require_manifest
        else not (directory / "manifest.json").exists() or manifest_ok,
    )

    passed = sum(1 for item in assertions if item["passed"])
    return {
        "schema": "inferlab.public-edge-assertions.v0.28",
        "passed": passed,
        "total": len(assertions),
        "all_passed": passed == len(assertions),
        "assertions": assertions,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--require-manifest", action="store_true")
    args = parser.parse_args()
    report = evaluate(args.evidence_dir, args.require_manifest)
    encoded = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.write_text(encoded, encoding="utf-8")
    else:
        sys.stdout.write(encoded)
    if not report["all_passed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
