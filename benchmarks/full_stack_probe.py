#!/usr/bin/env python3
"""Drive the retained real-worker, Raft-configured full-stack experiment."""

from __future__ import annotations

import argparse
import collections
import json
import time
import urllib.error
import urllib.request


def now_ms() -> float:
    return round(time.time() * 1000, 3)


def request_json(
    url: str,
    method: str = "GET",
    payload=None,
    timeout: float = 2.0,
    headers: dict[str, str] | None = None,
) -> dict:
    encoded = None
    request_headers = dict(headers or {})
    if payload is not None:
        encoded = json.dumps(payload).encode()
        request_headers["content-type"] = "application/json"
    request = urllib.request.Request(
        url,
        data=encoded,
        headers=request_headers,
        method=method,
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            body = response.read()
            return response_result(response, body)
    except urllib.error.HTTPError as error:
        return response_result(error, error.read())
    except OSError as error:
        return {
            "status": None,
            "error": repr(error),
            "observed_at_ms": now_ms(),
        }


def response_result(response, body: bytes) -> dict:
    parsed = None
    if body:
        try:
            parsed = json.loads(body)
        except json.JSONDecodeError:
            parsed = body.decode(errors="replace")

    def integer_header(name: str) -> int | None:
        value = response.headers.get(name)
        return int(value) if value is not None else None

    return {
        "status": response.status,
        "body": parsed,
        "worker": response.headers.get("x-inferlab-worker"),
        "attempts": integer_header("x-inferlab-attempts"),
        "config_revision": integer_header("x-inferlab-config-revision"),
        "config_term": integer_header("x-inferlab-config-term"),
        "control_cluster_id": response.headers.get(
            "x-inferlab-control-cluster"
        ),
        "observed_at_ms": now_ms(),
    }


def parse_urls(raw: str) -> list[str]:
    return [url.strip().rstrip("/") for url in raw.split(",") if url.strip()]


def statuses(urls: list[str]) -> list[dict]:
    return [
        {"url": url, **request_json(f"{url}/v1/control/status", timeout=0.4)}
        for url in urls
    ]


def wait_for_leader(
    urls: list[str],
    timeout_seconds: float,
    since_ms: float | None,
) -> dict:
    started = time.monotonic()
    samples = 0
    while time.monotonic() - started < timeout_seconds:
        observed = statuses(urls)
        samples += 1
        leaders = [
            entry
            for entry in observed
            if entry["status"] == 200 and entry["body"]["role"] == "leader"
        ]
        if len(leaders) == 1:
            elected_at = now_ms()
            return {
                "schema": "inferlab.full-stack-leader.v0.13",
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


def parse_workers(raw: str) -> list[dict]:
    workers = []
    for entry in raw.split(","):
        worker_id, remainder = entry.split("=", 1)
        base_url, separator, raw_weight = remainder.rpartition("@")
        if not separator:
            base_url = remainder
            raw_weight = "1"
        workers.append(
            {
                "id": worker_id.strip(),
                "base_url": base_url.strip().rstrip("/"),
                "weight": int(raw_weight),
            }
        )
    if not workers or any(not worker["id"] for worker in workers):
        raise SystemExit("--workers requires id=url@weight entries")
    return workers


def write_configuration(
    urls: list[str],
    policy: str,
    workers: list[dict],
) -> dict:
    payload = {"routing_policy": policy, "workers": workers}
    started = time.monotonic()
    attempts = []
    while time.monotonic() - started < 4:
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
                "schema": "inferlab.full-stack-config-write.v0.13",
                "written_at_ms": now_ms(),
                "attempts": attempts,
                "committed": response["body"],
            }
        time.sleep(0.05)
    raise SystemExit("routing configuration did not commit")


def wait_gateway(
    gateway_url: str,
    policy: str,
    revision: int,
    worker_ids: list[str],
    timeout_seconds: float,
) -> dict:
    started = time.monotonic()
    samples = 0
    while time.monotonic() - started < timeout_seconds:
        response = request_json(f"{gateway_url}/internal/workers")
        samples += 1
        if response["status"] == 200:
            body = response["body"]
            observed_ids = sorted(worker["id"] for worker in body["workers"])
            if (
                body["routing_policy"] == policy
                and body["routing_snapshot"]["control_revision"] == revision
                and observed_ids == sorted(worker_ids)
            ):
                return {
                    "schema": "inferlab.full-stack-gateway-snapshot.v0.13",
                    "observed_at_ms": now_ms(),
                    "apply_latency_ms": round(
                        (time.monotonic() - started) * 1000, 3
                    ),
                    "samples": samples,
                    "expected_revision": revision,
                    "expected_policy": policy,
                    "expected_worker_ids": sorted(worker_ids),
                    "status": response,
                }
        time.sleep(0.025)
    raise SystemExit(f"gateway did not apply revision {revision}")


def completion_body(stream: bool, prompt: str, speculative_tokens: int) -> dict:
    return {
        "model": "inferlab-tiny",
        "stream": stream,
        "temperature": 0,
        "speculative_tokens": speculative_tokens,
        "max_tokens": 8,
        "messages": [{"role": "user", "content": prompt}],
    }


def completion_summary(result: dict) -> dict:
    body = result.get("body")
    if result.get("status") != 200 or not isinstance(body, dict):
        return result
    generation = body["inferlab"]["generation"]
    return {
        **{key: value for key, value in result.items() if key != "body"},
        "content": body["choices"][0]["message"]["content"],
        "finish_reason": body["choices"][0]["finish_reason"],
        "usage": body["usage"],
        "generation": generation,
    }


def request_set(
    gateway_url: str,
    requests: int,
    prompt: str,
    speculative_tokens: int,
    cache_key: str | None,
) -> dict:
    started_at = now_ms()
    started = time.monotonic()
    observations = []
    for number in range(requests):
        headers = {}
        if cache_key is not None:
            headers["x-inferlab-cache-key"] = cache_key
        observations.append(
            completion_summary(
                request_json(
                    f"{gateway_url}/v1/chat/completions",
                    method="POST",
                    payload=completion_body(
                        False,
                        f"{prompt}{number if cache_key is None else ''}",
                        speculative_tokens,
                    ),
                    timeout=10,
                    headers=headers,
                )
            )
        )
    worker_counts = collections.Counter(
        item["worker"] for item in observations if item.get("worker")
    )
    return {
        "schema": "inferlab.full-stack-request-set.v0.13",
        "started_at_ms": started_at,
        "observed_at_ms": now_ms(),
        "duration_ms": round((time.monotonic() - started) * 1000, 3),
        "requested": requests,
        "succeeded": sum(item.get("status") == 200 for item in observations),
        "worker_counts": dict(sorted(worker_counts.items())),
        "requests": observations,
    }


def affinity_probe(gateway_url: str, cache_key: str) -> dict:
    result = request_set(
        gateway_url,
        requests=2,
        prompt="teach me streaming",
        speculative_tokens=0,
        cache_key=cache_key,
    )
    first, second = result["requests"]
    result.update(
        {
            "schema": "inferlab.full-stack-affinity.v0.13",
            "cache_key": cache_key,
            "same_worker": first.get("worker") == second.get("worker"),
            "same_content": first.get("content") == second.get("content"),
            "first_prefix_cache_hit": first.get("generation", {}).get(
                "prefix_cache_hit"
            ),
            "second_prefix_cache_hit": second.get("generation", {}).get(
                "prefix_cache_hit"
            ),
        }
    )
    return result


def stream_probe(
    gateway_url: str,
    prompt: str,
    speculative_tokens: int,
) -> dict:
    request = urllib.request.Request(
        f"{gateway_url}/v1/chat/completions",
        data=json.dumps(
            completion_body(True, prompt, speculative_tokens)
        ).encode(),
        headers={"content-type": "application/json"},
        method="POST",
    )
    started_at = now_ms()
    started = time.monotonic()
    with urllib.request.urlopen(request, timeout=10) as response:
        events = []
        for raw in response:
            line = raw.decode().strip()
            if line.startswith("data: "):
                value = line[6:]
                events.append(value if value == "[DONE]" else json.loads(value))
        pieces = []
        finish_reason = None
        generation = None
        for event in events:
            if not isinstance(event, dict) or "choices" not in event:
                continue
            choice = event["choices"][0]
            content = choice.get("delta", {}).get("content")
            if content is not None:
                pieces.append(content)
            if choice.get("finish_reason") is not None:
                finish_reason = choice["finish_reason"]
                generation = event.get("inferlab", {}).get("generation")
        return {
            "schema": "inferlab.full-stack-stream.v0.13",
            "started_at_ms": started_at,
            "observed_at_ms": now_ms(),
            "duration_ms": round((time.monotonic() - started) * 1000, 3),
            "status": response.status,
            "worker": response.headers.get("x-inferlab-worker"),
            "attempts": int(response.headers["x-inferlab-attempts"]),
            "config_revision": int(
                response.headers["x-inferlab-config-revision"]
            ),
            "config_term": int(response.headers["x-inferlab-config-term"]),
            "control_cluster_id": response.headers.get(
                "x-inferlab-control-cluster"
            ),
            "pieces": pieces,
            "content": "".join(pieces),
            "finish_reason": finish_reason,
            "generation": generation,
            "done_received": bool(events) and events[-1] == "[DONE]",
        }


def worker_health(raw_workers: str) -> dict:
    observations = {}
    for worker in parse_workers(raw_workers):
        observations[worker["id"]] = request_json(
            worker["base_url"] + "/health"
        )
    return {
        "schema": "inferlab.full-stack-worker-health.v0.13",
        "observed_at_ms": now_ms(),
        "workers": observations,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    leader = subparsers.add_parser("wait-leader")
    leader.add_argument("--urls", required=True)
    leader.add_argument("--timeout", type=float, default=4)
    leader.add_argument("--since-ms", type=float)

    write = subparsers.add_parser("write-config")
    write.add_argument("--urls", required=True)
    write.add_argument("--policy", required=True)
    write.add_argument("--workers", required=True)

    gateway = subparsers.add_parser("wait-gateway")
    gateway.add_argument("--gateway-url", required=True)
    gateway.add_argument("--policy", required=True)
    gateway.add_argument("--revision", type=int, required=True)
    gateway.add_argument("--worker-ids", required=True)
    gateway.add_argument("--timeout", type=float, default=4)

    affinity = subparsers.add_parser("affinity")
    affinity.add_argument("--gateway-url", required=True)
    affinity.add_argument("--cache-key", default="v0.13/shared-prefix")

    requests = subparsers.add_parser("requests")
    requests.add_argument("--gateway-url", required=True)
    requests.add_argument("--requests", type=int, default=1)
    requests.add_argument("--prompt", default="teach me streaming")
    requests.add_argument("--speculative-tokens", type=int, default=0)
    requests.add_argument("--cache-key")

    stream = subparsers.add_parser("stream")
    stream.add_argument("--gateway-url", required=True)
    stream.add_argument("--prompt", default="teach me streaming")
    stream.add_argument("--speculative-tokens", type=int, default=3)

    health = subparsers.add_parser("worker-health")
    health.add_argument("--workers", required=True)

    args = parser.parse_args()
    if args.command == "wait-leader":
        result = wait_for_leader(
            parse_urls(args.urls), args.timeout, args.since_ms
        )
    elif args.command == "write-config":
        result = write_configuration(
            parse_urls(args.urls), args.policy, parse_workers(args.workers)
        )
    elif args.command == "wait-gateway":
        result = wait_gateway(
            args.gateway_url.rstrip("/"),
            args.policy,
            args.revision,
            [value for value in args.worker_ids.split(",") if value],
            args.timeout,
        )
    elif args.command == "affinity":
        result = affinity_probe(args.gateway_url.rstrip("/"), args.cache_key)
    elif args.command == "requests":
        result = request_set(
            args.gateway_url.rstrip("/"),
            args.requests,
            args.prompt,
            args.speculative_tokens,
            args.cache_key,
        )
    elif args.command == "stream":
        result = stream_probe(
            args.gateway_url.rstrip("/"),
            args.prompt,
            args.speculative_tokens,
        )
    else:
        result = worker_health(args.workers)
    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
