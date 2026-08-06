#!/usr/bin/env python3
"""Send one service-authenticated control request and retain its exact outcome."""

from __future__ import annotations

import argparse
import json
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


def load_json(path: Path | None) -> Any:
    if path is None:
        return None
    with path.open(encoding="utf-8") as source:
        return json.load(source)


def decode_response(response: Any) -> Any:
    raw = response.read().decode("utf-8")
    if not raw:
        return None
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        return raw


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", required=True)
    parser.add_argument("--method", choices=("GET", "POST"), required=True)
    parser.add_argument("--authentication", type=Path)
    parser.add_argument("--body", type=Path)
    args = parser.parse_args()

    authentication = load_json(args.authentication)
    body = load_json(args.body)
    encoded_body = None
    if body is not None:
        encoded_body = json.dumps(body, separators=(",", ":"), sort_keys=True).encode("utf-8")

    headers = {"Accept": "application/json"}
    if encoded_body is not None:
        headers["Content-Type"] = "application/json"
    if authentication is not None:
        for field, header in AUTHENTICATION_HEADERS.items():
            headers[header] = str(authentication[field])

    request = urllib.request.Request(
        args.url,
        data=encoded_body,
        headers=headers,
        method=args.method,
    )
    started_ns = time.perf_counter_ns()
    try:
        with urllib.request.urlopen(request, timeout=2) as response:
            status = response.status
            response_body = decode_response(response)
    except urllib.error.HTTPError as error:
        status = error.code
        response_body = decode_response(error)
    except urllib.error.URLError as error:
        status = None
        response_body = {"transport_error": str(error.reason)}
    duration_ms = (time.perf_counter_ns() - started_ns) / 1_000_000

    result = {
        "schema": "inferlab.service-request-probe.v0.20",
        "request": {
            "method": args.method,
            "url": args.url,
            "body": body,
            "authentication": authentication,
        },
        "response": {
            "status": status,
            "body": response_body,
        },
        "duration_ms": round(duration_ms, 3),
    }
    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
