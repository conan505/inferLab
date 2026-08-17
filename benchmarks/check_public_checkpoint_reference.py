#!/usr/bin/env python3
"""Dependency-free validator for the retained v0.32 checkpoint reference."""

from __future__ import annotations

import argparse
import hashlib
import sys
from pathlib import Path
from typing import Any

from check_public_tokenizer_reference import MAX_LOCK_BYTES, read_bounded_regular
from public_checkpoint_reference_v032 import (
    CHECKPOINT_REFERENCE_SCHEMA,
    validate_checkpoint_reference,
)
from public_tokenizer_reference_v032 import (
    LOCK_SCHEMA,
    ReferenceValidationError,
    canonical_json_bytes,
    strict_json_loads,
    validate_lock,
)


CHECK_SCHEMA = "inferlab.public-checkpoint-reference-check.v1"
MAX_CHECKPOINT_REFERENCE_BYTES = 256 * 1024


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
        maximum_bytes=MAX_CHECKPOINT_REFERENCE_BYTES,
        label="checkpoint reference",
    )
    reference = strict_json_loads(reference_bytes, label="checkpoint reference")
    if canonical_json_bytes(reference) != reference_bytes:
        raise ReferenceValidationError("checkpoint reference is not canonical JSON")
    counts = validate_checkpoint_reference(reference, lock_sha256=lock_sha256)
    return {
        "schema": CHECK_SCHEMA,
        "status": "passed",
        "validated_reference_schema": CHECKPOINT_REFERENCE_SCHEMA,
        "validated_lock_schema": LOCK_SCHEMA,
        "reference_bytes": len(reference_bytes),
        "reference_sha256": hashlib.sha256(reference_bytes).hexdigest(),
        "inventory": counts,
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
    parser.add_argument("--reference", type=Path, required=True)
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
        encoded = canonical_json_bytes(result)
        if args.output is None:
            sys.stdout.buffer.write(encoded)
        else:
            args.output.write_bytes(encoded)
    except (OSError, ReferenceValidationError) as error:
        print(f"checkpoint reference check failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
