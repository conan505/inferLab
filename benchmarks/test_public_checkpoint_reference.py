#!/usr/bin/env python3
"""Focused tests for the dependency-free v0.32 checkpoint reference."""

from __future__ import annotations

import copy
import hashlib
import unittest

import generate_public_checkpoint_reference as generator
import public_checkpoint_reference_v032 as checkpoint
from public_tokenizer_reference_v032 import (
    CANONICAL_ARCHITECTURE,
    MILESTONE,
    ReferenceValidationError,
    canonical_json_bytes,
)


LOCK_SHA256 = "a" * 64


def synthetic_reference() -> dict[str, object]:
    cursor = 0
    tensors: list[dict[str, object]] = []
    for name, shape in sorted(checkpoint.expected_tensor_shapes().items()):
        elements = checkpoint.tensor_elements(shape)
        byte_count = elements * 2
        tensors.append(
            {
                "name": name,
                "dtype": "F16",
                "shape": shape,
                "data_offsets": [cursor, cursor + byte_count],
                "elements": elements,
                "bytes": byte_count,
            }
        )
        cursor += byte_count
    inventory_sha256 = hashlib.sha256(canonical_json_bytes(tensors)).hexdigest()
    return {
        "schema": checkpoint.CHECKPOINT_REFERENCE_SCHEMA,
        "milestone": MILESTONE,
        "scope": checkpoint.expected_scope(),
        "source": checkpoint.expected_source(LOCK_SHA256),
        "reference": checkpoint.expected_reference_metadata(),
        "architecture": CANONICAL_ARCHITECTURE,
        "checkpoint": checkpoint.expected_checkpoint_summary(inventory_sha256),
        "tensors": tensors,
    }


class PublicCheckpointReferenceTests(unittest.TestCase):
    def test_complete_inventory_validates(self) -> None:
        counts = checkpoint.validate_checkpoint_reference(
            synthetic_reference(),
            lock_sha256=LOCK_SHA256,
        )
        self.assertEqual(counts["tensors"], 76)
        self.assertEqual(counts["elements"], 14_067_712)
        self.assertEqual(counts["data_bytes"], 28_135_424)

    def test_tensor_type_shape_and_offset_drift_fail(self) -> None:
        mutations = (
            lambda tensor: tensor.__setitem__("dtype", "F32"),
            lambda tensor: tensor.__setitem__("shape", [1]),
            lambda tensor: tensor.__setitem__("data_offsets", [1, tensor["bytes"] + 1]),
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                document = synthetic_reference()
                mutation(document["tensors"][0])
                with self.assertRaises(ReferenceValidationError):
                    checkpoint.validate_checkpoint_reference(
                        document,
                        lock_sha256=LOCK_SHA256,
                    )

    def test_unknown_tensor_field_and_stale_inventory_digest_fail(self) -> None:
        document = synthetic_reference()
        document["tensors"][0]["unknown"] = True
        with self.assertRaisesRegex(ReferenceValidationError, "keys drifted"):
            checkpoint.validate_checkpoint_reference(
                document,
                lock_sha256=LOCK_SHA256,
            )

        document = synthetic_reference()
        document["checkpoint"]["tensor_inventory_sha256"] = "b" * 64
        with self.assertRaisesRegex(ReferenceValidationError, "summary"):
            checkpoint.validate_checkpoint_reference(
                document,
                lock_sha256=LOCK_SHA256,
            )

    def test_f16_finiteness_check_is_bit_exact(self) -> None:
        self.assertTrue(generator.f16_chunk_is_finite(b"\x00\x3c\x00\xbc"))
        self.assertFalse(generator.f16_chunk_is_finite(b"\x00\x7c"))
        self.assertFalse(generator.f16_chunk_is_finite(b"\x01\x7e"))
        with self.assertRaisesRegex(ReferenceValidationError, "odd"):
            generator.f16_chunk_is_finite(b"\x00")

    def test_header_metadata_and_tensor_names_are_exact(self) -> None:
        document = synthetic_reference()
        header = {"__metadata__": {"format": "pt"}}
        for tensor in document["tensors"]:
            header[tensor["name"]] = {
                "dtype": tensor["dtype"],
                "shape": tensor["shape"],
                "data_offsets": tensor["data_offsets"],
            }
        parsed = generator.parse_tensors(header)
        self.assertEqual(parsed, document["tensors"])

        mutated = copy.deepcopy(header)
        mutated["__metadata__"]["format"] = "pickle"
        with self.assertRaisesRegex(ReferenceValidationError, "metadata"):
            generator.parse_tensors(mutated)


if __name__ == "__main__":
    unittest.main()
