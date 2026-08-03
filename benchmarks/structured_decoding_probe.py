#!/usr/bin/env python3
"""Exercise sampled JSON-schema decoding through the gateway SSE contract."""

from __future__ import annotations

import argparse
import json
import urllib.error
import urllib.request
from pathlib import Path


def schema() -> dict:
    return {
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


def payload(stream: bool, seed: int) -> dict:
    return {
        "model": "inferlab-tiny",
        "stream": stream,
        "temperature": 1.0,
        "top_p": 1.0,
        "seed": seed,
        "max_tokens": 6,
        "messages": [{"role": "user", "content": "teach me streaming"}],
        "response_format": schema(),
    }


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


def non_stream(url: str, seed: int) -> dict:
    with request(url, payload(False, seed)) as response:
        body = json.loads(response.read())
        content = body["choices"][0]["message"]["content"]
        return {
            "status": response.status,
            "worker": response.headers.get("x-inferlab-worker"),
            "content": content,
            "parsed": json.loads(content),
            "finish_reason": body["choices"][0]["finish_reason"],
            "metrics": body["inferlab"]["generation"],
        }


def stream(url: str, seed: int) -> dict:
    with request(url, payload(True, seed)) as response:
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
        content = "".join(pieces)
        return {
            "status": response.status,
            "worker": response.headers.get("x-inferlab-worker"),
            "events": events,
            "pieces": pieces,
            "content": content,
            "parsed": json.loads(content),
            "finish_reason": finish_reason,
            "metrics": metrics,
            "done_received": events[-1] == "[DONE]" if events else False,
        }


def invalid_schema(url: str) -> dict:
    body = payload(False, 1)
    body["response_format"]["json_schema"]["schema"][
        "additionalProperties"
    ] = True
    try:
        request(url, body)
    except urllib.error.HTTPError as error:
        return {"status": error.code, "body": json.loads(error.read())}
    raise RuntimeError("invalid JSON schema unexpectedly succeeded")


def impossible_bans(url: str) -> dict:
    body = payload(False, 1)
    body["banned_token_ids"] = [4, 9, 15]
    try:
        request(url, body)
    except urllib.error.HTTPError as error:
        return {"status": error.code, "body": json.loads(error.read())}
    raise RuntimeError("grammar-exhausting token bans unexpectedly succeeded")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    first = non_stream(args.url, 7_007)
    replay = non_stream(args.url, 7_007)
    streamed = stream(args.url, 8_008)
    result = {
        "non_stream": first,
        "replay": replay,
        "same_seed_replay_matches": first["content"] == replay["content"],
        "stream": streamed,
        "invalid_schema": invalid_schema(args.url),
        "impossible_bans": impossible_bans(args.url),
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(
        f"structured stream status={streamed['status']} "
        f"pieces={len(streamed['pieces'])} "
        f"valid={isinstance(streamed['parsed'], dict)} "
        f"replay={result['same_seed_replay_matches']}"
    )


if __name__ == "__main__":
    main()
