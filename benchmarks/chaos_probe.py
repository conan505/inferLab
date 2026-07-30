#!/usr/bin/env python3
"""Run continuous open-loop traffic while sampling InferLab gateway state."""

import argparse
import concurrent.futures
import json
import math
import os
import platform
import subprocess
import sys
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
    index = max(0, math.ceil(len(ordered) * quantile) - 1)
    return round(ordered[index], 3)


def distribution(values: list[float]) -> dict:
    return {
        "p50": percentile(values, 0.50),
        "p90": percentile(values, 0.90),
        "p95": percentile(values, 0.95),
        "p99": percentile(values, 0.99),
        "max": round(max(values), 3) if values else None,
    }


def numeric_header(response, name: str):
    value = response.headers.get(name)
    return int(value) if value is not None else None


def parse_error_type(body: bytes):
    try:
        return json.loads(body)["error"]["type"]
    except (json.JSONDecodeError, KeyError, TypeError):
        return None


def one_request(
    url: str,
    timeout: float,
    request_number: int,
    experiment_started: float,
    scheduled_at: float,
) -> dict:
    payload = json.dumps(
        {
            "model": "inferlab-fake",
            "stream": False,
            "messages": [
                {
                    "role": "user",
                    "content": f"continuous chaos request {request_number}",
                }
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
        "scheduled_ms": round(
            (scheduled_at - experiment_started) * 1000, 3
        ),
        "started_ms": round((started - experiment_started) * 1000, 3),
        "dispatch_lag_ms": round(max(0.0, started - scheduled_at) * 1000, 3),
    }
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            body = json.load(response)
            completed = time.perf_counter()
            return {
                **common,
                "completed_ms": round(
                    (completed - experiment_started) * 1000, 3
                ),
                "status": response.status,
                "worker": response.headers.get("x-inferlab-worker"),
                "attempts": numeric_header(response, "x-inferlab-attempts"),
                "error_type": None,
                "e2e_ms": round((completed - started) * 1000, 3),
                "response_id": body.get("id"),
            }
    except urllib.error.HTTPError as error:
        completed = time.perf_counter()
        body = error.read()
        return {
            **common,
            "completed_ms": round(
                (completed - experiment_started) * 1000, 3
            ),
            "status": error.code,
            "worker": error.headers.get("x-inferlab-worker"),
            "attempts": numeric_header(error, "x-inferlab-attempts"),
            "error_type": parse_error_type(body),
            "e2e_ms": round((completed - started) * 1000, 3),
        }
    except Exception as error:  # retain client-observed transport failures
        completed = time.perf_counter()
        return {
            **common,
            "completed_ms": round(
                (completed - experiment_started) * 1000, 3
            ),
            "status": None,
            "worker": None,
            "attempts": None,
            "error_type": None,
            "error": repr(error),
            "e2e_ms": round((completed - started) * 1000, 3),
        }


def fetch_json(url: str, timeout: float = 2.0) -> dict:
    with urllib.request.urlopen(url, timeout=timeout) as response:
        return json.load(response)


def process_rss_kib(pid: int):
    result = subprocess.run(
        ["ps", "-o", "rss=", "-p", str(pid)],
        check=False,
        capture_output=True,
        text=True,
    )
    value = result.stdout.strip()
    return int(value) if value else None


def sample_gateway(
    status_url: str,
    gateway_pid: int,
    experiment_started: float,
    interval: float,
    stop: threading.Event,
    samples: list[dict],
) -> None:
    sequence = 0
    while not stop.is_set():
        scheduled = experiment_started + sequence * interval
        delay = scheduled - time.perf_counter()
        if delay > 0 and stop.wait(delay):
            break
        elapsed_ms = round(
            (time.perf_counter() - experiment_started) * 1000, 3
        )
        try:
            status = fetch_json(status_url)
            samples.append(
                {
                    "elapsed_ms": elapsed_ms,
                    "rss_kib": process_rss_kib(gateway_pid),
                    "admission": status["admission"],
                    "resilience": status["resilience"],
                    "workers": status["workers"],
                }
            )
        except (OSError, KeyError, ValueError) as error:
            samples.append(
                {
                    "elapsed_ms": elapsed_ms,
                    "error": repr(error),
                }
            )
        sequence += 1


def command_output(command: list[str]):
    result = subprocess.run(
        command,
        check=False,
        capture_output=True,
        text=True,
    )
    value = result.stdout.strip()
    return value or None


def write_ready_file(path: str, started_epoch_ms: float) -> None:
    temporary = f"{path}.tmp"
    with open(temporary, "w", encoding="utf-8") as destination:
        json.dump({"started_epoch_ms": started_epoch_ms}, destination)
        destination.write("\n")
    os.replace(temporary, path)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", required=True)
    parser.add_argument("--status-url", required=True)
    parser.add_argument("--duration-seconds", type=float, required=True)
    parser.add_argument("--offered-rate-rps", type=float, required=True)
    parser.add_argument("--request-timeout", type=float, default=2.0)
    parser.add_argument("--status-interval", type=float, default=0.1)
    parser.add_argument("--gateway-pid", type=int, required=True)
    parser.add_argument("--ready-file", required=True)
    args = parser.parse_args()
    if (
        args.duration_seconds <= 0
        or args.offered_rate_rps <= 0
        or args.request_timeout <= 0
        or args.status_interval <= 0
        or args.gateway_pid <= 0
    ):
        parser.error("durations, rate, intervals, and PID must be positive")

    experiment_started = time.perf_counter()
    started_epoch_ms = round(time.time() * 1000, 3)
    write_ready_file(args.ready_file, started_epoch_ms)

    gateway_samples: list[dict] = []
    stop_sampling = threading.Event()
    sampler = threading.Thread(
        target=sample_gateway,
        args=(
            args.status_url,
            args.gateway_pid,
            experiment_started,
            args.status_interval,
            stop_sampling,
            gateway_samples,
        ),
        daemon=True,
    )
    sampler.start()

    request_count = math.floor(
        args.duration_seconds * args.offered_rate_rps
    )
    futures = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=64) as executor:
        for request_number in range(1, request_count + 1):
            scheduled_at = (
                experiment_started
                + (request_number - 1) / args.offered_rate_rps
            )
            delay = scheduled_at - time.perf_counter()
            if delay > 0:
                time.sleep(delay)
            futures.append(
                executor.submit(
                    one_request,
                    args.url,
                    args.request_timeout,
                    request_number,
                    experiment_started,
                    scheduled_at,
                )
            )
        requests = [future.result() for future in futures]

    stop_sampling.set()
    sampler.join(timeout=2)
    final_status = fetch_json(args.status_url)
    completed_seconds = time.perf_counter() - experiment_started
    status_counts = Counter(sample["status"] for sample in requests)
    successful = [sample for sample in requests if sample["status"] == 200]
    errors = [sample for sample in requests if sample["status"] != 200]
    attempts = [
        sample["attempts"]
        for sample in requests
        if sample.get("attempts") is not None
    ]
    dispatch_lags = [sample["dispatch_lag_ms"] for sample in requests]

    report = {
        "schema": "inferlab.chaos-run.v0.0.9",
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "environment": {
            "platform": platform.platform(),
            "machine": platform.machine(),
            "python": sys.version.split()[0],
            "rustc": command_output(["rustc", "--version"]),
            "git_commit": command_output(["git", "rev-parse", "HEAD"]),
        },
        "config": {
            "url": args.url,
            "status_url": args.status_url,
            "duration_seconds": args.duration_seconds,
            "offered_rate_rps": args.offered_rate_rps,
            "request_timeout_seconds": args.request_timeout,
            "status_interval_seconds": args.status_interval,
            "gateway_pid": args.gateway_pid,
            "scheduled_requests": request_count,
            "started_epoch_ms": started_epoch_ms,
        },
        "summary": {
            "wall_seconds": round(completed_seconds, 6),
            "status_counts": {
                ("transport" if status is None else str(status)): count
                for status, count in sorted(
                    status_counts.items(), key=lambda item: str(item[0])
                )
            },
            "successful_requests": len(successful),
            "error_requests": len(errors),
            "success_rate_percent": round(
                len(successful) / len(requests) * 100, 3
            ),
            "e2e_ms": distribution(
                [sample["e2e_ms"] for sample in requests]
            ),
            "successful_e2e_ms": distribution(
                [sample["e2e_ms"] for sample in successful]
            ),
            "dispatch_lag_ms": distribution(dispatch_lags),
            "worker_counts": dict(
                sorted(
                    Counter(
                        sample["worker"]
                        for sample in successful
                        if sample["worker"] is not None
                    ).items()
                )
            ),
            "client_observed_attempts": sum(attempts),
            "retried_responses": sum(attempt > 1 for attempt in attempts),
            "transport_errors": sum(
                sample["status"] is None for sample in requests
            ),
            "gateway_status_sample_errors": sum(
                "error" in sample for sample in gateway_samples
            ),
        },
        "gateway_status_after": final_status,
        "gateway_samples": gateway_samples,
        "requests": sorted(
            requests, key=lambda sample: sample["request_number"]
        ),
    }
    print(json.dumps(report, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
