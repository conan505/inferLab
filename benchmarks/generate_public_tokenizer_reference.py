#!/usr/bin/env python3
"""Generate the pinned v0.32 tokenizer oracle from verified local bytes.

The generator has no Hub client and performs no network operation.  It opens
only ``tokenizer.json`` and ``tokenizer_config.json`` from an explicit asset
directory, verifies their exact locked lengths and digests, then passes the
same tokenizer bytes to the exact maintained Python reference package pinned
below.  It never opens or retains checkpoint weight bytes.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import stat
import sys
import tempfile
import unicodedata
from pathlib import Path
from typing import Any, NoReturn

from public_tokenizer_reference_v032 import (
    DECODE_CASES,
    ENCODE_CASES,
    LOCKED_FILES,
    MAX_CONTEXT_TOKENS,
    REFERENCE_PACKAGE,
    REFERENCE_SCHEMA,
    REFERENCE_VERSION,
    TOKENIZER_BYTES,
    TOKENIZER_CONFIG_BYTES,
    TOKENIZER_CONFIG_FILE,
    TOKENIZER_CONFIG_SHA256,
    TOKENIZER_FILE,
    TOKENIZER_MAX_ID,
    TOKENIZER_SHA256,
    TOKENIZER_DOMAIN_SIZE,
    ReferenceValidationError,
    canonical_json_bytes,
    decoded_descriptor,
    expected_decode_cases,
    expected_decode_rejections,
    expected_decode_without_special_tokens,
    expected_production,
    expected_reference_metadata,
    expected_relationships,
    expected_scope,
    expected_source,
    ids_descriptor,
    materialize_text,
    reject_non_finite,
    strict_json_loads,
    text_descriptor,
    validate_lock,
    validate_reference,
)


LOCK_MAX_BYTES = 64 * 1024


def fail(message: str) -> NoReturn:
    raise ReferenceValidationError(message)


def read_exact_regular_file(path: Path, *, expected_bytes: int, label: str) -> bytes:
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    flags |= getattr(os, "O_NONBLOCK", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        fail(f"{label}: cannot open exact regular file: {error}")
    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode):
            fail(f"{label}: source is not a regular file")
        if before.st_size != expected_bytes:
            fail(f"{label}: expected {expected_bytes} bytes, found {before.st_size}")
        chunks: list[bytes] = []
        remaining = expected_bytes
        while remaining:
            chunk = os.read(descriptor, min(remaining, 1024 * 1024))
            if not chunk:
                fail(f"{label}: file became short while reading")
            chunks.append(chunk)
            remaining -= len(chunk)
        if os.read(descriptor, 1):
            fail(f"{label}: file exceeds its exact byte bound")
        after = os.fstat(descriptor)
        identity_before = (before.st_dev, before.st_ino, before.st_size)
        identity_after = (after.st_dev, after.st_ino, after.st_size)
        if identity_before != identity_after:
            fail(f"{label}: opened identity changed while reading")
        return b"".join(chunks)
    finally:
        os.close(descriptor)


def read_locked_artifact_from_directory(
    asset_directory: Path,
    *,
    name: str,
    expected_bytes: int,
    expected_sha256: str,
) -> bytes:
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    flags |= getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NONBLOCK", 0)
    try:
        directory_descriptor = os.open(asset_directory, flags)
    except OSError as error:
        fail(f"asset directory cannot be opened safely: {error}")
    try:
        directory_before = os.fstat(directory_descriptor)
        if not stat.S_ISDIR(directory_before.st_mode):
            fail("asset directory is not a directory")
        file_flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0)
        file_flags |= getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
        try:
            descriptor = os.open(name, file_flags, dir_fd=directory_descriptor)
        except OSError as error:
            fail(f"{name}: cannot open exact regular file: {error}")
        try:
            before = os.fstat(descriptor)
            if not stat.S_ISREG(before.st_mode):
                fail(f"{name}: source is not a regular file")
            if before.st_size != expected_bytes:
                fail(
                    f"{name}: expected {expected_bytes} bytes, "
                    f"found {before.st_size}"
                )
            chunks: list[bytes] = []
            remaining = expected_bytes
            while remaining:
                chunk = os.read(descriptor, min(remaining, 1024 * 1024))
                if not chunk:
                    fail(f"{name}: file became short while reading")
                chunks.append(chunk)
                remaining -= len(chunk)
            if os.read(descriptor, 1):
                fail(f"{name}: file exceeds its exact byte bound")
            after = os.fstat(descriptor)
            if (
                before.st_dev,
                before.st_ino,
                before.st_size,
            ) != (
                after.st_dev,
                after.st_ino,
                after.st_size,
            ):
                fail(f"{name}: opened identity changed while reading")
            data = b"".join(chunks)
        finally:
            os.close(descriptor)
        directory_after = os.fstat(directory_descriptor)
        if (directory_before.st_dev, directory_before.st_ino) != (
            directory_after.st_dev,
            directory_after.st_ino,
        ):
            fail("asset directory identity changed while reading tokenizer artifacts")
    finally:
        os.close(directory_descriptor)

    digest = hashlib.sha256(data).hexdigest()
    if digest != expected_sha256:
        fail(f"{name}: SHA-256 drifted")
    return data


def validate_raw_tokenizer(document: Any) -> None:
    if not isinstance(document, dict):
        fail("tokenizer root must be an object")
    if document.get("version") != "1.0":
        fail("tokenizer serialization version drifted")
    if document.get("truncation") is not None or document.get("padding") is not None:
        fail("tokenizer unexpectedly configures truncation or padding")
    if document.get("normalizer") != {"type": "NFC"}:
        fail("tokenizer NFC normalizer drifted")
    byte_level = {
        "type": "ByteLevel",
        "add_prefix_space": False,
        "trim_offsets": True,
        "use_regex": True,
    }
    if document.get("pre_tokenizer") != byte_level:
        fail("tokenizer ByteLevel pre-tokenizer drifted")
    if document.get("decoder") != byte_level:
        fail("tokenizer ByteLevel decoder drifted")
    expected_post_processor = {
        "type": "TemplateProcessing",
        "single": [{"Sequence": {"id": "A", "type_id": 0}}],
        "pair": [
            {"Sequence": {"id": "A", "type_id": 0}},
            {"Sequence": {"id": "B", "type_id": 1}},
        ],
        "special_tokens": {},
    }
    if document.get("post_processor") != expected_post_processor:
        fail("tokenizer post-processor drifted")

    model = document.get("model")
    if not isinstance(model, dict) or model.get("type") != "BPE":
        fail("tokenizer BPE model drifted")
    vocabulary = model.get("vocab")
    merges = model.get("merges")
    if not isinstance(vocabulary, dict) or len(vocabulary) != 50_254:
        fail("tokenizer base vocabulary count drifted")
    if not isinstance(merges, list) or len(merges) != 50_009:
        fail("tokenizer merge count drifted")

    added_tokens = document.get("added_tokens")
    if not isinstance(added_tokens, list) or len(added_tokens) != 25:
        fail("tokenizer added-token count drifted")
    expected_specials = {
        0: "<|endoftext|>",
        1: "<|padding|>",
    }
    for token in added_tokens[:2]:
        if not isinstance(token, dict):
            fail("configured special token is not an object")
        token_id = token.get("id")
        if (
            token.get("content") != expected_specials.get(token_id)
            or token.get("special") is not True
        ):
            fail("configured special token drifted")
    expected_spaces = list(range(24, 1, -1))
    for index, (token, spaces) in enumerate(zip(added_tokens[2:], expected_spaces)):
        expected_id = 50_254 + index
        if not isinstance(token, dict):
            fail("configured multi-space token is not an object")
        if token.get("id") != expected_id or token.get("content") != " " * spaces:
            fail("configured multi-space token drifted")
        if token.get("special") is not False or token.get("normalized") is not True:
            fail("configured multi-space token flags drifted")


def validate_raw_tokenizer_config(document: Any) -> None:
    if not isinstance(document, dict):
        fail("tokenizer_config.json root must be an object")
    expected = {
        "add_bos_token": False,
        "add_eos_token": False,
        "add_prefix_space": False,
        "bos_token": "<|endoftext|>",
        "eos_token": "<|endoftext|>",
        "unk_token": "<|endoftext|>",
        "pad_token": None,
        "clean_up_tokenization_spaces": True,
        "tokenizer_class": "GPTNeoXTokenizer",
    }
    for key, expected_value in expected.items():
        observed = document.get(key)
        if type(observed) is not type(expected_value) or observed != expected_value:
            fail(f"tokenizer_config.json value drifted for {key}")


def load_reference_tokenizer(tokenizer_bytes: bytes) -> Any:
    try:
        import tokenizers
        from tokenizers import Tokenizer
    except ImportError as error:
        fail(f"install exact reference package {REFERENCE_PACKAGE}=={REFERENCE_VERSION}: {error}")
    if tokenizers.__version__ != REFERENCE_VERSION:
        fail(
            f"reference package drifted: expected {REFERENCE_PACKAGE}=={REFERENCE_VERSION}, "
            f"found {tokenizers.__version__}"
        )
    try:
        tokenizer_text = tokenizer_bytes.decode("utf-8", errors="strict")
        tokenizer = Tokenizer.from_str(tokenizer_text)
    except Exception as error:
        fail(f"maintained tokenizer reference rejected exact tokenizer bytes: {error}")
    if tokenizer.get_vocab_size(with_added_tokens=False) != 50_254:
        fail("reference base vocabulary count drifted")
    if tokenizer.get_vocab_size(with_added_tokens=True) != TOKENIZER_DOMAIN_SIZE:
        fail("reference contiguous decodable token-ID domain size drifted")
    return tokenizer


def generate(lock_path: Path, asset_directory: Path) -> dict[str, Any]:
    try:
        lock_metadata = os.lstat(lock_path)
    except OSError as error:
        fail(f"source lock cannot be inspected safely: {error}")
    if not stat.S_ISREG(lock_metadata.st_mode):
        fail("source lock is not a regular file")
    if not 0 < lock_metadata.st_size <= LOCK_MAX_BYTES:
        fail(f"source lock size is outside the 1..{LOCK_MAX_BYTES} byte bound")
    lock_bytes = read_exact_regular_file(
        lock_path,
        expected_bytes=lock_metadata.st_size,
        label="source lock",
    )
    lock = strict_json_loads(lock_bytes, label="source lock")
    validate_lock(lock)
    if len(LOCKED_FILES) != 6:
        fail("internal locked-file inventory drifted")

    tokenizer_bytes = read_locked_artifact_from_directory(
        asset_directory,
        name=TOKENIZER_FILE,
        expected_bytes=TOKENIZER_BYTES,
        expected_sha256=TOKENIZER_SHA256,
    )
    tokenizer_config_bytes = read_locked_artifact_from_directory(
        asset_directory,
        name=TOKENIZER_CONFIG_FILE,
        expected_bytes=TOKENIZER_CONFIG_BYTES,
        expected_sha256=TOKENIZER_CONFIG_SHA256,
    )
    tokenizer_document = strict_json_loads(tokenizer_bytes, label=TOKENIZER_FILE)
    tokenizer_config_document = strict_json_loads(
        tokenizer_config_bytes,
        label=TOKENIZER_CONFIG_FILE,
    )
    validate_raw_tokenizer(tokenizer_document)
    validate_raw_tokenizer_config(tokenizer_config_document)
    tokenizer = load_reference_tokenizer(tokenizer_bytes)

    cases: list[dict[str, Any]] = []
    for spec in ENCODE_CASES:
        text = materialize_text(spec.input_spec)
        try:
            tokenizer.encode_special_tokens = spec.special_token_policy == "encode_as_text"
            encoding = tokenizer.encode(text, add_special_tokens=spec.add_special_tokens)
            ids = list(encoding.ids)
            decoded_with = tokenizer.decode(ids, skip_special_tokens=False)
            decoded_without = tokenizer.decode(ids, skip_special_tokens=True)
        except Exception as error:
            fail(f"maintained tokenizer reference failed case {spec.name}: {error}")
        if any(token_id < 0 or token_id > TOKENIZER_MAX_ID for token_id in ids):
            fail(
                f"maintained tokenizer emitted an alignment-only model row for {spec.name}"
            )
        if spec.name == "context-2048" and len(ids) != MAX_CONTEXT_TOKENS:
            fail("context-2048 corpus recipe did not produce exactly 2048 IDs")
        if spec.name == "context-2049" and len(ids) != MAX_CONTEXT_TOKENS + 1:
            fail("context-2049 corpus recipe did not produce exactly 2049 IDs")
        normalized = unicodedata.normalize("NFC", text)
        if decoded_with != normalized:
            fail(f"unexpected maintained-reference round trip for {spec.name}")
        if decoded_without != expected_decode_without_special_tokens(spec):
            fail(f"unexpected maintained-reference special-token skip for {spec.name}")
        cases.append(
            {
                "name": spec.name,
                "category": spec.category,
                "special_token_policy": spec.special_token_policy,
                "add_special_tokens": spec.add_special_tokens,
                "input": text_descriptor(spec.input_spec),
                "reference_encoding": {
                    "ids": ids_descriptor(ids),
                    "decoded_with_special_tokens": decoded_descriptor(
                        decoded_with,
                        spec.input_spec,
                    ),
                    "decoded_without_special_tokens": decoded_descriptor(
                        decoded_without,
                        spec.input_spec,
                    ),
                    "round_trip": "exact" if decoded_with == text else "normalized_nfc",
                },
                "production": expected_production(spec),
            }
        )

    for item in DECODE_CASES:
        try:
            decoded = tokenizer.decode(
                item["ids"],
                skip_special_tokens=item["skip_special_tokens"],
            )
        except Exception as error:
            fail(f"maintained tokenizer reference failed decode case {item['name']}: {error}")
        if decoded != item["expected_text"]:
            fail(f"maintained tokenizer reference decode drifted for {item['name']}")
    lossy = tokenizer.decode([127], skip_special_tokens=False)
    if lossy != "�":
        fail("maintained tokenizer incomplete-UTF-8 observation drifted")

    lock_sha256 = hashlib.sha256(lock_bytes).hexdigest()
    document = {
        "schema": REFERENCE_SCHEMA,
        "milestone": "v0.32",
        "scope": expected_scope(),
        "source": expected_source(lock_sha256),
        "reference": expected_reference_metadata(),
        "encode_cases": cases,
        "decode_cases": expected_decode_cases(),
        "decode_rejections": expected_decode_rejections(),
        "relationships": expected_relationships(),
    }
    reject_non_finite(document)
    validate_reference(document, lock_sha256=lock_sha256)
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
    parser = argparse.ArgumentParser(
        description=(
            "Generate deterministic v0.32 tokenizer oracle vectors from exact "
            "verified local tokenizer bytes; no network or weight access."
        )
    )
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
        print(f"tokenizer reference generation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
