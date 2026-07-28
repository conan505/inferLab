#!/usr/bin/env python3
"""Dependency-free InferLab load probe.

This is evidence tooling, not a production load generator. It measures the client-observed
time to the first non-empty SSE line and total response time, then prints machine-readable JSON.
"""

import argparse
import concurrent.futures
import json
import math
import time
import urllib.error
import urllib.request
from collections import Counter
from datetime import datetime, timezone


def one_request(url: str, timeout: float, request_number: int) -> dict:
    payload = json.dumps(
        {
            "model": "inferlab-fake",
            "stream": True,
            "messages": [
                {"role": "user", "content": f"benchmark request {request_number}"}
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
    first_event_at = None
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            worker = response.headers.get("x-inferlab-worker", "unknown")
            for raw_line in response:
                if first_event_at is None and raw_line.strip():
                    first_event_at = time.perf_counter()
            finished = time.perf_counter()
            return {
                "ok": True,
                "status": response.status,
                "worker": worker,
                "ttft_ms": (first_event_at - started) * 1000
                if first_event_at
                else None,
                "e2e_ms": (finished - started) * 1000,
            }
    except urllib.error.HTTPError as error:
        return failure(started, f"HTTP {error.code}")
    except Exception as error:  # benchmark output should retain all failures
        return failure(started, repr(error))


def failure(started: float, message: str) -> dict:
    return {
        "ok": False,
        "error": message,
        "e2e_ms": (time.perf_counter() - started) * 1000,
    }


def percentile(values: list, quantile: float):
    if not values:
        return None
    ordered = sorted(values)
    index = max(0, math.ceil(quantile * len(ordered)) - 1)
    return round(ordered[index], 3)


def distribution(values: list) -> dict:
    return {
        "p50": percentile(values, 0.50),
        "p90": percentile(values, 0.90),
        "p95": percentile(values, 0.95),
        "p99": percentile(values, 0.99),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--url",
        default="http://127.0.0.1:8080/v1/chat/completions",
    )
    parser.add_argument("--requests", type=int, default=30)
    parser.add_argument("--concurrency", type=int, default=5)
    parser.add_argument("--timeout", type=float, default=10.0)
    parser.add_argument("--label", default="unlabelled")
    args = parser.parse_args()

    if args.requests < 1 or args.concurrency < 1:
        parser.error("--requests and --concurrency must be positive")

    wall_started = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(
        max_workers=args.concurrency
    ) as executor:
        results = list(
            executor.map(
                lambda number: one_request(args.url, args.timeout, number),
                range(args.requests),
            )
        )
    wall_seconds = time.perf_counter() - wall_started

    successes = [result for result in results if result["ok"]]
    failures = [result for result in results if not result["ok"]]
    report = {
        "schema": "inferlab.benchmark.v0.0.4",
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "config": {
            "label": args.label,
            "url": args.url,
            "requests": args.requests,
            "concurrency": args.concurrency,
            "timeout_seconds": args.timeout,
        },
        "summary": {
            "successful": len(successes),
            "failed": len(failures),
            "wall_seconds": round(wall_seconds, 6),
            "requests_per_second": round(len(successes) / wall_seconds, 3),
            "worker_counts": dict(
                sorted(Counter(item["worker"] for item in successes).items())
            ),
            "ttft_ms": distribution(
                [item["ttft_ms"] for item in successes if item["ttft_ms"] is not None]
            ),
            "e2e_ms": distribution([item["e2e_ms"] for item in successes]),
            "worker_e2e_ms": {
                worker: distribution(
                    [
                        item["e2e_ms"]
                        for item in successes
                        if item["worker"] == worker
                    ]
                )
                for worker in sorted({item["worker"] for item in successes})
            },
        },
        "failures": failures,
        "samples": results,
    }
    print(json.dumps(report, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
