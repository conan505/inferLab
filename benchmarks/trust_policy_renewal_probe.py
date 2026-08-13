#!/usr/bin/env python3
"""Bounded live probes and fault gate for the v0.31 trust-policy renewal proof."""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import http.client
import json
import os
import re
import signal
import socket
import ssl
import sys
import threading
import time
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any, Callable


SCHEMA_PREFIX = "inferlab.trust-policy-renewal"
MAX_RESPONSE_BYTES = 1_048_576
MAX_SSE_BYTES = 1_048_576
MAX_SSE_LINES = 512
MAX_SSE_EVENTS = 256
MAX_SSE_LINE_BYTES = 65_536
HOST_PATH = re.compile(
    r"(?:/Users/|/home/|/tmp/|/private/var/|/var/folders/|/workspace/|/github/workspace)"
)
PRIVATE_MARKERS = (
    "-----BEGIN PRIVATE KEY-----",
    "-----BEGIN RSA PRIVATE KEY-----",
    "-----BEGIN EC PRIVATE KEY-----",
    "PRIVATE_KEY_B64",
    "PRIVATE_KEY_BASE64",
    "ROOT_PRIVATE_KEY",
)
SENSITIVE_FIELDS = {
    "private_key",
    "private_key_pem",
    "private_key_base64",
    "root_private_key_b64",
    "seed",
    "api_key",
    "authorization",
    "nonce",
    "request_id",
    "snapshot_path",
    "template_path",
    "state_path",
    "bundle_path",
    "base_url",
    "distributor_url",
    "certificate_chain_pem",
    "issuer_ca_pem",
}
RENEWER_STATUS_FIELDS = (
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
)
AUTHENTICATION_HEADERS = {
    "schema": "x-inferlab-service-auth-schema",
    "algorithm": "x-inferlab-service-auth-algorithm",
    "service_id": "x-inferlab-service-id",
    "audience_id": "x-inferlab-service-audience",
    "issued_at_ms": "x-inferlab-service-issued-at-ms",
    "nonce": "x-inferlab-service-nonce",
    "signature": "x-inferlab-service-signature",
}


def wall_ms() -> int:
    return time.time_ns() // 1_000_000


def monotonic_ms() -> float:
    return round(time.monotonic_ns() / 1_000_000, 3)


def exact_int(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def atomic_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    temporary.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    temporary.replace(path)


def load_object(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"{path.name} must contain one JSON object")
    return value


def authentication_headers(path: Path | None) -> dict[str, str]:
    if path is None:
        return {}
    authentication = load_object(path)
    missing = sorted(set(AUTHENTICATION_HEADERS) - set(authentication))
    if missing:
        raise ValueError(f"authentication object is missing fields: {missing}")
    return {
        header: str(authentication[field])
        for field, header in AUTHENTICATION_HEADERS.items()
    }


def tls_client_context(ca: Path, cert: Path, key: Path) -> ssl.SSLContext:
    context = ssl.create_default_context(cafile=str(ca))
    context.minimum_version = ssl.TLSVersion.TLSv1_3
    context.maximum_version = ssl.TLSVersion.TLSv1_3
    context.check_hostname = True
    context.load_cert_chain(str(cert), str(key))
    return context


def tls_server_context(ca: Path, cert: Path, key: Path) -> ssl.SSLContext:
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.minimum_version = ssl.TLSVersion.TLSv1_3
    context.maximum_version = ssl.TLSVersion.TLSv1_3
    context.verify_mode = ssl.CERT_REQUIRED
    context.load_verify_locations(cafile=str(ca))
    context.load_cert_chain(str(cert), str(key))
    return context


def read_capped(response: http.client.HTTPResponse, limit: int = MAX_RESPONSE_BYTES) -> bytes:
    chunks: list[bytes] = []
    total = 0
    while True:
        chunk = response.read(min(65_536, limit + 1 - total))
        if not chunk:
            return b"".join(chunks)
        chunks.append(chunk)
        total += len(chunk)
        if total > limit:
            raise ValueError("response exceeds proof byte limit")


def decode_body(raw: bytes) -> Any:
    if not raw:
        return None
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        return raw.decode("utf-8", errors="replace")


def peer_fingerprint(connection: http.client.HTTPSConnection) -> str:
    if connection.sock is None:
        raise ValueError("TLS connection has no active socket")
    certificate = connection.sock.getpeercert(binary_form=True)
    if not certificate:
        raise ValueError("TLS peer omitted its certificate")
    return hashlib.sha256(certificate).hexdigest()


def response_peer_fingerprint(
    connection: http.client.HTTPSConnection, response: http.client.HTTPResponse
) -> str:
    if connection.sock is not None:
        return peer_fingerprint(connection)
    raw_socket = getattr(getattr(response, "fp", None), "raw", None)
    socket_object = getattr(raw_socket, "_sock", None)
    if socket_object is None:
        raise ValueError("TLS response has no peer socket")
    certificate = socket_object.getpeercert(binary_form=True)
    if not certificate:
        raise ValueError("TLS response peer omitted its certificate")
    return hashlib.sha256(certificate).hexdigest()


def request(
    url: str,
    *,
    method: str = "GET",
    body_path: Path | None = None,
    headers: dict[str, str] | None = None,
    ca: Path | None = None,
    cert: Path | None = None,
    key: Path | None = None,
    timeout: float = 3.0,
) -> dict[str, Any]:
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme not in {"http", "https"} or parsed.hostname is None:
        raise ValueError("proof URL must be absolute HTTP(S)")
    path = urllib.parse.urlunsplit(("", "", parsed.path or "/", parsed.query, ""))
    body = body_path.read_bytes() if body_path is not None else None
    request_headers = dict(headers or {})
    if body is not None:
        request_headers.setdefault("content-type", "application/json")
    started_wall = wall_ms()
    started_mono = monotonic_ms()
    fingerprint = None
    if parsed.scheme == "https":
        if ca is None or cert is None or key is None:
            raise ValueError("HTTPS proof request requires CA, certificate and key")
        connection: http.client.HTTPConnection = http.client.HTTPSConnection(
            parsed.hostname,
            parsed.port or 443,
            context=tls_client_context(ca, cert, key),
            timeout=timeout,
        )
    else:
        connection = http.client.HTTPConnection(parsed.hostname, parsed.port or 80, timeout=timeout)
    try:
        connection.request(method, path, body=body, headers=request_headers)
        response = connection.getresponse()
        if isinstance(connection, http.client.HTTPSConnection):
            fingerprint = response_peer_fingerprint(connection, response)
        raw = read_capped(response)
        def integer_header(name: str) -> int | None:
            value = response.getheader(name)
            return int(value) if value is not None else None

        return {
            "method": method,
            "path": path,
            "status": response.status,
            "etag": response.getheader("etag"),
            "started_at_ms": started_wall,
            "observed_at_ms": wall_ms(),
            "duration_ms": round(monotonic_ms() - started_mono, 3),
            "tls_peer_certificate_sha256": fingerprint,
            "worker": response.getheader("x-inferlab-worker"),
            "attempts": integer_header("x-inferlab-attempts"),
            "config_revision": integer_header("x-inferlab-config-revision"),
            "body_sha256": hashlib.sha256(raw).hexdigest(),
            "body": decode_body(raw),
        }
    finally:
        connection.close()


def wait_for(
    predicate: Callable[[], dict[str, Any] | None], timeout: float, label: str
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    samples = 0
    while time.monotonic() < deadline:
        samples += 1
        try:
            value = predicate()
        except (
            OSError,
            ValueError,
            ssl.SSLError,
            socket.timeout,
            http.client.HTTPException,
        ):
            value = None
        if value is not None:
            return {"samples": samples, "result": value}
        time.sleep(0.025)
    raise SystemExit(f"timed out waiting for {label}")


def wait_until_wall(target_ms: int, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    while wall_ms() < target_ms:
        if time.monotonic() >= deadline:
            raise SystemExit("timed out waiting for signed wall-clock barrier")
        time.sleep(min(0.01, max(0.001, (target_ms - wall_ms()) / 1_000)))


def project_renewer_status(body: Any) -> dict[str, Any]:
    if not isinstance(body, dict):
        raise ValueError("renewer status body is not an object")
    if set(body) != set(RENEWER_STATUS_FIELDS):
        missing = sorted(set(RENEWER_STATUS_FIELDS) - set(body))
        unexpected = sorted(set(body) - set(RENEWER_STATUS_FIELDS))
        raise ValueError(
            f"renewer status field mismatch: missing={missing}, unexpected={unexpected}"
        )
    return {key: body[key] for key in RENEWER_STATUS_FIELDS}


def project_distributor_status(body: Any) -> dict[str, Any]:
    if not isinstance(body, dict):
        raise ValueError("distributor status body is not an object")
    snapshot = body.get("snapshot")
    storage = body.get("storage")
    return {
        "schema": body.get("schema"),
        "cluster_id": body.get("cluster_id"),
        "expected_receiver_mode": body.get("expected_receiver_mode"),
        "snapshot": {
            key: snapshot.get(key)
            for key in (
                "policy_schema",
                "generation",
                "issued_at_ms",
                "expires_at_ms",
                "root_key_id",
                "etag",
            )
        }
        if isinstance(snapshot, dict)
        else None,
        "expected_receivers": body.get("expected_receivers"),
        "acked_receivers": body.get("acked_receivers"),
        "pending_receivers": body.get("pending_receivers"),
        "receipt_count": body.get("receipt_count"),
        "receipts": body.get("receipts"),
        "storage": {
            "mutation_poisoned": storage.get("mutation_poisoned"),
            "error_code": storage.get("error_code"),
        }
        if isinstance(storage, dict)
        else None,
    }


def project_control(body: Any) -> dict[str, Any]:
    if not isinstance(body, dict):
        raise ValueError("control status body is not an object")
    authentication = body.get("service_authentication")
    committed = body.get("committed_configuration")
    if not isinstance(authentication, dict):
        raise ValueError("control status omits service authentication")
    configuration = committed.get("configuration") if isinstance(committed, dict) else None
    workers = configuration.get("workers") if isinstance(configuration, dict) else None
    return {
        "node_id": body.get("node_id"),
        "cluster_id": body.get("cluster_id"),
        "role": body.get("role"),
        "term": body.get("term"),
        "leader_id": body.get("leader_id"),
        "commit_index": body.get("commit_index"),
        "last_applied": body.get("last_applied"),
        "last_log_index": body.get("last_log_index"),
        "storage_healthy": body.get("storage_healthy"),
        "committed_configuration": {
            "cluster_id": committed.get("cluster_id") if isinstance(committed, dict) else None,
            "revision": committed.get("revision") if isinstance(committed, dict) else None,
            "term": committed.get("term") if isinstance(committed, dict) else None,
            "routing_policy": configuration.get("routing_policy") if isinstance(configuration, dict) else None,
            "worker_ids": sorted(
                item.get("id") for item in workers if isinstance(item, dict)
            )
            if isinstance(workers, list)
            else [],
        },
        "service_authentication": {
            key: authentication.get(key)
            for key in (
                "required",
                "trust_policy_schema",
                "trust_policy_generation",
                "trust_policy_root_key_id",
                "trust_policy_loaded_at_ms",
                "trust_policy_expires_at_ms",
                "trust_policy_validity",
                "trust_policy_remaining_ms",
                "trust_policy_transport_mode",
                "trust_policy_server_authentication",
                "trust_policy_client_authentication",
                "trust_policy_bootstrap_source",
                "trust_policy_last_fetch_outcome",
                "trust_policy_consecutive_fetch_failures",
                "trust_policy_receipts_posted",
                "trust_policy_receipt_failures",
                "trust_policy_last_receipt_generation",
                "trust_policy_expiration_rejections",
            )
        },
    }


def control_capture(url: str) -> dict[str, Any]:
    observation = request(url.rstrip("/") + "/v1/control/status", timeout=0.7)
    if observation["status"] != 200:
        raise ValueError("control status was not 200")
    observation["body"] = project_control(observation["body"])
    return observation


def exact_cluster(controls: list[dict[str, Any]], revision: int) -> bool:
    expected = {"control-a", "control-b", "control-c"}
    leaders = [item for item in controls if item.get("role") == "leader"]
    terms = {item.get("term") for item in controls}
    return (
        len(controls) == 3
        and {item.get("node_id") for item in controls} == expected
        and len(leaders) == 1
        and len(terms) == 1
        and all(item.get("leader_id") == leaders[0].get("node_id") for item in controls)
        and all(item.get("storage_healthy") is True for item in controls)
        and all(
            item.get("committed_configuration", {}).get("revision") == revision
            for item in controls
        )
    )


def command_capture(args: argparse.Namespace) -> None:
    result = request(
        args.url,
        method=args.method,
        body_path=args.body,
        headers=authentication_headers(args.authentication),
        ca=args.ca_cert,
        cert=args.client_cert,
        key=args.client_key,
        timeout=args.timeout,
    )
    print(
        json.dumps(
            {
                "schema": f"{SCHEMA_PREFIX}-capture.v0.31",
                "probe_pid": os.getpid(),
                "observation": result,
            },
            indent=2,
            sort_keys=True,
        )
    )
    if result["status"] != args.expect_status:
        raise SystemExit(1)


def command_wait_renewer(args: argparse.Namespace) -> None:
    def predicate() -> dict[str, Any] | None:
        observation = request(
            args.url.rstrip("/") + "/v1/service-trust/renewal/status", timeout=0.7
        )
        if observation["status"] != 200:
            return None
        status = project_renewer_status(observation["body"])
        if args.phase is not None and status.get("phase") != args.phase:
            return None
        if args.ready is not None and status.get("ready") is not args.ready:
            return None
        for key in ("distributor_generation", "committed_generation", "pending_generation"):
            expected = getattr(args, key)
            if expected is not None and status.get(key) != expected:
                return None
        if args.pending_absent and status.get("pending_generation") is not None:
            return None
        if args.last_error_kind is not None and status.get("last_error_kind") != args.last_error_kind:
            return None
        if args.clean_error and status.get("last_error_kind") is not None:
            return None
        for key in (
            "attempts",
            "successful_renewals",
            "transient_failures",
            "rejected_states",
            "late_recoveries",
        ):
            minimum = getattr(args, f"min_{key}")
            if minimum is not None and (
                not exact_int(status.get(key)) or status[key] < minimum
            ):
                return None
        observation["body"] = status
        return observation

    observed = wait_for(predicate, args.timeout, "policy renewer status")
    print(
        json.dumps(
            {
                "schema": f"{SCHEMA_PREFIX}-renewer-status.v0.31",
                "observed_at_ms": wall_ms(),
                **observed,
            },
            indent=2,
            sort_keys=True,
        )
    )


def command_wait_distributor(args: argparse.Namespace) -> None:
    expected_receivers = sorted(item for item in args.expected_receivers.split(",") if item)

    def predicate() -> dict[str, Any] | None:
        observation = request(
            args.url.rstrip("/") + "/v1/service-trust/status",
            ca=args.ca_cert,
            cert=args.client_cert,
            key=args.client_key,
            timeout=1,
        )
        if observation["status"] != 200:
            return None
        body = project_distributor_status(observation["body"])
        snapshot = body.get("snapshot") or {}
        if snapshot.get("generation") != args.generation:
            return None
        if args.complete_receipts:
            if (
                body.get("acked_receivers") != expected_receivers
                or body.get("pending_receivers") != []
                or body.get("receipt_count") != len(expected_receivers)
            ):
                return None
        observation["body"] = body
        return observation

    observed = wait_for(predicate, args.timeout, "distributor generation and receipts")
    print(
        json.dumps(
            {
                "schema": f"{SCHEMA_PREFIX}-distributor.v0.31",
                "observed_at_ms": wall_ms(),
                **observed,
            },
            indent=2,
            sort_keys=True,
        )
    )


def command_wait_controls(args: argparse.Namespace) -> None:
    urls = [item for item in args.urls.split(",") if item]

    def predicate() -> dict[str, Any] | None:
        captures = [control_capture(url) for url in urls]
        controls = [capture["body"] for capture in captures]
        if not exact_cluster(controls, args.revision):
            return None
        for control in controls:
            authentication = control["service_authentication"]
            if (
                authentication.get("trust_policy_generation") != args.generation
                or authentication.get("trust_policy_validity") != args.validity
                or authentication.get("trust_policy_transport_mode") != "mutual-tls"
                or authentication.get("trust_policy_server_authentication") is not True
                or authentication.get("trust_policy_client_authentication") is not True
            ):
                return None
            if args.expires_at_ms is not None and (
                authentication.get("trust_policy_expires_at_ms") != args.expires_at_ms
            ):
                return None
            if args.require_receipt_generation and (
                authentication.get("trust_policy_last_receipt_generation") != args.generation
            ):
                return None
        return {
            "leader_id": next(
                item["node_id"] for item in controls if item["role"] == "leader"
            ),
            "term": controls[0]["term"],
            "revision": args.revision,
            "controls": controls,
        }

    observed = wait_for(predicate, args.timeout, "control policy generation")
    print(
        json.dumps(
            {
                "schema": f"{SCHEMA_PREFIX}-controls.v0.31",
                "observed_at_ms": wall_ms(),
                **observed,
            },
            indent=2,
            sort_keys=True,
        )
    )


def command_wait_expired(args: argparse.Namespace) -> None:
    urls = [item for item in args.urls.split(",") if item]

    def predicate() -> dict[str, Any] | None:
        captures = [control_capture(url) for url in urls]
        controls = [capture["body"] for capture in captures]
        if not exact_cluster(controls, args.revision):
            return None
        for control in controls:
            authentication = control["service_authentication"]
            if (
                authentication.get("trust_policy_generation") != args.generation
                or authentication.get("trust_policy_validity") != "expired"
                or authentication.get("trust_policy_expires_at_ms") != args.expires_at_ms
                or authentication.get("trust_policy_transport_mode") != "mutual-tls"
                or authentication.get("trust_policy_server_authentication") is not True
                or authentication.get("trust_policy_client_authentication") is not True
            ):
                return None
        return {
            "leader_id": next(
                item["node_id"] for item in controls if item["role"] == "leader"
            ),
            "term": controls[0]["term"],
            "revision": args.revision,
            "controls": controls,
        }

    wait_until_wall(args.expires_at_ms, args.timeout)
    observed = wait_for(predicate, args.timeout, "expired control policy generation")
    print(
        json.dumps(
            {
                "schema": f"{SCHEMA_PREFIX}-controls.v0.31",
                "observed_at_ms": wall_ms(),
                **observed,
            },
            indent=2,
            sort_keys=True,
        )
    )


def command_protected_cutoff(args: argparse.Namespace) -> None:
    if args.before_ms <= 0 or args.after_ms < 0:
        raise SystemExit("cutoff offsets must be positive-before and nonnegative-after")
    base = args.control_url.rstrip("/") + "/v1/control/config"
    baseline = control_capture(args.control_url)["body"]
    baseline_rejections = baseline["service_authentication"][
        "trust_policy_expiration_rejections"
    ]
    wait_until_wall(args.expires_at_ms - args.before_ms, args.timeout)
    before = request(
        base,
        headers=authentication_headers(args.before_authentication),
        timeout=args.timeout,
    )
    if isinstance(before.get("body"), dict):
        configuration = before["body"]
        before["body"] = {
            "cluster_id": configuration.get("cluster_id"),
            "revision": configuration.get("revision"),
        }
    wait_until_wall(args.expires_at_ms + args.after_ms, args.timeout)
    signed = request(
        base,
        headers=authentication_headers(args.after_authentication),
        timeout=args.timeout,
    )
    signed_repeat = request(
        base,
        headers=authentication_headers(args.after_authentication_repeat),
        timeout=args.timeout,
    )
    missing = request(base, timeout=args.timeout)
    status = control_capture(args.control_url)
    final_rejections = status["body"]["service_authentication"][
        "trust_policy_expiration_rejections"
    ]
    print(
        json.dumps(
            {
                "schema": f"{SCHEMA_PREFIX}-protected-requests.v0.31",
                "generation": args.generation,
                "expires_at_ms": args.expires_at_ms,
                "before_expiry": before,
                "after_expiry_signed": signed,
                "after_expiry_signed_repeat": signed_repeat,
                "after_expiry_missing": missing,
                "post_expiry_control": status["body"],
                "expiration_rejections_before": baseline_rejections,
                "expiration_rejections_after": final_rejections,
                "expiration_rejection_delta": final_rejections - baseline_rejections,
            },
            indent=2,
            sort_keys=True,
        )
    )


def completion_payload(prompt: str, stream: bool) -> bytes:
    return json.dumps(
        {
            "model": "inferlab-tiny",
            "stream": stream,
            "temperature": 0,
            "speculative_tokens": 0,
            "max_tokens": 8,
            "messages": [{"role": "user", "content": prompt}],
        },
        separators=(",", ":"),
    ).encode()


def project_generation(value: Any) -> Any:
    if not isinstance(value, dict):
        return value
    projected = dict(value)
    decoding = projected.get("decoding")
    if not isinstance(decoding, dict) or decoding.get("seed") != 0:
        raise ValueError("proof completion decoding seed is not zero")
    decoding = dict(decoding)
    decoding.pop("seed")
    projected["decoding"] = decoding
    return projected


def project_completion(body: Any) -> Any:
    if not isinstance(body, dict):
        return body
    projected = {
        key: value
        for key, value in body.items()
        if key not in {"id", "created", "system_fingerprint"}
    }
    inferlab = projected.get("inferlab")
    if isinstance(inferlab, dict):
        inferlab = {key: value for key, value in inferlab.items() if key != "request_id"}
        inferlab["generation"] = project_generation(inferlab.get("generation"))
        projected["inferlab"] = inferlab
    return projected


def command_completion(args: argparse.Namespace) -> None:
    temporary = args.temporary_body
    temporary.write_bytes(completion_payload(args.prompt, False))
    try:
        observation = request(
            args.url.rstrip("/") + "/v1/chat/completions",
            method="POST",
            body_path=temporary,
            timeout=args.timeout,
        )
    finally:
        temporary.unlink(missing_ok=True)
    observation["body"] = project_completion(observation["body"])
    print(
        json.dumps(
            {"schema": f"{SCHEMA_PREFIX}-json.v0.31", "observation": observation},
            indent=2,
            sort_keys=True,
        )
    )


def command_stream(args: argparse.Namespace) -> None:
    url = args.url.rstrip("/") + "/v1/chat/completions"
    req = urllib.request.Request(
        url,
        data=completion_payload(args.prompt, True),
        method="POST",
        headers={"content-type": "application/json"},
    )
    started_wall = wall_ms()
    started_mono = monotonic_ms()
    events: list[Any] = []
    offsets: list[float] = []
    pieces: list[str] = []
    done_seen = False
    total_bytes = 0
    total_lines = 0
    pending = bytearray()
    finish_reason = None
    generation = None

    def expired(_signum: int, _frame: Any) -> None:
        raise TimeoutError("SSE proof stream exceeded total deadline")

    previous = signal.signal(signal.SIGALRM, expired)
    signal.setitimer(signal.ITIMER_REAL, args.timeout)
    try:
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with opener.open(req, timeout=args.timeout) as response:
            while True:
                chunk = response.read1(4096)
                if not chunk:
                    break
                total_bytes += len(chunk)
                if total_bytes > MAX_SSE_BYTES:
                    raise ValueError("SSE proof stream exceeded byte ceiling")
                pending.extend(chunk)
                while b"\n" in pending:
                    line, _, remainder = pending.partition(b"\n")
                    pending = bytearray(remainder)
                    total_lines += 1
                    if total_lines > MAX_SSE_LINES or len(line) > MAX_SSE_LINE_BYTES:
                        raise ValueError("SSE proof stream exceeded line ceiling")
                    text = line.rstrip(b"\r").decode("utf-8")
                    if not text:
                        continue
                    if not text.startswith("data: "):
                        raise ValueError("unexpected SSE field")
                    value = text[6:]
                    offsets.append(round(monotonic_ms() - started_mono, 3))
                    if value == "[DONE]":
                        if done_seen:
                            raise ValueError("duplicate SSE terminal sentinel")
                        done_seen = True
                        events.append("[DONE]")
                        continue
                    if done_seen:
                        raise ValueError("SSE data followed terminal sentinel")
                    event = json.loads(value)
                    event.pop("id", None)
                    event.pop("created", None)
                    event.pop("system_fingerprint", None)
                    inferlab = event.get("inferlab")
                    if isinstance(inferlab, dict):
                        inferlab.pop("request_id", None)
                        inferlab["generation"] = project_generation(inferlab.get("generation"))
                    events.append(event)
                    choice = event.get("choices", [{}])[0]
                    content = choice.get("delta", {}).get("content")
                    if isinstance(content, str) and content:
                        pieces.append(content)
                    if choice.get("finish_reason") is not None:
                        finish_reason = choice["finish_reason"]
                        generation = event.get("inferlab", {}).get("generation")
                    if len(events) > MAX_SSE_EVENTS:
                        raise ValueError("SSE proof stream exceeded event ceiling")
            if pending:
                raise ValueError("SSE proof stream ended with unterminated line")
            status = response.status
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous)
    print(
        json.dumps(
            {
                "schema": f"{SCHEMA_PREFIX}-sse.v0.31",
                "method": "POST",
                "path": "/v1/chat/completions",
                "started_at_ms": started_wall,
                "observed_at_ms": wall_ms(),
                "duration_ms": round(monotonic_ms() - started_mono, 3),
                "status": status,
                "event_count": len(events),
                "content_event_count": len(pieces),
                "offsets_ms": offsets,
                "pieces": pieces,
                "content": "".join(pieces),
                "finish_reason": finish_reason,
                "generation": generation,
                "done_received": done_seen,
                "eof_after_done": done_seen,
            },
            indent=2,
            sort_keys=True,
        )
    )


def read_http_request(connection: ssl.SSLSocket) -> tuple[str, str, dict[str, str], bytes]:
    stream = connection.makefile("rb")
    line = stream.readline(MAX_SSE_LINE_BYTES)
    if not line:
        raise ValueError("empty downstream request")
    pieces = line.decode("ascii", errors="strict").rstrip("\r\n").split(" ")
    if len(pieces) != 3 or pieces[2] != "HTTP/1.1":
        raise ValueError("fault gate requires HTTP/1.1")
    method, path = pieces[0], pieces[1]
    headers: dict[str, str] = {}
    while True:
        line = stream.readline(MAX_SSE_LINE_BYTES)
        if line in {b"\r\n", b"\n"}:
            break
        if not line:
            raise ValueError("truncated downstream request headers")
        name, value = line.decode("iso-8859-1").split(":", 1)
        headers[name.lower()] = value.strip()
    length = int(headers.get("content-length", "0"))
    if length < 0 or length > MAX_RESPONSE_BYTES:
        raise ValueError("fault gate request exceeds byte limit")
    body = stream.read(length)
    if len(body) != length:
        raise ValueError("truncated downstream request body")
    return method, path, headers, body


def gate_mode(path: Path) -> str:
    try:
        mode = load_object(path).get("mode")
    except (OSError, ValueError, json.JSONDecodeError):
        return "pass"
    return (
        mode
        if mode in {
            "pass",
            "drop-post-response",
            "unavailable",
            "publication-unavailable",
        }
        else "invalid"
    )


def gate_forward(
    args: argparse.Namespace,
    downstream: ssl.SSLSocket,
    method: str,
    path: str,
    headers: dict[str, str],
    body: bytes,
) -> None:
    upstream = http.client.HTTPSConnection(
        args.upstream_host,
        args.upstream_port,
        context=tls_client_context(args.upstream_ca, args.upstream_cert, args.upstream_key),
        timeout=args.request_timeout,
    )
    forwarded_headers = {
        key: value
        for key, value in headers.items()
        if key not in {"connection", "host", "content-length"}
    }
    forwarded_headers["host"] = args.upstream_server_name
    forwarded_headers["connection"] = "close"
    if body:
        forwarded_headers["content-length"] = str(len(body))
    try:
        upstream.request(method, path, body=body or None, headers=forwarded_headers)
        response = upstream.getresponse()
        fingerprint = response_peer_fingerprint(upstream, response)
        raw = read_capped(response)
        mode = gate_mode(args.mode_file)
        if mode == "drop-post-response" and method == "POST" and not args.drop_marker.exists():
            atomic_json(
                args.drop_marker,
                {
                    "schema": f"{SCHEMA_PREFIX}-fault-gate-drop.v0.31",
                    "method": method,
                    "path": path,
                    "upstream_status": response.status,
                    "upstream_body": decode_body(raw),
                    "upstream_tls_peer_certificate_sha256": fingerprint,
                    "dropped_at_ms": wall_ms(),
                    "response_forwarded": False,
                },
            )
            return
        response_headers = [
            (name, value)
            for name, value in response.getheaders()
            if name.lower() not in {"connection", "transfer-encoding", "content-length"}
        ]
        lines = [f"HTTP/1.1 {response.status} {response.reason}\r\n"]
        lines.extend(f"{name}: {value}\r\n" for name, value in response_headers)
        lines.extend([f"Content-Length: {len(raw)}\r\n", "Connection: close\r\n", "\r\n"])
        downstream.sendall("".join(lines).encode("iso-8859-1") + raw)
    finally:
        upstream.close()


def gate_connection(args: argparse.Namespace, raw: socket.socket, server: ssl.SSLContext) -> None:
    try:
        with server.wrap_socket(raw, server_side=True) as downstream:
            mode = gate_mode(args.mode_file)
            if mode == "drop-post-response" and args.drop_marker.exists():
                mode = "unavailable"
            if mode in {"unavailable", "invalid"}:
                if not args.outage_marker.exists():
                    atomic_json(
                        args.outage_marker,
                        {
                            "schema": f"{SCHEMA_PREFIX}-fault-gate-outage.v0.31",
                            "mode": mode,
                            "observed_at_ms": wall_ms(),
                            "request_forwarded": False,
                        },
                    )
                return
            method, path, headers, body = read_http_request(downstream)
            if mode == "publication-unavailable" and method == "POST":
                if not args.outage_marker.exists():
                    atomic_json(
                        args.outage_marker,
                        {
                            "schema": f"{SCHEMA_PREFIX}-fault-gate-outage.v0.31",
                            "mode": "unavailable",
                            "fault_mode": mode,
                            "method": method,
                            "path": path,
                            "observed_at_ms": wall_ms(),
                            "request_forwarded": False,
                        },
                    )
                return
            gate_forward(args, downstream, method, path, headers, body)
    except (OSError, ValueError, ssl.SSLError, http.client.HTTPException) as error:
        print(
            f"fault-gate connection rejected: {type(error).__name__}: {error}",
            file=sys.stderr,
            flush=True,
        )
        with contextlib.suppress(OSError):
            raw.close()


def command_fault_gate(args: argparse.Namespace) -> None:
    server = tls_server_context(args.downstream_ca, args.downstream_cert, args.downstream_key)
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind((args.listen_host, args.listen_port))
    listener.listen(32)
    atomic_json(
        args.ready_file,
        {
            "schema": f"{SCHEMA_PREFIX}-fault-gate-ready.v0.31",
            "pid": os.getpid(),
            "listen_host": args.listen_host,
            "listen_port": args.listen_port,
            "upstream_host": args.upstream_host,
            "upstream_port": args.upstream_port,
            "tls_protocol": "TLSv1.3",
            "ready_at_ms": wall_ms(),
        },
    )
    try:
        while True:
            raw, _ = listener.accept()
            thread = threading.Thread(
                target=gate_connection, args=(args, raw, server), daemon=True
            )
            thread.start()
    finally:
        listener.close()


def sensitive_paths(value: Any, path: str = "$") -> list[str]:
    found: list[str] = []
    if isinstance(value, dict):
        for key, child in value.items():
            normalized = key.lower().replace("-", "_")
            child_path = f"{path}.{key}"
            if normalized in SENSITIVE_FIELDS:
                found.append(child_path)
            found.extend(sensitive_paths(child, child_path))
    elif isinstance(value, list):
        for index, child in enumerate(value):
            found.extend(sensitive_paths(child, f"{path}[{index}]"))
    return found


def command_sanitize(args: argparse.Namespace) -> None:
    evidence = args.evidence_dir.resolve()
    proof_root = str(args.proof_root.resolve())
    project_root = str(args.project_root.resolve())
    scanned: list[str] = []
    problems: list[dict[str, Any]] = []
    for path in sorted(evidence.iterdir()):
        if path.name in {"manifest.json", "sanitizer.json"} or not path.is_file():
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        scanned.append(path.name)
        markers = [marker for marker in PRIVATE_MARKERS if marker in text]
        host_paths = [value for value in (proof_root, project_root) if value and value in text]
        if HOST_PATH.search(text):
            host_paths.append("generic-host-path")
        fields = sensitive_paths(json.loads(text)) if path.suffix == ".json" else []
        if markers or host_paths or fields:
            problems.append(
                {
                    "file": path.name,
                    "private_markers": markers,
                    "host_paths": sorted(set(host_paths)),
                    "sensitive_fields": fields,
                }
            )
    result = {
        "schema": f"{SCHEMA_PREFIX}-sanitizer.v0.31",
        "files_scanned": scanned,
        "problem_count": len(problems),
        "problems": problems,
    }
    print(json.dumps(result, indent=2, sort_keys=True))
    if problems:
        raise SystemExit(1)


def add_tls(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--ca-cert", type=Path, required=True)
    parser.add_argument("--client-cert", type=Path, required=True)
    parser.add_argument("--client-key", type=Path, required=True)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)

    capture = commands.add_parser("capture")
    capture.add_argument("--url", required=True)
    capture.add_argument("--method", default="GET")
    capture.add_argument("--body", type=Path)
    capture.add_argument("--authentication", type=Path)
    capture.add_argument("--expect-status", type=int, required=True)
    capture.add_argument("--timeout", type=float, default=3)
    capture.add_argument("--ca-cert", type=Path)
    capture.add_argument("--client-cert", type=Path)
    capture.add_argument("--client-key", type=Path)
    capture.set_defaults(function=command_capture)

    renewer = commands.add_parser("wait-renewer")
    renewer.add_argument("--url", required=True)
    renewer.add_argument("--phase")
    renewer.add_argument("--ready", action=argparse.BooleanOptionalAction)
    renewer.add_argument("--distributor-generation", type=int)
    renewer.add_argument("--committed-generation", type=int)
    renewer.add_argument("--pending-generation", type=int)
    renewer.add_argument("--pending-absent", action="store_true")
    renewer.add_argument("--last-error-kind")
    renewer.add_argument("--clean-error", action="store_true")
    for counter in (
        "attempts",
        "successful_renewals",
        "transient_failures",
        "rejected_states",
        "late_recoveries",
    ):
        renewer.add_argument(f"--min-{counter.replace('_', '-')}", type=int)
    renewer.add_argument("--timeout", type=float, default=20)
    renewer.set_defaults(function=command_wait_renewer)

    distributor = commands.add_parser("wait-distributor")
    distributor.add_argument("--url", required=True)
    distributor.add_argument("--generation", type=int, required=True)
    distributor.add_argument("--expected-receivers", required=True)
    distributor.add_argument("--complete-receipts", action="store_true")
    distributor.add_argument("--timeout", type=float, default=20)
    add_tls(distributor)
    distributor.set_defaults(function=command_wait_distributor)

    controls = commands.add_parser("wait-controls")
    controls.add_argument("--urls", required=True)
    controls.add_argument("--revision", type=int, required=True)
    controls.add_argument("--generation", type=int, required=True)
    controls.add_argument("--validity", choices=("valid", "expired"), required=True)
    controls.add_argument("--expires-at-ms", type=int)
    controls.add_argument("--require-receipt-generation", action="store_true")
    controls.add_argument("--timeout", type=float, default=20)
    controls.set_defaults(function=command_wait_controls)

    expired = commands.add_parser("wait-expired-controls")
    expired.add_argument("--urls", required=True)
    expired.add_argument("--revision", type=int, required=True)
    expired.add_argument("--generation", type=int, required=True)
    expired.add_argument("--expires-at-ms", type=int, required=True)
    expired.add_argument("--timeout", type=float, default=20)
    expired.set_defaults(function=command_wait_expired)

    cutoff = commands.add_parser("protected-cutoff")
    cutoff.add_argument("--control-url", required=True)
    cutoff.add_argument("--generation", type=int, required=True)
    cutoff.add_argument("--expires-at-ms", type=int, required=True)
    cutoff.add_argument("--before-authentication", type=Path, required=True)
    cutoff.add_argument("--after-authentication", type=Path, required=True)
    cutoff.add_argument("--after-authentication-repeat", type=Path, required=True)
    cutoff.add_argument("--before-ms", type=int, default=500)
    cutoff.add_argument("--after-ms", type=int, default=25)
    cutoff.add_argument("--timeout", type=float, default=15)
    cutoff.set_defaults(function=command_protected_cutoff)

    completion = commands.add_parser("completion")
    completion.add_argument("--url", required=True)
    completion.add_argument("--prompt", required=True)
    completion.add_argument("--temporary-body", type=Path, required=True)
    completion.add_argument("--timeout", type=float, default=15)
    completion.set_defaults(function=command_completion)

    stream = commands.add_parser("stream")
    stream.add_argument("--url", required=True)
    stream.add_argument("--prompt", required=True)
    stream.add_argument("--timeout", type=float, default=15)
    stream.set_defaults(function=command_stream)

    gate = commands.add_parser("fault-gate")
    gate.add_argument("--listen-host", default="127.0.0.1")
    gate.add_argument("--listen-port", type=int, required=True)
    gate.add_argument("--upstream-host", default="localhost")
    gate.add_argument("--upstream-port", type=int, required=True)
    gate.add_argument("--upstream-server-name", default="localhost")
    gate.add_argument("--downstream-ca", type=Path, required=True)
    gate.add_argument("--downstream-cert", type=Path, required=True)
    gate.add_argument("--downstream-key", type=Path, required=True)
    gate.add_argument("--upstream-ca", type=Path, required=True)
    gate.add_argument("--upstream-cert", type=Path, required=True)
    gate.add_argument("--upstream-key", type=Path, required=True)
    gate.add_argument("--mode-file", type=Path, required=True)
    gate.add_argument("--ready-file", type=Path, required=True)
    gate.add_argument("--drop-marker", type=Path, required=True)
    gate.add_argument("--outage-marker", type=Path, required=True)
    gate.add_argument("--request-timeout", type=float, default=3)
    gate.set_defaults(function=command_fault_gate)

    sanitize = commands.add_parser("sanitize-evidence")
    sanitize.add_argument("--evidence-dir", type=Path, required=True)
    sanitize.add_argument("--proof-root", type=Path, required=True)
    sanitize.add_argument("--project-root", type=Path, required=True)
    sanitize.set_defaults(function=command_sanitize)
    return parser


def main() -> None:
    args = build_parser().parse_args()
    args.function(args)


if __name__ == "__main__":
    main()
