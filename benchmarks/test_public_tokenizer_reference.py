#!/usr/bin/env python3
"""Focused tests for the dependency-free v0.32 tokenizer corpus checker."""

from __future__ import annotations

import hashlib
import json
import tempfile
import unicodedata
import unittest
from pathlib import Path

import check_public_tokenizer_reference as checker
import generate_public_tokenizer_reference as generator
import public_checkpoint_tokenizer_probe as production_probe
import public_tokenizer_reference_v032 as corpus


def synthetic_ids(case: corpus.EncodeCase) -> list[int]:
    if case.name == "context-2048":
        return [247] * 2_048
    if case.name == "context-2049":
        return [247] * 2_049
    if case.name.startswith("eot-recognize-"):
        return [0]
    if case.name.startswith("eot-text-"):
        return [29, 93, 423, 1171, 1156, 49_651]
    if case.name == "configured-padding-named-special-literal":
        return [1]
    if case.name == "repeated-whitespace":
        return [50_276, 50_275, 50_270, 50_254]
    if case.name == "eot-embedded-recognize":
        return [42, 0, 43]
    if case.name == "eot-embedded-as-text":
        return [42, 29, 93, 423, 1171, 1156, 49_651, 43]
    return [42]


def build_reference(lock_sha256: str) -> dict[str, object]:
    cases: list[dict[str, object]] = []
    for spec in corpus.ENCODE_CASES:
        text = corpus.materialize_text(spec.input_spec)
        decoded_with = unicodedata.normalize("NFC", text)
        decoded_without = corpus.expected_decode_without_special_tokens(spec)
        cases.append(
            {
                "name": spec.name,
                "category": spec.category,
                "special_token_policy": spec.special_token_policy,
                "add_special_tokens": spec.add_special_tokens,
                "input": corpus.text_descriptor(spec.input_spec),
                "reference_encoding": {
                    "ids": corpus.ids_descriptor(synthetic_ids(spec)),
                    "decoded_with_special_tokens": corpus.decoded_descriptor(
                        decoded_with,
                        spec.input_spec,
                    ),
                    "decoded_without_special_tokens": corpus.decoded_descriptor(
                        decoded_without,
                        spec.input_spec,
                    ),
                    "round_trip": "exact" if decoded_with == text else "normalized_nfc",
                },
                "production": corpus.expected_production(spec),
            }
        )
    return {
        "schema": corpus.REFERENCE_SCHEMA,
        "milestone": corpus.MILESTONE,
        "scope": corpus.expected_scope(),
        "source": corpus.expected_source(lock_sha256),
        "reference": corpus.expected_reference_metadata(),
        "encode_cases": cases,
        "decode_cases": corpus.expected_decode_cases(),
        "decode_rejections": corpus.expected_decode_rejections(),
        "relationships": corpus.expected_relationships(),
    }


class PublicTokenizerReferenceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.lock_path = (
            Path(__file__).resolve().parent.parent
            / "models"
            / "public"
            / "pythia-14m-v0.32.lock.json"
        )
        self.lock_bytes = self.lock_path.read_bytes()
        self.lock_sha256 = hashlib.sha256(self.lock_bytes).hexdigest()
        self.reference = build_reference(self.lock_sha256)
        self.reference_path = self.root / "reference.json"
        self.reference_path.write_bytes(corpus.canonical_json_bytes(self.reference))

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def test_complete_corpus_validates_without_third_party_dependencies(self) -> None:
        result = checker.check_reference(self.reference_path, self.lock_path)
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["corpus"]["encode_cases"], len(corpus.ENCODE_CASES))
        self.assertEqual(
            result["corpus"]["decode_cases"],
            len(corpus.DECODE_CASES),
        )
        self.assertEqual(
            result["corpus"]["decode_rejections"],
            len(corpus.DECODE_REJECTIONS),
        )
        self.assertEqual(result["scope"]["public_model_forward_passes"], 0)
        self.assertEqual(result["scope"]["retained_weight_bytes"], 0)

    def test_noncanonical_json_fails(self) -> None:
        self.reference_path.write_bytes(self.reference_path.read_bytes() + b" ")
        with self.assertRaisesRegex(corpus.ReferenceValidationError, "canonical JSON"):
            checker.check_reference(self.reference_path, self.lock_path)

    def test_duplicate_and_nonfinite_json_fail(self) -> None:
        with self.assertRaisesRegex(corpus.ReferenceValidationError, "duplicate JSON key"):
            corpus.strict_json_loads(b'{"schema":1,"schema":2}', label="fixture")
        with self.assertRaisesRegex(corpus.ReferenceValidationError, "non-finite"):
            corpus.strict_json_loads(b'{"value":1e309}', label="fixture")

    def test_reference_package_version_drift_fails(self) -> None:
        document = json.loads(self.reference_path.read_text(encoding="utf-8"))
        document["reference"]["version"] = "0.23.0"
        self.reference_path.write_bytes(corpus.canonical_json_bytes(document))
        with self.assertRaisesRegex(corpus.ReferenceValidationError, "metadata drifted"):
            checker.check_reference(self.reference_path, self.lock_path)

    def test_alignment_only_model_row_in_encode_output_fails(self) -> None:
        document = json.loads(self.reference_path.read_text(encoding="utf-8"))
        encoding = document["encode_cases"][0]["reference_encoding"]
        encoding["ids"] = corpus.ids_descriptor(
            [corpus.ALIGNMENT_ONLY_MODEL_ROW_FIRST]
        )
        self.reference_path.write_bytes(corpus.canonical_json_bytes(document))
        with self.assertRaisesRegex(corpus.ReferenceValidationError, "alignment-only"):
            checker.check_reference(self.reference_path, self.lock_path)

    def test_context_boundary_drift_fails(self) -> None:
        document = json.loads(self.reference_path.read_text(encoding="utf-8"))
        context = next(
            item for item in document["encode_cases"] if item["name"] == "context-2049"
        )
        context["reference_encoding"]["ids"] = corpus.ids_descriptor([247] * 2_048)
        self.reference_path.write_bytes(corpus.canonical_json_bytes(document))
        with self.assertRaisesRegex(corpus.ReferenceValidationError, "2049"):
            checker.check_reference(self.reference_path, self.lock_path)

    def test_special_token_policies_cannot_collapse(self) -> None:
        document = json.loads(self.reference_path.read_text(encoding="utf-8"))
        text_mode = next(
            item
            for item in document["encode_cases"]
            if item["name"] == "eot-text-no-postprocessor"
        )
        text_mode["reference_encoding"]["ids"] = corpus.ids_descriptor([0])
        self.reference_path.write_bytes(corpus.canonical_json_bytes(document))
        with self.assertRaisesRegex(corpus.ReferenceValidationError, "text-mode"):
            checker.check_reference(self.reference_path, self.lock_path)

    def test_strict_decode_rejection_kind_is_exact(self) -> None:
        document = json.loads(self.reference_path.read_text(encoding="utf-8"))
        strict = next(
            item
            for item in document["decode_rejections"]
            if item["name"] == "decode-incomplete-utf8-token-sequence"
        )
        strict["expected_error_kind"] = "lossy_replacement"
        self.reference_path.write_bytes(corpus.canonical_json_bytes(document))
        with self.assertRaisesRegex(corpus.ReferenceValidationError, "rejection vectors"):
            checker.check_reference(self.reference_path, self.lock_path)

    def test_raw_tokenizer_config_cleanup_and_null_pad_are_explicit(self) -> None:
        config = {
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
        generator.validate_raw_tokenizer_config(config)
        config["pad_token"] = "<|padding|>"
        with self.assertRaisesRegex(corpus.ReferenceValidationError, "pad_token"):
            generator.validate_raw_tokenizer_config(config)

    def test_lock_revision_drift_fails_before_reference(self) -> None:
        lock = json.loads(self.lock_bytes)
        lock["source"]["revision"] = "b" * 40
        lock_path = self.root / "drifted-lock.json"
        lock_path.write_text(json.dumps(lock), encoding="utf-8")
        with self.assertRaisesRegex(corpus.ReferenceValidationError, "source identity"):
            checker.check_reference(self.reference_path, lock_path)

    def test_checkpoint_value_and_unknown_lock_field_fail(self) -> None:
        for mutation, expected in (
            (lambda lock: lock["checkpoint"].__setitem__("tensor_count", 75), "checkpoint"),
            (lambda lock: lock.__setitem__("unexpected", True), "root keys"),
        ):
            lock = json.loads(self.lock_bytes)
            mutation(lock)
            lock_path = self.root / f"drifted-{expected.replace(' ', '-')}.json"
            lock_path.write_bytes(corpus.canonical_json_bytes(lock))
            with self.assertRaisesRegex(corpus.ReferenceValidationError, expected):
                checker.check_reference(self.reference_path, lock_path)

    def test_reference_symlink_is_rejected(self) -> None:
        alias = self.root / "reference-alias.json"
        alias.symlink_to(self.reference_path)
        with self.assertRaises(corpus.ReferenceValidationError):
            checker.check_reference(alias, self.lock_path)

    def test_exact_production_result_is_derived_from_the_oracle(self) -> None:
        result = production_probe.expected_result(self.reference)
        counts = production_probe.validate_result(result, reference=self.reference)
        self.assertEqual(counts["encode_cases"], len(corpus.ENCODE_CASES))
        self.assertEqual(
            counts["request_rejections"],
            len(production_probe.REQUEST_REJECTIONS),
        )

    def test_production_result_id_or_error_drift_fails(self) -> None:
        result = production_probe.expected_result(self.reference)
        accepted = next(
            item for item in result["encode_cases"] if item["result"]["outcome"] == "accept"
        )
        accepted["result"]["ids"]["count"] += 1
        with self.assertRaises(production_probe.ProbeError):
            production_probe.validate_result(result, reference=self.reference)

        result = production_probe.expected_result(self.reference)
        result["request_rejections"][0]["error_kind"] = "request_invalid"
        with self.assertRaises(production_probe.ProbeError):
            production_probe.validate_result(result, reference=self.reference)


if __name__ == "__main__":
    unittest.main()
