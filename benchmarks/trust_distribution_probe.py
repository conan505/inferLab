#!/usr/bin/env python3
"""Observe v0.23 trust publication, receipts, and receiver convergence.

The probe intentionally uses only the public loopback HTTP contracts. It emits
one deterministic JSON document that the milestone checker can evaluate later.
"""

from __future__ import annotations

import argparse
import http.server
import json
import os
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any


def wall_ms() -> float:
    return time.time() * 1000.0


def csv_set(raw: str) -> set[str]:
    return {item.strip() for item in raw.split(",") if item.strip()}


def fetch_json(url: str, timeout: float = 0.25) -> dict[str, Any]:
    started = wall_ms()
    request = urllib.request.Request(url)
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            raw = response.read()
            return {
                "url": url,
                "status": response.status,
                "duration_ms": wall_ms() - started,
                "body": json.loads(raw.decode("utf-8")),
            }
    except urllib.error.HTTPError as error:
        raw = error.read()
        try:
            body: Any = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            body = raw.decode("utf-8", errors="replace")
        return {
            "url": url,
            "status": error.code,
            "duration_ms": wall_ms() - started,
            "body": body,
        }
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as error:
        return {
            "url": url,
            "status": None,
            "duration_ms": wall_ms() - started,
            "error": str(error),
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


def distributor(args: argparse.Namespace) -> None:
    expected_acked = csv_set(args.acked_receivers)
    expected_pending = csv_set(args.pending_receivers)
    status_url = f"{args.url.rstrip('/')}/v1/service-trust/status"
    started = wall_ms()

    def sample() -> dict[str, Any]:
        return fetch_json(status_url)

    def matches(observation: dict[str, Any]) -> bool:
        body = observation.get("body", {})
        snapshot = body.get("snapshot") or {}
        return (
            observation.get("status") == 200
            and snapshot.get("generation") == args.generation
            and set(body.get("acked_receivers", [])) == expected_acked
            and set(body.get("pending_receivers", [])) == expected_pending
            and body.get("receipt_count") == len(expected_acked)
        )

    samples, observation = wait_loop(
        args.timeout,
        sample,
        matches,
        (
            f"distributor generation {args.generation}, acked "
            f"{sorted(expected_acked)}, pending {sorted(expected_pending)}"
        ),
    )
    completed = wall_ms()
    print(
        json.dumps(
            {
                "schema": "inferlab.trust-distributor-observation.v0.23",
                "started_at_ms": started,
                "completed_at_ms": completed,
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
    if not urls:
        raise SystemExit("at least one control URL is required")
    started = wall_ms()
    first_observed: dict[str, float] = {}

    def sample() -> list[dict[str, Any]]:
        return [fetch_json(f"{url}/v1/control/status") for url in urls]

    def matches(observations: list[dict[str, Any]]) -> bool:
        converged = True
        for url, observation in zip(urls, observations):
            authentication = observation.get("body", {}).get(
                "service_authentication", {}
            )
            matched = (
                observation.get("status") == 200
                and authentication.get("trust_policy_generation") == args.generation
                and authentication.get("trust_policy_rejections", 0)
                >= args.minimum_rejections
                and authentication.get("trust_policy_receipt_failures", 0)
                >= args.minimum_receipt_failures
            )
            if args.bootstrap_source:
                matched = matched and (
                    authentication.get("trust_policy_bootstrap_source")
                    == args.bootstrap_source
                )
            if matched:
                first_observed.setdefault(url, wall_ms())
            else:
                converged = False
        return converged

    samples, statuses = wait_loop(
        args.timeout,
        sample,
        matches,
        f"{len(urls)} controls at trust generation {args.generation}",
    )
    completed = wall_ms()
    print(
        json.dumps(
            {
                "schema": "inferlab.distributed-service-trust-controls.v0.23",
                "started_at_ms": started,
                "completed_at_ms": completed,
                "duration_ms": completed - started,
                "samples": samples,
                "expected_generation": args.generation,
                "minimum_rejections": args.minimum_rejections,
                "minimum_receipt_failures": args.minimum_receipt_failures,
                "expected_bootstrap_source": args.bootstrap_source,
                "observations": [
                    {
                        "url": url,
                        "first_observed_at_ms": first_observed[url],
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


def capture(args: argparse.Namespace) -> None:
    observation = fetch_json(args.url, timeout=args.timeout)
    print(
        json.dumps(
            {
                "schema": "inferlab.trust-distribution-http-capture.v0.23",
                "observation": observation,
            },
            indent=2,
            sort_keys=True,
        )
    )
    if observation.get("status") != args.status:
        raise SystemExit(1)


def proxy(args: argparse.Namespace) -> None:
    """Run a tiny loopback-only HTTP relay used to create an exact partition.

    The proof owns this child process and kills only its PID. This is not a
    production reverse proxy; it exists so node C can lose distributor
    transport while nodes A and B remain connected to the same authority.
    """

    target = args.target.rstrip("/")
    parsed = urllib.parse.urlsplit(f"http://{args.bind}")
    if parsed.hostname not in {"127.0.0.1", "localhost"}:
        raise SystemExit("proof relay must bind to loopback")
    if parsed.port is None:
        raise SystemExit("proof relay bind must include a port")

    class Relay(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
            self.relay()

        def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
            self.relay()

        def relay(self) -> None:
            length = int(self.headers.get("content-length", "0"))
            if length > args.max_body_bytes:
                self.send_error(413)
                return
            body = self.rfile.read(length) if length else None
            headers = {
                name: value
                for name, value in self.headers.items()
                if name.lower()
                in {"content-type", "if-none-match", "accept", "user-agent"}
            }
            request = urllib.request.Request(
                f"{target}{self.path}",
                data=body,
                headers=headers,
                method=self.command,
            )
            try:
                response = urllib.request.urlopen(request, timeout=args.timeout)
            except urllib.error.HTTPError as error:
                response = error
            except (urllib.error.URLError, TimeoutError):
                self.send_error(502)
                return
            with response:
                response_body = response.read(args.max_body_bytes + 1)
                if len(response_body) > args.max_body_bytes:
                    self.send_error(502)
                    return
                self.send_response(response.status)
                for name, value in response.headers.items():
                    if name.lower() in {
                        "content-type",
                        "etag",
                        "cache-control",
                    }:
                        self.send_header(name, value)
                self.send_header("content-length", str(len(response_body)))
                self.end_headers()
                if response_body:
                    self.wfile.write(response_body)

        def log_message(self, format: str, *values: Any) -> None:
            if args.verbose:
                super().log_message(format, *values)

    server = http.server.ThreadingHTTPServer(
        (parsed.hostname or "127.0.0.1", parsed.port), Relay
    )
    server.serve_forever(poll_interval=0.05)


def sanitize(args: argparse.Namespace) -> None:
    """Remove disposable host paths before evidence becomes an assertion input."""

    evidence = args.evidence_dir.resolve()
    proof_root = args.proof_root
    variants = {
        proof_root,
        os.path.normpath(proof_root),
        str(Path(proof_root).resolve()),
    }
    variants = {value for value in variants if value and value != os.path.sep}
    ordered_variants = sorted(variants, key=len, reverse=True)
    path_keys = {
        "snapshot_path",
        "cache_path",
        "floor_path",
        "state_path",
        "data_directory",
    }
    replacement_count = 0

    def redact(value: Any, key: str | None = None) -> Any:
        nonlocal replacement_count
        if isinstance(value, dict):
            return {
                item_key: redact(item_value, item_key)
                for item_key, item_value in value.items()
            }
        if isinstance(value, list):
            return [redact(item) for item in value]
        if not isinstance(value, str):
            return value
        if key in path_keys and os.path.isabs(value):
            replacement_count += 1
            return "<redacted-proof-path>"
        sanitized = value
        for variant in ordered_variants:
            if variant in sanitized:
                occurrences = sanitized.count(variant)
                replacement_count += occurrences
                sanitized = sanitized.replace(variant, "<proof-tmp>")
        return sanitized

    files = sorted(evidence.glob("*.json"))
    for path in files:
        with path.open(encoding="utf-8") as source:
            document = json.load(source)
        sanitized = redact(document)
        temporary = path.with_name(f".{path.name}.sanitized.tmp")
        temporary.write_text(
            json.dumps(sanitized, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        temporary.replace(path)

    remaining: list[str] = []

    def find_remaining(value: Any) -> None:
        if isinstance(value, dict):
            for item in value.values():
                find_remaining(item)
        elif isinstance(value, list):
            for item in value:
                find_remaining(item)
        elif isinstance(value, str) and any(
            variant in value for variant in ordered_variants
        ):
            remaining.append(value)

    for path in files:
        with path.open(encoding="utf-8") as source:
            find_remaining(json.load(source))
    if remaining:
        raise SystemExit("proof-root strings remain after evidence sanitization")

    print(
        json.dumps(
            {
                "schema": "inferlab.evidence-sanitization.v0.23",
                "files_sanitized": [path.name for path in files],
                "path_keys_redacted": sorted(path_keys),
                "replacement_count": replacement_count,
                "remaining_proof_root_strings": 0,
            },
            indent=2,
            sort_keys=True,
        )
    )


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser()
    commands = root.add_subparsers(dest="command", required=True)

    distributor_parser = commands.add_parser("wait-distributor")
    distributor_parser.add_argument("--url", required=True)
    distributor_parser.add_argument("--generation", type=int, required=True)
    distributor_parser.add_argument("--acked-receivers", default="")
    distributor_parser.add_argument("--pending-receivers", default="")
    distributor_parser.add_argument("--timeout", type=float, default=5.0)
    distributor_parser.set_defaults(function=distributor)

    controls_parser = commands.add_parser("wait-controls")
    controls_parser.add_argument("--urls", required=True)
    controls_parser.add_argument("--generation", type=int, required=True)
    controls_parser.add_argument("--minimum-rejections", type=int, default=0)
    controls_parser.add_argument("--minimum-receipt-failures", type=int, default=0)
    controls_parser.add_argument("--bootstrap-source")
    controls_parser.add_argument("--timeout", type=float, default=5.0)
    controls_parser.set_defaults(function=controls)

    capture_parser = commands.add_parser("capture")
    capture_parser.add_argument("--url", required=True)
    capture_parser.add_argument("--status", type=int, default=200)
    capture_parser.add_argument("--timeout", type=float, default=0.5)
    capture_parser.set_defaults(function=capture)

    proxy_parser = commands.add_parser("proxy")
    proxy_parser.add_argument("--bind", required=True)
    proxy_parser.add_argument("--target", required=True)
    proxy_parser.add_argument("--timeout", type=float, default=0.5)
    proxy_parser.add_argument("--max-body-bytes", type=int, default=1024 * 1024)
    proxy_parser.add_argument("--verbose", action="store_true")
    proxy_parser.set_defaults(function=proxy)

    sanitize_parser = commands.add_parser("sanitize-evidence")
    sanitize_parser.add_argument("--evidence-dir", type=Path, required=True)
    sanitize_parser.add_argument("--proof-root", required=True)
    sanitize_parser.set_defaults(function=sanitize)
    return root


def main() -> None:
    args = parser().parse_args()
    args.function(args)


if __name__ == "__main__":
    main()
