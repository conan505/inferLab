#!/usr/bin/env python3
"""Generate an independent v0.32 checkpoint inventory from exact local bytes.

This standard-library-only generator parses the pinned safetensors header and
streams the tensor payload for its full digest and finite-F16 check.  It does
not import a model framework, execute checkpoint code, retain weight bytes, or
perform any network operation.
"""

from __future__ import annotations

import argparse
import errno
import hashlib
import os
import stat
import struct
import sys
import tempfile
from pathlib import Path
from typing import Any, NoReturn

from check_public_tokenizer_reference import MAX_LOCK_BYTES, read_bounded_regular
from public_checkpoint_reference_v032 import (
    CHECKPOINT_FILE,
    CHECKPOINT_FILE_BYTES,
    CHECKPOINT_FILE_SHA256,
    CHECKPOINT_REFERENCE_SCHEMA,
    HEADER_PREFIX_BYTES,
    expected_checkpoint_summary,
    expected_reference_metadata,
    expected_scope,
    expected_source,
    expected_tensor_shapes,
    reject_non_finite,
    tensor_elements,
    validate_checkpoint_reference,
)
from public_tokenizer_reference_v032 import (
    CANONICAL_ARCHITECTURE,
    CANONICAL_CHECKPOINT,
    LOCKED_FILES,
    MILESTONE,
    ReferenceValidationError,
    canonical_json_bytes,
    strict_json_loads,
    validate_lock,
)


CHUNK_BYTES = 1024 * 1024


def fail(message: str) -> NoReturn:
    raise ReferenceValidationError(message)


def opened_directory(path: Path) -> tuple[int, os.stat_result]:
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0)
    flags |= getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
    flags |= getattr(os, "O_NONBLOCK", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        fail(f"asset directory cannot be opened safely: {error}")
    metadata = os.fstat(descriptor)
    if not stat.S_ISDIR(metadata.st_mode):
        os.close(descriptor)
        fail("asset directory is not a directory")
    return descriptor, metadata


def opened_checkpoint(directory_descriptor: int) -> tuple[int, os.stat_result]:
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0)
    flags |= getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
    try:
        descriptor = os.open(
            CHECKPOINT_FILE,
            flags,
            dir_fd=directory_descriptor,
        )
    except OSError as error:
        if error.errno == errno.ELOOP:
            fail("checkpoint source is a symlink")
        fail(f"checkpoint source cannot be opened safely: {error}")
    metadata = os.fstat(descriptor)
    if not stat.S_ISREG(metadata.st_mode):
        os.close(descriptor)
        fail("checkpoint source is not a regular file")
    if metadata.st_size != CHECKPOINT_FILE_BYTES:
        os.close(descriptor)
        fail("checkpoint source size drifted")
    return descriptor, metadata


def read_exact(descriptor: int, byte_count: int, label: str) -> bytes:
    chunks: list[bytes] = []
    remaining = byte_count
    while remaining:
        chunk = os.read(descriptor, min(remaining, CHUNK_BYTES))
        if not chunk:
            fail(f"{label} became short while read")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def f16_chunk_is_finite(data: bytes) -> bool:
    if len(data) % 2:
        fail("checkpoint F16 payload has an odd byte count")
    # Safetensors numbers are little-endian.  The high byte's low seven bits
    # contain the five-bit exponent plus the upper two mantissa bits.
    return not any((data[index] & 0x7C) == 0x7C for index in range(1, len(data), 2))


def read_checkpoint(
    asset_directory: Path,
) -> tuple[bytes, dict[str, Any], str, bool]:
    directory_descriptor, directory_before = opened_directory(asset_directory)
    try:
        expected_inventory = sorted(name for name, _size, _digest in LOCKED_FILES)
        try:
            observed_inventory = sorted(os.listdir(directory_descriptor))
        except OSError as error:
            fail(f"asset directory cannot be listed safely: {error}")
        if observed_inventory != expected_inventory:
            fail("asset directory inventory differs from the exact six-file lock")

        descriptor, file_before = opened_checkpoint(directory_descriptor)
        try:
            prefix = read_exact(descriptor, HEADER_PREFIX_BYTES, "checkpoint prefix")
            header_bytes = struct.unpack("<Q", prefix)[0]
            if header_bytes != CANONICAL_CHECKPOINT["header_bytes"]:
                fail("checkpoint header length drifted")
            header = read_exact(descriptor, header_bytes, "checkpoint header")
            if hashlib.sha256(header).hexdigest() != CANONICAL_CHECKPOINT["header_sha256"]:
                fail("checkpoint header SHA-256 drifted")

            digest = hashlib.sha256(prefix + header)
            remaining = CANONICAL_CHECKPOINT["data_bytes"]
            finite_payload = True
            while remaining:
                chunk = os.read(descriptor, min(remaining, CHUNK_BYTES))
                if not chunk:
                    fail("checkpoint tensor data became short while read")
                digest.update(chunk)
                finite_payload = finite_payload and f16_chunk_is_finite(chunk)
                remaining -= len(chunk)
            if os.read(descriptor, 1):
                fail("checkpoint source exceeds its exact byte bound")
            file_after = os.fstat(descriptor)
            identity_before = (
                file_before.st_dev,
                file_before.st_ino,
                file_before.st_size,
                file_before.st_mtime_ns,
                file_before.st_ctime_ns,
            )
            identity_after = (
                file_after.st_dev,
                file_after.st_ino,
                file_after.st_size,
                file_after.st_mtime_ns,
                file_after.st_ctime_ns,
            )
            if identity_before != identity_after:
                fail("checkpoint opened identity changed while read")
        finally:
            os.close(descriptor)

        directory_after = os.fstat(directory_descriptor)
        if (
            directory_before.st_dev,
            directory_before.st_ino,
            directory_before.st_mtime_ns,
            directory_before.st_ctime_ns,
        ) != (
            directory_after.st_dev,
            directory_after.st_ino,
            directory_after.st_mtime_ns,
            directory_after.st_ctime_ns,
        ):
            fail("asset directory changed while checkpoint was read")
        try:
            path_after = os.stat(asset_directory, follow_symlinks=False)
        except OSError as error:
            fail(f"asset directory path changed while checkpoint was read: {error}")
        if (path_after.st_dev, path_after.st_ino) != (
            directory_before.st_dev,
            directory_before.st_ino,
        ):
            fail("asset directory path identity changed while checkpoint was read")
    finally:
        os.close(directory_descriptor)

    full_sha256 = digest.hexdigest()
    if full_sha256 != CHECKPOINT_FILE_SHA256:
        fail("checkpoint file SHA-256 drifted")
    if not finite_payload:
        fail("checkpoint contains a non-finite F16 payload value")
    header_document = strict_json_loads(header, label="safetensors header")
    if not isinstance(header_document, dict):
        fail("safetensors header root must be an object")
    return header, header_document, full_sha256, finite_payload


def parse_tensors(header: dict[str, Any]) -> list[dict[str, Any]]:
    metadata = header.get("__metadata__")
    if metadata != {"format": "pt"}:
        fail("safetensors metadata drifted")
    entries = {name: value for name, value in header.items() if name != "__metadata__"}
    expected_shapes = expected_tensor_shapes()
    if set(entries) != set(expected_shapes):
        fail("safetensors tensor names drifted")

    tensors: list[dict[str, Any]] = []
    for name in sorted(entries):
        value = entries[name]
        if not isinstance(value, dict) or set(value) != {"dtype", "shape", "data_offsets"}:
            fail(f"safetensors tensor entry drifted for {name}")
        shape = value["shape"]
        offsets = value["data_offsets"]
        if value["dtype"] != "F16" or shape != expected_shapes[name]:
            fail(f"safetensors tensor type or shape drifted for {name}")
        elements = tensor_elements(shape)
        if (
            not isinstance(offsets, list)
            or len(offsets) != 2
            or any(type(offset) is not int or offset < 0 for offset in offsets)
            or offsets[1] - offsets[0] != elements * 2
        ):
            fail(f"safetensors tensor offsets drifted for {name}")
        tensors.append(
            {
                "name": name,
                "dtype": "F16",
                "shape": list(shape),
                "data_offsets": list(offsets),
                "elements": elements,
                "bytes": elements * 2,
            }
        )
    return tensors


def generate(lock_path: Path, asset_directory: Path) -> dict[str, Any]:
    lock_bytes = read_bounded_regular(
        lock_path,
        maximum_bytes=MAX_LOCK_BYTES,
        label="source lock",
    )
    lock = strict_json_loads(lock_bytes, label="source lock")
    validate_lock(lock)
    lock_sha256 = hashlib.sha256(lock_bytes).hexdigest()

    _header, header_document, _file_sha256, finite_payload = read_checkpoint(
        asset_directory
    )
    tensors = parse_tensors(header_document)
    inventory_sha256 = hashlib.sha256(canonical_json_bytes(tensors)).hexdigest()
    document = {
        "schema": CHECKPOINT_REFERENCE_SCHEMA,
        "milestone": MILESTONE,
        "scope": expected_scope(),
        "source": expected_source(lock_sha256),
        "reference": expected_reference_metadata(),
        "architecture": CANONICAL_ARCHITECTURE,
        "checkpoint": {
            **expected_checkpoint_summary(inventory_sha256),
            "finite_payload": finite_payload,
        },
        "tensors": tensors,
    }
    reject_non_finite(document)
    validate_checkpoint_reference(document, lock_sha256=lock_sha256)
    return document


def atomic_write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{path.name}.",
        suffix=".tmp",
        dir=path.parent,
    )
    temporary_path = Path(temporary_name)
    try:
        os.fchmod(descriptor, 0o600)
        with os.fdopen(descriptor, "wb", closefd=True) as output:
            output.write(data)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary_path, path)
        directory_descriptor = os.open(
            path.parent,
            os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_CLOEXEC", 0),
        )
        try:
            os.fsync(directory_descriptor)
        finally:
            os.close(directory_descriptor)
    except BaseException:
        try:
            os.close(descriptor)
        except OSError:
            pass
        try:
            temporary_path.unlink()
        except FileNotFoundError:
            pass
        raise


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lock", type=Path, required=True)
    parser.add_argument("--assets", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        document = generate(args.lock, args.assets)
        atomic_write(args.output, canonical_json_bytes(document))
    except (OSError, ReferenceValidationError) as error:
        print(f"checkpoint reference generation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
