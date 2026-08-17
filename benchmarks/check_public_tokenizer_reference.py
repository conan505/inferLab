#!/usr/bin/env python3
"""Dependency-free validator for the pinned v0.32 tokenizer oracle corpus."""

from __future__ import annotations

import argparse
import hashlib
import os
import stat
import sys
from pathlib import Path
from typing import Any

from public_tokenizer_reference_v032 import (
    CHECK_SCHEMA,
    LOCK_SCHEMA,
    REFERENCE_SCHEMA,
    ReferenceValidationError,
    canonical_json_bytes,
    strict_json_loads,
    validate_lock,
    validate_reference,
)


MAX_LOCK_BYTES = 64 * 1024
MAX_REFERENCE_BYTES = 256 * 1024


def read_bounded_regular(path: Path, *, maximum_bytes: int, label: str) -> bytes:
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    flags |= getattr(os, "O_NONBLOCK", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise ReferenceValidationError(f"{label}: cannot open safely: {error}") from error
    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode):
            raise ReferenceValidationError(f"{label}: source is not a regular file")
        if not 0 < before.st_size <= maximum_bytes:
            raise ReferenceValidationError(
                f"{label}: size is outside the 1..{maximum_bytes} byte bound"
            )
        chunks: list[bytes] = []
        remaining = before.st_size
        while remaining:
            chunk = os.read(descriptor, min(remaining, 64 * 1024))
            if not chunk:
                raise ReferenceValidationError(f"{label}: source became short")
            chunks.append(chunk)
            remaining -= len(chunk)
        if os.read(descriptor, 1):
            raise ReferenceValidationError(f"{label}: source exceeds its byte bound")
        after = os.fstat(descriptor)
        identity_before = (
            before.st_dev,
            before.st_ino,
            before.st_size,
            before.st_mtime_ns,
            before.st_ctime_ns,
        )
        identity_after = (
            after.st_dev,
            after.st_ino,
            after.st_size,
            after.st_mtime_ns,
            after.st_ctime_ns,
        )
        if identity_before != identity_after:
            raise ReferenceValidationError(f"{label}: source changed while read")
        return b"".join(chunks)
    finally:
        os.close(descriptor)


def check_reference(reference_path: Path, lock_path: Path) -> dict[str, Any]:
    lock_bytes = read_bounded_regular(
        lock_path,
        maximum_bytes=MAX_LOCK_BYTES,
        label="source lock",
    )
    lock = strict_json_loads(lock_bytes, label="source lock")
    validate_lock(lock)
    lock_sha256 = hashlib.sha256(lock_bytes).hexdigest()

    reference_bytes = read_bounded_regular(
        reference_path,
        maximum_bytes=MAX_REFERENCE_BYTES,
        label="tokenizer reference",
    )
    reference = strict_json_loads(reference_bytes, label="tokenizer reference")
    if canonical_json_bytes(reference) != reference_bytes:
        raise ReferenceValidationError("tokenizer reference is not canonical JSON")
    counts = validate_reference(reference, lock_sha256=lock_sha256)

    return {
        "schema": CHECK_SCHEMA,
        "status": "passed",
        "validated_reference_schema": REFERENCE_SCHEMA,
        "validated_lock_schema": LOCK_SCHEMA,
        "reference_bytes": len(reference_bytes),
        "reference_sha256": hashlib.sha256(reference_bytes).hexdigest(),
        "corpus": counts,
        "scope": {
            "public_model_forward_passes": 0,
            "public_model_generations": 0,
            "public_model_services_started": 0,
            "retained_weight_bytes": 0,
        },
    }


def parse_args() -> argparse.Namespace:
    project_root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--reference",
        type=Path,
        required=True,
    )
    parser.add_argument(
        "--lock",
        type=Path,
        default=project_root / "models" / "public" / "pythia-14m-v0.32.lock.json",
    )
    parser.add_argument("--output", type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        result = check_reference(args.reference, args.lock)
        rendered = canonical_json_bytes(result)
        if args.output is None:
            sys.stdout.buffer.write(rendered)
        else:
            args.output.write_bytes(rendered)
    except (OSError, ReferenceValidationError) as error:
        print(f"tokenizer reference check failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
