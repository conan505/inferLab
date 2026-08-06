#!/usr/bin/env python3
"""Observe signed service-trust policy convergence across control nodes."""

from __future__ import annotations

import argparse
import json
import time
import urllib.error
import urllib.request
from typing import Any


def now_ms() -> float:
    return time.time() * 1000.0


def fetch(url: str) -> dict[str, Any]:
    started = now_ms()
    request = urllib.request.Request(f"{url.rstrip('/')}/v1/control/status")
    try:
        with urllib.request.urlopen(request, timeout=0.25) as response:
            body = json.loads(response.read().decode("utf-8"))
            return {
                "url": url,
                "status": response.status,
                "duration_ms": now_ms() - started,
                "body": body,
            }
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as error:
        return {
            "url": url,
            "status": None,
            "duration_ms": now_ms() - started,
            "error": str(error),
        }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--urls", required=True)
    parser.add_argument("--generation", type=int, required=True)
    parser.add_argument("--minimum-rejections", type=int, default=0)
    parser.add_argument("--timeout", type=float, default=5.0)
    args = parser.parse_args()

    urls = [url.strip().rstrip("/") for url in args.urls.split(",") if url.strip()]
    if not urls:
        raise SystemExit("at least one URL is required")
    started = now_ms()
    deadline = time.monotonic() + args.timeout
    samples = 0
    first_observed: dict[str, float] = {}
    latest: list[dict[str, Any]] = []
    while True:
        samples += 1
        latest = [fetch(url) for url in urls]
        converged = True
        for sample in latest:
            authentication = sample.get("body", {}).get("service_authentication", {})
            matches = (
                sample.get("status") == 200
                and authentication.get("trust_policy_generation") == args.generation
                and authentication.get("trust_policy_rejections", 0)
                >= args.minimum_rejections
            )
            if matches:
                first_observed.setdefault(sample["url"], now_ms())
            else:
                converged = False
        if converged:
            break
        if time.monotonic() >= deadline:
            raise SystemExit(
                json.dumps(
                    {
                        "error": "service-trust convergence timed out",
                        "expected_generation": args.generation,
                        "minimum_rejections": args.minimum_rejections,
                        "latest": latest,
                    },
                    indent=2,
                    sort_keys=True,
                )
            )
        time.sleep(0.025)

    completed = now_ms()
    observations = [
        {
            "url": url,
            "first_observed_at_ms": first_observed[url],
            "convergence_latency_ms": first_observed[url] - started,
        }
        for url in urls
    ]
    print(
        json.dumps(
            {
                "schema": "inferlab.service-trust-convergence.v0.22",
                "started_at_ms": started,
                "completed_at_ms": completed,
                "duration_ms": completed - started,
                "samples": samples,
                "expected_generation": args.generation,
                "minimum_rejections": args.minimum_rejections,
                "observations": observations,
                "statuses": latest,
            },
            indent=2,
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
