#!/usr/bin/env python3
"""Standard-library probes for the v0.28 public-edge proof.

The tool deliberately disables ambient proxies, allowlists retained response
headers, removes generated completion/request identifiers, and never accepts a
credential value on the command line. Credential values are read from named
environment variables so ordinary proof output cannot echo them.
"""

from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import os
import re
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any


MAX_RETAINED_RESPONSE_BYTES = 1024 * 1024
MAX_DISCARDED_RESPONSE_BYTES = 4 * 1024 * 1024
RETAINED_HEADERS = {
    "content-type",
    "retry-after",
    "www-authenticate",
    "x-inferlab-attempts",
    "x-inferlab-worker",
}
COMPLETION_DYNAMIC_FIELDS = {
    "id",
    "created",
    "system_fingerprint",
}
SENSITIVE_FIELD_NAMES = {
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
PROOF_FORBIDDEN_VALUES = (
    "edge-public-alpha-00000001",
    "edge-public-bravo-00000002",
    "edge-operator-admin-00000003",
    "edge-wrong-credential-00000004",
    "teach me streaming",
    "edge-proof-request-id-00000005",
)
OPERATOR_STATUS_FORBIDDEN_FIELDS = {
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
}
PROOF_FORBIDDEN_HASHES = tuple(
    hashlib.sha256(value.encode()).hexdigest() for value in PROOF_FORBIDDEN_VALUES[:4]
)
EXPECTED_OPERATOR_STATUS_FIELDS = {
    "routing_policy",
    "routing_snapshot",
    "admission",
    "resilience",
    "workers",
    "control_plane",
    "routing_lease",
    "public_api_authentication",
    "operator_api_authentication",
    "public_edge",
}


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args: Any, **kwargs: Any) -> None:
        return None


def monotonic_ms() -> float:
    return time.perf_counter_ns() / 1_000_000


def wall_ms() -> int:
    return time.time_ns() // 1_000_000


def shared_monotonic_ns() -> int:
    """Read one cross-process monotonic timestamp for proof event ordering.

    Some older macOS Python runtimes expose a process-relative monotonic epoch,
    which cannot be compared with timestamps sampled by the proof shell. Perl's
    core Time::HiRes binding exposes the operating-system monotonic clock on
    both the local macOS proof host and the Linux CI host.
    """
    result = subprocess.run(
        [
            "perl",
            "-MTime::HiRes=clock_gettime,CLOCK_MONOTONIC",
            "-e",
            'printf "%.0f\\n", clock_gettime(CLOCK_MONOTONIC)*1000000000',
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    value = int(result.stdout.strip())
    if value <= 0:
        raise SystemExit("shared monotonic clock returned a nonpositive timestamp")
    return value


def require_environment(name: str | None) -> str | None:
    if name is None:
        return None
    value = os.environ.get(name)
    if value is None:
        raise SystemExit(f"required credential environment variable is absent: {name}")
    if not value:
        raise SystemExit(f"required credential environment variable is empty: {name}")
    return value


def authorization_value(args: argparse.Namespace) -> str | None:
    if getattr(args, "bearer_env", None) and getattr(args, "authorization_env", None):
        raise SystemExit("--bearer-env and --authorization-env are mutually exclusive")
    bearer = require_environment(getattr(args, "bearer_env", None))
    if bearer is not None:
        return f"Bearer {bearer}"
    return require_environment(getattr(args, "authorization_env", None))


def url_parts(url: str) -> tuple[str, int, str]:
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme != "http" or not parsed.hostname:
        raise SystemExit("the local v0.28 probe accepts only explicit http:// URLs")
    if parsed.username or parsed.password or parsed.fragment:
        raise SystemExit("probe URL must not contain credentials or a fragment")
    port = parsed.port or 80
    path = urllib.parse.urlunsplit(("", "", parsed.path or "/", parsed.query, ""))
    return parsed.hostname, port, path


def read_body_file(path: Path | None) -> bytes | None:
    if path is None:
        return None
    body = path.read_bytes()
    if len(body) > MAX_RETAINED_RESPONSE_BYTES * 2:
        raise SystemExit("probe request fixture exceeds its local safety bound")
    return body


def project_completion_body(value: Any) -> Any:
    if not isinstance(value, dict):
        return value
    projected = {
        key: item for key, item in value.items() if key not in COMPLETION_DYNAMIC_FIELDS
    }
    inferlab = projected.get("inferlab")
    if isinstance(inferlab, dict):
        projected["inferlab"] = {
            key: item for key, item in inferlab.items() if key != "request_id"
        }
    return projected


def decode_response_body(raw: bytes) -> Any:
    if not raw:
        return None
    try:
        return json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        text = raw.decode("utf-8", errors="replace")
        if len(text) > 4096:
            return {"text_bytes": len(raw), "truncated": True}
        return text


def operator_status_projection(value: Any) -> Any:
    """Retain only bounded status fields needed by the v0.28 proof."""
    if not isinstance(value, dict):
        return value
    admission = value.get("admission")
    projected_admission = None
    if isinstance(admission, dict):
        allowed = {
            "queue_capacity",
            "worker_execution_capacity",
            "outstanding_capacity",
            "outstanding",
            "executing",
            "queued",
            "rejected_total",
            "max_observed_outstanding",
            "max_observed_executing",
            "max_observed_queued",
        }
        projected_admission = {
            key: admission[key] for key in sorted(allowed) if key in admission
        }
    workers = value.get("workers")
    projected_workers: list[dict[str, Any]] = []
    if isinstance(workers, list):
        allowed = {"in_flight", "executing", "concurrency_limit"}
        for worker in workers:
            if isinstance(worker, dict):
                projected_workers.append(
                    {key: worker[key] for key in sorted(allowed) if key in worker}
                )
    return {
        "admission": projected_admission,
        "projection_attestation": {
            "full_status_scanned": True,
            "raw_top_level_schema_validated": True,
            "forbidden_field_matches": 0,
            "forbidden_value_matches": 0,
        },
        "public_edge": value.get("public_edge"),
        "workers": projected_workers,
    }


def showcase_status_projection(value: Any) -> Any:
    """Retain the exact bounded public status schema plus leak attestation."""
    if not isinstance(value, dict):
        return value
    return {
        "projection_attestation": {
            "full_status_scanned": True,
            "raw_top_level_schema_validated": True,
            "forbidden_field_matches": 0,
            "forbidden_value_matches": 0,
        },
        "public_api_authentication": value.get("public_api_authentication"),
        "public_edge": value.get("public_edge"),
        "release": value.get("release"),
        "routing_policy": value.get("routing_policy"),
        "routing_snapshot": value.get("routing_snapshot"),
        "worker_count": value.get("worker_count"),
    }


def validate_showcase_status_schema(value: Any) -> None:
    if not isinstance(value, dict) or set(value) != {
        "routing_policy",
        "worker_count",
        "routing_snapshot",
        "public_api_authentication",
        "public_edge",
        "release",
    }:
        raise SystemExit("public showcase status has an unexpected raw top-level schema")
    if value.get("routing_policy") != "round-robin" or value.get("worker_count") != 1:
        raise SystemExit("public showcase status has unexpected routing summary values")
    if value.get("routing_snapshot") != {
        "control_cluster_id": None,
        "control_signing_key_id": None,
        "control_revision": None,
        "control_term": None,
    }:
        raise SystemExit("public showcase status has an unexpected routing snapshot")
    if value.get("public_api_authentication") != {"enabled": True, "key_count": 2}:
        raise SystemExit("public showcase status has an unexpected auth summary")
    if value.get("public_edge") != {"mode": "hosted"}:
        raise SystemExit("public showcase status has an unexpected edge summary")
    if value.get("release") != {"version": "0.28.0"}:
        raise SystemExit("public showcase status has an unexpected release summary")


def validate_full_status(value: Any, *, operator: bool) -> None:
    """Reject proof secrets or identity-bearing fields before projection."""
    if operator:
        if not isinstance(value, dict) or set(value) != EXPECTED_OPERATOR_STATUS_FIELDS:
            raise SystemExit("operator status has an unexpected raw top-level schema")
    else:
        validate_showcase_status_schema(value)
    field_matches: list[str] = []
    value_matches: list[str] = []

    def visit(item: Any, path: str) -> None:
        if isinstance(item, dict):
            for key, child in item.items():
                child_path = f"{path}.{key}"
                if key.lower() in OPERATOR_STATUS_FORBIDDEN_FIELDS:
                    field_matches.append(child_path)
                visit(child, child_path)
        elif isinstance(item, list):
            for index, child in enumerate(item):
                visit(child, f"{path}[{index}]")
        elif isinstance(item, str):
            for forbidden in PROOF_FORBIDDEN_VALUES:
                if forbidden in item or urllib.parse.quote(forbidden, safe="") in item:
                    value_matches.append(path)
            if any(forbidden_hash in item for forbidden_hash in PROOF_FORBIDDEN_HASHES):
                value_matches.append(path)
            if HOST_PATH.search(item) or any(
                marker in item.upper() for marker in PRIVATE_MARKERS
            ):
                value_matches.append(path)

    visit(value, "$")
    if field_matches or value_matches:
        raise SystemExit(
            "full operator status failed pre-projection leak validation: "
            f"forbidden_fields={len(field_matches)}, "
            f"forbidden_values={len(value_matches)}"
        )


def raw_response_attestation(
    headers: Any,
    raw_body: bytes,
    decoded_body: Any,
    *,
    allow_completion_fields: bool,
    allow_topology_ids: bool,
    full_body_scanned: bool = True,
) -> dict[str, Any]:
    """Validate the full response before header/body projection or retention."""
    forbidden_header_matches = 0
    forbidden_body_field_matches = 0
    forbidden_value_matches = 0
    canonical_request_id_headers_omitted = 0
    completion_fields_projected = 0

    header_items = list(headers.items()) if hasattr(headers, "items") else []
    for raw_name, raw_value in header_items:
        name = str(raw_name).lower()
        value = str(raw_value)
        header_surface = f"{name}\n{value}"
        if name == "x-inferlab-request-id":
            canonical_request_id_headers_omitted += 1
            if any(
                item in value or urllib.parse.quote(item, safe="") in value
                for item in PROOF_FORBIDDEN_VALUES[:-1]
            ):
                forbidden_value_matches += 1
            if any(item in value.lower() for item in PROOF_FORBIDDEN_HASHES):
                forbidden_value_matches += 1
            if HOST_PATH.search(value) or any(
                marker in value.upper() for marker in PRIVATE_MARKERS
            ):
                forbidden_value_matches += 1
            continue
        normalized_name = name.replace("_", "-")
        if any(
            token in normalized_name
            for token in (
                "authorization",
                "api-key",
                "credential",
                "key-hash",
                "key-fingerprint",
                "request-id",
            )
        ) and name != "www-authenticate":
            forbidden_header_matches += 1
        if any(
            item in header_surface
            or urllib.parse.quote(item, safe="") in header_surface
            for item in PROOF_FORBIDDEN_VALUES
        ):
            forbidden_value_matches += 1
        if any(item in header_surface.lower() for item in PROOF_FORBIDDEN_HASHES):
            forbidden_value_matches += 1
        if HOST_PATH.search(header_surface) or any(
            marker in header_surface.upper() for marker in PRIVATE_MARKERS
        ):
            forbidden_value_matches += 1

    raw_text = raw_body.decode("utf-8", errors="replace")
    for forbidden in PROOF_FORBIDDEN_VALUES:
        if forbidden in raw_text or urllib.parse.quote(forbidden, safe="") in raw_text:
            forbidden_value_matches += 1
    if any(item in raw_text.lower() for item in PROOF_FORBIDDEN_HASHES):
        forbidden_value_matches += 1
    if HOST_PATH.search(raw_text) or any(
        marker in raw_text.upper() for marker in PRIVATE_MARKERS
    ):
        forbidden_value_matches += 1

    def visit(item: Any, path: tuple[str, ...] = ()) -> None:
        nonlocal completion_fields_projected
        nonlocal forbidden_body_field_matches
        nonlocal forbidden_value_matches
        if isinstance(item, dict):
            for key, child in item.items():
                lower = key.lower()
                if key == "id" and allow_topology_ids:
                    pass
                elif (
                    key == "request_id"
                    and allow_completion_fields
                    and path == ("inferlab",)
                ):
                    completion_fields_projected += 1
                elif lower in COMPLETION_DYNAMIC_FIELDS:
                    if key == lower and allow_completion_fields and not path:
                        completion_fields_projected += 1
                    else:
                        forbidden_body_field_matches += 1
                elif lower in OPERATOR_STATUS_FORBIDDEN_FIELDS:
                    forbidden_body_field_matches += 1
                elif lower in {"base_url", "worker_url"} and not allow_topology_ids:
                    forbidden_body_field_matches += 1
                visit(child, (*path, key))
        elif isinstance(item, list):
            for child in item:
                visit(child, path)
        elif isinstance(item, str):
            if any(
                forbidden in item or urllib.parse.quote(forbidden, safe="") in item
                for forbidden in PROOF_FORBIDDEN_VALUES
            ):
                forbidden_value_matches += 1
            if any(
                forbidden_hash in item.lower()
                for forbidden_hash in PROOF_FORBIDDEN_HASHES
            ):
                forbidden_value_matches += 1
            if HOST_PATH.search(item) or any(
                marker in item.upper() for marker in PRIVATE_MARKERS
            ):
                forbidden_value_matches += 1

    visit(decoded_body)
    if forbidden_header_matches or forbidden_body_field_matches or forbidden_value_matches:
        raise SystemExit(
            "raw response failed pre-projection leak validation: "
            f"headers={forbidden_header_matches}, "
            f"body_fields={forbidden_body_field_matches}, "
            f"values={forbidden_value_matches}"
        )
    return {
        "schema": "inferlab.public-edge-raw-response-scan.v0.28",
        "full_headers_scanned": True,
        "observed_body_scanned": True,
        "full_body_scanned": full_body_scanned,
        "canonical_request_id_headers_omitted": canonical_request_id_headers_omitted,
        "completion_fields_projected": completion_fields_projected,
        "forbidden_header_matches": 0,
        "forbidden_body_field_matches": 0,
        "forbidden_value_matches": 0,
    }


def retained_headers(headers: Any) -> dict[str, str]:
    result: dict[str, str] = {}
    for name in RETAINED_HEADERS:
        value = headers.get(name)
        if value is not None:
            result[name] = str(value)
    return dict(sorted(result.items()))


def unavailable_response_attestation() -> dict[str, Any]:
    return {
        "schema": "inferlab.public-edge-raw-response-scan.v0.28",
        "full_headers_scanned": False,
        "observed_body_scanned": False,
        "full_body_scanned": False,
        "canonical_request_id_headers_omitted": 0,
        "completion_fields_projected": 0,
        "forbidden_header_matches": 0,
        "forbidden_body_field_matches": 0,
        "forbidden_value_matches": 0,
    }


def observation(
    *,
    kind: str,
    method: str,
    path: str,
    status: int | None,
    headers: dict[str, str] | None,
    body: Any,
    raw_response_scan: dict[str, Any],
    started_wall_ms: int,
    duration_ms: float,
    transport_error: str | None = None,
) -> dict[str, Any]:
    result: dict[str, Any] = {
        "schema": "inferlab.public-edge-http-observation.v0.28",
        "kind": kind,
        "method": method,
        "path": path,
        "status": status,
        "started_at_ms": started_wall_ms,
        "duration_ms": round(duration_ms, 3),
        "headers": headers or {},
        "body": body,
        "raw_response_scan": raw_response_scan,
    }
    if transport_error is not None:
        result["transport_error"] = transport_error
    return result


def request_command(args: argparse.Namespace) -> dict[str, Any]:
    _, _, path = url_parts(args.url)
    body = read_body_file(args.body_file)
    headers = {"accept": "application/json"}
    if body is not None:
        headers["content-type"] = args.content_type
    authorization = authorization_value(args)
    if authorization is not None:
        headers["authorization"] = authorization
    request_id = require_environment(args.request_id_env)
    if request_id is not None:
        headers["x-inferlab-request-id"] = request_id
    outgoing = urllib.request.Request(
        args.url,
        data=body,
        headers=headers,
        method=args.method,
    )
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    started_wall = wall_ms()
    started = monotonic_ms()
    try:
        response = opener.open(outgoing, timeout=args.timeout)
    except urllib.error.HTTPError as error:
        response = error
    except (urllib.error.URLError, TimeoutError, OSError, http.client.HTTPException) as error:
        return observation(
            kind=args.kind,
            method=args.method,
            path=path,
            status=None,
            headers={},
            body=None,
            raw_response_scan=unavailable_response_attestation(),
            started_wall_ms=started_wall,
            duration_ms=monotonic_ms() - started,
            transport_error=type(getattr(error, "reason", error)).__name__,
        )
    with response:
        response_limit = (
            MAX_DISCARDED_RESPONSE_BYTES
            if args.discard_body
            else MAX_RETAINED_RESPONSE_BYTES
        )
        raw = response.read(response_limit + 1)
        if len(raw) > response_limit:
            raise SystemExit("response exceeds the probe's bounded read policy")
        full_body = decode_response_body(raw)
        completion_response = (
            response.status == 200
            and args.method == "POST"
            and path == "/v1/chat/completions"
        )
        raw_scan = raw_response_attestation(
            response.headers,
            raw,
            full_body,
            allow_completion_fields=completion_response,
            allow_topology_ids=args.projection == "operator-status",
        )
        decoded = None if args.discard_body else full_body
        if args.projection in {"operator-status", "showcase-status"}:
            if not isinstance(full_body, dict):
                raise SystemExit("status response is not a JSON object")
            validate_full_status(
                full_body, operator=args.projection == "operator-status"
            )
            decoded = (
                operator_status_projection(full_body)
                if args.projection == "operator-status"
                else showcase_status_projection(full_body)
            )
        elif completion_response:
            decoded = project_completion_body(full_body)
        return observation(
            kind=args.kind,
            method=args.method,
            path=path,
            status=response.status,
            headers=retained_headers(response.headers),
            body=decoded,
            raw_response_scan=raw_scan,
            started_wall_ms=started_wall,
            duration_ms=monotonic_ms() - started,
        )


def rate_sequence_command(args: argparse.Namespace) -> dict[str, Any]:
    sequence_started = monotonic_ms()
    case_start_offsets_ms: dict[str, float] = {}

    def invoke(name: str, bearer_env: str) -> dict[str, Any]:
        case_start_offsets_ms[name] = round(monotonic_ms() - sequence_started, 3)
        return request_command(
            argparse.Namespace(
                url=args.url,
                method="POST",
                body_file=args.body_file,
                content_type="application/json",
                timeout=args.timeout,
                kind="rate-sequence",
                bearer_env=bearer_env,
                authorization_env=None,
                request_id_env=None,
                discard_body=False,
                projection="default",
            )
        )

    cases: dict[str, dict[str, Any]] = {}
    cases["a_first"] = invoke("a_first", args.public_a_env)
    cases["a_second"] = invoke("a_second", args.public_a_env)
    cases["a_limited"] = invoke("a_limited", args.public_a_env)
    limited_completed_offset_ms = round(monotonic_ms() - sequence_started, 3)
    cases["b_independent"] = invoke("b_independent", args.public_b_env)
    time.sleep(args.refill_wait_ms / 1000)
    refilled_started_offset_ms = round(monotonic_ms() - sequence_started, 3)
    cases["a_refilled"] = invoke("a_refilled", args.public_a_env)
    return {
        "schema": "inferlab.public-edge-rate-limit.v0.28",
        "rate_requests_per_minute": args.rate_requests_per_minute,
        "rate_burst": args.rate_burst,
        "limited_completed_offset_ms": limited_completed_offset_ms,
        "refilled_started_offset_ms": refilled_started_offset_ms,
        "refill_wait_ms": round(
            refilled_started_offset_ms - limited_completed_offset_ms, 3
        ),
        "case_start_offsets_ms": case_start_offsets_ms,
        "cases": cases,
    }
def raw_http_exchange(
    host: str,
    port: int,
    request_parts: list[bytes],
    *,
    timeout: float,
) -> tuple[int | None, dict[str, str], bytes, str | None]:
    connection = socket.create_connection((host, port), timeout=timeout)
    connection.settimeout(timeout)
    transport_error = None
    try:
        for part in request_parts:
            try:
                connection.sendall(part)
            except (BrokenPipeError, ConnectionResetError) as error:
                transport_error = type(error).__name__
                break
        try:
            connection.shutdown(socket.SHUT_WR)
        except OSError:
            pass
        chunks: list[bytes] = []
        total = 0
        while total <= MAX_RETAINED_RESPONSE_BYTES:
            try:
                chunk = connection.recv(65536)
            except (socket.timeout, ConnectionResetError) as error:
                if not chunks:
                    transport_error = type(error).__name__
                break
            if not chunk:
                break
            chunks.append(chunk)
            total += len(chunk)
        if total > MAX_RETAINED_RESPONSE_BYTES:
            raise SystemExit("raw HTTP response exceeds the probe retention bound")
    finally:
        connection.close()
    raw = b"".join(chunks)
    if b"\r\n\r\n" not in raw:
        return None, {}, b"", transport_error or "IncompleteHttpResponse"
    raw_headers, body = raw.split(b"\r\n\r\n", 1)
    lines = raw_headers.split(b"\r\n")
    status = None
    if lines:
        fields = lines[0].decode("ascii", errors="replace").split()
        if len(fields) >= 2 and fields[1].isdigit():
            status = int(fields[1])
    headers: dict[str, str] = {}
    for line in lines[1:]:
        if b":" not in line:
            continue
        name, value = line.split(b":", 1)
        decoded_name = name.decode("ascii", errors="ignore").lower()
        decoded_value = value.decode("utf-8", errors="replace").strip()
        headers[decoded_name] = (
            f"{headers[decoded_name]}, {decoded_value}"
            if decoded_name in headers
            else decoded_value
        )
    # The proof server closes these raw HTTP/1.1 responses. Decode a simple
    # chunked response if the framework chose that transfer encoding.
    if headers.get("transfer-encoding", "").lower() == "chunked":
        decoded = bytearray()
        cursor = body
        while cursor:
            if b"\r\n" not in cursor:
                break
            size_line, cursor = cursor.split(b"\r\n", 1)
            try:
                size = int(size_line.split(b";", 1)[0], 16)
            except ValueError:
                break
            if size == 0:
                body = bytes(decoded)
                break
            if len(cursor) < size + 2:
                break
            decoded.extend(cursor[:size])
            cursor = cursor[size + 2 :]
    return status, headers, body, None if status is not None else transport_error


def duplicate_auth_command(args: argparse.Namespace) -> dict[str, Any]:
    host, port, path = url_parts(args.url)
    body = read_body_file(args.body_file) or b""
    authorization = authorization_value(args)
    if authorization is None:
        raise SystemExit("duplicate-auth requires a credential environment variable")
    started_wall = wall_ms()
    started = monotonic_ms()
    connection = http.client.HTTPConnection(host, port, timeout=args.timeout)
    status = None
    headers: Any = {}
    raw = b""
    error = None
    try:
        connection.putrequest("POST", path, skip_host=True)
        connection.putheader("Host", f"{host}:{port}")
        connection.putheader("Content-Type", "application/json")
        connection.putheader("Content-Length", str(len(body)))
        connection.putheader("Authorization", authorization)
        connection.putheader("Authorization", authorization)
        connection.putheader("Connection", "close")
        connection.endheaders(body)
        response = connection.getresponse()
        status = response.status
        headers = response.headers
        raw = response.read(MAX_RETAINED_RESPONSE_BYTES + 1)
        if len(raw) > MAX_RETAINED_RESPONSE_BYTES:
            raise SystemExit("duplicate-auth response exceeds the probe retention bound")
    except (OSError, http.client.HTTPException) as caught:
        error = type(caught).__name__
    finally:
        connection.close()
    decoded = decode_response_body(raw)
    raw_scan = (
        raw_response_attestation(
            headers,
            raw,
            decoded,
            allow_completion_fields=False,
            allow_topology_ids=False,
        )
        if status is not None
        else unavailable_response_attestation()
    )
    return observation(
        kind="duplicate-authorization",
        method="POST",
        path=path,
        status=status,
        headers=retained_headers(headers),
        body=decoded,
        raw_response_scan=raw_scan,
        started_wall_ms=started_wall,
        duration_ms=monotonic_ms() - started,
        transport_error=error,
    )


def chunked_oversize_command(args: argparse.Namespace) -> dict[str, Any]:
    host, port, path = url_parts(args.url)
    authorization = authorization_value(args)
    if authorization is None:
        raise SystemExit("chunked-oversize requires a credential environment variable")
    head = (
        f"POST {path} HTTP/1.1\r\n"
        f"Host: {host}:{port}\r\n"
        "Content-Type: application/json\r\n"
        "Transfer-Encoding: chunked\r\n"
        f"Authorization: {authorization}\r\n"
        "Connection: close\r\n\r\n"
    ).encode("ascii")
    remaining = args.size
    request_parts = [head]
    while remaining:
        size = min(args.chunk_size, remaining)
        chunk = b"x" * size
        request_parts.extend([f"{size:x}\r\n".encode("ascii"), chunk, b"\r\n"])
        remaining -= size
    request_parts.append(b"0\r\n\r\n")
    started_wall = wall_ms()
    started = monotonic_ms()
    status, headers, raw, error = raw_http_exchange(
        host, port, request_parts, timeout=args.timeout
    )
    decoded = decode_response_body(raw)
    raw_scan = (
        raw_response_attestation(
            headers,
            raw,
            decoded,
            allow_completion_fields=False,
            allow_topology_ids=False,
        )
        if status is not None
        else unavailable_response_attestation()
    )
    return observation(
        kind="chunked-oversize",
        method="POST",
        path=path,
        status=status,
        headers=retained_headers(headers),
        body=decoded,
        raw_response_scan=raw_scan,
        started_wall_ms=started_wall,
        duration_ms=monotonic_ms() - started,
        transport_error=error,
    )


def sse_command(args: argparse.Namespace) -> dict[str, Any]:
    host, port, path = url_parts(args.url)
    body = read_body_file(args.body_file)
    if body is None:
        raise SystemExit("SSE probe requires --body-file")
    authorization = authorization_value(args)
    if authorization is None:
        raise SystemExit("SSE probe requires a credential environment variable")
    connection = http.client.HTTPConnection(host, port, timeout=args.timeout)
    started_wall = wall_ms()
    started = monotonic_ms()
    connection.request(
        "POST",
        path,
        body=body,
        headers={
            "authorization": authorization,
            "content-type": "application/json",
            "accept": "text/event-stream",
            "connection": "close",
        },
    )
    response = connection.getresponse()
    events: list[dict[str, Any]] = []
    decoded_payloads: list[Any] = []
    raw_stream_lines: list[bytes] = []
    content_events = 0
    done = False
    eof_after_done = False
    disconnected = False
    release_handshake = False
    release_wait_ms = 0.0
    try:
        while True:
            line = response.readline()
            if not line:
                eof_after_done = done
                break
            raw_stream_lines.append(line)
            if not line.startswith(b"data:"):
                if line.strip():
                    raise SystemExit("SSE stream contains an unexpected non-data field")
                continue
            payload = line[5:].strip()
            offset_ms = round(monotonic_ms() - started, 3)
            if payload == b"[DONE]":
                if done:
                    raise SystemExit("SSE stream emitted more than one DONE sentinel")
                events.append({"offset_ms": offset_ms, "done": True})
                done = True
                continue
            if done:
                raise SystemExit("SSE stream emitted a data event after DONE")
            try:
                decoded = json.loads(payload.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError):
                raise SystemExit("SSE data event is not valid JSON")
            decoded_payloads.append(decoded)
            choices = decoded.get("choices") if isinstance(decoded, dict) else None
            content = None
            finish_reason = None
            if isinstance(choices, list) and choices and isinstance(choices[0], dict):
                delta = choices[0].get("delta")
                if isinstance(delta, dict) and isinstance(delta.get("content"), str):
                    content = delta["content"]
                finish_reason = choices[0].get("finish_reason")
            event = {
                "offset_ms": offset_ms,
                "content": content,
                "finish_reason": finish_reason,
            }
            events.append(event)
            if content:
                content_events += 1
                if args.disconnect_after_content and content_events >= args.disconnect_after_content:
                    if args.ready_file is not None:
                        args.ready_file.write_text(
                            f"{shared_monotonic_ns()}\n", encoding="utf-8"
                        )
                    if args.release_file is not None:
                        wait_started = monotonic_ms()
                        deadline = wait_started + args.release_timeout_ms
                        while not args.release_file.exists():
                            if monotonic_ms() >= deadline:
                                raise SystemExit("timed out waiting for disconnect release")
                            time.sleep(0.005)
                        release_wait_ms = round(monotonic_ms() - wait_started, 3)
                        release_handshake = True
                    elif args.hold_after_content_ms:
                        time.sleep(args.hold_after_content_ms / 1000)
                    disconnected = True
                    break
    finally:
        response.close()
        connection.close()
    offsets = [event["offset_ms"] for event in events]
    content_offsets = [
        event["offset_ms"]
        for event in events
        if isinstance(event.get("content"), str) and event["content"]
    ]
    content_read_span_ms = (
        round(content_offsets[-1] - content_offsets[0], 3)
        if len(content_offsets) >= 2
        else 0.0
    )
    result = {
        "schema": "inferlab.public-edge-sse-observation.v0.28",
        "path": path,
        "status": response.status,
        "headers": retained_headers(response.headers),
        "started_at_ms": started_wall,
        "duration_ms": round(monotonic_ms() - started, 3),
        "event_count": len(events),
        "content_event_count": content_events,
        "distinct_read_offsets": len(set(offsets)),
        "content_read_span_ms": content_read_span_ms,
        "done": done,
        "eof_after_done": eof_after_done,
        "disconnected_after_content": disconnected,
        "release_handshake": release_handshake,
        "release_wait_ms": release_wait_ms,
        "events": events,
        "raw_response_scan": raw_response_attestation(
            response.headers,
            b"".join(raw_stream_lines),
            decoded_payloads,
            allow_completion_fields=True,
            allow_topology_ids=False,
            full_body_scanned=not disconnected,
        ),
    }
    return result


def sensitive_paths(value: Any, path: str = "$.") -> list[str]:
    matches: list[str] = []
    if isinstance(value, dict):
        for key, item in value.items():
            child = f"{path}{key}"
            if key.lower() in SENSITIVE_FIELD_NAMES:
                matches.append(child)
            matches.extend(sensitive_paths(item, f"{child}."))
    elif isinstance(value, list):
        for index, item in enumerate(value):
            matches.extend(sensitive_paths(item, f"{path}[{index}]."))
    return matches


def sanitize_command(args: argparse.Namespace) -> dict[str, Any]:
    forbidden_document = json.loads(args.forbidden_values_file.read_text(encoding="utf-8"))
    if not isinstance(forbidden_document, dict) or not forbidden_document:
        raise SystemExit("forbidden-values file must contain a nonempty JSON object")
    forbidden: dict[str, str] = {}
    for label, value in forbidden_document.items():
        if not isinstance(label, str) or not isinstance(value, str) or not value:
            raise SystemExit("forbidden-values entries must be nonempty strings")
        forbidden[label] = value
    files = sorted(
        path
        for path in args.evidence_dir.iterdir()
        if path.is_file() and path.suffix in {".json", ".svg", ".prom"}
        and path.name not in {"manifest.json", "sanitizer.json"}
    )
    violations: list[str] = []
    for path in files:
        raw = path.read_text(encoding="utf-8", errors="replace")
        for label, value in forbidden.items():
            if value in raw or urllib.parse.quote(value, safe="") in raw:
                violations.append(f"{path.name}:forbidden:{label}")
        if args.proof_root in raw or args.project_root in raw or HOST_PATH.search(raw):
            violations.append(f"{path.name}:host-path")
        if any(marker in raw.upper() for marker in PRIVATE_MARKERS):
            violations.append(f"{path.name}:private-marker")
        if path.suffix == ".json":
            try:
                document = json.loads(raw)
            except json.JSONDecodeError:
                violations.append(f"{path.name}:invalid-json")
            else:
                for field_path in sensitive_paths(document):
                    violations.append(f"{path.name}:sensitive-field:{field_path}")
    if violations:
        raise SystemExit("retained evidence failed sanitization: " + ", ".join(violations))
    return {
        "schema": "inferlab.public-edge-sanitizer.v0.28",
        "files_scanned": [path.name for path in files],
        "forbidden_value_labels": sorted(forbidden),
        "forbidden_value_count": len(forbidden),
        "host_path_patterns_checked": True,
        "private_marker_checks": len(PRIVATE_MARKERS),
        "sensitive_json_fields_checked": sorted(SENSITIVE_FIELD_NAMES),
        "violations": 0,
    }


def add_authentication_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--bearer-env")
    parser.add_argument("--authorization-env")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)

    request_parser = commands.add_parser("request")
    request_parser.add_argument("--url", required=True)
    request_parser.add_argument("--method", default="GET")
    request_parser.add_argument("--body-file", type=Path)
    request_parser.add_argument("--content-type", default="application/json")
    request_parser.add_argument("--timeout", type=float, default=3.0)
    request_parser.add_argument("--kind", default="http")
    request_parser.add_argument("--discard-body", action="store_true")
    request_parser.add_argument(
        "--projection",
        choices=("default", "operator-status", "showcase-status"),
        default="default",
    )
    request_parser.add_argument("--request-id-env")
    add_authentication_arguments(request_parser)

    duplicate_parser = commands.add_parser("duplicate-auth")
    duplicate_parser.add_argument("--url", required=True)
    duplicate_parser.add_argument("--body-file", type=Path, required=True)
    duplicate_parser.add_argument("--timeout", type=float, default=3.0)
    add_authentication_arguments(duplicate_parser)

    chunked_parser = commands.add_parser("chunked-oversize")
    chunked_parser.add_argument("--url", required=True)
    chunked_parser.add_argument("--size", type=int, default=65537)
    chunked_parser.add_argument("--chunk-size", type=int, default=4096)
    chunked_parser.add_argument("--timeout", type=float, default=3.0)
    add_authentication_arguments(chunked_parser)

    sse_parser = commands.add_parser("sse")
    sse_parser.add_argument("--url", required=True)
    sse_parser.add_argument("--body-file", type=Path, required=True)
    sse_parser.add_argument("--timeout", type=float, default=20.0)
    sse_parser.add_argument("--disconnect-after-content", type=int, default=0)
    sse_parser.add_argument("--hold-after-content-ms", type=int, default=0)
    sse_parser.add_argument("--ready-file", type=Path)
    sse_parser.add_argument("--release-file", type=Path)
    sse_parser.add_argument("--release-timeout-ms", type=int, default=5000)
    add_authentication_arguments(sse_parser)

    rate_parser = commands.add_parser("rate-sequence")
    rate_parser.add_argument("--url", required=True)
    rate_parser.add_argument("--body-file", type=Path, required=True)
    rate_parser.add_argument("--public-a-env", required=True)
    rate_parser.add_argument("--public-b-env", required=True)
    rate_parser.add_argument("--rate-requests-per-minute", type=int, required=True)
    rate_parser.add_argument("--rate-burst", type=int, required=True)
    rate_parser.add_argument("--refill-wait-ms", type=int, default=1100)
    rate_parser.add_argument("--timeout", type=float, default=5.0)

    sanitize_parser = commands.add_parser("sanitize-evidence")
    sanitize_parser.add_argument("--evidence-dir", type=Path, required=True)
    sanitize_parser.add_argument("--forbidden-values-file", type=Path, required=True)
    sanitize_parser.add_argument("--proof-root", required=True)
    sanitize_parser.add_argument("--project-root", required=True)

    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if args.command == "request":
        result = request_command(args)
    elif args.command == "duplicate-auth":
        result = duplicate_auth_command(args)
    elif args.command == "chunked-oversize":
        if args.size <= 65536 or args.chunk_size <= 0:
            raise SystemExit("chunked oversize requires size > 65536 and positive chunk size")
        result = chunked_oversize_command(args)
    elif args.command == "sse":
        if (
            args.disconnect_after_content < 0
            or args.hold_after_content_ms < 0
            or args.release_timeout_ms <= 0
        ):
            raise SystemExit("disconnect count and hold duration must be nonnegative")
        if args.ready_file is not None and not args.disconnect_after_content:
            raise SystemExit("--ready-file requires --disconnect-after-content")
        if args.release_file is not None and not args.disconnect_after_content:
            raise SystemExit("--release-file requires --disconnect-after-content")
        result = sse_command(args)
    elif args.command == "rate-sequence":
        if (
            args.rate_requests_per_minute <= 0
            or args.rate_burst <= 0
            or args.refill_wait_ms <= 0
        ):
            raise SystemExit("rate sequence values must be positive")
        result = rate_sequence_command(args)
    elif args.command == "sanitize-evidence":
        result = sanitize_command(args)
    else:
        raise AssertionError(args.command)
    json.dump(result, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
