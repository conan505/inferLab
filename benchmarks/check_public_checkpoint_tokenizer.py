#!/usr/bin/env python3
"""Adversarial dependency-free checker for retained v0.32 evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import stat
import sys
from pathlib import Path
from typing import Any, Mapping, NoReturn

from check_public_checkpoint_reference import (
    CHECK_SCHEMA as CHECKPOINT_CHECK_SCHEMA,
    check_reference as check_checkpoint_reference,
)
from check_public_tokenizer_reference import (
    CHECK_SCHEMA as TOKENIZER_CHECK_SCHEMA,
    check_reference as check_tokenizer_reference,
    read_bounded_regular,
)
from generate_public_tokenizer_reference import atomic_write
from public_checkpoint_reference_v032 import (
    CHECKPOINT_FILE_SHA256,
    CHECKPOINT_REFERENCE_SCHEMA,
    validate_checkpoint_reference,
)
from public_checkpoint_tokenizer_probe import (
    OFFLINE_ENVIRONMENT,
    PROBE_SCOPE,
    ProbeError,
    RESULT_SCHEMA,
    validate_result,
)
from public_tokenizer_reference_v032 import (
    CANONICAL_ARCHITECTURE,
    CANONICAL_CHECKPOINT,
    LICENSE,
    LOCKED_FILES,
    LOCK_SCHEMA,
    MILESTONE,
    REFERENCE_PACKAGE,
    REFERENCE_SCHEMA,
    REFERENCE_VERSION,
    REPOSITORY,
    REVISION,
    ReferenceValidationError,
    canonical_json_bytes,
    exact_json_equal,
    strict_json_loads,
    validate_lock,
    validate_reference,
)


ASSERTION_SCHEMA = "inferlab.public-checkpoint-tokenizer-assertions.v0.32"
ACQUISITION_SCHEMA = "inferlab.public-model-acquisition-proof.v0.32"
CONTRACT_SCHEMA = "inferlab.public-checkpoint-tokenizer-proof-contract.v0.32"
ENVIRONMENT_SCHEMA = "inferlab.public-checkpoint-tokenizer-environment.v0.32"
FAILURE_SCHEMA = "inferlab.public-checkpoint-tokenizer-failures.v0.32"
MANIFEST_SCHEMA = "inferlab.public-checkpoint-tokenizer-manifest.v0.32"
PROCESS_SCHEMA = "inferlab.public-checkpoint-tokenizer-process-accounting.v0.32"
PRODUCTION_TEST_SCHEMA = "inferlab.public-checkpoint-tokenizer-tests.v0.32"
RUNTIME_BOUNDARY_SCHEMA = "inferlab.public-checkpoint-tokenizer-runtime.v0.32"
SANITIZER_SCHEMA = "inferlab.public-checkpoint-tokenizer-sanitizer.v0.32"
SOURCE_INTEGRITY_SCHEMA = "inferlab.public-checkpoint-tokenizer-source-integrity.v0.32"
TIMINGS_SCHEMA = "inferlab.public-checkpoint-tokenizer-timings.v0.32"

SOURCE_LOCK_SHA256 = "76da77f329e3135e15febf6f017b09e54eb008c45f9a1f4fb05683d4834aaa49"
TOTAL_ASSET_BYTES = 30_274_495
CACHE_KEY = f"pythia-14m/{REVISION}"
SHA256 = re.compile(r"^[0-9a-f]{64}$")
RUN_ID = re.compile(r"^[a-z0-9][a-z0-9._-]{0,95}$")
GIT_COMMIT = re.compile(r"^[0-9a-f]{40}$")

EXPECTED_FILES = {
    "artifact-acquisition.json",
    "assertions.json",
    "checkpoint-reference-check.json",
    "checkpoint-reference.json",
    "checkpoint-verification.json",
    "environment.json",
    "failure-matrix.json",
    "manifest.json",
    "process-continuity.json",
    "production-tests.json",
    "proof-contract.json",
    "public-checkpoint-tokenizer-proof.svg",
    "runtime-boundary.json",
    "sanitizer.json",
    "source-lock.json",
    "source-integrity.json",
    "timings.json",
    "tokenizer-production.json",
    "tokenizer-reference-check.json",
    "tokenizer-reference.json",
}
NON_MANIFEST_FILES = EXPECTED_FILES - {"manifest.json"}
DERIVED_FILES = {
    "assertions.json",
    "manifest.json",
    "public-checkpoint-tokenizer-proof.svg",
    "sanitizer.json",
}
MAX_EVIDENCE_FILE_BYTES = 2 * 1024 * 1024

PROOF_TOOL_FILES = (
    "benchmarks/check_public_checkpoint_reference.py",
    "benchmarks/check_public_checkpoint_tokenizer.py",
    "benchmarks/check_public_tokenizer_reference.py",
    "benchmarks/fetch_public_model_assets.py",
    "benchmarks/generate_public_checkpoint_reference.py",
    "benchmarks/generate_public_tokenizer_reference.py",
    "benchmarks/public_checkpoint_reference_v032.py",
    "benchmarks/public_checkpoint_tokenizer_probe.py",
    "benchmarks/public_tokenizer_reference_v032.py",
    "benchmarks/render_public_checkpoint_tokenizer_svg.py",
    "benchmarks/test_fetch_public_model_assets.py",
    "benchmarks/test_public_checkpoint_reference.py",
    "benchmarks/test_public_model_fetch_failure_matrix.py",
    "benchmarks/test_public_tokenizer_reference.py",
    "scripts/fetch-v0.32-assets.sh",
    "scripts/proof-v0.32.sh",
)
RUNTIME_BASE_PATHS = (
    "benchmarks/fetch_public_model_assets.py",
    "benchmarks/test_fetch_public_model_assets.py",
    "model-artifacts",
    "models/public/pythia-14m-v0.32.lock.json",
    "scripts/fetch-v0.32-assets.sh",
)

DIRECT_FAILURES = {
    "mutable-revision-lock": "lock_mismatch",
    "unknown-lock-field": "lock_invalid",
    "missing-asset": "inventory_mismatch",
    "extra-asset": "inventory_mismatch",
    "symlink-asset": "file_unsafe",
    "fifo-asset": "file_unsafe",
    "corrupt-config": "hash_mismatch",
    "corrupt-checkpoint": "hash_mismatch",
    "offline-cache-missing": "offline_cache_missing",
    "invalid-existing-cache": "existing_invalid_cache_refused",
}
REGRESSION_FAILURES = {
    "fetch-response-oversize": {
        "command": "python-fetch-failure-matrix",
        "test_name": (
            "PublicModelFetchFailureMatrixTests."
            "test_streamed_response_oversize_is_rejected_at_exact_bound"
        ),
    },
    "fetch-response-short": {
        "command": "python-fetch-failure-matrix",
        "test_name": (
            "PublicModelFetchFailureMatrixTests."
            "test_short_response_is_rejected_before_publication"
        ),
    },
    "fetch-response-hash-mismatch": {
        "command": "python-fetch-failure-matrix",
        "test_name": (
            "PublicModelFetchFailureMatrixTests."
            "test_exact_length_hash_mismatch_is_rejected"
        ),
    },
    "fetch-redirect-overflow": {
        "command": "python-fetch-failure-matrix",
        "test_name": (
            "PublicModelFetchFailureMatrixTests."
            "test_redirect_overflow_is_rejected_at_the_configured_limit"
        ),
    },
    "fetch-total-deadline": {
        "command": "python-fetch-failure-matrix",
        "test_name": (
            "PublicModelFetchFailureMatrixTests."
            "test_trickled_http_read_obeys_one_total_acquisition_deadline"
        ),
    },
    "fetch-lock-contention-deadline": {
        "command": "python-fetch-assets",
        "test_name": "AssetCacheTests.test_fetch_lock_contention_honors_acquisition_deadline",
    },
    "fetch-pre-publication-cleanup": {
        "command": "python-fetch-assets",
        "test_name": "AssetCacheTests.test_pre_rename_download_failure_leaves_no_final_generation",
    },
    "fetch-post-rename-indeterminate": {
        "command": "python-fetch-assets",
        "test_name": "AssetCacheTests.test_post_rename_failure_is_indeterminate_and_reconciles",
    },
    "config-contract-drift": {
        "command": "rust-workspace",
        "test_name": "verify::tests::verified_but_invalid_or_mismatched_config_is_rejected",
    },
    "checkpoint-header-drift": {
        "command": "rust-workspace",
        "test_name": "verify::tests::checkpoint_header_hash_and_parser_fail_closed",
    },
    "tensor-inventory-drift": {
        "command": "rust-workspace",
        "test_name": "verify::tests::tensor_dtype_shape_and_offsets_are_exact",
    },
    "tensor-dtype-drift": {
        "command": "rust-workspace",
        "test_name": "verify::tests::tensor_dtype_shape_and_offsets_are_exact",
    },
    "tensor-shape-drift": {
        "command": "rust-workspace",
        "test_name": "verify::tests::tensor_dtype_shape_and_offsets_are_exact",
    },
    "tensor-offset-drift": {
        "command": "rust-workspace",
        "test_name": "verify::tests::tensor_dtype_shape_and_offsets_are_exact",
    },
    "nonfinite-f16-payload": {
        "command": "rust-workspace",
        "test_name": (
            "verify::tests::non_finite_f16_payload_is_rejected_after_structural_validation"
        ),
    },
    "tokenizer-pipeline-drift": {
        "command": "rust-workspace",
        "test_name": (
            "tokenizer::tests::"
            "exact_document_contract_accepts_only_the_pinned_pipeline_and_domains"
        ),
    },
    "tokenizer-vocabulary-drift": {
        "command": "rust-workspace",
        "test_name": (
            "tokenizer::tests::"
            "exact_document_contract_accepts_only_the_pinned_pipeline_and_domains"
        ),
    },
    "lossy-token-sequence": {
        "command": "rust-workspace",
        "test_name": (
            "tokenizer::tests::"
            "strict_byte_decoder_rejects_lossy_sequences_and_preserves_valid_replacement_text"
        ),
    },
}
TEST_COMMANDS = {
    "rust-workspace": [
        "cargo",
        "test",
        "--locked",
        "--offline",
        "--workspace",
        "--all-targets",
    ],
    "python-fetch-assets": [
        "python3",
        "benchmarks/test_fetch_public_model_assets.py",
        "-v",
    ],
    "python-fetch-failure-matrix": [
        "python3",
        "benchmarks/test_public_model_fetch_failure_matrix.py",
        "-v",
    ],
    "python-tokenizer-reference": [
        "python3",
        "benchmarks/test_public_tokenizer_reference.py",
        "-v",
    ],
    "python-checkpoint-reference": [
        "python3",
        "benchmarks/test_public_checkpoint_reference.py",
        "-v",
    ],
}
TIMING_PHASES = {
    "asset_preparation",
    "checkpoint_offline_verification",
    "checkpoint_reference",
    "tokenizer_reference",
    "tokenizer_production",
    "failure_matrix",
    "production_tests",
}


class EvidenceError(RuntimeError):
    """Retained v0.32 evidence violates the finite proof contract."""


def fail(message: str) -> NoReturn:
    raise EvidenceError(message)


def exact_keys(value: Any, keys: set[str], label: str) -> Mapping[str, Any]:
    if not isinstance(value, Mapping):
        fail(f"{label} must be an object")
    actual = set(value)
    if actual != keys:
        fail(
            f"{label} keys drifted: missing={sorted(keys - actual)}, "
            f"extra={sorted(actual - keys)}"
        )
    return value


def load_json(
    directory: Path,
    name: str,
    *,
    canonical: bool = True,
) -> tuple[dict[str, Any], bytes]:
    data = read_bounded_regular(
        directory / name,
        maximum_bytes=MAX_EVIDENCE_FILE_BYTES,
        label=name,
    )
    value = strict_json_loads(data, label=name)
    if not isinstance(value, dict):
        fail(f"{name} root must be an object")
    if canonical and canonical_json_bytes(value) != data:
        fail(f"{name} is not canonical JSON")
    return value, data


def validate_inventory(directory: Path, require_manifest: bool) -> set[str]:
    try:
        metadata = os.lstat(directory)
    except OSError as error:
        fail(f"evidence directory cannot be inspected safely: {error}")
    if not stat.S_ISDIR(metadata.st_mode):
        fail("evidence directory is not a real directory")
    observed: set[str] = set()
    for entry in os.scandir(directory):
        if entry.name in observed:
            fail("evidence directory contains duplicate names")
        observed.add(entry.name)
        entry_metadata = entry.stat(follow_symlinks=False)
        if not stat.S_ISREG(entry_metadata.st_mode):
            fail(f"retained evidence entry is not a regular file: {entry.name}")
        if not 0 < entry_metadata.st_size <= MAX_EVIDENCE_FILE_BYTES:
            fail(f"retained evidence entry is outside its byte bound: {entry.name}")
    expected = EXPECTED_FILES if require_manifest else NON_MANIFEST_FILES
    if observed != expected:
        fail(
            f"retained inventory drifted: missing={sorted(expected - observed)}, "
            f"extra={sorted(observed - expected)}"
        )
    return observed


def expected_source(lock_sha256: str = SOURCE_LOCK_SHA256) -> dict[str, Any]:
    return {
        "repository": REPOSITORY,
        "revision": REVISION,
        "license": LICENSE,
        "lock_schema": LOCK_SCHEMA,
        "lock_sha256": lock_sha256,
    }


def validate_source_lock(directory: Path) -> tuple[dict[str, Any], bytes]:
    lock, lock_bytes = load_json(directory, "source-lock.json", canonical=False)
    validate_lock(lock)
    digest = hashlib.sha256(lock_bytes).hexdigest()
    if digest != SOURCE_LOCK_SHA256:
        fail("retained source lock bytes drifted")
    return lock, lock_bytes


def validate_contract(
    document: dict[str, Any],
    *,
    run_id: str,
) -> str:
    contract = exact_keys(
        document,
        {
            "schema",
            "milestone",
            "run_id",
            "scope",
            "source",
            "cache_mode",
            "runtime_commands",
            "external_network_authority",
            "public_model_network_authority",
            "regression_fixture_network",
            "proof_owned_long_lived_topology",
            "public_model_forward_passes",
            "public_model_generations",
            "public_model_services_started",
            "retained_weight_bytes",
        },
        "proof contract",
    )
    if (
        contract["schema"] != CONTRACT_SCHEMA
        or contract["milestone"] != MILESTONE
        or contract["run_id"] != run_id
        or contract["scope"]
        != "pinned-checkpoint-artifact-verification-and-production-tokenizer-parity"
        or contract["source"] != expected_source()
        or contract["runtime_commands"] != ["inspect", "tokenize"]
        or contract["proof_owned_long_lived_topology"] != []
        or contract["public_model_forward_passes"] != 0
        or contract["public_model_generations"] != 0
        or contract["public_model_services_started"] != 0
        or contract["retained_weight_bytes"] != 0
    ):
        fail("proof contract values drifted")
    mode = contract["cache_mode"]
    if mode not in {"clean-online", "offline-warm-cache"}:
        fail("proof cache mode is unsupported")
    acquisition_network = (
        "pinned_revision_https_only" if mode == "clean-online" else "disabled"
    )
    expected_external_network = {
        "asset_preparation": acquisition_network,
        "post_acquisition": "disabled",
        "hub_post_acquisition": "disabled",
        "cargo": "disabled",
    }
    expected_public_model_network = {
        "asset_preparation": acquisition_network,
        "checkpoint_inspection": "disabled",
        "tokenizer_reference": "disabled",
        "production_tokenizer": "disabled",
    }
    expected_fixture_network = {
        "loopback": "allowed_ephemeral_regression_fixtures",
        "external": "disabled",
        "public_model": "disabled",
    }
    if (
        contract["external_network_authority"] != expected_external_network
        or contract["public_model_network_authority"]
        != expected_public_model_network
        or contract["regression_fixture_network"] != expected_fixture_network
    ):
        fail("proof network-authority boundary drifted")
    return mode


def validate_acquisition(
    document: dict[str, Any],
    *,
    run_id: str,
    mode: str,
    lock: dict[str, Any],
) -> None:
    acquisition = exact_keys(
        document,
        {
            "schema",
            "run_id",
            "mode",
            "source",
            "cache_key",
            "immutable_urls",
            "file_count",
            "total_bytes",
            "files",
            "passes",
            "atomic_publication_commit_point",
            "retained_weight_bytes",
        },
        "artifact acquisition",
    )
    urls = [
        f"https://huggingface.co/{REPOSITORY}/resolve/{REVISION}/{name}"
        for name, _size, _digest in LOCKED_FILES
    ]
    expected_statuses = (
        ["downloaded", "warm-cache", "offline-verified"]
        if mode == "clean-online"
        else ["offline-verified", "offline-verified", "offline-verified"]
    )
    expected_passes = [
        {"name": name, "status": status}
        for name, status in zip(("initial", "replay", "offline"), expected_statuses)
    ]
    if (
        acquisition["schema"] != ACQUISITION_SCHEMA
        or acquisition["run_id"] != run_id
        or acquisition["mode"] != mode
        or acquisition["source"] != expected_source()
        or acquisition["cache_key"] != CACHE_KEY
        or acquisition["immutable_urls"] != urls
        or acquisition["file_count"] != 6
        or acquisition["total_bytes"] != TOTAL_ASSET_BYTES
        or acquisition["files"] != lock["files"]
        or acquisition["passes"] != expected_passes
        or acquisition["atomic_publication_commit_point"] != "directory_rename"
        or acquisition["retained_weight_bytes"] != 0
    ):
        fail("artifact acquisition evidence drifted")


def expected_verification_report() -> dict[str, Any]:
    architecture_keys = {
        "model_type",
        "architecture",
        "vocab_size",
        "max_position_embeddings",
        "hidden_size",
        "intermediate_size",
        "num_attention_heads",
        "num_hidden_layers",
        "bos_token_id",
        "eos_token_id",
        "hidden_act",
        "torch_dtype",
    }
    architecture = {
        key: value for key, value in CANONICAL_ARCHITECTURE.items() if key in architecture_keys
    }
    return {
        "schema": "inferlab.public-model-verification.v1",
        "repository": REPOSITORY,
        "revision": REVISION,
        "license": LICENSE,
        "verified_files": 6,
        "verified_bytes": TOTAL_ASSET_BYTES,
        "architecture": architecture,
        "checkpoint": {
            "file": CANONICAL_CHECKPOINT["file"],
            "format": CANONICAL_CHECKPOINT["format"],
            "sha256": CHECKPOINT_FILE_SHA256,
            "header_bytes": CANONICAL_CHECKPOINT["header_bytes"],
            "header_sha256": CANONICAL_CHECKPOINT["header_sha256"],
            "dtype": CANONICAL_CHECKPOINT["dtype"],
            "tensor_count": CANONICAL_CHECKPOINT["tensor_count"],
            "element_count": CANONICAL_CHECKPOINT["element_count"],
            "data_bytes": CANONICAL_CHECKPOINT["data_bytes"],
            "finite_payload": True,
        },
    }


def validate_reference_check(
    observed: dict[str, Any],
    expected: dict[str, Any],
    label: str,
) -> None:
    if not exact_json_equal(observed, expected):
        fail(f"{label} retained check differs from dependency-free replay")


def validate_runtime_boundary(document: dict[str, Any], *, run_id: str) -> None:
    expected = {
        "schema": RUNTIME_BOUNDARY_SCHEMA,
        "run_id": run_id,
        "rust_binary_commands": ["inspect", "tokenize"],
        "rust_binary_fetch_command": False,
        "rust_binary_network_client": False,
        "model_artifacts_direct_normal_dependencies": [
            "libc",
            "safetensors",
            "serde",
            "serde_json",
            "sha2",
            "tokenizers",
        ],
        "tokenizer_dependency": {
            "package": "tokenizers",
            "version": "=0.23.1",
            "default_features": False,
            "features": ["fancy-regex"],
        },
        "offline_environment": OFFLINE_ENVIRONMENT,
        "cargo_net_offline": True,
        "cargo_term_color": "never",
        "checkpoint_inspection_reports": 2,
        "checkpoint_inspection_byte_identical": True,
        "production_tokenizer_probes": 2,
        "production_tokenizer_byte_identical": True,
        "ambient_hub_cache_discovery": False,
    }
    if not exact_json_equal(document, expected):
        fail("runtime network and determinism boundary drifted")


def validate_process_accounting(
    document: dict[str, Any],
    *,
    run_id: str,
    mode: str,
) -> None:
    acquisition_network = (
        "pinned_revision_https_only" if mode == "clean-online" else "disabled"
    )
    expected = {
        "schema": PROCESS_SCHEMA,
        "run_id": run_id,
        "accounting_method": "static_synchronous_role_contract",
        "process_identity_sampling": False,
        "public_model_services_started": 0,
        "proof_owned_background_jobs_started": 0,
        "proof_owned_long_lived_topology": [],
        "regression_fixtures": {
            "ephemeral_child_processes": "allowed",
            "ephemeral_loopback_listeners": "allowed",
            "classification": "outside_public_model_runtime_continuity",
        },
        "synchronous_roles": [
            {
                "name": "asset-preparation",
                "external_network_authority": acquisition_network,
                "public_model_network_authority": acquisition_network,
                "loopback_fixture_authority": "disabled",
            },
            {
                "name": "checkpoint-reference",
                "external_network_authority": "disabled",
                "public_model_network_authority": "disabled",
                "loopback_fixture_authority": "disabled",
            },
            {
                "name": "inferlab-model-inspect",
                "external_network_authority": "disabled",
                "public_model_network_authority": "disabled",
                "loopback_fixture_authority": "disabled",
            },
            {
                "name": "maintained-tokenizer-reference",
                "external_network_authority": "disabled",
                "public_model_network_authority": "disabled",
                "loopback_fixture_authority": "disabled",
            },
            {
                "name": "proof-checkers",
                "external_network_authority": "disabled",
                "public_model_network_authority": "disabled",
                "loopback_fixture_authority": "disabled",
            },
            {
                "name": "regression-tests",
                "external_network_authority": "disabled",
                "public_model_network_authority": "disabled",
                "loopback_fixture_authority": "allowed_ephemeral",
            },
        ],
    }
    if not exact_json_equal(document, expected):
        fail("static process-role accounting drifted")


def validate_source_integrity(
    document: dict[str, Any],
    *,
    run_id: str,
) -> str:
    source = exact_keys(
        document,
        {
            "schema",
            "run_id",
            "runtime_base_commit",
            "runtime_paths",
            "runtime_paths_clean_against_base",
            "workspace_release_overlay",
            "proof_tool_files",
        },
        "proof source integrity",
    )
    runtime_base_commit = source["runtime_base_commit"]
    if (
        source["schema"] != SOURCE_INTEGRITY_SCHEMA
        or source["run_id"] != run_id
        or not isinstance(runtime_base_commit, str)
        or GIT_COMMIT.fullmatch(runtime_base_commit) is None
        or source["runtime_paths"] != list(RUNTIME_BASE_PATHS)
        or source["runtime_paths_clean_against_base"] is not True
    ):
        fail("runtime source attribution drifted")

    overlay = exact_keys(
        source["workspace_release_overlay"],
        {
            "status",
            "base_workspace_version",
            "observed_workspace_version",
            "cargo_manifest_version_replacements",
            "cargo_lock_version_replacements",
        },
        "workspace release overlay",
    )
    allowed_overlays = (
        {
            "status": "none",
            "base_workspace_version": "0.32.0",
            "observed_workspace_version": "0.32.0",
            "cargo_manifest_version_replacements": 0,
            "cargo_lock_version_replacements": 0,
        },
        {
            "status": "workspace_version_only",
            "base_workspace_version": "0.31.0",
            "observed_workspace_version": "0.32.0",
            "cargo_manifest_version_replacements": 1,
            "cargo_lock_version_replacements": 12,
        },
    )
    if not any(exact_json_equal(overlay, allowed) for allowed in allowed_overlays):
        fail("workspace release overlay is not the bounded v0.32 version transition")

    entries = source["proof_tool_files"]
    if not isinstance(entries, list) or len(entries) != len(PROOF_TOOL_FILES):
        fail("proof-tool source inventory count drifted")
    project_root = Path(__file__).resolve().parent.parent
    expected_entries: list[dict[str, Any]] = []
    for name in PROOF_TOOL_FILES:
        path = project_root / name
        try:
            metadata = os.lstat(path)
        except OSError as error:
            fail(f"proof-tool source cannot be inspected safely: {name}: {error}")
        if not stat.S_ISREG(metadata.st_mode):
            fail(f"proof-tool source is not a regular file: {name}")
        if not 0 < metadata.st_size <= MAX_EVIDENCE_FILE_BYTES:
            fail(f"proof-tool source is outside its byte bound: {name}")
        data = path.read_bytes()
        expected_entries.append(
            {
                "name": name,
                "bytes": len(data),
                "sha256": hashlib.sha256(data).hexdigest(),
            }
        )
    if not exact_json_equal(entries, expected_entries):
        fail("proof-tool source digest inventory drifted")
    return runtime_base_commit


def validate_environment(
    document: dict[str, Any],
    *,
    run_id: str,
    runtime_base_commit: str,
) -> None:
    environment = exact_keys(
        document,
        {
            "schema",
            "run_id",
            "runtime_base_commit",
            "operating_system",
            "architecture",
            "python",
            "rustc",
            "cargo",
            "reference_package",
            "offline_environment",
            "cargo_net_offline",
            "cargo_term_color",
        },
        "proof environment",
    )
    scalar_fields = ("operating_system", "architecture", "python", "rustc", "cargo")
    if (
        environment["schema"] != ENVIRONMENT_SCHEMA
        or environment["run_id"] != run_id
        or environment["runtime_base_commit"] != runtime_base_commit
        or any(
            not isinstance(environment[field], str)
            or not environment[field]
            or len(environment[field]) > 160
            or "\n" in environment[field]
            for field in scalar_fields
        )
        or environment["reference_package"]
        != {"package": REFERENCE_PACKAGE, "version": REFERENCE_VERSION}
        or environment["offline_environment"] != OFFLINE_ENVIRONMENT
        or environment["cargo_net_offline"] is not True
        or environment["cargo_term_color"] != "never"
    ):
        fail("proof environment metadata drifted")


def validate_tests(document: dict[str, Any], *, run_id: str) -> int:
    tests = exact_keys(
        document,
        {
            "schema",
            "run_id",
            "all_passed",
            "total_tests",
            "commands",
            "regression_coverage",
        },
        "production tests",
    )
    commands = tests["commands"]
    if not isinstance(commands, list) or len(commands) != len(TEST_COMMANDS):
        fail("production test command count drifted")
    observed_names: list[str] = []
    total = 0
    for command in commands:
        item = exact_keys(
            command,
            {"name", "argv", "status", "test_count", "duration_ms"},
            "production test command",
        )
        name = item["name"]
        if name not in TEST_COMMANDS or item["argv"] != TEST_COMMANDS[name]:
            fail("production test identity drifted")
        if (
            item["status"] != "passed"
            or type(item["test_count"]) is not int
            or item["test_count"] <= 0
            or type(item["duration_ms"]) is not int
            or item["duration_ms"] <= 0
        ):
            fail(f"production test command did not pass: {name}")
        observed_names.append(name)
        total += item["test_count"]
    if (
        observed_names != list(TEST_COMMANDS)
        or tests["schema"] != PRODUCTION_TEST_SCHEMA
        or tests["run_id"] != run_id
        or tests["all_passed"] is not True
        or tests["total_tests"] != total
    ):
        fail("production test summary drifted")
    expected_coverage = [
        {
            "failure_case": name,
            "command": binding["command"],
            "test_name": binding["test_name"],
            "observed_count": 1,
        }
        for name, binding in REGRESSION_FAILURES.items()
    ]
    if not exact_json_equal(tests["regression_coverage"], expected_coverage):
        fail("production regression coverage binding drifted")
    return total


def validate_failures(document: dict[str, Any], *, run_id: str) -> int:
    failures = exact_keys(
        document,
        {"schema", "run_id", "all_passed", "direct_cases", "regression_cases"},
        "failure matrix",
    )
    direct = failures["direct_cases"]
    regression = failures["regression_cases"]
    if not isinstance(direct, list) or not isinstance(regression, list):
        fail("failure matrix cases must be arrays")
    expected_direct = [
        {
            "name": name,
            "method": "proof-owned-real-copy",
            "expected_error_kind": error_kind,
            "observed_error_kind": error_kind,
            "exit_code": 1,
            "passed": True,
            "retained_payload_bytes": 0,
        }
        for name, error_kind in DIRECT_FAILURES.items()
    ]
    expected_regression = [
        {
            "name": name,
            "method": "focused-regression-suite",
            "command": binding["command"],
            "test_name": binding["test_name"],
            "observed_count": 1,
            "passed": True,
            "retained_payload_bytes": 0,
        }
        for name, binding in REGRESSION_FAILURES.items()
    ]
    if (
        failures["schema"] != FAILURE_SCHEMA
        or failures["run_id"] != run_id
        or failures["all_passed"] is not True
        or not exact_json_equal(direct, expected_direct)
        or not exact_json_equal(regression, expected_regression)
    ):
        fail("failure matrix results drifted")
    return len(direct) + len(regression)


def validate_timings(document: dict[str, Any], *, run_id: str) -> int:
    timings = exact_keys(
        document,
        {"schema", "run_id", "clock", "phases", "total_capture_ms"},
        "proof timings",
    )
    phases = timings["phases"]
    if not isinstance(phases, list) or len(phases) != len(TIMING_PHASES):
        fail("proof timing phase count drifted")
    observed_names: list[str] = []
    total = 0
    for phase in phases:
        item = exact_keys(phase, {"name", "duration_ms"}, "proof timing phase")
        if (
            item["name"] not in TIMING_PHASES
            or type(item["duration_ms"]) is not int
            or not 1 <= item["duration_ms"] <= 3_600_000
        ):
            fail("proof timing phase is invalid")
        observed_names.append(item["name"])
        total += item["duration_ms"]
    if (
        timings["schema"] != TIMINGS_SCHEMA
        or timings["run_id"] != run_id
        or timings["clock"] != "monotonic"
        or observed_names != sorted(TIMING_PHASES)
        or timings["total_capture_ms"] != total
    ):
        fail("proof timing summary drifted")
    return total


def validate_sanitizer(document: dict[str, Any], *, run_id: str) -> None:
    scanned = sorted(NON_MANIFEST_FILES - {"sanitizer.json"})
    expected = {
        "schema": SANITIZER_SCHEMA,
        "run_id": run_id,
        "status": "passed",
        "scanned_files": scanned,
        "scanned_file_count": len(scanned),
        "utf8_text_files": len(scanned),
        "absolute_host_path_matches": 0,
        "exact_runtime_path_matches": 0,
        "private_material_matches": 0,
        "retained_weight_files": 0,
        "retained_weight_bytes": 0,
    }
    if not exact_json_equal(document, expected):
        fail("retained-evidence sanitizer result drifted")


def validate_manifest(directory: Path, document: dict[str, Any]) -> None:
    manifest = exact_keys(document, {"schema", "file_count", "files"}, "manifest")
    entries = manifest["files"]
    if not isinstance(entries, list) or len(entries) != len(NON_MANIFEST_FILES):
        fail("manifest file count drifted")
    expected_entries: list[dict[str, Any]] = []
    for name in sorted(NON_MANIFEST_FILES):
        data = read_bounded_regular(
            directory / name,
            maximum_bytes=MAX_EVIDENCE_FILE_BYTES,
            label=f"manifest source {name}",
        )
        expected_entries.append(
            {
                "name": name,
                "bytes": len(data),
                "sha256": hashlib.sha256(data).hexdigest(),
            }
        )
    if (
        manifest["schema"] != MANIFEST_SCHEMA
        or manifest["file_count"] != len(expected_entries)
        or not exact_json_equal(entries, expected_entries)
    ):
        fail("manifest inventory or digest drifted")


def assertion(name: str, **observations: Any) -> dict[str, Any]:
    return {
        "name": name,
        "passed": True,
        "observations": observations,
    }


def check_evidence(directory: Path, *, require_manifest: bool) -> dict[str, Any]:
    inventory = validate_inventory(directory, require_manifest)
    lock, lock_bytes = validate_source_lock(directory)
    lock_sha256 = hashlib.sha256(lock_bytes).hexdigest()

    contract, _ = load_json(directory, "proof-contract.json")
    run_id = contract.get("run_id")
    if not isinstance(run_id, str) or RUN_ID.fullmatch(run_id) is None:
        fail("proof run ID is invalid")
    mode = validate_contract(contract, run_id=run_id)

    acquisition, _ = load_json(directory, "artifact-acquisition.json")
    validate_acquisition(acquisition, run_id=run_id, mode=mode, lock=lock)

    verification, verification_bytes = load_json(
        directory,
        "checkpoint-verification.json",
        canonical=False,
    )
    if not exact_json_equal(verification, expected_verification_report()):
        fail("offline checkpoint verification report drifted")

    checkpoint_reference, checkpoint_reference_bytes = load_json(
        directory,
        "checkpoint-reference.json",
    )
    checkpoint_counts = validate_checkpoint_reference(
        checkpoint_reference,
        lock_sha256=lock_sha256,
    )
    checkpoint_replay = check_checkpoint_reference(
        directory / "checkpoint-reference.json",
        directory / "source-lock.json",
    )
    checkpoint_check, _ = load_json(directory, "checkpoint-reference-check.json")
    validate_reference_check(
        checkpoint_check,
        checkpoint_replay,
        "checkpoint reference",
    )

    tokenizer_reference, tokenizer_reference_bytes = load_json(
        directory,
        "tokenizer-reference.json",
    )
    tokenizer_counts = validate_reference(
        tokenizer_reference,
        lock_sha256=lock_sha256,
    )
    tokenizer_replay = check_tokenizer_reference(
        directory / "tokenizer-reference.json",
        directory / "source-lock.json",
    )
    tokenizer_check, _ = load_json(directory, "tokenizer-reference-check.json")
    validate_reference_check(tokenizer_check, tokenizer_replay, "tokenizer reference")

    tokenizer_production, tokenizer_production_bytes = load_json(
        directory,
        "tokenizer-production.json",
    )
    production_counts = validate_result(
        tokenizer_production,
        reference=tokenizer_reference,
    )

    runtime, _ = load_json(directory, "runtime-boundary.json")
    validate_runtime_boundary(runtime, run_id=run_id)
    process_accounting, _ = load_json(directory, "process-continuity.json")
    validate_process_accounting(process_accounting, run_id=run_id, mode=mode)
    source_integrity, _ = load_json(directory, "source-integrity.json")
    runtime_base_commit = validate_source_integrity(
        source_integrity,
        run_id=run_id,
    )
    environment, _ = load_json(directory, "environment.json")
    validate_environment(
        environment,
        run_id=run_id,
        runtime_base_commit=runtime_base_commit,
    )
    tests, _ = load_json(directory, "production-tests.json")
    production_test_count = validate_tests(tests, run_id=run_id)
    failures, _ = load_json(directory, "failure-matrix.json")
    failure_case_count = validate_failures(failures, run_id=run_id)
    timings, _ = load_json(directory, "timings.json")
    capture_ms = validate_timings(timings, run_id=run_id)
    sanitizer, _ = load_json(directory, "sanitizer.json")
    validate_sanitizer(sanitizer, run_id=run_id)

    if require_manifest:
        manifest, _ = load_json(directory, "manifest.json")
        validate_manifest(directory, manifest)

    assertions = [
        assertion(
            "the retained inventory is exact, bounded, regular, and manifest-last",
            non_manifest_file_count=len(NON_MANIFEST_FILES),
            manifest_written_last=True,
        ),
        assertion(
            "the proof stops at checkpoint verification and tokenizer parity",
            public_model_forward_passes=0,
            public_model_generations=0,
            public_model_services_started=0,
            retained_weight_bytes=0,
        ),
        assertion(
            "one immutable repository revision and six exact files define the cache",
            file_count=6,
            total_bytes=TOTAL_ASSET_BYTES,
        ),
        assertion(
            "asset preparation preserves the external and public-model authority boundary",
            cache_mode=mode,
            immutable_url_count=6,
            loopback_regression_fixtures="allowed_ephemeral",
        ),
        assertion(
            "offline checkpoint inspection is byte-stable and path-free",
            report_bytes=len(verification_bytes),
            report_sha256=hashlib.sha256(verification_bytes).hexdigest(),
        ),
        assertion(
            "an independent bounded parser accounts for every exact F16 tensor",
            tensor_count=checkpoint_counts["tensors"],
            element_count=checkpoint_counts["elements"],
            data_bytes=checkpoint_counts["data_bytes"],
            reference_bytes=len(checkpoint_reference_bytes),
        ),
        assertion(
            "the maintained tokenizer oracle is exact, local, and version-pinned",
            package=f"{REFERENCE_PACKAGE}=={REFERENCE_VERSION}",
            reference_bytes=len(tokenizer_reference_bytes),
            encode_cases=tokenizer_counts["encode_cases"],
            relationships=tokenizer_counts["relationships"],
        ),
        assertion(
            "production encode and decode results equal every retained oracle vector",
            encode_cases=production_counts["encode_cases"],
            decode_cases=production_counts["decode_cases"],
            result_bytes=len(tokenizer_production_bytes),
        ),
        assertion(
            "literal-special policy and postprocessor insertion remain independent",
            semantic_modes=2,
            postprocessor_modes=2,
        ),
        assertion(
            "strict decoding rejects lossy, alignment-only, and out-of-range IDs",
            decode_rejections=production_counts["decode_rejections"],
            request_rejections=production_counts["request_rejections"],
        ),
        assertion(
            "Unicode, embedded NUL, whitespace, scripts, emoji, and context edges pass",
            maximum_context_tokens=2_048,
            rejected_context_tokens=2_049,
        ),
        assertion(
            "the complete acquisition, artifact, and tokenizer failure matrix passes",
            failure_cases=failure_case_count,
        ),
        assertion(
            "workspace, fetcher, oracle, and checkpoint regression suites pass",
            production_tests=production_test_count,
        ),
        assertion(
            "static role accounting starts no proof-owned background topology",
            proof_owned_background_jobs_started=0,
            public_model_services_started=0,
            process_identity_sampling=False,
        ),
        assertion(
            "the runtime base and every proof-tool byte are attributable",
            runtime_base_commit=runtime_base_commit,
            runtime_paths_clean_against_base=True,
            proof_tool_files=len(PROOF_TOOL_FILES),
        ),
        assertion(
            "retained evidence contains no host path, private material, or weight payload",
            scanned_files=sanitizer["scanned_file_count"],
            retained_weight_bytes=0,
        ),
        assertion(
            "all measured capture phases are finite and explicitly retained",
            phase_count=len(TIMING_PHASES),
            total_capture_ms=capture_ms,
        ),
    ]
    return {
        "schema": ASSERTION_SCHEMA,
        "run_id": run_id,
        "all_passed": True,
        "passed": len(assertions),
        "total": len(assertions),
        "assertions": assertions,
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--require-manifest", action="store_true")
    parser.add_argument("--output", type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        result = check_evidence(
            args.evidence_dir,
            require_manifest=args.require_manifest,
        )
        encoded = canonical_json_bytes(result)
        if args.output is None:
            sys.stdout.buffer.write(encoded)
        else:
            atomic_write(args.output, encoded)
    except (OSError, EvidenceError, ProbeError, ReferenceValidationError) as error:
        print(f"v0.32 retained-evidence check failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
