#!/usr/bin/env python3
"""Observe restart-safe gateway routing snapshots for the v0.14 proof."""

from __future__ import annotations

import argparse
import json
import time
import urllib.error
import urllib.request


def now_ms() -> float:
    return round(time.time() * 1000, 3)


def get_json(url: str, timeout: float = 0.5) -> dict:
    request = urllib.request.Request(url, method="GET")
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return {
            "status": response.status,
            "body": json.loads(response.read().decode()),
        }


def wait_status(args: argparse.Namespace) -> dict:
    started = time.monotonic()
    samples = 0
    last_observation = None
    while time.monotonic() - started < args.timeout:
        try:
            observation = get_json(
                args.gateway_url.rstrip("/") + "/internal/workers"
            )
            last_observation = observation
            samples += 1
            body = observation["body"]
            control = body.get("control_plane") or {}
            worker_ids = sorted(worker["id"] for worker in body["workers"])
            error = control.get("last_error") or ""
            if (
                observation["status"] == 200
                and body["routing_policy"] == args.policy
                and body["routing_snapshot"]["control_revision"]
                == args.revision
                and worker_ids == sorted(args.worker_ids.split(","))
                and control.get("bootstrap_source") == args.bootstrap_source
                and control.get("persisted_revision")
                == args.persisted_revision
                and (
                    args.last_error_contains is None
                    or args.last_error_contains in error
                )
            ):
                observed_at = now_ms()
                return {
                    "schema": "inferlab.gateway-restart-status.v0.14",
                    "observed_at_ms": observed_at,
                    "boot_latency_ms": (
                        round(observed_at - args.started_at_ms, 3)
                        if args.started_at_ms is not None
                        else None
                    ),
                    "wait_latency_ms": round(
                        (time.monotonic() - started) * 1000, 3
                    ),
                    "samples": samples,
                    "expected": {
                        "revision": args.revision,
                        "policy": args.policy,
                        "worker_ids": sorted(args.worker_ids.split(",")),
                        "bootstrap_source": args.bootstrap_source,
                        "persisted_revision": args.persisted_revision,
                        "last_error_contains": args.last_error_contains,
                    },
                    "status": observation,
                }
        except (OSError, ValueError, urllib.error.URLError):
            pass
        time.sleep(0.025)
    raise SystemExit(
        "gateway status did not reach the expected restart state; "
        f"last observation={last_observation}"
    )


def control_config(args: argparse.Namespace) -> dict:
    started = time.monotonic()
    attempts = []
    urls = [value.rstrip("/") for value in args.urls.split(",") if value]
    while time.monotonic() - started < args.timeout:
        for url in urls:
            try:
                response = get_json(url + "/v1/control/config")
                attempts.append({"url": url, "status": response["status"]})
                if response["status"] == 200:
                    return {
                        "schema": "inferlab.gateway-restart-control.v0.14",
                        "observed_at_ms": now_ms(),
                        "source_url": url,
                        "attempts": attempts,
                        "committed": response["body"],
                    }
            except (OSError, ValueError, urllib.error.URLError) as error:
                attempts.append({"url": url, "error": str(error)})
        time.sleep(0.025)
    raise SystemExit("no control-plane node returned a committed configuration")


def main() -> None:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    status = subparsers.add_parser("wait-status")
    status.add_argument("--gateway-url", required=True)
    status.add_argument("--revision", type=int, required=True)
    status.add_argument("--policy", required=True)
    status.add_argument("--worker-ids", required=True)
    status.add_argument("--bootstrap-source", required=True)
    status.add_argument("--persisted-revision", type=int, required=True)
    status.add_argument("--last-error-contains")
    status.add_argument("--started-at-ms", type=float)
    status.add_argument("--timeout", type=float, default=5)

    control = subparsers.add_parser("control-config")
    control.add_argument("--urls", required=True)
    control.add_argument("--timeout", type=float, default=4)

    args = parser.parse_args()
    result = wait_status(args) if args.command == "wait-status" else control_config(args)
    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
