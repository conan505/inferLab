#!/usr/bin/env python3
"""Observe the v0.24 mutual-TLS trust-distribution contract.

The probe uses only Python's standard library.  It deliberately keeps
certificate and private-key bytes out of its JSON output so retained evidence
can demonstrate transport behavior without retaining proof credentials.
"""

from __future__ import annotations

import argparse
import http.client
import json
import os
import socket
import ssl
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


def wall_ms() -> float:
    return time.time() * 1000.0


def monotonic_ms() -> float:
    return time.perf_counter() * 1000.0


def csv_set(raw: str) -> set[str]:
    return {item.strip() for item in raw.split(",") if item.strip()}


def tls_context(args: argparse.Namespace, *, require_client: bool = True) -> ssl.SSLContext:
    # Load only the proof-owned CA. Platform trust roots would make a
    # "wrong/private CA rejected" observation less precise.
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    context.verify_mode = ssl.CERT_REQUIRED
    context.check_hostname = True
    context.load_verify_locations(cafile=args.ca_cert)
    context.minimum_version = ssl.TLSVersion.TLSv1_3
    context.maximum_version = ssl.TLSVersion.TLSv1_3
    if require_client and args.client_cert and args.client_key:
        context.load_cert_chain(args.client_cert, args.client_key)
    return context


def public_error(error: BaseException) -> str:
    """Return stable error type/reason without paths, PEM, or key material."""

    reason = getattr(error, "reason", error)
    if isinstance(reason, BaseException):
        return type(reason).__name__
    return type(error).__name__


def request_json(
    url: str,
    context: ssl.SSLContext | None,
    *,
    method: str = "GET",
    data: bytes | None = None,
    timeout: float = 0.5,
) -> dict[str, Any]:
    started = monotonic_ms()
    request = urllib.request.Request(
        url,
        data=data,
        headers={"content-type": "application/json"} if data is not None else {},
        method=method,
    )
    handlers: list[Any] = [urllib.request.ProxyHandler({})]
    if context is not None:
        handlers.append(urllib.request.HTTPSHandler(context=context))
    opener = urllib.request.build_opener(*handlers)
    try:
        response = opener.open(request, timeout=timeout)
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
            "url_scheme": url.split(":", 1)[0],
            "status": None,
            "duration_ms": monotonic_ms() - started,
            "transport_error": public_error(error),
        }
    with response:
        raw = response.read()
        try:
            body: Any = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            body = raw.decode("utf-8", errors="replace")
        return {
            "url_scheme": url.split(":", 1)[0],
            "status": response.status,
            "duration_ms": monotonic_ms() - started,
            "etag": response.headers.get("etag"),
            "body": body,
        }


def wait_loop(timeout: float, sample: Any, matches: Any, description: str) -> tuple[int, Any]:
    deadline = time.monotonic() + timeout
    samples = 0
    latest: Any = None
    while True:
        samples += 1
        latest = sample()
        if matches(latest):
            return samples, latest
        if time.monotonic() >= deadline:
            raise SystemExit(
                json.dumps(
                    {"error": f"timed out waiting for {description}", "latest": latest},
                    indent=2,
                    sort_keys=True,
                )
            )
        time.sleep(0.025)


def handshake(args: argparse.Namespace) -> None:
    started = monotonic_ms()
    context = tls_context(args)
    with socket.create_connection((args.host, args.port), timeout=args.timeout) as raw:
        with context.wrap_socket(raw, server_hostname=args.server_hostname) as secured:
            peer = secured.getpeercert()
            cipher = secured.cipher()
            output = {
                "schema": "inferlab.mtls-handshake.v0.24",
                "duration_ms": monotonic_ms() - started,
                "tls_version": secured.version(),
                "cipher": cipher[0] if cipher else None,
                "server_hostname": args.server_hostname,
                "peer_subject_common_names": [
                    value
                    for group in peer.get("subject", ())
                    for key, value in group
                    if key == "commonName"
                ],
                "peer_subject_alt_names": [
                    value for kind, value in peer.get("subjectAltName", ()) if kind == "DNS"
                ],
                "client_certificate_presented": bool(args.client_cert and args.client_key),
            }
    print(json.dumps(output, indent=2, sort_keys=True))


def capture(args: argparse.Namespace) -> None:
    context = tls_context(args) if args.url.startswith("https://") else None
    observation = request_json(args.url, context, timeout=args.timeout)
    print(
        json.dumps(
            {"schema": "inferlab.mtls-http-capture.v0.24", "observation": observation},
            indent=2,
            sort_keys=True,
        )
    )
    if observation.get("status") != args.status:
        raise SystemExit(1)


def post(args: argparse.Namespace) -> None:
    context = tls_context(args)
    observation = request_json(
        args.url,
        context,
        method="POST",
        data=Path(args.body).read_bytes(),
        timeout=args.timeout,
    )
    print(
        json.dumps(
            {"schema": "inferlab.mtls-post-capture.v0.24", "observation": observation},
            indent=2,
            sort_keys=True,
        )
    )
    if observation.get("status") != args.status:
        raise SystemExit(1)


def expect_transport_failure(args: argparse.Namespace) -> None:
    context: ssl.SSLContext | None = None
    if args.url.startswith("https://"):
        context = tls_context(args, require_client=not args.omit_client_certificate)
    observation = request_json(args.url, context, timeout=args.timeout)
    failed_before_http = observation.get("status") is None
    print(
        json.dumps(
            {
                "schema": "inferlab.mtls-negative-transport.v0.24",
                "scenario": args.scenario,
                "failed_before_http_response": failed_before_http,
                "observation": observation,
            },
            indent=2,
            sort_keys=True,
        )
    )
    if not failed_before_http:
        raise SystemExit(1)


def distributor(args: argparse.Namespace) -> None:
    expected_acked = csv_set(args.acked_receivers)
    expected_pending = csv_set(args.pending_receivers)
    context = tls_context(args)
    status_url = f"{args.url.rstrip('/')}/v1/service-trust/status"
    started_at = wall_ms()
    started = monotonic_ms()

    def sample() -> dict[str, Any]:
        return request_json(status_url, context)

    def matches(observation: dict[str, Any]) -> bool:
        body = observation.get("body", {})
        snapshot = body.get("snapshot") or {}
        transport = body.get("transport_security") or {}
        return (
            observation.get("status") == 200
            and snapshot.get("generation") == args.generation
            and set(body.get("acked_receivers", [])) == expected_acked
            and set(body.get("pending_receivers", [])) == expected_pending
            and body.get("receipt_count") == len(expected_acked)
            and transport.get("mode") == "mutual-tls"
            and transport.get("client_certificate_required") is True
            and transport.get("minimum_protocol") == "TLSv1.3"
        )

    samples, observation = wait_loop(
        args.timeout,
        sample,
        matches,
        f"mTLS distributor generation {args.generation} receipts",
    )
    completed_at = wall_ms()
    completed = monotonic_ms()
    print(
        json.dumps(
            {
                "schema": "inferlab.mtls-trust-distributor-observation.v0.24",
                "started_at_ms": started_at,
                "completed_at_ms": completed_at,
                "duration_ms": completed - started,
                "samples": samples,
                "expected_generation": args.generation,
                "expected_acked_receivers": sorted(expected_acked),
                "expected_pending_receivers": sorted(expected_pending),
                "status": observation,
            },
            indent=2,
            sort_keys=True,
        )
    )


def controls(args: argparse.Namespace) -> None:
    urls = [url.strip().rstrip("/") for url in args.urls.split(",") if url.strip()]
    started_at = wall_ms()
    started = monotonic_ms()
    first_observed: dict[str, float] = {}
    first_observed_at: dict[str, float] = {}

    def sample() -> list[dict[str, Any]]:
        return [request_json(f"{url}/v1/control/status", None) for url in urls]

    def matches(observations: list[dict[str, Any]]) -> bool:
        converged = True
        for url, observation in zip(urls, observations):
            authentication = observation.get("body", {}).get("service_authentication", {})
            matched = (
                observation.get("status") == 200
                and authentication.get("trust_policy_generation") == args.generation
                and authentication.get("trust_policy_transport_mode") == "mutual-tls"
                and authentication.get("trust_policy_server_authentication") is True
                and authentication.get("trust_policy_client_authentication") is True
            )
            if args.bootstrap_source:
                matched = matched and authentication.get("trust_policy_bootstrap_source") == args.bootstrap_source
            if args.minimum_receipt_failures:
                matched = matched and authentication.get("trust_policy_receipt_failures", 0) >= args.minimum_receipt_failures
            if matched:
                first_observed.setdefault(url, monotonic_ms())
                first_observed_at.setdefault(url, wall_ms())
            else:
                converged = False
        return converged

    samples, statuses = wait_loop(
        args.timeout, sample, matches, f"{len(urls)} mTLS controls at generation {args.generation}"
    )
    completed_at = wall_ms()
    completed = monotonic_ms()
    print(
        json.dumps(
            {
                "schema": "inferlab.mtls-service-trust-controls.v0.24",
                "started_at_ms": started_at,
                "completed_at_ms": completed_at,
                "duration_ms": completed - started,
                "samples": samples,
                "expected_generation": args.generation,
                "expected_bootstrap_source": args.bootstrap_source,
                "minimum_receipt_failures": args.minimum_receipt_failures,
                "observations": [
                    {
                        "url": url,
                        "first_observed_at_ms": first_observed_at[url],
                        "convergence_latency_ms": first_observed[url] - started,
                    }
                    for url in urls
                ],
                "statuses": statuses,
            },
            indent=2,
            sort_keys=True,
        )
    )


def sanitize(args: argparse.Namespace) -> None:
    evidence = args.evidence_dir.resolve()
    proof_root = args.proof_root
    variants = {
        proof_root,
        os.path.normpath(proof_root),
        str(Path(proof_root).resolve()),
    }
    variants = {value for value in variants if value and value != os.path.sep}
    ordered_variants = sorted(variants, key=len, reverse=True)
    sensitive_keys = {
        "snapshot_path", "cache_path", "floor_path", "state_path", "data_directory",
        "ca_cert", "client_cert", "client_key", "tls_cert_path", "tls_key_path",
        "tls_client_ca_path",
    }
    forbidden_markers = ("-----BEGIN", "PRIVATE KEY", "CERTIFICATE-----")
    replacement_count = 0

    def redact(value: Any, key: str | None = None) -> Any:
        nonlocal replacement_count
        if isinstance(value, dict):
            return {item_key: redact(item_value, item_key) for item_key, item_value in value.items()}
        if isinstance(value, list):
            return [redact(item) for item in value]
        if not isinstance(value, str):
            return value
        if key in sensitive_keys:
            replacement_count += 1
            return "<redacted-sensitive-path>"
        sanitized = value
        for variant in ordered_variants:
            if variant in sanitized:
                replacement_count += sanitized.count(variant)
                sanitized = sanitized.replace(variant, "<proof-tmp>")
        if any(marker in sanitized for marker in forbidden_markers):
            replacement_count += 1
            return "<redacted-certificate-material>"
        return sanitized

    files = sorted(evidence.glob("*.json"))
    for path in files:
        with path.open(encoding="utf-8") as source:
            document = json.load(source)
        temporary = path.with_name(f".{path.name}.sanitized.tmp")
        temporary.write_text(json.dumps(redact(document), indent=2, sort_keys=True) + "\n", encoding="utf-8")
        temporary.replace(path)

    remaining: list[str] = []
    for path in files:
        text = path.read_text(encoding="utf-8")
        if any(variant in text for variant in ordered_variants) or any(marker in text for marker in forbidden_markers):
            remaining.append(path.name)
    if remaining:
        raise SystemExit(f"sensitive proof strings remain in: {remaining}")

    print(
        json.dumps(
            {
                "schema": "inferlab.evidence-sanitization.v0.24",
                "files_sanitized": [path.name for path in files],
                "sensitive_keys_redacted": sorted(sensitive_keys),
                "replacement_count": replacement_count,
                "remaining_proof_root_or_certificate_strings": 0,
            },
            indent=2,
            sort_keys=True,
        )
    )


def add_tls_arguments(parser: argparse.ArgumentParser, *, client_optional: bool = False) -> None:
    parser.add_argument("--ca-cert", required=True)
    parser.add_argument("--client-cert", required=not client_optional)
    parser.add_argument("--client-key", required=not client_optional)


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser()
    commands = root.add_subparsers(dest="command", required=True)

    item = commands.add_parser("handshake")
    item.add_argument("--host", default="127.0.0.1")
    item.add_argument("--port", type=int, required=True)
    item.add_argument("--server-hostname", required=True)
    item.add_argument("--timeout", type=float, default=1.0)
    add_tls_arguments(item)
    item.set_defaults(function=handshake)

    item = commands.add_parser("capture")
    item.add_argument("--url", required=True)
    item.add_argument("--status", type=int, default=200)
    item.add_argument("--timeout", type=float, default=0.5)
    add_tls_arguments(item, client_optional=True)
    item.set_defaults(function=capture)

    item = commands.add_parser("post")
    item.add_argument("--url", required=True)
    item.add_argument("--body", required=True)
    item.add_argument("--status", type=int, required=True)
    item.add_argument("--timeout", type=float, default=1.0)
    add_tls_arguments(item)
    item.set_defaults(function=post)

    item = commands.add_parser("expect-transport-failure")
    item.add_argument("--scenario", required=True)
    item.add_argument("--url", required=True)
    item.add_argument("--timeout", type=float, default=0.5)
    item.add_argument("--omit-client-certificate", action="store_true")
    add_tls_arguments(item, client_optional=True)
    item.set_defaults(function=expect_transport_failure)

    item = commands.add_parser("wait-distributor")
    item.add_argument("--url", required=True)
    item.add_argument("--generation", type=int, required=True)
    item.add_argument("--acked-receivers", default="")
    item.add_argument("--pending-receivers", default="")
    item.add_argument("--timeout", type=float, default=5.0)
    add_tls_arguments(item)
    item.set_defaults(function=distributor)

    item = commands.add_parser("wait-controls")
    item.add_argument("--urls", required=True)
    item.add_argument("--generation", type=int, required=True)
    item.add_argument("--bootstrap-source")
    item.add_argument("--minimum-receipt-failures", type=int, default=0)
    item.add_argument("--timeout", type=float, default=5.0)
    item.set_defaults(function=controls)

    item = commands.add_parser("sanitize-evidence")
    item.add_argument("--evidence-dir", type=Path, required=True)
    item.add_argument("--proof-root", required=True)
    item.set_defaults(function=sanitize)
    return root


def main() -> None:
    args = parser().parse_args()
    args.function(args)


if __name__ == "__main__":
    main()
