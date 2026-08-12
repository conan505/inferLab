#!/usr/bin/env python3
"""Bounded live probes for the v0.30 same-CA TLS identity handoff proof."""

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
import time
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any, Callable


SCHEMA_PREFIX = "inferlab.tls-identity-handoff"
HOST_PATH = re.compile(
    r"(?:/Users/|/home/|/tmp/|/private/var/|/var/folders/|/workspace/|/github/workspace)"
)
PRIVATE_MARKERS = (
    "-----BEGIN PRIVATE KEY-----",
    "-----BEGIN RSA PRIVATE KEY-----",
    "-----BEGIN EC PRIVATE KEY-----",
    "PRIVATE_KEY_B64",
    "PRIVATE_KEY_BASE64",
)
SENSITIVE_FIELDS = {
    "private_key",
    "private_key_pem",
    "private_key_base64",
    "seed",
    "api_key",
    "authorization",
    "nonce",
    "request_id",
    "snapshot_path",
    "bundle_path",
    "base_url",
    "certificate_chain_pem",
    "issuer_ca_pem",
}
MAX_RESPONSE_BYTES = 1_048_576
MAX_SSE_BYTES = 1_048_576
MAX_SSE_LINES = 512
MAX_SSE_EVENTS = 256
MAX_SSE_LINE_BYTES = 65_536


def wall_ms() -> int:
    return time.time_ns() // 1_000_000


def monotonic_ms() -> float:
    return round(time.monotonic_ns() / 1_000_000, 3)


def exact_int(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def atomic_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(path)


def tls_context(ca: Path, cert: Path, key: Path) -> ssl.SSLContext:
    context = ssl.create_default_context(cafile=str(ca))
    context.minimum_version = ssl.TLSVersion.TLSv1_3
    context.maximum_version = ssl.TLSVersion.TLSv1_3
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


def request(
    url: str,
    *,
    method: str = "GET",
    body_path: Path | None = None,
    ca: Path | None = None,
    cert: Path | None = None,
    key: Path | None = None,
    timeout: float = 3.0,
) -> dict[str, Any]:
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme not in {"http", "https"} or parsed.hostname is None:
        raise ValueError("proof URL must be an absolute HTTP(S) URL")
    path = urllib.parse.urlunsplit(("", "", parsed.path or "/", parsed.query, ""))
    body = body_path.read_bytes() if body_path is not None else None
    headers = {"content-type": "application/json"} if body is not None else {}
    started_wall = wall_ms()
    started_mono = monotonic_ms()
    fingerprint = None
    if parsed.scheme == "https":
        if ca is None or cert is None or key is None:
            raise ValueError("HTTPS proof request requires CA, certificate and key")
        connection: http.client.HTTPConnection = http.client.HTTPSConnection(
            parsed.hostname,
            parsed.port or 443,
            context=tls_context(ca, cert, key),
            timeout=timeout,
        )
    else:
        connection = http.client.HTTPConnection(parsed.hostname, parsed.port or 80, timeout=timeout)
    try:
        connection.request(method, path, body=body, headers=headers)
        response = connection.getresponse()
        if isinstance(connection, http.client.HTTPSConnection):
            fingerprint = peer_fingerprint(connection)
        raw = read_capped(response)
        return {
            "method": method,
            "path": path,
            "status": response.status,
            "started_at_ms": started_wall,
            "observed_at_ms": wall_ms(),
            "duration_ms": round(monotonic_ms() - started_mono, 3),
            "tls_peer_certificate_sha256": fingerprint,
            "body": decode_body(raw),
        }
    finally:
        connection.close()


def wait_for(predicate: Callable[[], dict[str, Any] | None], timeout: float, label: str) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    samples = 0
    while time.monotonic() < deadline:
        samples += 1
        try:
            value = predicate()
        except (OSError, ValueError, ssl.SSLError, socket.timeout, http.client.HTTPException):
            value = None
        if value is not None:
            return {"samples": samples, "result": value}
        time.sleep(0.025)
    raise SystemExit(f"timed out waiting for {label}")


def identity_status(value: Any) -> dict[str, Any] | None:
    if not isinstance(value, dict):
        return None
    return {
        key: value.get(key)
        for key in (
            "mode",
            "identity_id",
            "purpose",
            "server_name",
            "bundle_generation",
            "leaf_certificate_sha256",
            "certificate_chain_length",
            "issuer_ca_count",
            "successful_activations",
            "rejected_reloads",
            "last_error_kind",
            "activation_scope",
            "preaccepted_or_established_connections",
            "in_flight_operations",
        )
        if key in value
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
            ) if isinstance(workers, list) else [],
        },
        "service_authentication": {
            key: authentication.get(key)
            for key in (
                "required",
                "trust_policy_generation",
                "trust_policy_validity",
                "trust_policy_transport_mode",
                "trust_policy_server_authentication",
                "trust_policy_client_authentication",
                "trust_policy_bootstrap_source",
                "trust_policy_last_fetch_outcome",
                "trust_policy_consecutive_fetch_failures",
                "trust_policy_receipts_posted",
                "trust_policy_receipt_failures",
                "trust_policy_last_receipt_generation",
                "trust_policy_last_fetch_tls_bundle_generation",
                "trust_policy_last_receipt_tls_bundle_generation",
            )
        } | {"trust_policy_tls_identity": identity_status(authentication.get("trust_policy_tls_identity"))},
    }


def control_capture(url: str) -> dict[str, Any]:
    observation = request(url.rstrip("/") + "/v1/control/status", timeout=0.7)
    if observation["status"] != 200:
        raise ValueError("control status was not 200")
    observation["body"] = project_control(observation["body"])
    return observation


def exact_cluster(controls: list[dict[str, Any]], revision: int) -> bool:
    expected = {"control-a", "control-b", "control-c"}
    if len(controls) != 3 or {item.get("node_id") for item in controls} != expected:
        return False
    leaders = [item for item in controls if item.get("role") == "leader"]
    terms = {item.get("term") for item in controls}
    return (
        len(leaders) == 1
        and len(terms) == 1
        and all(item.get("leader_id") == leaders[0].get("node_id") for item in controls)
        and all(item.get("storage_healthy") is True for item in controls)
        and all(item.get("committed_configuration", {}).get("revision") == revision for item in controls)
    )


def command_capture(args: argparse.Namespace) -> None:
    result = request(
        args.url,
        method=args.method,
        body_path=args.body,
        ca=args.ca_cert,
        cert=args.client_cert,
        key=args.client_key,
        timeout=args.timeout,
    )
    print(json.dumps({
        "schema": f"{SCHEMA_PREFIX}-capture.v0.30",
        "probe_pid": os.getpid(),
        "connection_model": "fresh-process-fresh-client",
        "observation": result,
    }, indent=2, sort_keys=True))
    if result["status"] != args.expect_status:
        raise SystemExit(1)


def command_wait_distributor(args: argparse.Namespace) -> None:
    expected_services = sorted(item for item in args.expected_services.split(",") if item)

    def predicate() -> dict[str, Any] | None:
        observation = request(
            args.url.rstrip("/") + "/v1/service-trust/status",
            ca=args.ca_cert,
            cert=args.client_cert,
            key=args.client_key,
            timeout=1,
        )
        body = observation.get("body")
        if observation["status"] != 200 or not isinstance(body, dict):
            return None
        snapshot = body.get("snapshot") or {}
        transport = body.get("transport_security") or {}
        identity = transport.get("identity") or {}
        if (
            snapshot.get("generation") != args.policy_generation
            or body.get("acked_receivers") != expected_services
            or body.get("pending_receivers") != []
            or body.get("receipt_count") != len(expected_services)
            or identity.get("bundle_generation") != args.tls_generation
            or identity.get("last_error_kind") != args.last_error_kind
            or not exact_int(identity.get("rejected_reloads"))
            or identity["rejected_reloads"] < args.min_rejections
            or (args.peer_sha256 is not None and observation["tls_peer_certificate_sha256"] != args.peer_sha256)
        ):
            return None
        return observation

    observed = wait_for(predicate, args.timeout, "distributor identity and receipt state")
    print(json.dumps({
        "schema": f"{SCHEMA_PREFIX}-distributor.v0.30",
        "observed_at_ms": wall_ms(),
        **observed,
    }, indent=2, sort_keys=True))


def parse_generation_map(raw: str) -> dict[str, int]:
    return {key: int(value) for key, value in (item.split("=", 1) for item in raw.split(",") if item)}


def command_wait_controls(args: argparse.Namespace) -> None:
    urls = [item for item in args.urls.split(",") if item]
    generations = parse_generation_map(args.tls_generations)

    def predicate() -> dict[str, Any] | None:
        captures = [control_capture(url) for url in urls]
        controls = [capture["body"] for capture in captures]
        if not exact_cluster(controls, args.revision):
            return None
        for control in controls:
            authentication = control["service_authentication"]
            identity = authentication.get("trust_policy_tls_identity") or {}
            expected_generation = generations.get(control["node_id"])
            if (
                authentication.get("trust_policy_generation") != args.policy_generation
                or authentication.get("trust_policy_transport_mode") != "mutual-tls"
                or authentication.get("trust_policy_server_authentication") is not True
                or authentication.get("trust_policy_client_authentication") is not True
                or identity.get("bundle_generation") != expected_generation
                or identity.get("last_error_kind") is not None
                or authentication.get("trust_policy_last_fetch_tls_bundle_generation") != expected_generation
            ):
                return None
            if args.require_receipt_generation and (
                authentication.get("trust_policy_last_receipt_tls_bundle_generation") != expected_generation
                or authentication.get("trust_policy_last_receipt_generation") != args.policy_generation
            ):
                return None
        return {
            "leader_id": next(item["node_id"] for item in controls if item["role"] == "leader"),
            "term": controls[0]["term"],
            "revision": args.revision,
            "controls": controls,
        }

    observed = wait_for(predicate, args.timeout, "control TLS identity generations")
    print(json.dumps({
        "schema": f"{SCHEMA_PREFIX}-controls.v0.30",
        "observed_at_ms": wall_ms(),
        **observed,
    }, indent=2, sort_keys=True))


def command_wait_control(args: argparse.Namespace) -> None:
    def predicate() -> dict[str, Any] | None:
        observation = control_capture(args.url)
        body = observation["body"]
        authentication = body["service_authentication"]
        identity = authentication.get("trust_policy_tls_identity") or {}
        if (
            body.get("node_id") != args.identity_id
            or identity.get("bundle_generation") != args.tls_generation
            or identity.get("last_error_kind") != args.last_error_kind
            or not exact_int(identity.get("rejected_reloads"))
            or identity["rejected_reloads"] < args.min_rejections
            or authentication.get("trust_policy_last_fetch_tls_bundle_generation") != args.tls_generation
        ):
            return None
        return observation

    observed = wait_for(predicate, args.timeout, "one control TLS identity state")
    print(json.dumps({
        "schema": f"{SCHEMA_PREFIX}-control.v0.30",
        "observed_at_ms": wall_ms(),
        **observed,
    }, indent=2, sort_keys=True))


def read_http_response(stream: Any) -> tuple[int, Any]:
    status_line = stream.readline(MAX_SSE_LINE_BYTES)
    if not status_line:
        raise ValueError("held TLS connection ended before HTTP response")
    pieces = status_line.decode("ascii", errors="strict").rstrip("\r\n").split(" ", 2)
    if len(pieces) < 2 or not pieces[1].isdigit():
        raise ValueError("invalid HTTP status on held TLS connection")
    headers: dict[str, str] = {}
    while True:
        line = stream.readline(MAX_SSE_LINE_BYTES)
        if line in {b"\r\n", b"\n"}:
            break
        if not line:
            raise ValueError("held TLS connection ended in HTTP headers")
        name, value = line.decode("iso-8859-1").split(":", 1)
        headers[name.lower()] = value.strip()
    if "content-length" not in headers:
        raise ValueError("held TLS response omitted content-length")
    length = int(headers["content-length"])
    if length > MAX_RESPONSE_BYTES:
        raise ValueError("held TLS response exceeds proof byte limit")
    body = stream.read(length)
    if len(body) != length:
        raise ValueError("held TLS response body was truncated")
    return int(pieces[1]), decode_body(body)


def command_hold_connection(args: argparse.Namespace) -> None:
    parsed = urllib.parse.urlsplit(args.url)
    if parsed.scheme != "https" or parsed.hostname is None:
        raise SystemExit("held connection requires an HTTPS URL")
    context = tls_context(args.ca_cert, args.client_cert, args.client_key)
    raw = socket.create_connection((parsed.hostname, parsed.port or 443), timeout=args.timeout)
    raw.settimeout(args.timeout)
    connection = context.wrap_socket(raw, server_hostname=parsed.hostname)
    stream = connection.makefile("rwb", buffering=0)
    fingerprint = hashlib.sha256(connection.getpeercert(binary_form=True)).hexdigest()
    request_bytes = (
        f"GET {parsed.path or '/health'} HTTP/1.1\r\n"
        f"Host: {parsed.hostname}\r\nConnection: keep-alive\r\nAccept: application/json\r\n\r\n"
    ).encode("ascii")
    try:
        stream.write(request_bytes)
        first_status, first_body = read_http_response(stream)
        ready = {
            "schema": f"{SCHEMA_PREFIX}-held-ready.v0.30",
            "pid": os.getpid(),
            "opened_at_ms": wall_ms(),
            "tls_peer_certificate_sha256": fingerprint,
            "first_status": first_status,
        }
        atomic_json(args.ready, ready)
        deadline = time.monotonic() + args.timeout
        while not args.release.is_file():
            if time.monotonic() >= deadline:
                raise SystemExit("timed out waiting for held-connection release barrier")
            time.sleep(0.01)
        stream.write(request_bytes)
        second_status, second_body = read_http_response(stream)
        result = {
            "schema": f"{SCHEMA_PREFIX}-held-connection.v0.30",
            "pid": os.getpid(),
            "opened_at_ms": ready["opened_at_ms"],
            "released_at_ms": wall_ms(),
            "tls_peer_certificate_sha256": fingerprint,
            "first_status": first_status,
            "second_status": second_status,
            "first_body": first_body,
            "second_body": second_body,
            "same_tls_connection": True,
        }
        atomic_json(args.output, result)
    finally:
        with contextlib.suppress(Exception):
            stream.close()
        with contextlib.suppress(Exception):
            connection.close()


def completion_payload(prompt: str, stream: bool) -> bytes:
    return json.dumps({
        "model": "inferlab-tiny",
        "stream": stream,
        "temperature": 0,
        "speculative_tokens": 0,
        "max_tokens": 8,
        "messages": [{"role": "user", "content": prompt}],
    }, separators=(",", ":")).encode()


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
    projected = {key: value for key, value in body.items() if key not in {"id", "created", "system_fingerprint"}}
    inferlab = projected.get("inferlab")
    if isinstance(inferlab, dict):
        inferlab = {key: value for key, value in inferlab.items() if key != "request_id"}
        inferlab["generation"] = project_generation(inferlab.get("generation"))
        projected["inferlab"] = inferlab
    return projected


def command_completion(args: argparse.Namespace) -> None:
    temporary = Path(args.temporary_body)
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
    print(json.dumps({"schema": f"{SCHEMA_PREFIX}-json.v0.30", "observation": observation}, indent=2, sort_keys=True))


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
        raise TimeoutError("SSE proof stream exceeded its total deadline")

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
    print(json.dumps({
        "schema": f"{SCHEMA_PREFIX}-sse.v0.30",
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
    }, indent=2, sort_keys=True))


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
        if path.name == "manifest.json" or not path.is_file():
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        scanned.append(path.name)
        markers = [marker for marker in PRIVATE_MARKERS if marker in text]
        host_paths = [value for value in (proof_root, project_root) if value and value in text]
        if HOST_PATH.search(text):
            host_paths.append("generic-host-path")
        fields = sensitive_paths(json.loads(text)) if path.suffix == ".json" else []
        if markers or host_paths or fields:
            problems.append({
                "file": path.name,
                "private_markers": markers,
                "host_paths": sorted(set(host_paths)),
                "sensitive_fields": fields,
            })
    result = {
        "schema": f"{SCHEMA_PREFIX}-sanitizer.v0.30",
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
    capture.add_argument("--expect-status", type=int, required=True)
    capture.add_argument("--timeout", type=float, default=3)
    capture.add_argument("--ca-cert", type=Path)
    capture.add_argument("--client-cert", type=Path)
    capture.add_argument("--client-key", type=Path)

    distributor = commands.add_parser("wait-distributor")
    distributor.add_argument("--url", required=True)
    distributor.add_argument("--policy-generation", type=int, required=True)
    distributor.add_argument("--tls-generation", type=int, required=True)
    distributor.add_argument("--expected-services", required=True)
    distributor.add_argument("--last-error-kind")
    distributor.add_argument("--min-rejections", type=int, default=0)
    distributor.add_argument("--peer-sha256")
    distributor.add_argument("--timeout", type=float, default=15)
    add_tls(distributor)

    controls = commands.add_parser("wait-controls")
    controls.add_argument("--urls", required=True)
    controls.add_argument("--revision", type=int, required=True)
    controls.add_argument("--policy-generation", type=int, required=True)
    controls.add_argument("--tls-generations", required=True)
    controls.add_argument("--require-receipt-generation", action="store_true")
    controls.add_argument("--timeout", type=float, default=15)

    control = commands.add_parser("wait-control")
    control.add_argument("--url", required=True)
    control.add_argument("--identity-id", required=True)
    control.add_argument("--tls-generation", type=int, required=True)
    control.add_argument("--last-error-kind")
    control.add_argument("--min-rejections", type=int, default=0)
    control.add_argument("--timeout", type=float, default=15)

    held = commands.add_parser("hold-connection")
    held.add_argument("--url", required=True)
    held.add_argument("--ready", type=Path, required=True)
    held.add_argument("--release", type=Path, required=True)
    held.add_argument("--output", type=Path, required=True)
    held.add_argument("--timeout", type=float, default=30)
    add_tls(held)

    completion = commands.add_parser("completion")
    completion.add_argument("--url", required=True)
    completion.add_argument("--prompt", required=True)
    completion.add_argument("--temporary-body", required=True)
    completion.add_argument("--timeout", type=float, default=15)

    stream = commands.add_parser("stream")
    stream.add_argument("--url", required=True)
    stream.add_argument("--prompt", required=True)
    stream.add_argument("--timeout", type=float, default=15)

    sanitize = commands.add_parser("sanitize-evidence")
    sanitize.add_argument("--evidence-dir", type=Path, required=True)
    sanitize.add_argument("--proof-root", type=Path, required=True)
    sanitize.add_argument("--project-root", type=Path, required=True)
    return parser


def main() -> None:
    args = build_parser().parse_args()
    {
        "capture": command_capture,
        "wait-distributor": command_wait_distributor,
        "wait-controls": command_wait_controls,
        "wait-control": command_wait_control,
        "hold-connection": command_hold_connection,
        "completion": command_completion,
        "stream": command_stream,
        "sanitize-evidence": command_sanitize,
    }[args.command](args)


if __name__ == "__main__":
    main()
