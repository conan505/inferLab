#!/usr/bin/env python3
"""Check the retained v0.7 model, parity, and streaming claims."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path

EXPECTED_TEXT = "InferLab turns prompts into real tokens."


def assertion(name: str, passed: bool, observed) -> dict:
    return {"name": name, "passed": bool(passed), "observed": observed}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", type=Path, required=True)
    parser.add_argument("--metadata", type=Path, required=True)
    parser.add_argument("--parity", type=Path, nargs="+", required=True)
    parser.add_argument("--stream", type=Path, required=True)
    parser.add_argument("--non-stream", type=Path, required=True)
    parser.add_argument("--environment", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    metadata = json.loads(args.metadata.read_text())
    parities = [json.loads(path.read_text()) for path in args.parity]
    stream = json.loads(args.stream.read_text())
    non_stream = json.loads(args.non_stream.read_text())
    environment = json.loads(args.environment.read_text())
    actual_digest = hashlib.sha256(args.model.read_bytes()).hexdigest()
    maximum_error = max(
        parity["max_abs_logit_error"] for parity in parities
    )
    visible_times = [
        event["at_ms"] for event in stream["content_events"]
    ]
    monotonic = all(
        current > previous
        for previous, current in zip(visible_times, visible_times[1:])
    )
    health_model = stream["worker_health"]["body"]["model"]
    non_stream_body = non_stream["body"]

    assertions = [
        assertion(
            "checkpoint checksum matches metadata",
            actual_digest == metadata["sha256"],
            actual_digest,
        ),
        assertion(
            "checkpoint regeneration metadata is v1 FP32",
            metadata["format"] == "inferlab-tiny-fp32"
            and metadata["version"] == 1
            and metadata["architecture"]["dtype"] == "float32",
            {
                "format": metadata["format"],
                "version": metadata["version"],
                "dtype": metadata["architecture"]["dtype"],
            },
        ),
        assertion(
            "checkpoint contains one decoder layer",
            metadata["architecture"]["layers"] == 1,
            metadata["architecture"]["layers"],
        ),
        assertion(
            "three prompts were independently compared",
            len(parities) == 3,
            len(parities),
        ),
        assertion(
            "all PyTorch parity reports pass",
            all(parity["passed"] for parity in parities),
            [parity["passed"] for parity in parities],
        ),
        assertion(
            "greedy token IDs match PyTorch",
            all(parity["token_ids_match"] for parity in parities),
            [parity["token_ids_match"] for parity in parities],
        ),
        assertion(
            "tokenizer outputs match PyTorch",
            all(parity["prompt_tokens_match"] for parity in parities),
            [parity["prompt_tokens_match"] for parity in parities],
        ),
        assertion(
            "maximum absolute logit error is within 1e-4",
            maximum_error <= 1.0e-4,
            maximum_error,
        ),
        assertion(
            "all prompts produce the expected readable sentence",
            all(
                parity["generated_text"] == EXPECTED_TEXT
                for parity in parities
            ),
            [parity["generated_text"] for parity in parities],
        ),
        assertion(
            "worker health exposes the loaded architecture",
            health_model["vocabulary"] == 16
            and health_model["dimension"] == 16
            and health_model["heads"] == 4,
            health_model,
        ),
        assertion(
            "gateway stream succeeds through the C++ worker",
            stream["status"] == 200
            and stream["headers"].get("x-inferlab-worker") == "cpu-worker-a",
            {
                "status": stream["status"],
                "worker": stream["headers"].get("x-inferlab-worker"),
            },
        ),
        assertion(
            "stream emits seven visible model tokens",
            len(stream["content_events"]) == 7,
            len(stream["content_events"]),
        ),
        assertion(
            "streamed pieces reconstruct the expected text",
            stream["content"] == EXPECTED_TEXT,
            stream["content"],
        ),
        assertion(
            "stream ends with stop and DONE",
            stream["finish_reason"] == "stop" and stream["done_received"],
            {
                "finish_reason": stream["finish_reason"],
                "done_received": stream["done_received"],
            },
        ),
        assertion(
            "token timestamps are strictly increasing",
            monotonic,
            visible_times,
        ),
        assertion(
            "stream spans at least 50 ms under injected pacing",
            stream["stream_span_ms"] >= 50.0,
            stream["stream_span_ms"],
        ),
        assertion(
            "non-streaming gateway response uses the same real tokens",
            non_stream["status"] == 200
            and non_stream_body["choices"][0]["message"]["content"]
            == EXPECTED_TEXT
            and non_stream_body["usage"]["completion_tokens"] == 7,
            {
                "status": non_stream["status"],
                "content": non_stream_body["choices"][0]["message"]["content"],
                "completion_tokens": non_stream_body["usage"][
                    "completion_tokens"
                ],
            },
        ),
        assertion(
            "evidence environment used the committed checkpoint",
            environment["model_sha256"] == actual_digest,
            environment["model_sha256"],
        ),
    ]
    passed_count = sum(item["passed"] for item in assertions)
    result = {
        "passed": passed_count == len(assertions),
        "assertions_passed": passed_count,
        "assertions_total": len(assertions),
        "maximum_abs_logit_error": maximum_error,
        "prompts": [parity["prompt"] for parity in parities],
        "stream_span_ms": stream["stream_span_ms"],
        "assertions": assertions,
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    for item in assertions:
        print(f"{'PASS' if item['passed'] else 'FAIL'} {item['name']}")
    print(f"{passed_count}/{len(assertions)} assertions passed")
    if not result["passed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
