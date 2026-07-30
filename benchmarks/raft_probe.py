#!/usr/bin/env python3
"""Drive and observe the InferLab v0.6 Raft control plane."""

from __future__ import annotations

import argparse
import json
import time
import urllib.error
import urllib.request


def now_ms() -> float:
    return round(time.time() * 1000, 3)


def request_json(url: str, method: str = "GET", payload=None, timeout=0.35):
    encoded = None
    headers = {}
    if payload is not None:
        encoded = json.dumps(payload).encode("utf-8")
        headers["content-type"] = "application/json"
    request = urllib.request.Request(
        url, data=encoded, headers=headers, method=method
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            body = response.read()
            return {
                "status": response.status,
                "body": json.loads(body) if body else None,
                "worker": response.headers.get("x-inferlab-worker"),
            }
    except urllib.error.HTTPError as error:
        body = error.read()
        return {
            "status": error.code,
            "body": json.loads(body) if body else None,
            "worker": error.headers.get("x-inferlab-worker"),
        }
    except OSError as error:
        return {"status": None, "error": repr(error)}


def statuses(urls: list[str]) -> list[dict]:
    observations = []
    for url in urls:
        response = request_json(f"{url}/v1/control/status")
        observations.append({"url": url, **response})
    return observations


def wait_for_leader(
    urls: list[str], timeout_seconds: float, since_ms: float | None
) -> dict:
    started = time.monotonic()
    samples = 0
    while time.monotonic() - started < timeout_seconds:
        observed = statuses(urls)
        samples += 1
        leaders = [
            entry
            for entry in observed
            if entry["status"] == 200
            and entry["body"]["role"] == "leader"
        ]
        if len(leaders) == 1:
            elected_at = now_ms()
            return {
                "schema": "inferlab.raft-leader-observation.v0.6",
                "observed_at_ms": elected_at,
                "latency_ms": (
                    round(elected_at - since_ms, 3)
                    if since_ms is not None
                    else round((time.monotonic() - started) * 1000, 3)
                ),
                "samples": samples,
                "leader_id": leaders[0]["body"]["node_id"],
                "leader_url": leaders[0]["url"],
                "term": leaders[0]["body"]["term"],
                "statuses": observed,
            }
        time.sleep(0.025)
    raise SystemExit("timed out waiting for exactly one Raft leader")


def configuration(policy: str, weights: list[int]) -> dict:
    if len(weights) != 3 or any(weight <= 0 for weight in weights):
        raise SystemExit("--weights must contain three positive integers")
    return {
        "routing_policy": policy,
        "workers": [
            {
                "id": f"worker-{letter}",
                "base_url": f"http://127.0.0.1:{port}",
                "weight": weight,
            }
            for letter, port, weight in zip(
                ["a", "b", "c"], [9821, 9822, 9823], weights
            )
        ],
    }


def write_configuration(
    urls: list[str], policy: str, weights: list[int]
) -> dict:
    payload = configuration(policy, weights)
    started = time.monotonic()
    attempts = []
    while time.monotonic() - started < 3:
        leader = wait_for_leader(urls, 1, None)
        response = request_json(
            f"{leader['leader_url']}/v1/control/config",
            method="PUT",
            payload=payload,
            timeout=2.5,
        )
        attempts.append(
            {
                "leader_id": leader["leader_id"],
                "leader_url": leader["leader_url"],
                "term": leader["term"],
                "response": response,
            }
        )
        if response["status"] == 200:
            return {
                "schema": "inferlab.raft-config-write.v0.6",
                "written_at_ms": now_ms(),
                "attempts": attempts,
                "committed": response["body"],
            }
        time.sleep(0.05)
    raise SystemExit("configuration did not commit through a leader")


def wait_for_configuration(
    urls: list[str],
    policy: str,
    expected_nodes: int,
    minimum_revision: int,
    timeout_seconds: float,
) -> dict:
    started = time.monotonic()
    while time.monotonic() - started < timeout_seconds:
        observed = statuses(urls)
        live = [entry for entry in observed if entry["status"] == 200]
        matching = [
            entry
            for entry in live
            if entry["body"]["committed_configuration"] is not None
            and entry["body"]["committed_configuration"]["revision"]
            >= minimum_revision
            and entry["body"]["committed_configuration"]["configuration"][
                "routing_policy"
            ]
            == policy
        ]
        revisions = {
            entry["body"]["committed_configuration"]["revision"]
            for entry in matching
        }
        if (
            len(live) == expected_nodes
            and len(matching) == expected_nodes
            and len(revisions) == 1
        ):
            return {
                "schema": "inferlab.raft-convergence.v0.6",
                "observed_at_ms": now_ms(),
                "convergence_ms": round(
                    (time.monotonic() - started) * 1000, 3
                ),
                "expected_nodes": expected_nodes,
                "routing_policy": policy,
                "revision": revisions.pop(),
                "statuses": observed,
            }
        time.sleep(0.025)
    raise SystemExit(
        f"timed out waiting for {expected_nodes} nodes to converge on {policy}"
    )


def gateway_probe(
    gateway_url: str,
    expected_policy: str,
    minimum_revision: int,
    requests: int,
) -> dict:
    started = time.monotonic()
    results = []
    for number in range(1, requests + 1):
        result = request_json(
            f"{gateway_url}/v1/chat/completions",
            method="POST",
            payload={
                "model": "inferlab-fake",
                "stream": False,
                "messages": [
                    {
                        "role": "user",
                        "content": f"Raft election request {number}",
                    }
                ],
            },
            timeout=1.5,
        )
        results.append(result)
    deadline = time.monotonic() + 3
    status = None
    while time.monotonic() < deadline:
        status = request_json(f"{gateway_url}/internal/workers")
        if (
            status["status"] == 200
            and status["body"]["routing_policy"] == expected_policy
            and status["body"]["control_plane"]["revision"]
            >= minimum_revision
        ):
            break
        time.sleep(0.025)
    return {
        "schema": "inferlab.gateway-control-probe.v0.6",
        "observed_at_ms": now_ms(),
        "duration_ms": round((time.monotonic() - started) * 1000, 3),
        "expected_policy": expected_policy,
        "minimum_revision": minimum_revision,
        "requests": results,
        "gateway_status": status,
    }


def parse_urls(raw: str) -> list[str]:
    return [url.strip().rstrip("/") for url in raw.split(",") if url.strip()]


def main() -> None:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    leader_parser = subparsers.add_parser("wait-leader")
    leader_parser.add_argument("--urls", required=True)
    leader_parser.add_argument("--timeout", type=float, default=3)
    leader_parser.add_argument("--since-ms", type=float)

    write_parser = subparsers.add_parser("write-config")
    write_parser.add_argument("--urls", required=True)
    write_parser.add_argument("--policy", required=True)
    write_parser.add_argument("--weights", default="1,1,1")

    converge_parser = subparsers.add_parser("wait-config")
    converge_parser.add_argument("--urls", required=True)
    converge_parser.add_argument("--policy", required=True)
    converge_parser.add_argument("--expected-nodes", type=int, required=True)
    converge_parser.add_argument("--minimum-revision", type=int, required=True)
    converge_parser.add_argument("--timeout", type=float, default=4)

    gateway_parser = subparsers.add_parser("gateway-probe")
    gateway_parser.add_argument("--gateway-url", required=True)
    gateway_parser.add_argument("--expected-policy", required=True)
    gateway_parser.add_argument("--minimum-revision", type=int, required=True)
    gateway_parser.add_argument("--requests", type=int, default=6)

    args = parser.parse_args()
    if args.command == "wait-leader":
        result = wait_for_leader(
            parse_urls(args.urls), args.timeout, args.since_ms
        )
    elif args.command == "write-config":
        result = write_configuration(
            parse_urls(args.urls),
            args.policy,
            [int(value) for value in args.weights.split(",")],
        )
    elif args.command == "wait-config":
        result = wait_for_configuration(
            parse_urls(args.urls),
            args.policy,
            args.expected_nodes,
            args.minimum_revision,
            args.timeout,
        )
    else:
        result = gateway_probe(
            args.gateway_url,
            args.expected_policy,
            args.minimum_revision,
            args.requests,
        )
    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
