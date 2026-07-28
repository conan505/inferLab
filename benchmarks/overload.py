#!/usr/bin/env python3
"""Open-loop overload probe for InferLab admission control."""

import argparse
import concurrent.futures
import json
import math
import subprocess
import threading
import time
import urllib.error
import urllib.request
from collections import Counter
from datetime import datetime, timezone


def percentile(values: list[float], quantile: float):
    if not values:
        return None
    ordered = sorted(values)
    index = max(0, math.ceil(quantile * len(ordered)) - 1)
    return round(ordered[index], 3)


def distribution(values: list[float]) -> dict:
    return {
        "p50": percentile(values, 0.50),
        "p90": percentile(values, 0.90),
        "p95": percentile(values, 0.95),
        "p99": percentile(values, 0.99),
        "max": round(max(values), 3) if values else None,
    }


def one_request(
    url: str,
    timeout: float,
    request_number: int,
    benchmark_started: float,
    scheduled_at: float,
) -> dict:
    payload = json.dumps(
        {
            "model": "inferlab-fake",
            "stream": True,
            "messages": [
                {"role": "user", "content": f"overload request {request_number}"}
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
    common = {
        "request_number": request_number,
        "scheduled_ms": round((scheduled_at - benchmark_started) * 1000, 3),
        "dispatch_lag_ms": round(max(0.0, started - scheduled_at) * 1000, 3),
    }
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            response.read()
            return {
                **common,
                "status": response.status,
                "worker": response.headers.get("x-inferlab-worker"),
                "retry_after": response.headers.get("retry-after"),
                "e2e_ms": round((time.perf_counter() - started) * 1000, 3),
            }
    except urllib.error.HTTPError as error:
        body = error.read()
        try:
            error_type = json.loads(body)["error"]["type"]
        except (json.JSONDecodeError, KeyError, TypeError):
            error_type = None
        return {
            **common,
            "status": error.code,
            "worker": error.headers.get("x-inferlab-worker"),
            "retry_after": error.headers.get("retry-after"),
            "error_type": error_type,
            "e2e_ms": round((time.perf_counter() - started) * 1000, 3),
        }
    except Exception as error:  # retain transport failures as evidence
        return {
            **common,
            "status": None,
            "error": repr(error),
            "e2e_ms": round((time.perf_counter() - started) * 1000, 3),
        }


def process_rss_kib(pid: int):
    result = subprocess.run(
        ["ps", "-o", "rss=", "-p", str(pid)],
        check=False,
        capture_output=True,
        text=True,
    )
    value = result.stdout.strip()
    return int(value) if value else None


def sample_resources(
    pid: int,
    status_url: str,
    benchmark_started: float,
    interval: float,
    stop: threading.Event,
    samples: list[dict],
) -> None:
    while not stop.is_set():
        rss_kib = process_rss_kib(pid)
        try:
            admission = fetch_json(status_url)["admission"]
        except (OSError, KeyError, ValueError):
            admission = None
        if rss_kib is not None and admission is not None:
            samples.append(
                {
                    "elapsed_ms": round(
                        (time.perf_counter() - benchmark_started) * 1000, 3
                    ),
                    "rss_kib": rss_kib,
                    "executing": admission["executing"],
                    "queued": admission["queued"],
                    "outstanding": admission["outstanding"],
                }
            )
        stop.wait(interval)


def fetch_json(url: str) -> dict:
    with urllib.request.urlopen(url, timeout=5) as response:
        return json.load(response)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--url",
        default="http://127.0.0.1:8080/v1/chat/completions",
    )
    parser.add_argument(
        "--status-url",
        default="http://127.0.0.1:8080/internal/workers",
    )
    parser.add_argument("--requests", type=int, default=160)
    parser.add_argument("--offered-rate-rps", type=float, default=40.0)
    parser.add_argument("--estimated-capacity-rps", type=float, default=8.0)
    parser.add_argument("--timeout", type=float, default=5.0)
    parser.add_argument("--gateway-pid", type=int, required=True)
    parser.add_argument("--rss-interval", type=float, default=0.05)
    args = parser.parse_args()

    if (
        args.requests < 1
        or args.offered_rate_rps <= 0
        or args.estimated_capacity_rps <= 0
        or args.gateway_pid <= 0
    ):
        parser.error("counts, rates, and --gateway-pid must be positive")

    benchmark_started = time.perf_counter()
    resource_samples: list[dict] = []
    stop_sampling = threading.Event()
    sampler = threading.Thread(
        target=sample_resources,
        args=(
            args.gateway_pid,
            args.status_url,
            benchmark_started,
            args.rss_interval,
            stop_sampling,
            resource_samples,
        ),
        daemon=True,
    )
    sampler.start()

    futures = []
    with concurrent.futures.ThreadPoolExecutor(
        max_workers=min(args.requests, 256)
    ) as executor:
        for number in range(args.requests):
            scheduled_at = benchmark_started + number / args.offered_rate_rps
            delay = scheduled_at - time.perf_counter()
            if delay > 0:
                time.sleep(delay)
            futures.append(
                executor.submit(
                    one_request,
                    args.url,
                    args.timeout,
                    number,
                    benchmark_started,
                    scheduled_at,
                )
            )
        results = [future.result() for future in futures]

    stop_sampling.set()
    sampler.join(timeout=1)
    status = fetch_json(args.status_url)
    final_rss_kib = process_rss_kib(args.gateway_pid)
    if final_rss_kib is not None:
        resource_samples.append(
            {
                "elapsed_ms": round(
                    (time.perf_counter() - benchmark_started) * 1000, 3
                ),
                "rss_kib": final_rss_kib,
                "executing": status["admission"]["executing"],
                "queued": status["admission"]["queued"],
                "outstanding": status["admission"]["outstanding"],
            }
        )
    wall_seconds = time.perf_counter() - benchmark_started
    status_counts = Counter(item["status"] for item in results)
    accepted = [item for item in results if item["status"] == 200]
    rejected = [item for item in results if item["status"] == 429]
    transport_errors = [item for item in results if item["status"] is None]
    rss_values = [sample["rss_kib"] for sample in resource_samples]
    rss_summary = {
        "first_kib": rss_values[0] if rss_values else None,
        "last_kib": rss_values[-1] if rss_values else None,
        "min_kib": min(rss_values) if rss_values else None,
        "max_kib": max(rss_values) if rss_values else None,
        "max_increase_from_first_kib": (
            max(rss_values) - rss_values[0] if rss_values else None
        ),
    }

    report = {
        "schema": "inferlab.backpressure-overload.v0.0.6",
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "config": {
            "url": args.url,
            "requests": args.requests,
            "offered_rate_rps": args.offered_rate_rps,
            "estimated_capacity_rps": args.estimated_capacity_rps,
            "offered_load_multiple": round(
                args.offered_rate_rps / args.estimated_capacity_rps, 3
            ),
            "timeout_seconds": args.timeout,
        },
        "summary": {
            "wall_seconds": round(wall_seconds, 6),
            "status_counts": {
                str(status): count
                for status, count in sorted(
                    status_counts.items(), key=lambda item: str(item[0])
                )
            },
            "accepted_latency_ms": distribution(
                [item["e2e_ms"] for item in accepted]
            ),
            "rejected_latency_ms": distribution(
                [item["e2e_ms"] for item in rejected]
            ),
            "dispatch_lag_ms": distribution(
                [item["dispatch_lag_ms"] for item in results]
            ),
            "transport_errors": len(transport_errors),
            "worker_counts": dict(
                sorted(Counter(item["worker"] for item in accepted).items())
            ),
            "rss": rss_summary,
        },
        "gateway_status_after": status,
        "resource_samples": resource_samples,
        "samples": results,
    }
    print(json.dumps(report, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
