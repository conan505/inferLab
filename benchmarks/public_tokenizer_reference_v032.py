#!/usr/bin/env python3
"""Deterministic corpus and schema helpers for the v0.32 tokenizer oracle.

This module intentionally has no third-party dependencies.  The generator is
the only component that imports the maintained Python tokenizer reference.
The retained-evidence checker can therefore replay the schema and corpus
invariants without loading model artifacts or installing tokenizer code.
"""

from __future__ import annotations

import hashlib
import json
import math
import struct
import unicodedata
from dataclasses import dataclass
from typing import Any, Iterable, Mapping, Sequence


REFERENCE_SCHEMA = "inferlab.public-tokenizer-reference.v1"
CHECK_SCHEMA = "inferlab.public-tokenizer-reference-check.v1"
MILESTONE = "v0.32"

REPOSITORY = "EleutherAI/pythia-14m"
REVISION = "cf967c0a9a04383db6f7b1108d86b2962634b4ac"
LICENSE = "Apache-2.0"
LOCK_SCHEMA = "inferlab.public-model-lock.v1"

REFERENCE_PACKAGE = "tokenizers"
REFERENCE_VERSION = "0.23.1"
REFERENCE_RELEASE_TAG = "v0.23.1"

TOKENIZER_FILE = "tokenizer.json"
TOKENIZER_BYTES = 2_114_042
TOKENIZER_SHA256 = "870f4e2baa6b683221fa52004d5d6f40ab8c9d31961617304b78c910c2c3caf2"
TOKENIZER_CONFIG_FILE = "tokenizer_config.json"
TOKENIZER_CONFIG_BYTES = 4_834
TOKENIZER_CONFIG_SHA256 = "eee017c5bd133137f45907bd0a6e781e2ccd1a533734b7ed2a2f2f4446659809"

TOKENIZER_DOMAIN_SIZE = 50_277
TOKENIZER_MAX_ID = 50_276
MODEL_VOCAB_ROWS = 50_304
MODEL_MAX_ID = 50_303
ALIGNMENT_ONLY_MODEL_ROW_FIRST = 50_277
ALIGNMENT_ONLY_MODEL_ROW_LAST = 50_303
MAX_CONTEXT_TOKENS = 2_048
SPECIAL_TOKEN_POLICIES = ("recognize_configured", "encode_as_text")

LOCKED_FILES = (
    (
        "README.md",
        10_560,
        "d1f2cf1d5181daedeaa70208ddd5cc5251867bde9acf6db7bb45a2265e25e163",
    ),
    (
        "config.json",
        698,
        "f97f966a66c444890ed461fff2a51eefb15d74303df05b948124719f199b0b17",
    ),
    (
        "model.safetensors",
        28_143_920,
        "116a02532db461f91386a5b20f942ff2c8d4de7341e21b55caafc3d7b25f49a1",
    ),
    (
        "special_tokens_map.json",
        441,
        "10b8c8852c1e1f70b54d9aff61728408c28971c0e97a6c5a7b2debbd1d3e9c0c",
    ),
    (TOKENIZER_FILE, TOKENIZER_BYTES, TOKENIZER_SHA256),
    (
        TOKENIZER_CONFIG_FILE,
        TOKENIZER_CONFIG_BYTES,
        TOKENIZER_CONFIG_SHA256,
    ),
)

CANONICAL_CHECKPOINT = {
    "file": "model.safetensors",
    "format": "safetensors",
    "header_bytes": 8_488,
    "header_sha256": (
        "da85647d12efa36759dba812776603f6989559e6bf75446d3273c5fd0fe0e11d"
    ),
    "dtype": "F16",
    "tensor_count": 76,
    "element_count": 14_067_712,
    "data_bytes": 28_135_424,
}

CANONICAL_ARCHITECTURE = {
    "model_type": "gpt_neox",
    "architecture": "GPTNeoXForCausalLM",
    "vocab_size": 50_304,
    "max_position_embeddings": 2_048,
    "hidden_size": 128,
    "intermediate_size": 512,
    "num_attention_heads": 4,
    "num_hidden_layers": 6,
    "bos_token_id": 0,
    "eos_token_id": 0,
    "hidden_act": "gelu",
    "layer_norm_eps": 0.00001,
    "rotary_pct": 0.25,
    "rotary_emb_base": 10_000,
    "attention_bias": True,
    "tie_word_embeddings": False,
    "use_parallel_residual": True,
    "torch_dtype": "float16",
}


def _literal(text: str) -> dict[str, Any]:
    return {"kind": "literal", "text": text}


def _repeat(unit: str, count: int) -> dict[str, Any]:
    return {"kind": "repeat", "unit": unit, "count": count}


@dataclass(frozen=True)
class EncodeCase:
    name: str
    category: str
    input_spec: Mapping[str, Any]
    special_token_policy: str = "recognize_configured"
    add_special_tokens: bool = False
    production_outcome: str = "accept"
    production_error_kind: str | None = None


NFC_TEXT = "Café Ångström résumé"
NFD_TEXT = unicodedata.normalize("NFD", NFC_TEXT)

ENCODE_CASES = (
    EncodeCase("ascii", "ascii", _literal("Hello, InferLab!")),
    EncodeCase(
        "punctuation",
        "punctuation",
        _literal('"Wait... what?!" (yes): [ok] {done}; /\\ @#$%^&*_'),
    ),
    EncodeCase("leading-whitespace", "whitespace", _literal("   leading")),
    EncodeCase("trailing-whitespace", "whitespace", _literal("trailing   ")),
    EncodeCase(
        "repeated-whitespace",
        "whitespace",
        _literal(
            "A  B   C        D"
            + (" " * 24)
            + "E"
            + (" " * 25)
            + "F"
        ),
    ),
    EncodeCase(
        "tabs-newlines",
        "whitespace",
        _literal("\tline one\nline two\r\n\tline three\n"),
    ),
    EncodeCase("unicode-nfc", "unicode-normalization", _literal(NFC_TEXT)),
    EncodeCase("unicode-nfd", "unicode-normalization", _literal(NFD_TEXT)),
    EncodeCase(
        "combining-marks",
        "unicode-combining-marks",
        _literal("Z\u0351a\u0308l\u0323g\u0334o\u0311"),
    ),
    EncodeCase("emoji-zwj", "unicode-emoji", _literal("👩🏽\u200d💻🚀🙂")),
    EncodeCase("cjk", "unicode-script", _literal("你好，世界。日本語テスト")),
    EncodeCase(
        "arabic",
        "unicode-script",
        _literal("مرحبًا بالعالم، كيف حالك؟"),
    ),
    EncodeCase("embedded-nul", "length-aware-text", _literal("left\u0000right")),
    EncodeCase(
        "literal-replacement-character",
        "valid-unicode-replacement-character",
        _literal("left�right"),
    ),
    EncodeCase(
        "eot-recognize-no-postprocessor",
        "literal-special-token",
        _literal("<|endoftext|>"),
        special_token_policy="recognize_configured",
        add_special_tokens=False,
    ),
    EncodeCase(
        "eot-recognize-with-postprocessor",
        "literal-special-token",
        _literal("<|endoftext|>"),
        special_token_policy="recognize_configured",
        add_special_tokens=True,
    ),
    EncodeCase(
        "eot-text-no-postprocessor",
        "literal-special-token",
        _literal("<|endoftext|>"),
        special_token_policy="encode_as_text",
        add_special_tokens=False,
    ),
    EncodeCase(
        "eot-text-with-postprocessor",
        "literal-special-token",
        _literal("<|endoftext|>"),
        special_token_policy="encode_as_text",
        add_special_tokens=True,
    ),
    EncodeCase(
        "eot-embedded-recognize",
        "literal-special-token",
        _literal("before<|endoftext|>after"),
        special_token_policy="recognize_configured",
        add_special_tokens=False,
    ),
    EncodeCase(
        "eot-embedded-as-text",
        "literal-special-token",
        _literal("before<|endoftext|>after"),
        special_token_policy="encode_as_text",
        add_special_tokens=False,
    ),
    EncodeCase(
        "configured-padding-named-special-literal",
        "configured-special-token",
        _literal("<|padding|>"),
        add_special_tokens=False,
    ),
    EncodeCase(
        "context-2048",
        "context-boundary",
        _repeat(" a", MAX_CONTEXT_TOKENS),
    ),
    EncodeCase(
        "context-2049",
        "context-boundary",
        _repeat(" a", MAX_CONTEXT_TOKENS + 1),
        production_outcome="reject",
        production_error_kind="context_length_exceeded",
    ),
)

DECODE_REJECTIONS = (
    {
        "name": "decode-first-alignment-only-model-row",
        "category": "alignment-only-model-row-decode",
        "ids": [ALIGNMENT_ONLY_MODEL_ROW_FIRST],
        "expected_error_kind": "alignment_only_model_row",
    },
    {
        "name": "decode-last-alignment-only-model-row",
        "category": "alignment-only-model-row-decode",
        "ids": [ALIGNMENT_ONLY_MODEL_ROW_LAST],
        "expected_error_kind": "alignment_only_model_row",
    },
    {
        "name": "decode-mixed-alignment-only-model-row",
        "category": "alignment-only-model-row-decode",
        "ids": [0, ALIGNMENT_ONLY_MODEL_ROW_FIRST, 2],
        "expected_error_kind": "alignment_only_model_row",
    },
    {
        "name": "decode-incomplete-utf8-token-sequence",
        "category": "strict-utf8-decode",
        "ids": [127],
        "expected_error_kind": "invalid_utf8_token_sequence",
    },
    {
        "name": "decode-first-out-of-range-model-row",
        "category": "token-id-domain",
        "ids": [MODEL_MAX_ID + 1],
        "expected_error_kind": "token_id_out_of_range",
    },
)

DECODE_CASES = (
    {
        "name": "decode-complete-multibyte-utf8",
        "category": "strict-utf8-decode",
        "ids": [127, 104],
        "skip_special_tokens": False,
        "expected_text": "é",
    },
)

RELATIONSHIPS = (
    {
        "name": "nfc-nfd-encode-equivalence",
        "kind": "equal_ids",
        "cases": ["unicode-nfc", "unicode-nfd"],
    },
    {
        "name": "recognized-eot-postprocessor-flag-does-not-change-ids",
        "kind": "equal_ids",
        "cases": [
            "eot-recognize-no-postprocessor",
            "eot-recognize-with-postprocessor",
        ],
    },
    {
        "name": "text-eot-postprocessor-flag-does-not-change-ids",
        "kind": "equal_ids",
        "cases": ["eot-text-no-postprocessor", "eot-text-with-postprocessor"],
    },
    {
        "name": "literal-eot-policy-is-semantic",
        "kind": "different_ids",
        "cases": ["eot-recognize-no-postprocessor", "eot-text-no-postprocessor"],
    },
    {
        "name": "embedded-eot-policy-is-semantic",
        "kind": "different_ids",
        "cases": ["eot-embedded-recognize", "eot-embedded-as-text"],
    },
)


class ReferenceValidationError(ValueError):
    """The retained tokenizer reference violates its pinned schema."""


def canonical_json_bytes(value: Any) -> bytes:
    return (
        json.dumps(
            value,
            allow_nan=False,
            ensure_ascii=False,
            indent=2,
            sort_keys=True,
        )
        + "\n"
    ).encode("utf-8")


def strict_json_loads(data: bytes, *, label: str) -> Any:
    def object_pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                raise ReferenceValidationError(f"{label}: duplicate JSON key {key!r}")
            result[key] = value
        return result

    def reject_constant(value: str) -> Any:
        raise ReferenceValidationError(f"{label}: non-finite JSON number {value}")

    def parse_float(value: str) -> float:
        parsed = float(value)
        if not math.isfinite(parsed):
            raise ReferenceValidationError(f"{label}: non-finite JSON number {value}")
        return parsed

    try:
        text = data.decode("utf-8", errors="strict")
        return json.loads(
            text,
            object_pairs_hook=object_pairs,
            parse_constant=reject_constant,
            parse_float=parse_float,
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ReferenceValidationError(f"{label}: invalid JSON: {error}") from error


def materialize_text(input_spec: Mapping[str, Any]) -> str:
    kind = input_spec.get("kind")
    if kind == "literal":
        text = input_spec.get("text")
        if not isinstance(text, str):
            raise ReferenceValidationError("literal input must contain text")
        return text
    if kind == "repeat":
        unit = input_spec.get("unit")
        count = input_spec.get("count")
        if not isinstance(unit, str) or not isinstance(count, int) or isinstance(count, bool):
            raise ReferenceValidationError(
                "repeat input must contain a string unit and integer count"
            )
        if not 1 <= count <= MAX_CONTEXT_TOKENS + 1:
            raise ReferenceValidationError("repeat count is outside the v0.32 corpus bound")
        return unit * count
    raise ReferenceValidationError(f"unsupported input representation {kind!r}")


def text_descriptor(input_spec: Mapping[str, Any]) -> dict[str, Any]:
    text = materialize_text(input_spec)
    raw = text.encode("utf-8")
    result = dict(input_spec)
    result["utf8_bytes"] = len(raw)
    result["sha256"] = hashlib.sha256(raw).hexdigest()
    return result


def decoded_descriptor(text: str, input_spec: Mapping[str, Any] | None = None) -> dict[str, Any]:
    raw = text.encode("utf-8")
    if input_spec is not None and input_spec.get("kind") == "repeat":
        unit = input_spec.get("unit")
        count = input_spec.get("count")
        if isinstance(unit, str) and isinstance(count, int) and text == unit * count:
            return {
                "kind": "repeat",
                "unit": unit,
                "count": count,
                "utf8_bytes": len(raw),
                "sha256": hashlib.sha256(raw).hexdigest(),
            }
    return {
        "kind": "literal",
        "text": text,
        "utf8_bytes": len(raw),
        "sha256": hashlib.sha256(raw).hexdigest(),
    }


def ids_sha256(ids: Iterable[int]) -> str:
    digest = hashlib.sha256()
    for token_id in ids:
        if (
            not isinstance(token_id, int)
            or isinstance(token_id, bool)
            or not 0 <= token_id <= 0xFFFFFFFF
        ):
            raise ReferenceValidationError(f"invalid token ID {token_id!r}")
        digest.update(struct.pack(">I", token_id))
    return digest.hexdigest()


def ids_descriptor(ids: Sequence[int]) -> dict[str, Any]:
    if not ids:
        representation: dict[str, Any] = {"kind": "list", "values": []}
        maximum_id: int | None = None
    elif len(ids) >= 128 and all(token_id == ids[0] for token_id in ids):
        representation = {"kind": "repeat", "id": ids[0], "count": len(ids)}
        maximum_id = ids[0]
    else:
        representation = {"kind": "list", "values": list(ids)}
        maximum_id = max(ids)
    return {
        "count": len(ids),
        "maximum_id": maximum_id,
        "sha256_u32be": ids_sha256(ids),
        "representation": representation,
    }


def expand_ids(descriptor: Mapping[str, Any]) -> list[int]:
    representation = descriptor.get("representation")
    if not isinstance(representation, Mapping):
        raise ReferenceValidationError("ID descriptor has no representation")
    kind = representation.get("kind")
    if kind == "list":
        values = representation.get("values")
        if not isinstance(values, list):
            raise ReferenceValidationError("list ID representation has no values")
        ids = values
    elif kind == "repeat":
        token_id = representation.get("id")
        count = representation.get("count")
        if not isinstance(token_id, int) or isinstance(token_id, bool):
            raise ReferenceValidationError("repeat ID representation has invalid id")
        if (
            not isinstance(count, int)
            or isinstance(count, bool)
            or not 1 <= count <= MAX_CONTEXT_TOKENS + 1
        ):
            raise ReferenceValidationError("repeat ID representation has invalid count")
        ids = [token_id] * count
    else:
        raise ReferenceValidationError(f"unsupported ID representation {kind!r}")
    for token_id in ids:
        if (
            not isinstance(token_id, int)
            or isinstance(token_id, bool)
            or not 0 <= token_id <= 0xFFFFFFFF
        ):
            raise ReferenceValidationError(f"invalid token ID {token_id!r}")
    if descriptor.get("count") != len(ids):
        raise ReferenceValidationError("ID count does not match representation")
    expected_maximum = max(ids) if ids else None
    if descriptor.get("maximum_id") != expected_maximum:
        raise ReferenceValidationError("maximum token ID does not match representation")
    if descriptor.get("sha256_u32be") != ids_sha256(ids):
        raise ReferenceValidationError("token ID digest does not match representation")
    return ids


def expand_text(descriptor: Mapping[str, Any]) -> str:
    text = materialize_text(descriptor)
    raw = text.encode("utf-8")
    if descriptor.get("utf8_bytes") != len(raw):
        raise ReferenceValidationError("text byte count does not match representation")
    if descriptor.get("sha256") != hashlib.sha256(raw).hexdigest():
        raise ReferenceValidationError("text digest does not match representation")
    return text


def expected_lock_files() -> list[dict[str, Any]]:
    return [
        {"name": name, "bytes": size, "sha256": digest}
        for name, size, digest in LOCKED_FILES
    ]


def exact_json_equal(observed: Any, expected: Any) -> bool:
    if type(observed) is not type(expected):
        return False
    if isinstance(expected, dict):
        return set(observed) == set(expected) and all(
            exact_json_equal(observed[key], value) for key, value in expected.items()
        )
    if isinstance(expected, list):
        return len(observed) == len(expected) and all(
            exact_json_equal(left, right)
            for left, right in zip(observed, expected)
        )
    return observed == expected


def validate_lock(lock: Any) -> None:
    if not isinstance(lock, dict):
        raise ReferenceValidationError("lock root must be an object")
    if set(lock) != {"schema", "source", "files", "checkpoint", "architecture"}:
        raise ReferenceValidationError("lock root keys drifted")
    if lock.get("schema") != LOCK_SCHEMA:
        raise ReferenceValidationError("lock schema drifted")
    if not exact_json_equal(
        lock.get("source"),
        {
        "repository": REPOSITORY,
        "revision": REVISION,
        "license": LICENSE,
        },
    ):
        raise ReferenceValidationError("lock source identity drifted")
    if not exact_json_equal(lock.get("files"), expected_lock_files()):
        raise ReferenceValidationError("lock file identities drifted")
    if not exact_json_equal(lock.get("checkpoint"), CANONICAL_CHECKPOINT):
        raise ReferenceValidationError("lock checkpoint contract drifted")
    if not exact_json_equal(lock.get("architecture"), CANONICAL_ARCHITECTURE):
        raise ReferenceValidationError("lock architecture contract drifted")


def expected_scope() -> dict[str, Any]:
    return {
        "checkpoint_artifact_verification": True,
        "production_tokenizer_parity": True,
        "public_model_forward_passes": 0,
        "public_model_generations": 0,
        "public_model_services_started": 0,
        "retains_weight_bytes": False,
    }


def expected_source(lock_sha256: str) -> dict[str, Any]:
    return {
        "repository": REPOSITORY,
        "revision": REVISION,
        "license": LICENSE,
        "lock_schema": LOCK_SCHEMA,
        "lock_sha256": lock_sha256,
        "locked_file_count": len(LOCKED_FILES),
        "tokenizer_file": {
            "name": TOKENIZER_FILE,
            "bytes": TOKENIZER_BYTES,
            "sha256": TOKENIZER_SHA256,
        },
        "tokenizer_config_file": {
            "name": TOKENIZER_CONFIG_FILE,
            "bytes": TOKENIZER_CONFIG_BYTES,
            "sha256": TOKENIZER_CONFIG_SHA256,
            "clean_up_tokenization_spaces": True,
            "pad_token": None,
        },
        "tokenizer_pipeline": {
            "serialization_version": "1.0",
            "model": "BPE",
            "base_vocabulary_entries": 50_254,
            "merge_rules": 50_009,
            "normalizer": "NFC",
            "pre_tokenizer": "ByteLevel",
            "decoder": "ByteLevel",
            "add_prefix_space": False,
            "trim_offsets": True,
            "use_regex": True,
        },
        "contiguous_decodable_token_id_domain": {
            "count": TOKENIZER_DOMAIN_SIZE,
            "first": 0,
            "last": TOKENIZER_MAX_ID,
        },
        "model_row_domain": {
            "count": MODEL_VOCAB_ROWS,
            "first": 0,
            "last": MODEL_MAX_ID,
        },
        "alignment_only_model_rows": {
            "count": (
                ALIGNMENT_ONLY_MODEL_ROW_LAST - ALIGNMENT_ONLY_MODEL_ROW_FIRST + 1
            ),
            "first": ALIGNMENT_ONLY_MODEL_ROW_FIRST,
            "last": ALIGNMENT_ONLY_MODEL_ROW_LAST,
        },
        "maximum_context_tokens": MAX_CONTEXT_TOKENS,
    }


def expected_reference_metadata() -> dict[str, Any]:
    return {
        "implementation": "Hugging Face Tokenizers Python binding",
        "package": REFERENCE_PACKAGE,
        "version": REFERENCE_VERSION,
        "release_tag": REFERENCE_RELEASE_TAG,
        "api": "tokenizers.Tokenizer.from_str",
        "artifact_input": "exact verified local tokenizer.json bytes",
        "network_access": False,
        "hub_cache_access": False,
        "transformers_cleanup_applied": False,
        "special_token_policy_mapping": {
            "recognize_configured": {
                "upstream_encode_special_tokens": False,
            },
            "encode_as_text": {
                "upstream_encode_special_tokens": True,
            },
        },
        "postprocessor_special_insertion": (
            "separate add_special_tokens boolean; pinned template inserts none"
        ),
    }


def expected_production(case: EncodeCase) -> dict[str, Any]:
    if case.production_outcome == "accept":
        return {"outcome": "accept"}
    if case.production_outcome == "reject" and case.production_error_kind:
        return {
            "outcome": "reject",
            "error_kind": case.production_error_kind,
        }
    raise ReferenceValidationError(f"invalid production expectation for {case.name}")


def expected_decode_without_special_tokens(case: EncodeCase) -> str:
    normalized = unicodedata.normalize("NFC", materialize_text(case.input_spec))
    if case.special_token_policy == "recognize_configured":
        return normalized.replace("<|endoftext|>", "").replace("<|padding|>", "")
    if case.special_token_policy == "encode_as_text":
        return normalized
    raise ReferenceValidationError(
        f"unsupported special-token policy {case.special_token_policy!r}"
    )


def expected_decode_cases() -> list[dict[str, Any]]:
    return [
        {
            "name": item["name"],
            "category": item["category"],
            "ids": list(item["ids"]),
            "skip_special_tokens": item["skip_special_tokens"],
            "reference_decoded": decoded_descriptor(item["expected_text"]),
            "production": {"outcome": "accept"},
        }
        for item in DECODE_CASES
    ]


def expected_decode_rejections() -> list[dict[str, Any]]:
    result: list[dict[str, Any]] = []
    for item in DECODE_REJECTIONS:
        if item["expected_error_kind"] == "invalid_utf8_token_sequence":
            oracle = {
                "oracle_action": "observed_lossy_and_overridden_by_strict_boundary",
                "maintained_reference_decoded": decoded_descriptor("�"),
            }
        else:
            oracle = {
                "oracle_action": "not_invoked_outside_contiguous_decodable_domain",
            }
        result.append(
            {
                **item,
                **oracle,
                "production_outcome": "reject",
            }
        )
    return result


def expected_relationships() -> list[dict[str, Any]]:
    return [dict(item) for item in RELATIONSHIPS]


def _expect_exact_keys(value: Any, keys: set[str], label: str) -> Mapping[str, Any]:
    if not isinstance(value, Mapping):
        raise ReferenceValidationError(f"{label} must be an object")
    actual = set(value)
    if actual != keys:
        missing = sorted(keys - actual)
        extra = sorted(actual - keys)
        raise ReferenceValidationError(f"{label} keys drifted: missing={missing}, extra={extra}")
    return value


def validate_reference(document: Any, *, lock_sha256: str) -> dict[str, int]:
    root = _expect_exact_keys(
        document,
        {
            "schema",
            "milestone",
            "scope",
            "source",
            "reference",
            "encode_cases",
            "decode_cases",
            "decode_rejections",
            "relationships",
        },
        "reference",
    )
    if root["schema"] != REFERENCE_SCHEMA or root["milestone"] != MILESTONE:
        raise ReferenceValidationError("reference schema or milestone drifted")
    if root["scope"] != expected_scope():
        raise ReferenceValidationError("reference scope drifted")
    if root["source"] != expected_source(lock_sha256):
        raise ReferenceValidationError("reference source metadata drifted")
    if root["reference"] != expected_reference_metadata():
        raise ReferenceValidationError("reference implementation metadata drifted")
    if root["decode_cases"] != expected_decode_cases():
        raise ReferenceValidationError("decode acceptance vectors drifted")
    if root["decode_rejections"] != expected_decode_rejections():
        raise ReferenceValidationError("decode rejection vectors drifted")
    if root["relationships"] != expected_relationships():
        raise ReferenceValidationError("reference relationships drifted")

    encode_cases = root["encode_cases"]
    if not isinstance(encode_cases, list):
        raise ReferenceValidationError("encode_cases must be an array")
    if len(encode_cases) != len(ENCODE_CASES):
        raise ReferenceValidationError("encode case count drifted")

    by_name: dict[str, Mapping[str, Any]] = {}
    observed_emitted_ids: list[int] = []
    for spec, value in zip(ENCODE_CASES, encode_cases):
        case = _expect_exact_keys(
            value,
            {
                "name",
                "category",
                "special_token_policy",
                "add_special_tokens",
                "input",
                "reference_encoding",
                "production",
            },
            f"encode case {spec.name}",
        )
        if case["name"] != spec.name or case["category"] != spec.category:
            raise ReferenceValidationError(f"encode case identity drifted for {spec.name}")
        if (
            case["special_token_policy"] != spec.special_token_policy
            or case["special_token_policy"] not in SPECIAL_TOKEN_POLICIES
        ):
            raise ReferenceValidationError(f"special-token policy drifted for {spec.name}")
        if case["add_special_tokens"] is not spec.add_special_tokens:
            raise ReferenceValidationError(f"add_special_tokens drifted for {spec.name}")
        if case["input"] != text_descriptor(spec.input_spec):
            raise ReferenceValidationError(f"input drifted for {spec.name}")
        if case["production"] != expected_production(spec):
            raise ReferenceValidationError(f"production expectation drifted for {spec.name}")

        encoding = _expect_exact_keys(
            case["reference_encoding"],
            {
                "ids",
                "decoded_with_special_tokens",
                "decoded_without_special_tokens",
                "round_trip",
            },
            f"reference encoding {spec.name}",
        )
        ids = expand_ids(encoding["ids"])
        if any(token_id > TOKENIZER_MAX_ID for token_id in ids):
            raise ReferenceValidationError(
                f"alignment-only model row emitted for {spec.name}"
            )
        observed_emitted_ids.extend(ids)
        input_text = materialize_text(spec.input_spec)
        decoded_with = expand_text(encoding["decoded_with_special_tokens"])
        decoded_without = expand_text(encoding["decoded_without_special_tokens"])
        normalized = unicodedata.normalize("NFC", input_text)
        if decoded_with != normalized:
            raise ReferenceValidationError(f"upstream round-trip truth drifted for {spec.name}")
        if decoded_without != expected_decode_without_special_tokens(spec):
            raise ReferenceValidationError(
                f"upstream special-token skip behavior drifted for {spec.name}"
            )
        expected_round_trip = "exact" if normalized == input_text else "normalized_nfc"
        if encoding["round_trip"] != expected_round_trip:
            raise ReferenceValidationError(f"round-trip label drifted for {spec.name}")
        if spec.name == "context-2048" and len(ids) != MAX_CONTEXT_TOKENS:
            raise ReferenceValidationError("2048 context vector does not encode to 2048 IDs")
        if spec.name == "context-2049" and len(ids) != MAX_CONTEXT_TOKENS + 1:
            raise ReferenceValidationError("2049 context vector does not encode to 2049 IDs")
        if spec.name in {"context-2048", "context-2049"} and any(
            token_id != 247 for token_id in ids
        ):
            raise ReferenceValidationError("context recipe no longer repeats token ID 247")
        if spec.name.startswith("eot-recognize-") and ids != [0]:
            raise ReferenceValidationError(
                "recognized literal end-of-text token no longer encodes as ID 0"
            )
        if spec.name.startswith("eot-text-") and ids != [29, 93, 423, 1171, 1156, 49651]:
            raise ReferenceValidationError(
                "text-mode literal end-of-text token encoding drifted"
            )
        if spec.name == "eot-embedded-recognize" and 0 not in ids:
            raise ReferenceValidationError("recognized embedded end-of-text ID is absent")
        if spec.name == "eot-embedded-as-text" and 0 in ids:
            raise ReferenceValidationError("text-mode embedded end-of-text became EOS")
        if spec.name == "configured-padding-named-special-literal" and ids != [1]:
            raise ReferenceValidationError(
                "configured padding-named special no longer encodes as ID 1"
            )
        if spec.name == "repeated-whitespace" and not {
            50_254,
            50_270,
            50_275,
            50_276,
        }.issubset(ids):
            raise ReferenceValidationError("multi-space added-token coverage drifted")
        by_name[spec.name] = case

    if not observed_emitted_ids or max(observed_emitted_ids) != TOKENIZER_MAX_ID:
        raise ReferenceValidationError(
            "proof corpus no longer observes reachability of maximum decodable ID 50276"
        )

    for relationship in RELATIONSHIPS:
        left, right = relationship["cases"]
        left_ids = expand_ids(by_name[left]["reference_encoding"]["ids"])
        right_ids = expand_ids(by_name[right]["reference_encoding"]["ids"])
        if relationship["kind"] == "equal_ids" and left_ids != right_ids:
            raise ReferenceValidationError(f"relationship {relationship['name']} failed")
        if relationship["kind"] == "different_ids" and left_ids == right_ids:
            raise ReferenceValidationError(f"relationship {relationship['name']} failed")

    return {
        "encode_cases": len(encode_cases),
        "decode_cases": len(DECODE_CASES),
        "decode_rejections": len(DECODE_REJECTIONS),
        "relationships": len(RELATIONSHIPS),
    }


def reject_non_finite(value: Any, *, label: str = "value") -> None:
    """Defensive recursive finite-number check for programmatically built JSON."""

    if isinstance(value, float) and not math.isfinite(value):
        raise ReferenceValidationError(f"{label} contains a non-finite number")
    if isinstance(value, Mapping):
        for key, child in value.items():
            reject_non_finite(child, label=f"{label}.{key}")
    elif isinstance(value, list):
        for index, child in enumerate(value):
            reject_non_finite(child, label=f"{label}[{index}]")
