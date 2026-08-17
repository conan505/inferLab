#!/usr/bin/env python3
"""Capture v0.32 production-tokenizer parity through the offline Rust CLI."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any, NoReturn

from check_public_tokenizer_reference import (
    MAX_LOCK_BYTES,
    MAX_REFERENCE_BYTES,
    read_bounded_regular,
)
from generate_public_tokenizer_reference import atomic_write
from public_tokenizer_reference_v032 import (
    ENCODE_CASES,
    REFERENCE_SCHEMA,
    ReferenceValidationError,
    canonical_json_bytes,
    decoded_descriptor,
    exact_json_equal,
    expand_ids,
    expand_text,
    ids_descriptor,
    strict_json_loads,
    validate_lock,
    validate_reference,
)


REQUEST_SCHEMA = "inferlab.tokenizer.request.v1"
RESPONSE_SCHEMA = "inferlab.tokenizer.response.v1"
RESULT_SCHEMA = "inferlab.public-tokenizer-production-results.v1"
MAX_REQUEST_BYTES = 1024 * 1024
MAX_STDOUT_BYTES = 2 * 1024 * 1024
MAX_STDERR_BYTES = 64 * 1024
OFFLINE_ENVIRONMENT = {
    "hf_hub_offline": True,
    "hf_datasets_offline": True,
    "transformers_offline": True,
}
PROBE_SCOPE = {
    "public_model_forward_passes": 0,
    "public_model_generations": 0,
    "public_model_services_started": 0,
    "retained_weight_bytes": 0,
}
REQUEST_REJECTIONS = (
    ("request-invalid-utf8", "request_invalid_utf8"),
    ("request-duplicate-key", "request_invalid"),
    ("request-unknown-field", "request_invalid"),
    ("request-schema-mismatch", "request_schema_mismatch"),
    ("request-trailing-json", "request_invalid"),
    ("request-oversize", "request_oversize"),
)


class ProbeError(RuntimeError):
    """A finite production-tokenizer proof capture failure."""


def fail(message: str) -> NoReturn:
    raise ProbeError(message)


def strict_object(value: Any, keys: set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{label} must be an object")
    actual = set(value)
    if actual != keys:
        fail(
            f"{label} keys drifted: missing={sorted(keys - actual)}, "
            f"extra={sorted(actual - keys)}"
        )
    return value


class OfflineTokenizerCli:
    def __init__(
        self,
        binary: Path,
        lock: Path,
        assets: Path,
        timeout_seconds: float,
    ) -> None:
        self.command = [
            os.fspath(binary),
            "tokenize",
            "--lock",
            os.fspath(lock),
            "--assets",
            os.fspath(assets),
        ]
        self.timeout_seconds = timeout_seconds
        self.environment = os.environ.copy()
        self.environment.update(
            {
                "HF_HUB_OFFLINE": "1",
                "HF_DATASETS_OFFLINE": "1",
                "TRANSFORMERS_OFFLINE": "1",
            }
        )

    def invoke(self, request: bytes) -> subprocess.CompletedProcess[bytes]:
        try:
            result = subprocess.run(
                self.command,
                input=request,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
                timeout=self.timeout_seconds,
                env=self.environment,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            fail(f"offline tokenizer CLI did not finish safely: {type(error).__name__}")
        if len(result.stdout) > MAX_STDOUT_BYTES:
            fail("offline tokenizer CLI stdout exceeded its proof bound")
        if len(result.stderr) > MAX_STDERR_BYTES:
            fail("offline tokenizer CLI stderr exceeded its proof bound")
        return result

    def success(self, request: dict[str, Any], operation: str) -> dict[str, Any]:
        result = self.invoke(canonical_json_bytes(request))
        if result.returncode != 0 or result.stderr:
            fail(f"{operation} unexpectedly failed with exit {result.returncode}")
        response = strict_json_loads(result.stdout, label=f"{operation} response")
        if operation == "encode":
            keys = {"schema", "operation", "token_count", "ids"}
        elif operation == "decode":
            keys = {"schema", "operation", "token_count", "text"}
        else:  # pragma: no cover - internal caller contract
            fail(f"unsupported operation {operation!r}")
        response = strict_object(response, keys, f"{operation} response")
        if response["schema"] != RESPONSE_SCHEMA or response["operation"] != operation:
            fail(f"{operation} response identity drifted")
        token_count = response["token_count"]
        if not isinstance(token_count, int) or isinstance(token_count, bool) or token_count < 0:
            fail(f"{operation} response token_count is invalid")
        return response

    def operation_failure(
        self,
        request: dict[str, Any],
        expected_error_kind: str,
    ) -> None:
        result = self.invoke(canonical_json_bytes(request))
        expected = f"production tokenizer failed: {expected_error_kind}\n".encode("ascii")
        if result.returncode != 1 or result.stdout or result.stderr != expected:
            fail(f"operation rejection drifted for {expected_error_kind}")

    def request_failure(self, request: bytes, expected_error_kind: str) -> None:
        result = self.invoke(request)
        expected = f"tokenizer request failed: {expected_error_kind}\n".encode("ascii")
        if result.returncode != 2 or result.stdout or result.stderr != expected:
            fail(f"request rejection drifted for {expected_error_kind}")


def validate_encode_response(response: dict[str, Any], expected: dict[str, Any]) -> list[int]:
    ids = response["ids"]
    if not isinstance(ids, list):
        fail("encode response IDs must be an array")
    for token_id in ids:
        if (
            not isinstance(token_id, int)
            or isinstance(token_id, bool)
            or not 0 <= token_id <= 0xFFFFFFFF
        ):
            fail("encode response contains an invalid token ID")
    if response["token_count"] != len(ids):
        fail("encode response token_count differs from IDs")
    if ids != expand_ids(expected):
        fail("production encode IDs differ from the maintained reference")
    return ids


def validate_decode_response(
    response: dict[str, Any],
    ids: list[int],
    expected: dict[str, Any],
) -> str:
    text = response["text"]
    if not isinstance(text, str):
        fail("decode response text must be a string")
    if response["token_count"] != len(ids):
        fail("decode response token_count differs from request IDs")
    if text != expand_text(expected):
        fail("production decoded text differs from the maintained reference")
    return text


def encode_request(case: dict[str, Any], text: str) -> dict[str, Any]:
    return {
        "schema": REQUEST_SCHEMA,
        "operation": "encode",
        "text": text,
        "literal_specials": case["special_token_policy"],
        "add_special_tokens": case["add_special_tokens"],
    }


def decode_request(ids: list[int], configured_specials: str) -> dict[str, Any]:
    return {
        "schema": REQUEST_SCHEMA,
        "operation": "decode",
        "ids": ids,
        "configured_specials": configured_specials,
    }


def capture_encode_cases(
    cli: OfflineTokenizerCli,
    reference: dict[str, Any],
) -> list[dict[str, Any]]:
    captured: list[dict[str, Any]] = []
    for spec, case in zip(ENCODE_CASES, reference["encode_cases"]):
        text = expand_text(case["input"])
        request = encode_request(case, text)
        production = case["production"]
        if production["outcome"] == "reject":
            error_kind = production["error_kind"]
            cli.operation_failure(request, error_kind)
            captured.append(
                {
                    "name": spec.name,
                    "input": case["input"],
                    "literal_specials": case["special_token_policy"],
                    "add_special_tokens": case["add_special_tokens"],
                    "result": {"outcome": "reject", "error_kind": error_kind},
                }
            )
            continue

        response = cli.success(request, "encode")
        encoding = case["reference_encoding"]
        ids = validate_encode_response(response, encoding["ids"])
        decoded: dict[str, Any] = {}
        for mode, expected_key in (
            ("preserve_configured", "decoded_with_special_tokens"),
            ("skip_configured", "decoded_without_special_tokens"),
        ):
            decode_response = cli.success(decode_request(ids, mode), "decode")
            text_result = validate_decode_response(
                decode_response,
                ids,
                encoding[expected_key],
            )
            decoded[mode] = decoded_descriptor(text_result, case["input"])
        captured.append(
            {
                "name": spec.name,
                "input": case["input"],
                "literal_specials": case["special_token_policy"],
                "add_special_tokens": case["add_special_tokens"],
                "result": {
                    "outcome": "accept",
                    "ids": ids_descriptor(ids),
                    "decoded": decoded,
                },
            }
        )
    return captured


def capture_decode_cases(
    cli: OfflineTokenizerCli,
    reference: dict[str, Any],
) -> list[dict[str, Any]]:
    captured: list[dict[str, Any]] = []
    for case in reference["decode_cases"]:
        ids = case["ids"]
        mode = "skip_configured" if case["skip_special_tokens"] else "preserve_configured"
        response = cli.success(decode_request(ids, mode), "decode")
        text = validate_decode_response(response, ids, case["reference_decoded"])
        captured.append(
            {
                "name": case["name"],
                "ids": ids,
                "configured_specials": mode,
                "result": {
                    "outcome": "accept",
                    "text": decoded_descriptor(text),
                },
            }
        )
    return captured


def capture_decode_rejections(
    cli: OfflineTokenizerCli,
    reference: dict[str, Any],
) -> list[dict[str, Any]]:
    captured: list[dict[str, Any]] = []
    for case in reference["decode_rejections"]:
        request = decode_request(case["ids"], "preserve_configured")
        error_kind = case["expected_error_kind"]
        cli.operation_failure(request, error_kind)
        captured.append(
            {
                "name": case["name"],
                "ids": case["ids"],
                "configured_specials": "preserve_configured",
                "result": {"outcome": "reject", "error_kind": error_kind},
            }
        )
    return captured


def capture_request_rejections(cli: OfflineTokenizerCli) -> list[dict[str, str]]:
    duplicate = (
        '{"schema":"inferlab.tokenizer.request.v1","operation":"encode",'
        '"text":"not-retained","text":"duplicate-not-retained",'
        '"literal_specials":"encode_as_text","add_special_tokens":false}'
    ).encode("utf-8")
    unknown = canonical_json_bytes(
        {
            "schema": REQUEST_SCHEMA,
            "operation": "decode",
            "ids": [],
            "configured_specials": "preserve_configured",
            "unknown": True,
        }
    )
    wrong_schema = canonical_json_bytes(
        {
            "schema": "inferlab.tokenizer.request.v0",
            "operation": "decode",
            "ids": [],
            "configured_specials": "preserve_configured",
        }
    )
    valid = canonical_json_bytes(
        {
            "schema": REQUEST_SCHEMA,
            "operation": "decode",
            "ids": [],
            "configured_specials": "preserve_configured",
        }
    )
    payloads = (
        b"\xff",
        duplicate,
        unknown,
        wrong_schema,
        valid + b"{}",
        b" " * (MAX_REQUEST_BYTES + 1),
    )
    captured: list[dict[str, str]] = []
    for (name, error_kind), request in zip(REQUEST_REJECTIONS, payloads):
        cli.request_failure(request, error_kind)
        captured.append(
            {
                "name": name,
                "result": "reject",
                "error_kind": error_kind,
                "payload_retained": "no",
            }
        )
    return captured


def expected_result(reference: dict[str, Any]) -> dict[str, Any]:
    encode_cases: list[dict[str, Any]] = []
    for spec, case in zip(ENCODE_CASES, reference["encode_cases"]):
        production = case["production"]
        if production["outcome"] == "reject":
            result = {
                "outcome": "reject",
                "error_kind": production["error_kind"],
            }
        else:
            encoding = case["reference_encoding"]
            result = {
                "outcome": "accept",
                "ids": encoding["ids"],
                "decoded": {
                    "preserve_configured": encoding["decoded_with_special_tokens"],
                    "skip_configured": encoding["decoded_without_special_tokens"],
                },
            }
        encode_cases.append(
            {
                "name": spec.name,
                "input": case["input"],
                "literal_specials": case["special_token_policy"],
                "add_special_tokens": case["add_special_tokens"],
                "result": result,
            }
        )

    decode_cases = [
        {
            "name": case["name"],
            "ids": case["ids"],
            "configured_specials": (
                "skip_configured"
                if case["skip_special_tokens"]
                else "preserve_configured"
            ),
            "result": {
                "outcome": "accept",
                "text": case["reference_decoded"],
            },
        }
        for case in reference["decode_cases"]
    ]
    decode_rejections = [
        {
            "name": case["name"],
            "ids": case["ids"],
            "configured_specials": "preserve_configured",
            "result": {
                "outcome": "reject",
                "error_kind": case["expected_error_kind"],
            },
        }
        for case in reference["decode_rejections"]
    ]
    request_rejections = [
        {
            "name": name,
            "result": "reject",
            "error_kind": error_kind,
            "payload_retained": "no",
        }
        for name, error_kind in REQUEST_REJECTIONS
    ]
    reference_bytes = canonical_json_bytes(reference)
    return {
        "schema": RESULT_SCHEMA,
        "reference_schema": REFERENCE_SCHEMA,
        "reference_sha256": hashlib.sha256(reference_bytes).hexdigest(),
        "request_schema": REQUEST_SCHEMA,
        "response_schema": RESPONSE_SCHEMA,
        "offline_environment": OFFLINE_ENVIRONMENT,
        "scope": PROBE_SCOPE,
        "encode_cases": encode_cases,
        "decode_cases": decode_cases,
        "decode_rejections": decode_rejections,
        "request_rejections": request_rejections,
    }


def validate_result(
    result: Any,
    *,
    reference: dict[str, Any],
) -> dict[str, int]:
    if not exact_json_equal(result, expected_result(reference)):
        fail("retained production tokenizer result differs from the exact oracle")
    return {
        "encode_cases": len(result["encode_cases"]),
        "decode_cases": len(result["decode_cases"]),
        "decode_rejections": len(result["decode_rejections"]),
        "request_rejections": len(result["request_rejections"]),
    }


def capture(args: argparse.Namespace) -> dict[str, Any]:
    lock_bytes = read_bounded_regular(
        args.lock,
        maximum_bytes=MAX_LOCK_BYTES,
        label="source lock",
    )
    lock = strict_json_loads(lock_bytes, label="source lock")
    validate_lock(lock)
    lock_sha256 = hashlib.sha256(lock_bytes).hexdigest()

    reference_bytes = read_bounded_regular(
        args.reference,
        maximum_bytes=MAX_REFERENCE_BYTES,
        label="tokenizer reference",
    )
    reference = strict_json_loads(reference_bytes, label="tokenizer reference")
    if not isinstance(reference, dict):
        fail("tokenizer reference root must be an object")
    if canonical_json_bytes(reference) != reference_bytes:
        fail("tokenizer reference is not canonical JSON")
    validate_reference(reference, lock_sha256=lock_sha256)

    cli = OfflineTokenizerCli(args.binary, args.lock, args.assets, args.timeout_seconds)
    result = {
        "schema": RESULT_SCHEMA,
        "reference_schema": REFERENCE_SCHEMA,
        "reference_sha256": hashlib.sha256(reference_bytes).hexdigest(),
        "request_schema": REQUEST_SCHEMA,
        "response_schema": RESPONSE_SCHEMA,
        "offline_environment": OFFLINE_ENVIRONMENT,
        "scope": PROBE_SCOPE,
        "encode_cases": capture_encode_cases(cli, reference),
        "decode_cases": capture_decode_cases(cli, reference),
        "decode_rejections": capture_decode_rejections(cli, reference),
        "request_rejections": capture_request_rejections(cli),
    }
    validate_result(result, reference=reference)
    return result


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--lock", type=Path, required=True)
    parser.add_argument("--assets", type=Path, required=True)
    parser.add_argument("--reference", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--timeout-seconds", type=float, default=30.0)
    args = parser.parse_args()
    if not 1.0 <= args.timeout_seconds <= 120.0:
        parser.error("--timeout-seconds must be between 1 and 120")
    return args


def main() -> int:
    args = parse_args()
    try:
        result = capture(args)
        atomic_write(args.output, canonical_json_bytes(result))
    except (OSError, ProbeError, ReferenceValidationError) as error:
        print(f"production tokenizer probe failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
