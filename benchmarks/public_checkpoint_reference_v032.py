#!/usr/bin/env python3
"""Dependency-free schema helpers for the v0.32 checkpoint reference."""

from __future__ import annotations

import hashlib
import math
from typing import Any, Mapping, Sequence

from public_tokenizer_reference_v032 import (
    CANONICAL_ARCHITECTURE,
    CANONICAL_CHECKPOINT,
    LICENSE,
    LOCK_SCHEMA,
    MILESTONE,
    REPOSITORY,
    REVISION,
    ReferenceValidationError,
    canonical_json_bytes,
    exact_json_equal,
)


CHECKPOINT_REFERENCE_SCHEMA = "inferlab.public-checkpoint-reference.v1"
CHECKPOINT_FILE = "model.safetensors"
CHECKPOINT_FILE_BYTES = 28_143_920
CHECKPOINT_FILE_SHA256 = (
    "116a02532db461f91386a5b20f942ff2c8d4de7341e21b55caafc3d7b25f49a1"
)
HEADER_PREFIX_BYTES = 8


def expected_tensor_shapes() -> dict[str, list[int]]:
    hidden = 128
    intermediate = 512
    vocabulary = 50_304
    result = {
        "embed_out.weight": [vocabulary, hidden],
        "gpt_neox.embed_in.weight": [vocabulary, hidden],
        "gpt_neox.final_layer_norm.bias": [hidden],
        "gpt_neox.final_layer_norm.weight": [hidden],
    }
    per_layer = {
        "attention.dense.bias": [hidden],
        "attention.dense.weight": [hidden, hidden],
        "attention.query_key_value.bias": [hidden * 3],
        "attention.query_key_value.weight": [hidden * 3, hidden],
        "input_layernorm.bias": [hidden],
        "input_layernorm.weight": [hidden],
        "mlp.dense_4h_to_h.bias": [hidden],
        "mlp.dense_4h_to_h.weight": [hidden, intermediate],
        "mlp.dense_h_to_4h.bias": [intermediate],
        "mlp.dense_h_to_4h.weight": [intermediate, hidden],
        "post_attention_layernorm.bias": [hidden],
        "post_attention_layernorm.weight": [hidden],
    }
    for layer in range(6):
        prefix = f"gpt_neox.layers.{layer}."
        for suffix, shape in per_layer.items():
            result[prefix + suffix] = list(shape)
    return result


def tensor_elements(shape: Sequence[int]) -> int:
    result = 1
    if not shape:
        raise ReferenceValidationError("checkpoint tensor shape cannot be empty")
    for dimension in shape:
        if type(dimension) is not int or dimension <= 0:
            raise ReferenceValidationError("checkpoint tensor dimension is invalid")
        result *= dimension
    return result


def expected_scope() -> dict[str, Any]:
    return {
        "checkpoint_artifact_verification": True,
        "public_model_forward_passes": 0,
        "public_model_generations": 0,
        "public_model_services_started": 0,
        "retained_weight_bytes": 0,
    }


def expected_source(lock_sha256: str) -> dict[str, Any]:
    return {
        "repository": REPOSITORY,
        "revision": REVISION,
        "license": LICENSE,
        "lock_schema": LOCK_SCHEMA,
        "lock_sha256": lock_sha256,
    }


def expected_reference_metadata() -> dict[str, Any]:
    return {
        "implementation": "Python standard library",
        "parser": "bounded strict-JSON safetensors header inspection",
        "artifact_input": "exact descriptor-pinned local model.safetensors bytes",
        "third_party_packages": [],
        "network_access": False,
        "hub_cache_access": False,
        "executes_checkpoint_code": False,
    }


def expected_checkpoint_summary(inventory_sha256: str) -> dict[str, Any]:
    return {
        "file": CHECKPOINT_FILE,
        "file_bytes": CHECKPOINT_FILE_BYTES,
        "file_sha256": CHECKPOINT_FILE_SHA256,
        "format": CANONICAL_CHECKPOINT["format"],
        "prefix_bytes": HEADER_PREFIX_BYTES,
        "header_bytes": CANONICAL_CHECKPOINT["header_bytes"],
        "header_sha256": CANONICAL_CHECKPOINT["header_sha256"],
        "data_bytes": CANONICAL_CHECKPOINT["data_bytes"],
        "dtype": CANONICAL_CHECKPOINT["dtype"],
        "tensor_count": CANONICAL_CHECKPOINT["tensor_count"],
        "element_count": CANONICAL_CHECKPOINT["element_count"],
        "finite_payload": True,
        "tensor_inventory_sha256": inventory_sha256,
    }


def _expect_exact_keys(
    value: Any,
    keys: set[str],
    label: str,
) -> Mapping[str, Any]:
    if not isinstance(value, Mapping):
        raise ReferenceValidationError(f"{label} must be an object")
    actual = set(value)
    if actual != keys:
        raise ReferenceValidationError(
            f"{label} keys drifted: missing={sorted(keys - actual)}, "
            f"extra={sorted(actual - keys)}"
        )
    return value


def validate_checkpoint_reference(
    document: Any,
    *,
    lock_sha256: str,
) -> dict[str, int]:
    root = _expect_exact_keys(
        document,
        {
            "schema",
            "milestone",
            "scope",
            "source",
            "reference",
            "architecture",
            "checkpoint",
            "tensors",
        },
        "checkpoint reference",
    )
    if root["schema"] != CHECKPOINT_REFERENCE_SCHEMA or root["milestone"] != MILESTONE:
        raise ReferenceValidationError("checkpoint reference identity drifted")
    if not exact_json_equal(root["scope"], expected_scope()):
        raise ReferenceValidationError("checkpoint reference scope drifted")
    if not exact_json_equal(root["source"], expected_source(lock_sha256)):
        raise ReferenceValidationError("checkpoint reference source drifted")
    if not exact_json_equal(root["reference"], expected_reference_metadata()):
        raise ReferenceValidationError("checkpoint reference implementation drifted")
    if not exact_json_equal(root["architecture"], CANONICAL_ARCHITECTURE):
        raise ReferenceValidationError("checkpoint reference architecture drifted")

    tensors = root["tensors"]
    if not isinstance(tensors, list):
        raise ReferenceValidationError("checkpoint tensors must be an array")
    shapes = expected_tensor_shapes()
    if len(tensors) != len(shapes):
        raise ReferenceValidationError("checkpoint tensor count drifted")

    names: list[str] = []
    intervals: list[tuple[int, int, str]] = []
    total_elements = 0
    for index, value in enumerate(tensors):
        tensor = _expect_exact_keys(
            value,
            {"name", "dtype", "shape", "data_offsets", "elements", "bytes"},
            f"checkpoint tensor {index}",
        )
        name = tensor["name"]
        if not isinstance(name, str) or name not in shapes:
            raise ReferenceValidationError("checkpoint tensor name drifted")
        names.append(name)
        if tensor["dtype"] != "F16" or tensor["shape"] != shapes[name]:
            raise ReferenceValidationError(f"checkpoint tensor contract drifted for {name}")
        elements = tensor_elements(tensor["shape"])
        if type(tensor["elements"]) is not int or tensor["elements"] != elements:
            raise ReferenceValidationError(f"checkpoint element count drifted for {name}")
        byte_count = elements * 2
        if type(tensor["bytes"]) is not int or tensor["bytes"] != byte_count:
            raise ReferenceValidationError(f"checkpoint byte count drifted for {name}")
        offsets = tensor["data_offsets"]
        if (
            not isinstance(offsets, list)
            or len(offsets) != 2
            or any(type(offset) is not int or offset < 0 for offset in offsets)
            or offsets[1] - offsets[0] != byte_count
        ):
            raise ReferenceValidationError(f"checkpoint offsets drifted for {name}")
        intervals.append((offsets[0], offsets[1], name))
        total_elements += elements

    if names != sorted(shapes):
        raise ReferenceValidationError("checkpoint tensor inventory is not exactly sorted")
    if len(set(names)) != len(names):
        raise ReferenceValidationError("checkpoint tensor inventory contains duplicates")
    cursor = 0
    for start, end, _name in sorted(intervals):
        if start != cursor or end <= start:
            raise ReferenceValidationError("checkpoint tensor data is gapped or overlapping")
        cursor = end
    if cursor != CANONICAL_CHECKPOINT["data_bytes"]:
        raise ReferenceValidationError("checkpoint tensor data extent drifted")
    if total_elements != CANONICAL_CHECKPOINT["element_count"]:
        raise ReferenceValidationError("checkpoint parameter total drifted")

    inventory_sha256 = hashlib.sha256(canonical_json_bytes(tensors)).hexdigest()
    if not exact_json_equal(
        root["checkpoint"],
        expected_checkpoint_summary(inventory_sha256),
    ):
        raise ReferenceValidationError("checkpoint summary drifted")
    expected_file_bytes = (
        HEADER_PREFIX_BYTES
        + CANONICAL_CHECKPOINT["header_bytes"]
        + CANONICAL_CHECKPOINT["data_bytes"]
    )
    if expected_file_bytes != CHECKPOINT_FILE_BYTES:
        raise ReferenceValidationError("checkpoint file arithmetic drifted")
    return {
        "tensors": len(tensors),
        "elements": total_elements,
        "data_bytes": cursor,
    }


def reject_non_finite(value: Any, *, label: str = "value") -> None:
    if isinstance(value, float) and not math.isfinite(value):
        raise ReferenceValidationError(f"{label} contains a non-finite number")
    if isinstance(value, Mapping):
        for key, child in value.items():
            reject_non_finite(child, label=f"{label}.{key}")
    elif isinstance(value, list):
        for index, child in enumerate(value):
            reject_non_finite(child, label=f"{label}[{index}]")
