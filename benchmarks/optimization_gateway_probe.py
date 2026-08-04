#!/usr/bin/env python3
"""Exercise quantized and speculative generation through HTTP and gateway SSE."""

from __future__ import annotations

import argparse
import json
import urllib.error
import urllib.request
from pathlib import Path


def request(url: str, body: dict):
    return urllib.request.urlopen(
        urllib.request.Request(
            url,
            data=json.dumps(body).encode(),
            headers={"content-type": "application/json"},
            method="POST",
        ),
        timeout=20,
    )


def body(
    stream: bool,
    speculative_tokens: int,
    seed: int = 0,
    temperature: float = 0,
) -> dict:
    return {
        "model": "inferlab-tiny",
        "stream": stream,
        "temperature": temperature,
        "seed": seed,
        "speculative_tokens": speculative_tokens,
        "max_tokens": 8,
        "messages": [{"role": "user", "content": "teach me streaming"}],
    }


def non_stream(
    url: str,
    speculative_tokens: int,
    seed: int = 0,
    temperature: float = 0,
) -> dict:
    with request(
        url,
        body(False, speculative_tokens, seed, temperature),
    ) as response:
        payload = json.loads(response.read())
        return {
            "status": response.status,
            "worker": response.headers.get("x-inferlab-worker"),
            "content": payload["choices"][0]["message"]["content"],
            "finish_reason": payload["choices"][0]["finish_reason"],
            "metrics": payload["inferlab"]["generation"],
        }


def stream(url: str) -> dict:
    with request(url, body(True, 3)) as response:
        events = []
        for raw in response:
            line = raw.decode().strip()
            if not line.startswith("data: "):
                continue
            data = line[6:]
            events.append(data if data == "[DONE]" else json.loads(data))
        pieces = []
        finish_reason = None
        metrics = None
        for event in events:
            if not isinstance(event, dict) or "choices" not in event:
                continue
            choice = event["choices"][0]
            piece = choice.get("delta", {}).get("content")
            if piece is not None:
                pieces.append(piece)
            if choice.get("finish_reason") is not None:
                finish_reason = choice["finish_reason"]
                metrics = event.get("inferlab", {}).get("generation")
        return {
            "status": response.status,
            "worker": response.headers.get("x-inferlab-worker"),
            "pieces": pieces,
            "content": "".join(pieces),
            "finish_reason": finish_reason,
            "metrics": metrics,
            "done_received": bool(events) and events[-1] == "[DONE]",
        }


def invalid_structured_speculation(url: str) -> dict:
    payload = body(False, 3)
    payload["max_tokens"] = 6
    payload["response_format"] = {
        "type": "json_schema",
        "json_schema": {
            "name": "inference_summary",
            "strict": True,
            "schema": {
                "type": "object",
                "properties": {
                    "answer": {
                        "type": "string",
                        "enum": ["InferLab", "systems", "tokens"],
                    },
                    "confidence": {
                        "type": "string",
                        "enum": ["high", "medium", "low"],
                    },
                },
                "required": ["answer", "confidence"],
                "additionalProperties": False,
            },
        },
    }
    try:
        request(url, payload)
    except urllib.error.HTTPError as error:
        return {"status": error.code, "body": json.loads(error.read())}
    raise RuntimeError("structured speculation unexpectedly succeeded")


def health(url: str) -> dict:
    with urllib.request.urlopen(url, timeout=20) as response:
        return json.loads(response.read())


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--gateway-url", required=True)
    parser.add_argument("--int4-worker-url", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    baseline = non_stream(args.gateway_url, 0)
    speculative = non_stream(args.gateway_url, 3)
    replay_first = non_stream(args.gateway_url, 3, 7_007, 2.0)
    replay_second = non_stream(args.gateway_url, 3, 7_007, 2.0)
    int4 = non_stream(args.int4_worker_url, 0)
    result = {
        "baseline": baseline,
        "speculative": speculative,
        "same_output": baseline["content"] == speculative["content"],
        "sample_replay": {
            "first": replay_first,
            "second": replay_second,
            "matches": replay_first["content"] == replay_second["content"],
        },
        "stream": stream(args.gateway_url),
        "invalid_structured_speculation": invalid_structured_speculation(
            args.gateway_url
        ),
        "int4": int4,
        "int4_health": health(args.int4_worker_url.rsplit("/", 3)[0] + "/health"),
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(
        "optimization gateway "
        f"same_output={result['same_output']} "
        f"target_calls={speculative['metrics']['speculation']['target_forward_calls']} "
        f"int4={result['int4_health']['model']['dtype']}"
    )


if __name__ == "__main__":
    main()
