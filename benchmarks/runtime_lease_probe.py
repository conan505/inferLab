#!/usr/bin/env python3
"""Observe InferLab's runtime routing-lease boundary as JSON evidence."""

from __future__ import annotations

import argparse
import json
import time
import urllib.error
import urllib.request
from typing import Any


def now_ms() -> float:
    return round(time.time() * 1000, 3)


def request_json(
    url: str,
    *,
    method: str = "GET",
    payload: dict[str, Any] | None = None,
    timeout: float = 10,
) -> dict[str, Any]:
    data = None if payload is None else json.dumps(payload).encode()
    request = urllib.request.Request(
        url,
        data=data,
        headers={"content-type": "application/json"} if data else {},
        method=method,
    )
    started_at_ms = now_ms()
    started = time.monotonic()
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            body = json.loads(response.read())
            return response_summary(
                response.status,
                response.headers,
                body,
                started_at_ms,
                started,
            )
    except urllib.error.HTTPError as error:
        body = json.loads(error.read())
        return response_summary(
            error.code,
            error.headers,
            body,
            started_at_ms,
            started,
        )


def response_summary(
    status: int,
    headers: Any,
    body: Any,
    started_at_ms: float,
    started: float,
) -> dict[str, Any]:
    return {
        "status": status,
        "started_at_ms": started_at_ms,
        "observed_at_ms": now_ms(),
        "duration_ms": round((time.monotonic() - started) * 1000, 3),
        "attempts": optional_int(headers.get("x-inferlab-attempts")),
        "worker": headers.get("x-inferlab-worker"),
        "config_revision": optional_int(
            headers.get("x-inferlab-config-revision")
        ),
        "config_term": optional_int(headers.get("x-inferlab-config-term")),
        "retry_after": optional_int(headers.get("retry-after")),
        "body": body,
    }


def optional_int(value: str | None) -> int | None:
    return None if value is None else int(value)


def wait_state(args: argparse.Namespace) -> dict[str, Any]:
    started_at_ms = now_ms()
    started = time.monotonic()
    samples = 0
    last: dict[str, Any] | None = None
    while time.monotonic() - started < args.timeout:
        last = request_json(f"{args.gateway_url}/internal/workers", timeout=1)
        samples += 1
        body = last.get("body", {})
        lease = body.get("routing_lease") or {}
        routing = body.get("routing_snapshot") or {}
        if (
            last["status"] == 200
            and lease.get("state") == args.state
            and (
                args.revision is None
                or routing.get("control_revision") == args.revision
            )
            and (
                args.minimum_renewals is None
                or lease.get("renewals", 0) >= args.minimum_renewals
            )
        ):
            return {
                "schema": "inferlab.runtime-routing-lease-state.v0.16",
                "started_at_ms": started_at_ms,
                "observed_at_ms": now_ms(),
                "wait_duration_ms": round(
                    (time.monotonic() - started) * 1000, 3
                ),
                "samples": samples,
                "expected_state": args.state,
                "expected_revision": args.revision,
                "minimum_renewals": args.minimum_renewals,
                "status": last,
            }
        time.sleep(0.025)
    raise SystemExit(
        f"gateway did not reach routing lease state {args.state}; last={last}"
    )


def readiness(args: argparse.Namespace) -> dict[str, Any]:
    return {
        "schema": "inferlab.runtime-routing-lease-readiness.v0.16",
        **request_json(f"{args.gateway_url}/readyz"),
    }


def completion(args: argparse.Namespace) -> dict[str, Any]:
    result = request_json(
        f"{args.gateway_url}/v1/chat/completions",
        method="POST",
        payload={
            "model": "inferlab-tiny",
            "stream": False,
            "temperature": 0,
            "speculative_tokens": args.speculative_tokens,
            "max_tokens": 8,
            "messages": [{"role": "user", "content": args.prompt}],
        },
    )
    body = result.get("body")
    if result.get("status") == 200 and isinstance(body, dict):
        result["content"] = body["choices"][0]["message"]["content"]
        result["finish_reason"] = body["choices"][0]["finish_reason"]
        result["generation"] = body["inferlab"]["generation"]
        del result["body"]
    return {
        "schema": "inferlab.runtime-routing-lease-request.v0.16",
        **result,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    state = subparsers.add_parser("wait-state")
    state.add_argument("--gateway-url", required=True)
    state.add_argument("--state", required=True)
    state.add_argument("--revision", type=int)
    state.add_argument("--minimum-renewals", type=int)
    state.add_argument("--timeout", type=float, default=5)

    ready = subparsers.add_parser("readiness")
    ready.add_argument("--gateway-url", required=True)

    request = subparsers.add_parser("request")
    request.add_argument("--gateway-url", required=True)
    request.add_argument("--prompt", required=True)
    request.add_argument("--speculative-tokens", type=int, default=3)

    args = parser.parse_args()
    if args.command == "wait-state":
        result = wait_state(args)
    elif args.command == "readiness":
        result = readiness(args)
    else:
        result = completion(args)
    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
