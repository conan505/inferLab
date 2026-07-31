#!/usr/bin/env python3
"""Observe real token timing through the gateway and C++ worker."""

from __future__ import annotations

import argparse
import json
import time
import urllib.request
from pathlib import Path

EXPECTED_TEXT = "InferLab turns prompts into real tokens."


def post(url: str, payload: dict):
    request = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={"content-type": "application/json"},
        method="POST",
    )
    return urllib.request.urlopen(request, timeout=10)


def stream_probe(url: str, prompt: str) -> dict:
    payload = {
        "model": "inferlab-tiny",
        "stream": True,
        "temperature": 0,
        "max_tokens": 8,
        "messages": [{"role": "user", "content": prompt}],
    }
    started = time.perf_counter_ns()
    with post(url, payload) as response:
        status = response.status
        headers = {name.lower(): value for name, value in response.headers.items()}
        events = []
        while True:
            raw = response.readline()
            if not raw:
                break
            line = raw.decode("utf-8").strip()
            if not line.startswith("data: "):
                continue
            data = line[6:]
            elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000.0
            event = {"at_ms": elapsed_ms, "data": data}
            if data != "[DONE]":
                event["json"] = json.loads(data)
            events.append(event)

    content_events = []
    finish_reason = None
    for event in events:
        parsed = event.get("json")
        if not parsed or "choices" not in parsed:
            continue
        choice = parsed["choices"][0]
        content = choice.get("delta", {}).get("content")
        if content is not None:
            content_events.append(
                {"at_ms": event["at_ms"], "content": content}
            )
        if choice.get("finish_reason") is not None:
            finish_reason = choice["finish_reason"]
    intervals = [
        current["at_ms"] - previous["at_ms"]
        for previous, current in zip(content_events, content_events[1:])
    ]
    return {
        "status": status,
        "headers": headers,
        "events": events,
        "content_events": content_events,
        "content": "".join(event["content"] for event in content_events),
        "finish_reason": finish_reason,
        "done_received": any(event["data"] == "[DONE]" for event in events),
        "ttft_ms": content_events[0]["at_ms"] if content_events else None,
        "stream_span_ms": (
            content_events[-1]["at_ms"] - content_events[0]["at_ms"]
            if len(content_events) > 1
            else 0.0
        ),
        "inter_token_ms": intervals,
    }


def non_stream_probe(url: str, prompt: str) -> dict:
    payload = {
        "model": "inferlab-tiny",
        "stream": False,
        "temperature": 0,
        "max_tokens": 8,
        "messages": [{"role": "user", "content": prompt}],
    }
    started = time.perf_counter_ns()
    with post(url, payload) as response:
        return {
            "status": response.status,
            "headers": {
                name.lower(): value for name, value in response.headers.items()
            },
            "latency_ms": (
                time.perf_counter_ns() - started
            ) / 1_000_000.0,
            "body": json.loads(response.read()),
        }


def get_json(url: str) -> dict:
    with urllib.request.urlopen(url, timeout=5) as response:
        return {"status": response.status, "body": json.loads(response.read())}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", required=True)
    parser.add_argument("--worker-health-url", required=True)
    parser.add_argument("--prompt", default="teach me streaming")
    parser.add_argument("--stream-output", type=Path, required=True)
    parser.add_argument("--non-stream-output", type=Path, required=True)
    args = parser.parse_args()
    stream = stream_probe(args.url, args.prompt)
    stream["worker_health"] = get_json(args.worker_health_url)
    non_stream = non_stream_probe(args.url, args.prompt)
    args.stream_output.write_text(json.dumps(stream, indent=2) + "\n")
    args.non_stream_output.write_text(json.dumps(non_stream, indent=2) + "\n")
    print(
        f"stream status={stream['status']} "
        f"tokens={len(stream['content_events'])} "
        f"span={stream['stream_span_ms']:.3f} ms "
        f"text_match={stream['content'] == EXPECTED_TEXT}"
    )


if __name__ == "__main__":
    main()
