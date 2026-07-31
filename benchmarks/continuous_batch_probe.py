#!/usr/bin/env python3
"""Compare one-slot scheduling with v0.8 continuous batching over HTTP."""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import statistics
import time
import urllib.request
from pathlib import Path

MAX_TOKEN_PATTERN = [2, 4, 6, 8]


def percentile(values: list[float], quantile: float) -> float:
    ordered = sorted(values)
    if not ordered:
        return 0.0
    index = min(len(ordered) - 1, int((len(ordered) - 1) * quantile + 0.999999))
    return ordered[index]


def get_json(url: str) -> dict:
    with urllib.request.urlopen(url, timeout=10) as response:
        return json.loads(response.read())


def complete(url: str, ordinal: int, max_tokens: int) -> dict:
    payload = {
        "model": "inferlab-tiny",
        "stream": False,
        "temperature": 0,
        "max_tokens": max_tokens,
        "messages": [
            {"role": "user", "content": f"batch request {ordinal}"}
        ],
    }
    request = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={"content-type": "application/json"},
        method="POST",
    )
    started = time.perf_counter_ns()
    with urllib.request.urlopen(request, timeout=30) as response:
        body = json.loads(response.read())
        latency_ms = (time.perf_counter_ns() - started) / 1_000_000.0
        return {
            "ordinal": ordinal,
            "requested_max_tokens": max_tokens,
            "status": response.status,
            "latency_ms": latency_ms,
            "request_id": body["inferlab"]["request_id"],
            "finish_reason": body["choices"][0]["finish_reason"],
            "completion_tokens": body["usage"]["completion_tokens"],
            "text": body["choices"][0]["message"]["content"],
            "generation": body["inferlab"]["generation"],
        }


def run_level(
    url: str,
    concurrency: int,
    request_count: int,
    ordinal_base: int,
) -> dict:
    started = time.perf_counter_ns()
    with concurrent.futures.ThreadPoolExecutor(
        max_workers=concurrency
    ) as executor:
        futures = [
            executor.submit(
                complete,
                url,
                ordinal_base + index,
                MAX_TOKEN_PATTERN[index % len(MAX_TOKEN_PATTERN)],
            )
            for index in range(request_count)
        ]
        requests = [future.result() for future in futures]
    wall_ms = (time.perf_counter_ns() - started) / 1_000_000.0
    requests.sort(key=lambda item: item["ordinal"])
    latencies = [item["latency_ms"] for item in requests]
    completion_tokens = sum(
        item["completion_tokens"] for item in requests
    )
    return {
        "concurrency": concurrency,
        "request_count": request_count,
        "wall_ms": wall_ms,
        "request_throughput_per_second": request_count / (wall_ms / 1_000),
        "token_throughput_per_second": completion_tokens / (wall_ms / 1_000),
        "latency_ms": {
            "mean": statistics.fmean(latencies),
            "p50": percentile(latencies, 0.50),
            "p95": percentile(latencies, 0.95),
            "max": max(latencies),
        },
        "completion_tokens": completion_tokens,
        "requests": requests,
    }


def run_configuration(
    name: str,
    url: str,
    scheduler_url: str,
    concurrency_levels: list[int],
    request_count: int,
) -> dict:
    levels = []
    ordinal = 0
    for concurrency in concurrency_levels:
        levels.append(run_level(url, concurrency, request_count, ordinal))
        ordinal += request_count
    return {
        "name": name,
        "levels": levels,
        "scheduler": get_json(scheduler_url)["scheduler"],
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--serial-url", required=True)
    parser.add_argument("--serial-scheduler-url", required=True)
    parser.add_argument("--continuous-url", required=True)
    parser.add_argument("--continuous-scheduler-url", required=True)
    parser.add_argument("--concurrency", default="1,2,4,8")
    parser.add_argument("--requests-per-level", type=int, default=24)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    levels = [int(value) for value in args.concurrency.split(",")]
    if any(value <= 0 for value in levels):
        raise SystemExit("concurrency values must be positive")
    if args.requests_per_level <= 0:
        raise SystemExit("requests per level must be positive")

    serial = run_configuration(
        "one-slot",
        args.serial_url,
        args.serial_scheduler_url,
        levels,
        args.requests_per_level,
    )
    continuous = run_configuration(
        "continuous-four-slot",
        args.continuous_url,
        args.continuous_scheduler_url,
        levels,
        args.requests_per_level,
    )

    backfill_workload = [2, 8, 4, 8, 2, 4, 6, 8]
    backfill_started = time.perf_counter_ns()
    with concurrent.futures.ThreadPoolExecutor(
        max_workers=len(backfill_workload)
    ) as executor:
        futures = [
            executor.submit(complete, args.continuous_url, 10_000 + index, limit)
            for index, limit in enumerate(backfill_workload)
        ]
        backfill_requests = [future.result() for future in futures]
    backfill_wall_ms = (
        time.perf_counter_ns() - backfill_started
    ) / 1_000_000.0
    continuous["scheduler"] = get_json(args.continuous_scheduler_url)[
        "scheduler"
    ]
    backfill_ids = {item["request_id"] for item in backfill_requests}
    backfill_trace = [
        event
        for event in continuous["scheduler"]["trace"]
        if event["request_id"] in backfill_ids
    ]

    result = {
        "workload": {
            "concurrency_levels": levels,
            "requests_per_level": args.requests_per_level,
            "max_token_pattern": MAX_TOKEN_PATTERN,
            "batch_tick_note": (
                "Each worker applies the configured delay once per scheduler "
                "batch. It makes slot sharing observable and is not a GPU "
                "kernel benchmark. C++ token computation remains per session."
            ),
        },
        "configurations": [serial, continuous],
        "backfill": {
            "wall_ms": backfill_wall_ms,
            "requested_max_tokens": backfill_workload,
            "requests": sorted(
                backfill_requests, key=lambda item: item["ordinal"]
            ),
            "trace": backfill_trace,
        },
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    serial_high = serial["levels"][-1]
    continuous_high = continuous["levels"][-1]
    gain = (
        continuous_high["request_throughput_per_second"]
        / serial_high["request_throughput_per_second"]
    )
    print(
        f"concurrency={levels[-1]} one-slot="
        f"{serial_high['request_throughput_per_second']:.2f} req/s "
        f"continuous="
        f"{continuous_high['request_throughput_per_second']:.2f} req/s "
        f"gain={gain:.2f}x"
    )


if __name__ == "__main__":
    main()
