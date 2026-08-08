#!/usr/bin/env python3
"""Capture the v0.27 signed service-trust expiry proof.

The probe is deliberately standard-library only.  HTTPS uses only the
proof-owned CA, never ambient proxies, and never retains certificate or key
bytes.  Wall time is retained only where the signed absolute deadline is the
subject of the experiment; elapsed durations use a monotonic clock.
"""

from __future__ import annotations

import argparse
import base64
import binascii
import hashlib
import http.client
import json
import os
import ssl
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


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
    return time.perf_counter_ns() / 1_000_000


def csv_values(raw: str) -> list[str]:
    return [item.strip() for item in raw.split(",") if item.strip()]


def private_ca_context(
    ca_cert: str,
    client_cert: str | None,
    client_key: str | None,
) -> ssl.SSLContext:
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    context.verify_mode = ssl.CERT_REQUIRED
    context.check_hostname = True
    context.load_verify_locations(cafile=ca_cert)
    context.minimum_version = ssl.TLSVersion.TLSv1_3
    context.maximum_version = ssl.TLSVersion.TLSv1_3
    if client_cert is not None or client_key is not None:
        if not client_cert or not client_key:
            raise SystemExit("client certificate and key must be supplied together")
        context.load_cert_chain(client_cert, client_key)
    return context


def opener_for(
    url: str,
    *,
    ca_cert: str | None = None,
    client_cert: str | None = None,
    client_key: str | None = None,
) -> urllib.request.OpenerDirector:
    handlers: list[Any] = [urllib.request.ProxyHandler({}), NoRedirect()]
    if url.startswith("https://"):
        if not ca_cert:
            raise SystemExit("HTTPS proof request requires --ca-cert")
        handlers.append(
            urllib.request.HTTPSHandler(
                context=private_ca_context(ca_cert, client_cert, client_key)
            )
        )
    return urllib.request.build_opener(*handlers)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args: Any, **kwargs: Any) -> None:
        return None


def decode_body(raw: bytes) -> Any:
    if not raw:
        return None
    try:
        return json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        return raw.decode("utf-8", errors="replace")


def stable_transport_error(error: BaseException) -> str:
    reason = getattr(error, "reason", error)
    if isinstance(reason, BaseException):
        return type(reason).__name__
    return type(error).__name__


def request(
    url: str,
    *,
    method: str = "GET",
    body: Any = None,
    headers: dict[str, str] | None = None,
    timeout: float = 1.0,
    ca_cert: str | None = None,
    client_cert: str | None = None,
    client_key: str | None = None,
) -> dict[str, Any]:
    encoded: bytes | None = None
    request_headers = dict(headers or {})
    if body is not None:
        encoded = json.dumps(body, separators=(",", ":"), sort_keys=True).encode()
        request_headers.setdefault("content-type", "application/json")
    outgoing = urllib.request.Request(
        url, data=encoded, headers=request_headers, method=method
    )
    started_at_ms = wall_ms()
    started = monotonic_ms()
    opener = opener_for(
        url,
        ca_cert=ca_cert,
        client_cert=client_cert,
        client_key=client_key,
    )
    try:
        response = opener.open(outgoing, timeout=timeout)
    except urllib.error.HTTPError as error:
        response = error
    except (
        urllib.error.URLError,
        TimeoutError,
        ssl.SSLError,
        OSError,
        http.client.HTTPException,
    ) as error:
        return {
            "status": None,
            "started_at_ms": started_at_ms,
            "completed_at_ms": wall_ms(),
            "duration_ms": round(monotonic_ms() - started, 3),
            "transport_error": stable_transport_error(error),
        }
    with response:
        raw = response.read()
        return {
            "status": response.status,
            "started_at_ms": started_at_ms,
            "completed_at_ms": wall_ms(),
            "duration_ms": round(monotonic_ms() - started, 3),
            "etag": response.headers.get("etag"),
            "content_type": response.headers.get("content-type"),
            "body": decode_body(raw),
        }


def authentication_headers(path: Path | None) -> dict[str, str]:
    if path is None:
        return {}
    authentication = json.loads(path.read_text(encoding="utf-8"))
    missing = sorted(set(AUTHENTICATION_HEADERS) - set(authentication))
    if missing:
        raise SystemExit(f"authentication object is missing fields: {missing}")
    return {
        header: str(authentication[field])
        for field, header in AUTHENTICATION_HEADERS.items()
    }


def public_authentication(path: Path | None) -> dict[str, Any] | None:
    if path is None:
        return None
    authentication = json.loads(path.read_text(encoding="utf-8"))
    signature = str(authentication.get("signature", ""))
    try:
        decoded_signature = base64.b64decode(signature, validate=True)
    except (ValueError, binascii.Error):
        decoded_signature = b""
    return {
        "schema": authentication.get("schema"),
        "algorithm": authentication.get("algorithm"),
        "service_id": authentication.get("service_id"),
        "audience_id": authentication.get("audience_id"),
        "issued_at_ms": authentication.get("issued_at_ms"),
        "nonce": authentication.get("nonce"),
        "signature_present": bool(authentication.get("signature")),
        "signature_bytes": len(decoded_signature),
        "signature_sha256": hashlib.sha256(decoded_signature).hexdigest()
        if decoded_signature
        else None,
    }


def wait_loop(
    timeout: float,
    sample: Any,
    matches: Any,
    description: str,
) -> tuple[int, Any]:
    deadline = time.monotonic() + timeout
    count = 0
    latest: Any = None
    while True:
        count += 1
        latest = sample()
        if matches(latest):
            return count, latest
        if time.monotonic() >= deadline:
            raise SystemExit(
                json.dumps(
                    {"error": f"timed out waiting for {description}", "latest": latest},
                    indent=2,
                    sort_keys=True,
                )
            )
        time.sleep(0.025)


def command_capture(args: argparse.Namespace) -> None:
    body = None
    if args.body:
        body = json.loads(args.body.read_text(encoding="utf-8"))
    observation = request(
        args.url,
        method=args.method,
        body=body,
        timeout=args.timeout,
        ca_cert=args.ca_cert,
        client_cert=args.client_cert,
        client_key=args.client_key,
    )
    result = {
        "schema": "inferlab.trust-expiry-http-capture.v0.27",
        "method": args.method,
        "observation": observation,
    }
    print(json.dumps(result, indent=2, sort_keys=True))
    if observation.get("status") != args.expect_status:
        raise SystemExit(1)


def command_expect_transport_failure(args: argparse.Namespace) -> None:
    observation = request(
        args.url,
        timeout=args.timeout,
        ca_cert=args.ca_cert,
        client_cert=args.client_cert,
        client_key=args.client_key,
    )
    result = {
        "schema": "inferlab.trust-expiry-transport-failure.v0.27",
        "scenario": args.scenario,
        "failed_before_http_response": observation.get("status") is None,
        "observation": observation,
    }
    print(json.dumps(result, indent=2, sort_keys=True))
    if observation.get("status") is not None:
        raise SystemExit(1)


def command_wait_controls(args: argparse.Namespace) -> None:
    urls = csv_values(args.urls)
    if not urls:
        raise SystemExit("at least one control URL is required")
    started_at_ms = wall_ms()
    started = monotonic_ms()

    def sample() -> list[dict[str, Any]]:
        return [request(f"{url.rstrip('/')}/v1/control/status") for url in urls]

    def matches(observations: list[dict[str, Any]]) -> bool:
        for observation in observations:
            auth = observation.get("body", {}).get("service_authentication", {})
            if observation.get("status") != 200:
                return False
            if auth.get("trust_policy_generation") != args.generation:
                return False
            if args.validity and auth.get("trust_policy_validity") != args.validity:
                return False
            if (
                args.expires_at_ms is not None
                and auth.get("trust_policy_expires_at_ms") != args.expires_at_ms
            ):
                return False
            if (
                args.bootstrap_source
                and auth.get("trust_policy_bootstrap_source") != args.bootstrap_source
            ):
                return False
            if (
                args.last_fetch_outcome
                and auth.get("trust_policy_last_fetch_outcome")
                != args.last_fetch_outcome
            ):
                return False
            if auth.get("trust_policy_expiration_rejections", 0) < args.min_expiration_rejections:
                return False
        return True

    samples, statuses = wait_loop(
        args.timeout,
        sample,
        matches,
        f"{len(urls)} controls at generation {args.generation}",
    )
    print(
        json.dumps(
            {
                "schema": "inferlab.trust-expiry-controls.v0.27",
                "started_at_ms": started_at_ms,
                "completed_at_ms": wall_ms(),
                "duration_ms": round(monotonic_ms() - started, 3),
                "samples": samples,
                "expected": {
                    "generation": args.generation,
                    "validity": args.validity,
                    "expires_at_ms": args.expires_at_ms,
                    "bootstrap_source": args.bootstrap_source,
                    "last_fetch_outcome": args.last_fetch_outcome,
                    "minimum_expiration_rejections": args.min_expiration_rejections,
                },
                "statuses": statuses,
            },
            indent=2,
            sort_keys=True,
        )
    )


def command_wait_distributor(args: argparse.Namespace) -> None:
    expected_acked = sorted(csv_values(args.acked_receivers))
    status_url = f"{args.url.rstrip('/')}/v1/service-trust/status"
    started_at_ms = wall_ms()
    started = monotonic_ms()

    def sample() -> dict[str, Any]:
        return request(
            status_url,
            ca_cert=args.ca_cert,
            client_cert=args.client_cert,
            client_key=args.client_key,
        )

    def matches(observation: dict[str, Any]) -> bool:
        body = observation.get("body", {})
        snapshot = body.get("snapshot") or {}
        return (
            observation.get("status") == 200
            and snapshot.get("generation") == args.generation
            and snapshot.get("policy_schema") == args.policy_schema
            and snapshot.get("expires_at_ms") == args.expires_at_ms
            and body.get("acked_receivers") == expected_acked
            and body.get("pending_receivers") == []
            and body.get("receipt_count") == len(expected_acked)
        )

    samples, status = wait_loop(
        args.timeout,
        sample,
        matches,
        f"distributor generation {args.generation} and receipts",
    )
    print(
        json.dumps(
            {
                "schema": "inferlab.trust-expiry-distributor.v0.27",
                "started_at_ms": started_at_ms,
                "completed_at_ms": wall_ms(),
                "duration_ms": round(monotonic_ms() - started, 3),
                "samples": samples,
                "expected_generation": args.generation,
                "expected_policy_schema": args.policy_schema,
                "expected_expires_at_ms": args.expires_at_ms,
                "expected_acked_receivers": expected_acked,
                "status": status,
            },
            indent=2,
            sort_keys=True,
        )
    )


def command_conditional_get(args: argparse.Namespace) -> None:
    url = f"{args.url.rstrip('/')}/v1/service-trust/snapshot"
    initial = request(
        url,
        ca_cert=args.ca_cert,
        client_cert=args.client_cert,
        client_key=args.client_key,
    )
    if initial.get("status") != 200 or not initial.get("etag"):
        raise SystemExit("could not obtain the current snapshot ETag")
    conditional = request(
        url,
        headers={"if-none-match": str(initial["etag"])},
        ca_cert=args.ca_cert,
        client_cert=args.client_cert,
        client_key=args.client_key,
    )
    result = {
        "schema": "inferlab.trust-expiry-conditional-get.v0.27",
        "expected_generation": args.generation,
        "expected_expires_at_ms": args.expires_at_ms,
        "etag_stable": conditional.get("etag") in {None, initial.get("etag")},
        "initial": initial,
        "conditional": conditional,
    }
    print(json.dumps(result, indent=2, sort_keys=True))
    body = initial.get("body", {})
    if not (
        body.get("generation") == args.generation
        and body.get("expires_at_ms") == args.expires_at_ms
        and conditional.get("status") == 304
        and conditional.get("body") is None
        and result["etag_stable"] is True
    ):
        raise SystemExit(1)


def wait_until_wall(target_ms: int) -> None:
    while True:
        remaining = target_ms - wall_ms()
        if remaining <= 0:
            return
        time.sleep(min(remaining / 1000.0, 0.025))


def completion_payload(prompt: str) -> dict[str, Any]:
    return {
        "model": "inferlab-tiny",
        "stream": True,
        "temperature": 0,
        "speculative_tokens": 2,
        "max_tokens": 8,
        "messages": [{"role": "user", "content": prompt}],
    }


def run_stream(gateway_url: str, prompt: str, timeout: float) -> dict[str, Any]:
    url = f"{gateway_url.rstrip('/')}/v1/chat/completions"
    outgoing = urllib.request.Request(
        url,
        data=json.dumps(completion_payload(prompt)).encode(),
        headers={"content-type": "application/json"},
        method="POST",
    )
    started_at_ms = wall_ms()
    started = monotonic_ms()
    opener = urllib.request.build_opener(
        urllib.request.ProxyHandler({}),
        NoRedirect(),
    )
    with opener.open(outgoing, timeout=timeout) as response:
        events: list[Any] = []
        for raw in response:
            line = raw.decode("utf-8", errors="strict").strip()
            if line.startswith("data: "):
                encoded = line[6:]
                events.append(encoded if encoded == "[DONE]" else json.loads(encoded))
        pieces: list[str] = []
        finish_reason = None
        for event in events:
            if not isinstance(event, dict) or not event.get("choices"):
                continue
            choice = event["choices"][0]
            content = choice.get("delta", {}).get("content")
            if isinstance(content, str):
                pieces.append(content)
            if choice.get("finish_reason") is not None:
                finish_reason = choice["finish_reason"]
        return {
            "status": response.status,
            "started_at_ms": started_at_ms,
            "completed_at_ms": wall_ms(),
            "duration_ms": round(monotonic_ms() - started, 3),
            "worker": response.headers.get("x-inferlab-worker"),
            "attempts": int(response.headers.get("x-inferlab-attempts", "0")),
            "config_revision": int(
                response.headers.get("x-inferlab-config-revision", "0")
            ),
            "pieces": pieces,
            "content": "".join(pieces),
            "finish_reason": finish_reason,
            "event_count": len(events),
            "done_received": bool(events) and events[-1] == "[DONE]",
        }


def command_cutoff(args: argparse.Namespace) -> None:
    if args.stream_start_before_ms <= args.pre_request_before_ms:
        raise SystemExit("stream must start before the pre-expiry protected request")
    if wall_ms() >= args.expires_at_ms - args.stream_start_before_ms:
        raise SystemExit("not enough signed lifetime remains for the cutoff schedule")

    stream_result: dict[str, Any] = {}
    stream_error: list[str] = []

    def stream_target() -> None:
        try:
            wait_until_wall(args.expires_at_ms - args.stream_start_before_ms)
            stream_result.update(run_stream(args.gateway_url, args.prompt, args.timeout))
        except BaseException as error:  # retained as a stable type, then fail below
            stream_error.append(type(error).__name__)

    stream_thread = threading.Thread(target=stream_target, name="expiry-sse", daemon=False)
    stream_thread.start()

    wait_until_wall(args.expires_at_ms - args.pre_request_before_ms)
    pre = request(
        f"{args.control_url.rstrip('/')}/v1/control/config",
        headers=authentication_headers(args.pre_authentication),
        timeout=args.timeout,
    )
    wait_until_wall(args.expires_at_ms + args.post_request_after_ms)
    post = request(
        f"{args.control_url.rstrip('/')}/v1/control/config",
        headers=authentication_headers(args.post_authentication),
        timeout=args.timeout,
    )
    missing = request(
        f"{args.control_url.rstrip('/')}/v1/control/config",
        timeout=args.timeout,
    )
    status = request(f"{args.control_url.rstrip('/')}/v1/control/status")
    stream_thread.join(timeout=args.timeout + 2)
    if stream_thread.is_alive():
        raise SystemExit("pre-expiry SSE did not finish within the proof timeout")

    result = {
        "schema": "inferlab.trust-expiry-cutoff.v0.27",
        "expires_at_ms": args.expires_at_ms,
        "schedule": {
            "stream_start_before_ms": args.stream_start_before_ms,
            "pre_request_before_ms": args.pre_request_before_ms,
            "post_request_after_ms": args.post_request_after_ms,
        },
        "pre_expiry_authentication": public_authentication(args.pre_authentication),
        "post_expiry_authentication": public_authentication(args.post_authentication),
        "pre_expiry_signed_request": pre,
        "post_expiry_signed_request": post,
        "post_expiry_missing_authentication_request": missing,
        "post_expiry_control_status": status,
        "pre_expiry_stream": stream_result,
        "stream_error": stream_error,
    }
    print(json.dumps(result, indent=2, sort_keys=True))
    auth = status.get("body", {}).get("service_authentication", {})
    expired_body = {
        "error": {
            "code": "unauthorized",
            "message": "signed service-trust policy is expired",
            "leader_id": None,
        }
    }
    if not (
        pre.get("status") == 200
        and pre.get("started_at_ms", args.expires_at_ms) < args.expires_at_ms
        and pre.get("completed_at_ms", args.expires_at_ms) < args.expires_at_ms
        and post.get("status") == 401
        and post.get("started_at_ms", 0) >= args.expires_at_ms
        and post.get("body") == expired_body
        and missing.get("status") == 401
        and missing.get("started_at_ms", 0) >= args.expires_at_ms
        and missing.get("body") == expired_body
        and stream_result.get("status") == 200
        and stream_result.get("done_received") is True
        and stream_result.get("worker") == "cpu-trust-expiry"
        and stream_result.get("attempts") == 1
        and stream_result.get("config_revision") == 2
        and stream_result.get("finish_reason") == "stop"
        and isinstance(stream_result.get("content"), str)
        and bool(stream_result.get("content"))
        and isinstance(stream_result.get("pieces"), list)
        and bool(stream_result.get("pieces"))
        and all(
            isinstance(piece, str) and bool(piece)
            for piece in stream_result.get("pieces", [])
        )
        and stream_result.get("content") == "".join(stream_result.get("pieces", []))
        and stream_result.get("event_count")
        == len(stream_result.get("pieces", [])) + 3
        and stream_result.get("started_at_ms", args.expires_at_ms) < args.expires_at_ms
        and stream_result.get("completed_at_ms", 0) >= args.expires_at_ms
        and not stream_error
        and status.get("status") == 200
        and auth.get("trust_policy_validity") == "expired"
        and auth.get("trust_policy_remaining_ms") == 0
        and auth.get("trust_policy_expiration_rejections", 0) >= 2
    ):
        raise SystemExit(1)


def command_sanitize(args: argparse.Namespace) -> None:
    evidence = args.evidence_dir.resolve()
    roots = {
        args.proof_root,
        os.path.normpath(args.proof_root),
        str(Path(args.proof_root).resolve()),
        args.project_root,
        os.path.normpath(args.project_root),
        str(Path(args.project_root).resolve()),
    }
    roots = {value for value in roots if value and value != os.path.sep}
    ordered_roots = sorted(roots, key=len, reverse=True)
    sensitive_keys = {
        "snapshot_path",
        "cache_path",
        "floor_path",
        "state_path",
        "data_directory",
        "ca_cert",
        "client_cert",
        "client_key",
        "tls_cert_path",
        "tls_key_path",
        "tls_client_ca_path",
        "log_path",
    }
    forbidden = ("-----BEGIN", "PRIVATE KEY", "CERTIFICATE-----")
    replacement_count = 0

    def redact(value: Any, key: str | None = None) -> Any:
        nonlocal replacement_count
        if isinstance(value, dict):
            return {item_key: redact(item, item_key) for item_key, item in value.items()}
        if isinstance(value, list):
            return [redact(item) for item in value]
        if not isinstance(value, str):
            return value
        if key in sensitive_keys:
            replacement_count += 1
            return "<redacted-sensitive-value>"
        sanitized = value
        for root in ordered_roots:
            if root in sanitized:
                replacement_count += sanitized.count(root)
                sanitized = sanitized.replace(root, "<proof-root>")
        if any(marker in sanitized for marker in forbidden):
            replacement_count += 1
            return "<redacted-certificate-material>"
        return sanitized

    files = sorted(path for path in evidence.iterdir() if path.suffix == ".json")
    for path in files:
        document = json.loads(path.read_text(encoding="utf-8"))
        temporary = path.with_name(f".{path.name}.sanitized.tmp")
        temporary.write_text(
            json.dumps(redact(document), indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        temporary.replace(path)

    remaining = []
    for path in files:
        retained = path.read_text(encoding="utf-8")
        if any(root in retained for root in ordered_roots) or any(
            marker in retained for marker in forbidden
        ):
            remaining.append(path.name)
    if remaining:
        raise SystemExit(f"sensitive proof strings remain in: {remaining}")
    print(
        json.dumps(
            {
                "schema": "inferlab.evidence-sanitization.v0.27",
                "files_sanitized": [path.name for path in files],
                "sensitive_keys_redacted": sorted(sensitive_keys),
                "replacement_count": replacement_count,
                "remaining_sensitive_strings": 0,
            },
            indent=2,
            sort_keys=True,
        )
    )


def add_tls(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--ca-cert", required=True)
    parser.add_argument("--client-cert", required=True)
    parser.add_argument("--client-key", required=True)


def build_parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser()
    commands = root.add_subparsers(dest="command", required=True)

    item = commands.add_parser("capture")
    item.add_argument("--url", required=True)
    item.add_argument("--method", choices=("GET", "POST", "PUT"), default="GET")
    item.add_argument("--body", type=Path)
    item.add_argument("--expect-status", type=int, required=True)
    item.add_argument("--timeout", type=float, default=1.0)
    item.add_argument("--ca-cert")
    item.add_argument("--client-cert")
    item.add_argument("--client-key")
    item.set_defaults(function=command_capture)

    item = commands.add_parser("expect-transport-failure")
    item.add_argument("--scenario", required=True)
    item.add_argument("--url", required=True)
    item.add_argument("--timeout", type=float, default=0.5)
    add_tls(item)
    item.set_defaults(function=command_expect_transport_failure)

    item = commands.add_parser("wait-controls")
    item.add_argument("--urls", required=True)
    item.add_argument("--generation", type=int, required=True)
    item.add_argument("--validity", choices=("valid", "expired", "legacy-unbounded"))
    item.add_argument("--expires-at-ms", type=int)
    item.add_argument("--bootstrap-source")
    item.add_argument("--last-fetch-outcome")
    item.add_argument("--min-expiration-rejections", type=int, default=0)
    item.add_argument("--timeout", type=float, default=10.0)
    item.set_defaults(function=command_wait_controls)

    item = commands.add_parser("wait-distributor")
    item.add_argument("--url", required=True)
    item.add_argument("--generation", type=int, required=True)
    item.add_argument("--policy-schema", required=True)
    item.add_argument("--expires-at-ms", type=int, required=True)
    item.add_argument("--acked-receivers", required=True)
    item.add_argument("--timeout", type=float, default=10.0)
    add_tls(item)
    item.set_defaults(function=command_wait_distributor)

    item = commands.add_parser("conditional-get")
    item.add_argument("--url", required=True)
    item.add_argument("--generation", type=int, required=True)
    item.add_argument("--expires-at-ms", type=int, required=True)
    add_tls(item)
    item.set_defaults(function=command_conditional_get)

    item = commands.add_parser("cutoff")
    item.add_argument("--gateway-url", required=True)
    item.add_argument("--control-url", required=True)
    item.add_argument("--expires-at-ms", type=int, required=True)
    item.add_argument("--pre-authentication", type=Path, required=True)
    item.add_argument("--post-authentication", type=Path, required=True)
    item.add_argument("--stream-start-before-ms", type=int, default=1500)
    item.add_argument("--pre-request-before-ms", type=int, default=400)
    item.add_argument("--post-request-after-ms", type=int, default=25)
    item.add_argument("--prompt", default="trust expiry admitted stream")
    item.add_argument("--timeout", type=float, default=15.0)
    item.set_defaults(function=command_cutoff)

    item = commands.add_parser("sanitize-evidence")
    item.add_argument("--evidence-dir", type=Path, required=True)
    item.add_argument("--proof-root", required=True)
    item.add_argument("--project-root", required=True)
    item.set_defaults(function=command_sanitize)
    return root


def main() -> None:
    args = build_parser().parse_args()
    args.function(args)


if __name__ == "__main__":
    main()
