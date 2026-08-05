#!/usr/bin/env python3
"""Exercise both CPU attention kernels directly and through gateway SSE."""

from __future__ import annotations

import argparse
import json
import urllib.request
from pathlib import Path


def request(url: str, stream: bool):
    body = {
        "model": "inferlab-tiny",
        "stream": stream,
        "temperature": 0,
        "max_tokens": 8,
        "messages": [{"role": "user", "content": "teach me streaming"}],
    }
    return urllib.request.urlopen(
        urllib.request.Request(
            url,
            data=json.dumps(body).encode(),
            headers={"content-type": "application/json"},
            method="POST",
        ),
        timeout=20,
    )


def non_stream(url: str) -> dict:
    with request(url, False) as response:
        payload = json.loads(response.read())
        return {
            "status": response.status,
            "worker": response.headers.get("x-inferlab-worker"),
            "content": payload["choices"][0]["message"]["content"],
            "finish_reason": payload["choices"][0]["finish_reason"],
            "usage": payload["usage"],
            "generation": payload["inferlab"]["generation"],
        }


def stream(url: str) -> dict:
    with request(url, True) as response:
        events = []
        for raw in response:
            line = raw.decode().strip()
            if line.startswith("data: "):
                data = line[6:]
                events.append(data if data == "[DONE]" else json.loads(data))
        pieces = []
        finish_reason = None
        for event in events:
            if not isinstance(event, dict) or "choices" not in event:
                continue
            choice = event["choices"][0]
            content = choice.get("delta", {}).get("content")
            if content is not None:
                pieces.append(content)
            if choice.get("finish_reason") is not None:
                finish_reason = choice["finish_reason"]
        return {
            "status": response.status,
            "worker": response.headers.get("x-inferlab-worker"),
            "pieces": pieces,
            "content": "".join(pieces),
            "finish_reason": finish_reason,
            "done_received": bool(events) and events[-1] == "[DONE]",
        }


def health(completion_url: str) -> dict:
    base = completion_url.removesuffix("/v1/chat/completions")
    with urllib.request.urlopen(base + "/health", timeout=20) as response:
        return json.loads(response.read())


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--materialized-worker-url", required=True)
    parser.add_argument("--online-worker-url", required=True)
    parser.add_argument("--gateway-url", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    materialized = non_stream(args.materialized_worker_url)
    online = non_stream(args.online_worker_url)
    gateway = non_stream(args.gateway_url)
    streamed = stream(args.gateway_url)
    result = {
        "materialized": materialized,
        "online_tiled": online,
        "same_direct_output": materialized["content"] == online["content"],
        "gateway": gateway,
        "gateway_matches_online": gateway["content"] == online["content"],
        "stream": streamed,
        "stream_reconstructs_gateway": streamed["content"] == gateway["content"],
        "materialized_health": health(args.materialized_worker_url),
        "online_health": health(args.online_worker_url),
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(
        "attention gateway "
        f"same_direct_output={result['same_direct_output']} "
        f"gateway_worker={gateway['worker']} "
        f"stream_matches={result['stream_reconstructs_gateway']}"
    )


if __name__ == "__main__":
    main()
