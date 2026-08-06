#!/usr/bin/env python3
"""Submit or deliberately mutate v0.19 authorized control writes."""

from __future__ import annotations

import argparse
import json
import time
import urllib.error
import urllib.request
from pathlib import Path


def now_ms() -> float:
    return round(time.time() * 1000, 3)


def read_json(path: Path) -> dict:
    with path.open(encoding="utf-8") as source:
        return json.load(source)


def request_json(url: str, method: str, payload: dict | None = None) -> dict:
    data = None if payload is None else json.dumps(payload).encode("utf-8")
    request = urllib.request.Request(
        url,
        data=data,
        method=method,
        headers={"Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(request, timeout=3) as response:
            raw = response.read().decode("utf-8")
            return {
                "status": response.status,
                "body": json.loads(raw) if raw else None,
            }
    except urllib.error.HTTPError as error:
        raw = error.read().decode("utf-8")
        try:
            body = json.loads(raw) if raw else None
        except json.JSONDecodeError:
            body = {"raw": raw}
        return {"status": error.code, "body": body}


def submit(url: str, body_path: Path) -> dict:
    body = read_json(body_path)
    started_at_ms = now_ms()
    response = request_json(url.rstrip("/") + "/v1/control/config", "PUT", body)
    return {
        "schema": "inferlab.control-write-attempt.v0.19",
        "started_at_ms": started_at_ms,
        "observed_at_ms": now_ms(),
        "request": body,
        "response": response,
    }


def status(url: str) -> dict:
    observed_at_ms = now_ms()
    response = request_json(url.rstrip("/") + "/v1/control/status", "GET")
    return {
        "schema": "inferlab.control-write-status.v0.19",
        "observed_at_ms": observed_at_ms,
        "response": response,
    }


def mutate_worker(body_path: Path, worker_id: str) -> dict:
    body = read_json(body_path)
    original = body["configuration"]["workers"][0]["id"]
    body["configuration"]["workers"][0]["id"] = worker_id
    body["tamper_evidence"] = {
        "original_worker_id": original,
        "tampered_worker_id": worker_id,
        "signature_unchanged": True,
    }
    return body


def main() -> None:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    submit_parser = subparsers.add_parser("submit")
    submit_parser.add_argument("--url", required=True)
    submit_parser.add_argument("--body", type=Path, required=True)

    status_parser = subparsers.add_parser("status")
    status_parser.add_argument("--url", required=True)

    mutate_parser = subparsers.add_parser("mutate-worker")
    mutate_parser.add_argument("--body", type=Path, required=True)
    mutate_parser.add_argument("--worker-id", required=True)

    args = parser.parse_args()
    if args.command == "submit":
        result = submit(args.url, args.body)
    elif args.command == "status":
        result = status(args.url)
    else:
        result = mutate_worker(args.body, args.worker_id)
    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
