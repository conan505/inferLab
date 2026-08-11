#!/usr/bin/env python3
"""Bounded live probes and redaction helpers for the v0.29 signer handoff proof."""

from __future__ import annotations

import argparse
import base64
import contextlib
import hashlib
import json
import os
import re
import signal
import ssl
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any, Callable


SCHEMA_PREFIX = "inferlab.signer-handoff"
SERVICE_HEADERS = {
    "schema": "x-inferlab-service-auth-schema",
    "algorithm": "x-inferlab-service-auth-algorithm",
    "service_id": "x-inferlab-service-id",
    "audience_id": "x-inferlab-service-audience",
    "issued_at_ms": "x-inferlab-service-issued-at-ms",
    "nonce": "x-inferlab-service-nonce",
    "signature": "x-inferlab-service-signature",
}
RETAINED_RESPONSE_HEADERS = {
    "x-inferlab-attempts",
    "x-inferlab-config-revision",
    "x-inferlab-config-term",
    "x-inferlab-control-cluster",
    "x-inferlab-control-key-id",
    "x-inferlab-worker",
}
HOST_PATH = re.compile(r"(?:/Users/|/home/|/tmp/|/private/var/|/var/folders/|/workspace/|/github/workspace)")
PRIVATE_MARKERS = (
    "-----BEGIN PRIVATE KEY-----",
    "-----BEGIN RSA PRIVATE KEY-----",
    "-----BEGIN EC PRIVATE KEY-----",
    "PRIVATE_KEY_B64",
    "PRIVATE_KEY_BASE64",
)
MAX_SSE_BYTES = 1_048_576
MAX_SSE_LINES = 512
MAX_SSE_EVENTS = 256
MAX_SSE_LINE_BYTES = 65_536
SENSITIVE_FIELDS = {
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


def wall_ms() -> int:
    return time.time_ns() // 1_000_000


def monotonic_ms() -> float:
    return round(time.monotonic_ns() / 1_000_000, 3)


@contextlib.contextmanager
def total_deadline(seconds: float, label: str):
    def expired(_signum: int, _frame: Any) -> None:
        raise TimeoutError(f"{label} exceeded its total deadline")

    previous_handler = signal.signal(signal.SIGALRM, expired)
    signal.setitimer(signal.ITIMER_REAL, seconds)
    try:
        yield
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous_handler)


def read_capped(stream: Any, limit: int = 1_048_576) -> bytes:
    chunks: list[bytes] = []
    total = 0
    while True:
        chunk = stream.read(min(65_536, limit + 1 - total))
        if not chunk:
            return b"".join(chunks)
        chunks.append(chunk)
        total += len(chunk)
        if total > limit:
            raise ValueError("response exceeds proof byte limit")


def exact_int(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def load_json(path: Path) -> Any:
    with path.open(encoding="utf-8") as source:
        return json.load(source)


def tls_context(ca: Path | None, cert: Path | None, key: Path | None) -> ssl.SSLContext | None:
    if ca is None and cert is None and key is None:
        return None
    if ca is None or cert is None or key is None:
        raise SystemExit("TLS capture requires --ca-cert, --client-cert and --client-key together")
    context = ssl.create_default_context(cafile=str(ca))
    context.minimum_version = ssl.TLSVersion.TLSv1_3
    context.maximum_version = ssl.TLSVersion.TLSv1_3
    context.load_cert_chain(str(cert), str(key))
    return context


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: ANN001
        return None


def opener(context: ssl.SSLContext | None) -> urllib.request.OpenerDirector:
    handlers: list[Any] = [urllib.request.ProxyHandler({}), NoRedirect()]
    if context is not None:
        handlers.append(urllib.request.HTTPSHandler(context=context))
    return urllib.request.build_opener(*handlers)


def authentication_headers(path: Path | None) -> dict[str, str]:
    if path is None:
        return {}
    authentication = load_json(path)
    if not isinstance(authentication, dict) or set(authentication) != set(SERVICE_HEADERS):
        raise SystemExit("authentication JSON has an unexpected schema")
    return {
        header: str(authentication[field])
        for field, header in SERVICE_HEADERS.items()
    }


def decode_body(raw: bytes) -> Any:
    if not raw:
        return None
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        return raw.decode("utf-8", errors="replace")


def project_control_status(body: Any) -> dict[str, Any]:
    if not isinstance(body, dict):
        raise ValueError("control status body is not an object")
    signing = body.get("service_signing")
    authentication = body.get("service_authentication")
    committed = body.get("committed_configuration")
    if not isinstance(signing, dict) or not isinstance(authentication, dict):
        raise ValueError("control status omits signer/authentication state")
    projected_committed = None
    if isinstance(committed, dict):
        configuration = committed.get("configuration")
        workers = configuration.get("workers") if isinstance(configuration, dict) else None
        projected_committed = {
            "cluster_id": committed.get("cluster_id"),
            "revision": committed.get("revision"),
            "term": committed.get("term"),
            "routing_policy": configuration.get("routing_policy") if isinstance(configuration, dict) else None,
            "worker_ids": sorted(
                item.get("id") for item in workers if isinstance(item, dict)
            ) if isinstance(workers, list) else [],
        }
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
        "local_service_credential_id": body.get("local_service_credential_id"),
        "committed_configuration": projected_committed,
        "service_signing": {
            key: signing.get(key)
            for key in (
                "mode",
                "service_id",
                "active_credential_id",
                "bundle_generation",
                "configured_credential_count",
                "successful_activations",
                "rejected_reloads",
                "last_error_kind",
            )
        },
        "service_authentication": {
            key: authentication.get(key)
            for key in (
                "required",
                "trusted_service_ids",
                "trusted_service_credentials",
                "revoked_service_credentials",
                "gateway_service_ids",
                "verifications",
                "authentication_rejections",
                "credential_revocation_rejections",
                "authorized_peer_rpcs",
                "authorized_gateway_reads",
                "last_verified_service_id",
                "last_verified_service_credential",
                "last_rejected_service_id",
                "last_rejected_service_credential",
                "trust_policy_generation",
                "trust_policy_validity",
            )
        },
    }


def project_gateway_status(body: Any) -> dict[str, Any]:
    if not isinstance(body, dict):
        raise ValueError("gateway operator status body is not an object")
    control_plane = body.get("control_plane")
    signing = control_plane.get("service_signing") if isinstance(control_plane, dict) else None
    snapshot = body.get("routing_snapshot")
    workers = body.get("workers")
    if not isinstance(signing, dict) or not isinstance(snapshot, dict) or not isinstance(workers, list):
        raise ValueError("gateway status omits signer/routing state")
    return {
        "routing_policy": body.get("routing_policy"),
        "worker_ids": sorted(item.get("id") for item in workers if isinstance(item, dict)),
        "routing_snapshot": {
            key: snapshot.get(key)
            for key in ("control_cluster_id", "control_revision", "control_term", "control_signing_key_id")
        },
        "control_plane": {
            key: control_plane.get(key)
            for key in (
                "enabled",
                "service_authentication_enabled",
                "service_id",
                "service_credential_id",
                "revision",
                "term",
                "last_error",
            )
        },
        "service_signing": {
            key: signing.get(key)
            for key in (
                "mode",
                "active_credential_id",
                "bundle_generation",
                "configured_credential_count",
                "successful_activations",
                "rejected_reloads",
                "last_error_kind",
            )
        },
    }


def project_success_completion(body: Any) -> Any:
    if not isinstance(body, dict):
        return body
    inferlab = body.get("inferlab")
    if not isinstance(inferlab, dict) or not isinstance(inferlab.get("generation"), dict):
        raise ValueError("successful completion omits generation metrics")
    projected_inferlab = {
        key: value for key, value in inferlab.items() if key not in {"request_id", "generation"}
    }
    projected_inferlab["generation"] = project_generation_metrics(inferlab["generation"])
    projected = {
        key: value
        for key, value in body.items()
        if key not in {"id", "created", "system_fingerprint", "inferlab"}
    }
    if projected_inferlab is not None:
        projected["inferlab"] = projected_inferlab
    return projected


def project_generation_metrics(generation: dict[str, Any]) -> dict[str, Any]:
    projected = dict(generation)
    decoding = projected.get("decoding")
    if not isinstance(decoding, dict) or not exact_int(decoding.get("seed")) or decoding["seed"] != 0:
        raise ValueError("proof completion decoding seed is not the expected public zero value")
    projected_decoding = dict(decoding)
    projected_decoding.pop("seed")
    projected["decoding"] = projected_decoding
    return projected


def project_control_configuration(body: Any) -> dict[str, Any]:
    if not isinstance(body, dict):
        raise ValueError("control configuration body is not an object")
    configuration = body.get("configuration")
    workers = configuration.get("workers") if isinstance(configuration, dict) else None
    return {
        "cluster_id": body.get("cluster_id"),
        "revision": body.get("revision"),
        "term": body.get("term"),
        "routing_policy": configuration.get("routing_policy") if isinstance(configuration, dict) else None,
        "worker_ids": sorted(item.get("id") for item in workers if isinstance(item, dict)) if isinstance(workers, list) else [],
        "authentication_key_id": body.get("authentication", {}).get("key_id") if isinstance(body.get("authentication"), dict) else None,
    }


def response_record(
    response: Any,
    raw: bytes,
    *,
    method: str,
    url: str,
    started_wall: int,
    started_mono: float,
    projection: str,
) -> dict[str, Any]:
    body = decode_body(raw)
    if projection == "control-status" and response.status == 200:
        body = project_control_status(body)
    elif projection == "gateway-status" and response.status == 200:
        body = project_gateway_status(body)
    elif projection == "completion" and response.status == 200:
        body = project_success_completion(body)
    elif projection == "control-config" and response.status == 200:
        body = project_control_configuration(body)
    headers = {
        key.lower(): value
        for key, value in response.headers.items()
        if key.lower() in RETAINED_RESPONSE_HEADERS
    }
    return {
        "method": method,
        "path": urllib.parse.urlsplit(url).path,
        "started_at_ms": started_wall,
        "observed_at_ms": wall_ms(),
        "duration_ms": round(monotonic_ms() - started_mono, 3),
        "status": response.status,
        "headers": dict(sorted(headers.items())),
        "body": body,
    }


def request(
    url: str,
    *,
    method: str = "GET",
    body_path: Path | None = None,
    auth_path: Path | None = None,
    timeout: float = 3,
    context: ssl.SSLContext | None = None,
    projection: str = "raw",
) -> dict[str, Any]:
    body = None if body_path is None else body_path.read_bytes()
    headers = authentication_headers(auth_path)
    if body is not None:
        headers["content-type"] = "application/json"
    req = urllib.request.Request(url, data=body, method=method, headers=headers)
    started_wall = wall_ms()
    started_mono = monotonic_ms()
    with total_deadline(timeout, "HTTP proof capture"):
        try:
            with opener(context).open(req, timeout=timeout) as response:
                return response_record(
                    response,
                    read_capped(response),
                    method=method,
                    url=url,
                    started_wall=started_wall,
                    started_mono=started_mono,
                    projection=projection,
                )
        except urllib.error.HTTPError as error:
            return response_record(
                error,
                read_capped(error),
                method=method,
                url=url,
                started_wall=started_wall,
                started_mono=started_mono,
                projection=projection,
            )


def wait_for(predicate: Callable[[], dict[str, Any] | None], timeout: float, label: str) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    samples = 0
    while time.monotonic() < deadline:
        samples += 1
        try:
            result = predicate()
        except (OSError, ValueError, urllib.error.URLError):
            result = None
        if result is not None:
            return {"samples": samples, "result": result}
        time.sleep(0.025)
    raise SystemExit(f"timed out waiting for {label}")


def control_snapshot(urls: list[str]) -> list[dict[str, Any]]:
    values = []
    for url in urls:
        observation = request(url.rstrip("/") + "/v1/control/status", projection="control-status", timeout=0.5)
        if observation["status"] != 200:
            raise ValueError("control status was not 200")
        values.append(observation["body"])
    return values


def exact_cluster(controls: list[dict[str, Any]], revision: int | None = None) -> bool:
    ids = {"control-a", "control-b", "control-c"}
    if len(controls) != 3 or {item.get("node_id") for item in controls} != ids:
        return False
    leaders = [item for item in controls if item.get("role") == "leader"]
    if len(leaders) != 1:
        return False
    leader_id = leaders[0].get("node_id")
    terms = {item.get("term") for item in controls}
    if len(terms) != 1 or any(item.get("leader_id") != leader_id for item in controls):
        return False
    if revision is not None:
        for item in controls:
            committed = item.get("committed_configuration")
            if not isinstance(committed, dict) or committed.get("revision") != revision:
                return False
    return all(item.get("storage_healthy") is True for item in controls)


def command_wait_controls(args: argparse.Namespace) -> None:
    urls = [item for item in args.urls.split(",") if item]

    def predicate() -> dict[str, Any] | None:
        controls = control_snapshot(urls)
        if not exact_cluster(controls, args.revision):
            return None
        expected = dict(item.split("=", 1) for item in args.expected_signers.split(","))
        generations = (
            {key: int(value) for key, value in (item.split("=", 1) for item in args.expected_generations.split(","))}
            if args.expected_generations
            else {service: args.bundle_generation for service in expected}
        )
        for control in controls:
            signing = control["service_signing"]
            if (
                signing.get("bundle_generation") != generations.get(control["node_id"])
                or signing.get("active_credential_id") != expected.get(control["node_id"])
                or signing.get("last_error_kind") != args.last_error_kind
            ):
                return None
        return {
            "leader_id": next(item["node_id"] for item in controls if item["role"] == "leader"),
            "term": controls[0]["term"],
            "revision": args.revision,
            "controls": controls,
        }

    observed = wait_for(predicate, args.timeout, "control cluster signer state")
    print(json.dumps({
        "schema": f"{SCHEMA_PREFIX}-controls.v0.29",
        "observed_at_ms": wall_ms(),
        **observed,
    }, indent=2, sort_keys=True))


def command_wait_control_signer(args: argparse.Namespace) -> None:
    def predicate() -> dict[str, Any] | None:
        observation = request(
            args.url.rstrip("/") + "/v1/control/status",
            projection="control-status",
            timeout=0.5,
        )
        if observation["status"] != 200:
            return None
        body = observation["body"]
        signing = body.get("service_signing")
        if (
            body.get("node_id") != args.service_id
            or not isinstance(signing, dict)
            or signing.get("active_credential_id") != args.credential
            or signing.get("bundle_generation") != args.bundle_generation
            or signing.get("last_error_kind") != args.last_error_kind
            or not exact_int(signing.get("rejected_reloads"))
            or signing["rejected_reloads"] < args.min_rejections
        ):
            return None
        return observation

    observed = wait_for(predicate, args.timeout, "one control signer state")
    print(json.dumps({
        "schema": f"{SCHEMA_PREFIX}-control-signer.v0.29",
        "observed_at_ms": wall_ms(),
        **observed,
    }, indent=2, sort_keys=True))


def command_wait_gateway(args: argparse.Namespace) -> None:
    def predicate() -> dict[str, Any] | None:
        observation = request(args.url.rstrip("/") + "/internal/workers", projection="gateway-status", timeout=1)
        if observation["status"] != 200:
            return None
        body = observation["body"]
        signing = body["service_signing"]
        routing = body["routing_snapshot"]
        if (
            signing.get("bundle_generation") != args.bundle_generation
            or signing.get("active_credential_id") != args.credential
            or signing.get("last_error_kind") != args.last_error_kind
            or routing.get("control_revision") != args.revision
            or body.get("worker_ids") != [args.worker_id]
        ):
            return None
        return observation

    observed = wait_for(predicate, args.timeout, "gateway signer and routing state")
    print(json.dumps({
        "schema": f"{SCHEMA_PREFIX}-gateway.v0.29",
        "observed_at_ms": wall_ms(),
        **observed,
    }, indent=2, sort_keys=True))


def command_wait_distributor(args: argparse.Namespace) -> None:
    context = tls_context(args.ca_cert, args.client_cert, args.client_key)
    expected_services = sorted(item for item in args.expected_services.split(",") if item)

    def predicate() -> dict[str, Any] | None:
        observation = request(
            args.url.rstrip("/") + "/v1/service-trust/status",
            context=context,
            timeout=1,
        )
        if observation["status"] != 200 or not isinstance(observation["body"], dict):
            return None
        body = observation["body"]
        snapshot = body.get("snapshot")
        receipts = body.get("receipts")
        if (
            body.get("expected_receiver_mode") != "service-id"
            or body.get("expected_receivers") != expected_services
            or body.get("acked_receivers") != expected_services
            or body.get("pending_receivers") != []
            or body.get("receipt_count") != len(expected_services)
            or not isinstance(snapshot, dict)
            or snapshot.get("generation") != args.generation
            or not isinstance(receipts, list)
        ):
            return None
        credentials = sorted(
            receipt.get("receiver_credential_id")
            for receipt in receipts if isinstance(receipt, dict)
        )
        services = sorted(
            receipt.get("receiver_service_id")
            for receipt in receipts if isinstance(receipt, dict)
        )
        if services != expected_services or credentials != [args.credential] * len(expected_services):
            return None
        return observation

    observed = wait_for(predicate, args.timeout, "service-scoped distributor convergence")
    print(json.dumps({
        "schema": f"{SCHEMA_PREFIX}-distributor.v0.29",
        "observed_at_ms": wall_ms(),
        **observed,
    }, indent=2, sort_keys=True))


def completion_payload(prompt: str, stream: bool) -> bytes:
    return json.dumps({
        "model": "inferlab-tiny",
        "stream": stream,
        "temperature": 0,
        "speculative_tokens": 0,
        "max_tokens": 8,
        "messages": [{"role": "user", "content": prompt}],
    }, separators=(",", ":")).encode()


def command_completion(args: argparse.Namespace) -> None:
    url = args.url.rstrip("/") + "/v1/chat/completions"
    payload = completion_payload(args.prompt, False)
    temporary = Path(args.temporary_body)
    temporary.write_bytes(payload)
    try:
        observation = request(url, method="POST", body_path=temporary, projection="completion", timeout=args.timeout)
    finally:
        temporary.unlink(missing_ok=True)
    print(json.dumps({
        "schema": f"{SCHEMA_PREFIX}-json.v0.29",
        "observation": observation,
    }, indent=2, sort_keys=True))


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
    deadline_mono = started_mono + (args.timeout * 1_000)
    events: list[Any] = []
    offsets: list[float] = []
    done_seen = False
    total_bytes = 0
    total_lines = 0
    pending = bytearray()

    def deadline_expired(_signum: int, _frame: Any) -> None:
        raise TimeoutError("SSE proof stream exceeded its total deadline")

    def consume_line(raw_line: bytes) -> None:
        nonlocal done_seen, total_lines
        total_lines += 1
        if total_lines > MAX_SSE_LINES:
            raise SystemExit("SSE proof stream exceeded its line ceiling")
        if len(raw_line) > MAX_SSE_LINE_BYTES:
            raise SystemExit("SSE proof stream exceeded its line-byte ceiling")
        line = raw_line.rstrip(b"\r").decode("utf-8", errors="strict")
        if not line:
            return
        if not line.startswith("data: "):
            raise SystemExit("unexpected SSE field in proof stream")
        value = line[6:]
        offsets.append(round(monotonic_ms() - started_mono, 3))
        if value == "[DONE]":
            if done_seen:
                raise SystemExit("duplicate SSE terminal sentinel")
            done_seen = True
            events.append("[DONE]")
        else:
            if done_seen:
                raise SystemExit("SSE data followed terminal sentinel")
            decoded = json.loads(value)
            if isinstance(decoded, dict):
                decoded.pop("id", None)
                decoded.pop("created", None)
                decoded.pop("system_fingerprint", None)
                inferlab = decoded.get("inferlab")
                if isinstance(inferlab, dict):
                    inferlab.pop("request_id", None)
                    generation = inferlab.get("generation")
                    if isinstance(generation, dict):
                        inferlab["generation"] = project_generation_metrics(generation)
            events.append(decoded)
        if len(events) > MAX_SSE_EVENTS:
            raise SystemExit("SSE proof stream exceeded its event ceiling")

    previous_handler = signal.signal(signal.SIGALRM, deadline_expired)
    signal.setitimer(signal.ITIMER_REAL, args.timeout)
    try:
        with opener(None).open(req, timeout=args.timeout) as response:
            while True:
                chunk = response.read1(4_096)
                if not chunk:
                    break
                if monotonic_ms() > deadline_mono:
                    raise TimeoutError("SSE proof stream exceeded its total deadline")
                total_bytes += len(chunk)
                if total_bytes > MAX_SSE_BYTES:
                    raise SystemExit("SSE proof stream exceeded its byte ceiling")
                pending.extend(chunk)
                while b"\n" in pending:
                    raw_line, _, remainder = pending.partition(b"\n")
                    pending = bytearray(remainder)
                    consume_line(raw_line)
                if len(pending) > MAX_SSE_LINE_BYTES:
                    raise SystemExit("SSE proof stream exceeded its line-byte ceiling")
            if pending:
                raise SystemExit("SSE proof stream ended with an unterminated line")
            eof_after_done = done_seen
            response_status = response.status
            response_headers = {
                key.lower(): value
                for key, value in response.headers.items()
                if key.lower() in RETAINED_RESPONSE_HEADERS
            }
    except TimeoutError as error:
        raise SystemExit(str(error)) from error
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous_handler)

    pieces: list[str] = []
    finish_reason = None
    generation = None
    for event in events:
        if not isinstance(event, dict) or not isinstance(event.get("choices"), list):
            continue
        choice = event["choices"][0]
        content = choice.get("delta", {}).get("content")
        if isinstance(content, str) and content:
            pieces.append(content)
        if choice.get("finish_reason") is not None:
            finish_reason = choice["finish_reason"]
            generation = event.get("inferlab", {}).get("generation")
    result = {
        "schema": f"{SCHEMA_PREFIX}-sse.v0.29",
        "method": "POST",
        "path": "/v1/chat/completions",
        "started_at_ms": started_wall,
        "observed_at_ms": wall_ms(),
        "duration_ms": round(monotonic_ms() - started_mono, 3),
        "status": response_status,
        "headers": response_headers,
        "event_count": len(events),
        "content_event_count": len(pieces),
        "offsets_ms": offsets,
        "pieces": pieces,
        "content": "".join(pieces),
        "finish_reason": finish_reason,
        "generation": generation,
        "done_received": done_seen,
        "eof_after_done": eof_after_done,
    }
    print(json.dumps(result, indent=2, sort_keys=True))


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
        raw = path.read_bytes()
        text = raw.decode("utf-8", errors="replace")
        scanned.append(path.name)
        markers = [marker for marker in PRIVATE_MARKERS if marker in text]
        host_paths = [value for value in (proof_root, project_root) if value and value in text]
        if HOST_PATH.search(text):
            host_paths.append("generic-host-path")
        fields: list[str] = []
        if path.suffix == ".json":
            fields = sensitive_paths(json.loads(text))
        if markers or host_paths or fields:
            problems.append({
                "file": path.name,
                "private_markers": markers,
                "host_paths": sorted(set(host_paths)),
                "sensitive_fields": fields,
            })
    result = {
        "schema": f"{SCHEMA_PREFIX}-sanitizer.v0.29",
        "files_scanned": scanned,
        "private_markers": list(PRIVATE_MARKERS),
        "sensitive_fields": sorted(SENSITIVE_FIELDS),
        "problem_count": len(problems),
        "problems": problems,
    }
    print(json.dumps(result, indent=2, sort_keys=True))
    if problems:
        raise SystemExit(1)


def add_tls(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--ca-cert", type=Path)
    parser.add_argument("--client-cert", type=Path)
    parser.add_argument("--client-key", type=Path)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)

    capture = commands.add_parser("capture")
    capture.add_argument("--url", required=True)
    capture.add_argument("--method", default="GET")
    capture.add_argument("--body", type=Path)
    capture.add_argument("--authentication", type=Path)
    capture.add_argument("--projection", choices=("raw", "control-status", "gateway-status", "control-config", "completion"), default="raw")
    capture.add_argument("--expect-status", type=int, required=True)
    capture.add_argument("--timeout", type=float, default=3)
    add_tls(capture)

    controls = commands.add_parser("wait-controls")
    controls.add_argument("--urls", required=True)
    controls.add_argument("--revision", type=int, required=True)
    controls.add_argument("--bundle-generation", type=int)
    controls.add_argument("--expected-generations")
    controls.add_argument("--expected-signers", required=True)
    controls.add_argument("--last-error-kind")
    controls.add_argument("--timeout", type=float, default=15)

    control_signer = commands.add_parser("wait-control-signer")
    control_signer.add_argument("--url", required=True)
    control_signer.add_argument("--service-id", required=True)
    control_signer.add_argument("--credential", required=True)
    control_signer.add_argument("--bundle-generation", type=int, required=True)
    control_signer.add_argument("--last-error-kind")
    control_signer.add_argument("--min-rejections", type=int, default=0)
    control_signer.add_argument("--timeout", type=float, default=10)

    gateway = commands.add_parser("wait-gateway")
    gateway.add_argument("--url", required=True)
    gateway.add_argument("--revision", type=int, required=True)
    gateway.add_argument("--bundle-generation", type=int, required=True)
    gateway.add_argument("--credential", required=True)
    gateway.add_argument("--worker-id", required=True)
    gateway.add_argument("--last-error-kind")
    gateway.add_argument("--timeout", type=float, default=15)

    distributor = commands.add_parser("wait-distributor")
    distributor.add_argument("--url", required=True)
    distributor.add_argument("--generation", type=int, required=True)
    distributor.add_argument("--credential", required=True)
    distributor.add_argument("--expected-services", required=True)
    distributor.add_argument("--timeout", type=float, default=15)
    add_tls(distributor)

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
    if args.command == "capture":
        context = tls_context(args.ca_cert, args.client_cert, args.client_key)
        result = request(
            args.url,
            method=args.method,
            body_path=args.body,
            auth_path=args.authentication,
            timeout=args.timeout,
            context=context,
            projection=args.projection,
        )
        print(json.dumps({
            "schema": f"{SCHEMA_PREFIX}-capture.v0.29",
            "observation": result,
        }, indent=2, sort_keys=True))
        if result["status"] != args.expect_status:
            raise SystemExit(1)
    elif args.command == "wait-controls":
        command_wait_controls(args)
    elif args.command == "wait-control-signer":
        command_wait_control_signer(args)
    elif args.command == "wait-gateway":
        command_wait_gateway(args)
    elif args.command == "wait-distributor":
        command_wait_distributor(args)
    elif args.command == "completion":
        command_completion(args)
    elif args.command == "stream":
        command_stream(args)
    else:
        command_sanitize(args)


if __name__ == "__main__":
    main()
