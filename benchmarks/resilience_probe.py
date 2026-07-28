#!/usr/bin/env python3
"""Sequential HTTP probe for retry-budget and deadline experiments."""

import argparse
import json
import math
import time
import urllib.error
import urllib.request
from collections import Counter
from datetime import datetime, timezone


def percentile(values: list[float], quantile: float):
    ordered = sorted(values)
    if not ordered:
        return None
    return round(ordered[max(0, math.ceil(len(ordered) * quantile) - 1)], 3)


def fetch_json(url: str) -> dict:
    with urllib.request.urlopen(url, timeout=5) as response:
        return json.load(response)


def one_request(url: str, number: int, timeout: float) -> dict:
    payload = json.dumps(
        {
            "model": "inferlab-fake",
            "stream": False,
            "messages": [
                {"role": "user", "content": f"resilience request {number}"}
            ],
        }
    ).encode("utf-8")
    request = urllib.request.Request(
        url,
        data=payload,
        headers={"content-type": "application/json"},
        method="POST",
    )
    started = time.perf_counter()
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            body = json.load(response)
            return {
                "request_number": number,
                "status": response.status,
                "worker": response.headers.get("x-inferlab-worker"),
                "attempts": numeric_header(response, "x-inferlab-attempts"),
                "error_type": None,
                "e2e_ms": round((time.perf_counter() - started) * 1000, 3),
                "response_id": body.get("id"),
            }
    except urllib.error.HTTPError as error:
        body = error.read()
        try:
            error_type = json.loads(body)["error"]["type"]
        except (json.JSONDecodeError, KeyError, TypeError):
            error_type = None
        return {
            "request_number": number,
            "status": error.code,
            "worker": error.headers.get("x-inferlab-worker"),
            "attempts": numeric_header(error, "x-inferlab-attempts"),
            "error_type": error_type,
            "e2e_ms": round((time.perf_counter() - started) * 1000, 3),
        }
    except Exception as error:
        return {
            "request_number": number,
            "status": None,
            "error": repr(error),
            "e2e_ms": round((time.perf_counter() - started) * 1000, 3),
        }


def numeric_header(response, name: str):
    value = response.headers.get(name)
    return int(value) if value is not None else None


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", required=True)
    parser.add_argument("--status-url", required=True)
    parser.add_argument("--worker-health", action="append", default=[])
    parser.add_argument("--requests", type=int, required=True)
    parser.add_argument("--timeout", type=float, default=5.0)
    parser.add_argument("--label", required=True)
    args = parser.parse_args()
    if args.requests < 1 or args.timeout <= 0:
        parser.error("--requests and --timeout must be positive")

    samples = [
        one_request(args.url, number, args.timeout)
        for number in range(1, args.requests + 1)
    ]
    status_counts = Counter(sample["status"] for sample in samples)
    latencies = [sample["e2e_ms"] for sample in samples]
    report = {
        "schema": "inferlab.resilience-probe.v0.0.7",
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "label": args.label,
        "config": {
            "url": args.url,
            "requests": args.requests,
            "timeout_seconds": args.timeout,
        },
        "summary": {
            "status_counts": {
                str(status): count
                for status, count in sorted(
                    status_counts.items(), key=lambda item: str(item[0])
                )
            },
            "e2e_ms": {
                "p50": percentile(latencies, 0.50),
                "p95": percentile(latencies, 0.95),
                "p99": percentile(latencies, 0.99),
                "max": round(max(latencies), 3),
            },
            "retried_successes": sum(
                sample["status"] == 200 and (sample.get("attempts") or 0) > 1
                for sample in samples
            ),
            "transport_errors": sum(
                sample["status"] is None for sample in samples
            ),
        },
        "gateway_status_after": fetch_json(args.status_url),
        "worker_health_after": [
            fetch_json(url) for url in args.worker_health
        ],
        "samples": samples,
    }
    print(json.dumps(report, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
