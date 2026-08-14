#!/usr/bin/env python3
"""Focused filesystem tests for the v0.32 public-asset cache boundary."""

from __future__ import annotations

import errno
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

import fetch_public_model_assets as assets


class AssetCacheTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.real_lock_path = (
            Path(__file__).resolve().parent.parent
            / "models"
            / "public"
            / "pythia-14m-v0.32.lock.json"
        )
        self.payloads = {
            name: f"fixture:{name}\n".encode("utf-8") for name in assets.EXPECTED_FILES
        }
        self.source_lock = assets.SourceLock(
            repository=assets.REPOSITORY,
            revision=assets.REVISION,
            license=assets.LICENSE,
            files=tuple(
                assets.FileSpec(
                    name=name,
                    bytes=len(self.payloads[name]),
                    sha256=hashlib.sha256(self.payloads[name]).hexdigest(),
                )
                for name in assets.EXPECTED_FILES
            ),
        )
        self.cache = self.root / "cache"

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def populate_cache(self, directory: Path | None = None) -> Path:
        directory = self.cache if directory is None else directory
        directory.mkdir(mode=0o700, parents=True)
        os.chmod(directory, 0o700)
        for name, payload in self.payloads.items():
            path = directory / name
            path.write_bytes(payload)
            os.chmod(path, 0o600)
        return directory

    def test_exact_cache_verifies(self) -> None:
        self.populate_cache()
        assets.verify_cache(self.cache, self.source_lock)

    def test_missing_cache_fails(self) -> None:
        with self.assertRaises(assets.AssetError):
            assets.verify_cache(self.cache, self.source_lock)

    def test_offline_missing_cache_fails_without_fetch(self) -> None:
        with self.assertRaisesRegex(assets.AssetError, "offline verification"):
            assets.prepare_cache(self.root / "offline", self.source_lock, True, 1.0)

    def test_offline_warm_cache_verifies(self) -> None:
        cache_root = self.root / "offline"
        cache = cache_root / "pythia-14m" / assets.REVISION
        self.populate_cache(cache)
        status, cache_key = assets.prepare_cache(
            cache_root,
            self.source_lock,
            True,
            1.0,
        )
        self.assertEqual(status, "offline-verified")
        self.assertEqual(cache_key, f"pythia-14m/{assets.REVISION}")

    def test_extra_entry_fails_exact_inventory(self) -> None:
        self.populate_cache()
        extra = self.cache / "unexpected"
        extra.write_bytes(b"unexpected")
        os.chmod(extra, 0o600)
        with self.assertRaisesRegex(assets.AssetError, "inventory differs"):
            assets.verify_cache(self.cache, self.source_lock)

    def test_same_size_corruption_fails_digest(self) -> None:
        self.populate_cache()
        target = self.cache / assets.EXPECTED_FILES[0]
        target.write_bytes(b"x" * len(self.payloads[target.name]))
        os.chmod(target, 0o600)
        with self.assertRaisesRegex(assets.AssetError, "identity mismatch"):
            assets.verify_cache(self.cache, self.source_lock)

    def test_symlink_cache_entry_fails(self) -> None:
        self.populate_cache()
        name = assets.EXPECTED_FILES[0]
        target = self.cache / name
        target.unlink()
        target.symlink_to(self.real_lock_path)
        with self.assertRaises(assets.AssetError):
            assets.verify_cache(self.cache, self.source_lock)

    @unittest.skipUnless(hasattr(os, "mkfifo"), "FIFO fixtures require POSIX")
    def test_fifo_cache_entry_fails_without_blocking(self) -> None:
        self.populate_cache()
        name = assets.EXPECTED_FILES[0]
        target = self.cache / name
        target.unlink()
        os.mkfifo(target, 0o600)
        with self.assertRaisesRegex(assets.AssetError, "not a regular file"):
            assets.verify_cache(self.cache, self.source_lock)

    def test_symlink_cache_root_fails(self) -> None:
        real = self.root / "real"
        real.mkdir()
        alias = self.root / "alias"
        alias.symlink_to(real, target_is_directory=True)
        with self.assertRaisesRegex(assets.AssetError, "cache root"):
            assets.normalize_cache_root(alias)

    def test_non_finite_json_constant_fails(self) -> None:
        raw = json.loads(self.real_lock_path.read_text(encoding="utf-8"))
        raw["checkpoint"] = {"invalid": float("nan")}
        lock_path = self.root / "non-finite-lock.json"
        lock_path.write_text(json.dumps(raw), encoding="utf-8")
        with self.assertRaisesRegex(assets.AssetError, "non-finite JSON constant"):
            assets.load_source_lock(lock_path)

    def test_checked_in_lock_loads_exact_identity(self) -> None:
        source_lock = assets.load_source_lock(self.real_lock_path)
        self.assertEqual(source_lock.revision, assets.REVISION)
        self.assertEqual(source_lock.total_bytes, 30_274_495)

    def test_revision_size_and_digest_drift_fail(self) -> None:
        original = json.loads(self.real_lock_path.read_text(encoding="utf-8"))
        mutations = (
            ("revision", lambda value: value["source"].update(revision="b" * 40)),
            ("size", lambda value: value["files"][0].update(bytes=10_561)),
            ("digest", lambda value: value["files"][0].update(sha256="0" * 64)),
        )
        for label, mutate in mutations:
            with self.subTest(label=label):
                document = json.loads(json.dumps(original))
                mutate(document)
                lock_path = self.root / f"{label}-drift.json"
                lock_path.write_text(json.dumps(document), encoding="utf-8")
                with self.assertRaises(assets.AssetError):
                    assets.load_source_lock(lock_path)

    def test_checkpoint_and_architecture_unknown_fields_fail(self) -> None:
        original = json.loads(self.real_lock_path.read_text(encoding="utf-8"))
        for section in ("checkpoint", "architecture"):
            with self.subTest(section=section):
                document = json.loads(json.dumps(original))
                document[section]["unexpected"] = "rejected"
                lock_path = self.root / f"{section}-unknown.json"
                lock_path.write_text(json.dumps(document), encoding="utf-8")
                with self.assertRaisesRegex(assets.AssetError, "keys differ"):
                    assets.load_source_lock(lock_path)

    def test_checkpoint_and_architecture_value_drift_fail(self) -> None:
        original = json.loads(self.real_lock_path.read_text(encoding="utf-8"))
        mutations = (
            ("checkpoint-count", "checkpoint", "tensor_count", 75),
            ("architecture-size", "architecture", "hidden_size", 129),
            ("architecture-type", "architecture", "attention_bias", 1),
        )
        for label, section, key, value in mutations:
            with self.subTest(label=label):
                document = json.loads(json.dumps(original))
                document[section][key] = value
                lock_path = self.root / f"{label}.json"
                lock_path.write_text(json.dumps(document), encoding="utf-8")
                with self.assertRaisesRegex(assets.AssetError, "contract value differs"):
                    assets.load_source_lock(lock_path)

    def test_non_finite_exponent_fails(self) -> None:
        original = self.real_lock_path.read_text(encoding="utf-8")
        mutated = original.replace(
            '"layer_norm_eps": 0.00001',
            '"layer_norm_eps": 1e309',
        )
        self.assertNotEqual(mutated, original)
        lock_path = self.root / "non-finite-exponent.json"
        lock_path.write_text(mutated, encoding="utf-8")
        with self.assertRaisesRegex(assets.AssetError, "non-finite JSON number"):
            assets.load_source_lock(lock_path)

    def test_fetch_lock_contention_honors_acquisition_deadline(self) -> None:
        parent = self.root / "contended"
        parent.mkdir(mode=0o700)
        lock_path = parent / ".fetch.lock"
        child = subprocess.Popen(
            [
                sys.executable,
                "-c",
                """
import fcntl
import os
import sys
import time

descriptor = os.open(sys.argv[1], os.O_RDWR | os.O_CREAT, 0o600)
os.fchmod(descriptor, 0o600)
fcntl.flock(descriptor, fcntl.LOCK_EX)
print("locked", flush=True)
time.sleep(10)
""",
                str(lock_path),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            self.assertIsNotNone(child.stdout)
            self.assertEqual(child.stdout.readline().strip(), "locked")
            started = time.monotonic()
            with self.assertRaisesRegex(
                assets.AssetError,
                "asset acquisition exceeded its 0.2-second deadline",
            ):
                assets.install_cache(
                    parent / "generation",
                    self.source_lock,
                    0.2,
                )
            elapsed = time.monotonic() - started
            self.assertGreaterEqual(elapsed, 0.15)
            self.assertLess(elapsed, 2.0)
            self.assertFalse((parent / "generation").exists())
        finally:
            child.terminate()
            child.communicate(timeout=5)

    def test_pre_rename_download_failure_leaves_no_final_generation(self) -> None:
        def fail_download(*_arguments: object, **_keywords: object) -> None:
            raise assets.AssetError("injected bounded download failure")

        with mock.patch.object(assets, "download_file", side_effect=fail_download):
            with self.assertRaisesRegex(assets.AssetError, "bounded download failure"):
                assets.install_cache(self.cache, self.source_lock, 1.0)
        self.assertFalse(self.cache.exists())
        self.assertEqual(
            [path.name for path in self.root.iterdir() if ".staging-" in path.name],
            [],
        )

    def test_post_rename_failure_is_indeterminate_and_reconciles(self) -> None:
        def fixture_download(
            _opener: object,
            _source_lock: assets.SourceLock,
            item: assets.FileSpec,
            destination: Path,
            _timeout_seconds: float,
        ) -> None:
            destination.write_bytes(self.payloads[item.name])
            os.chmod(destination, 0o600)

        failures = (
            (
                "deadline",
                assets.acquisition_timeout_error(1.0),
                None,
            ),
            (
                "fsync",
                OSError(errno.EIO, "raw-host-detail-sentinel"),
                "raw-host-detail-sentinel",
            ),
        )
        for label, failure, forbidden_detail in failures:
            with self.subTest(label=label):
                cache_root = self.root / label
                cache = cache_root / "pythia-14m" / assets.REVISION
                with mock.patch.object(
                    assets,
                    "download_file",
                    side_effect=fixture_download,
                ), mock.patch.object(
                    assets,
                    "fsync_directory",
                    side_effect=[None, failure],
                ):
                    with self.assertRaisesRegex(
                        assets.PublicationOutcomeError,
                        "atomically published.*indeterminate.*rerun explicit verification",
                    ) as raised:
                        assets.install_cache(cache, self.source_lock, 1.0)
                if forbidden_detail is not None:
                    self.assertNotIn(forbidden_detail, str(raised.exception))

                assets.verify_cache(cache, self.source_lock)
                status, cache_key = assets.prepare_cache(
                    cache_root,
                    self.source_lock,
                    False,
                    1.0,
                )
                self.assertEqual(status, "warm-cache")
                self.assertEqual(cache_key, f"pythia-14m/{assets.REVISION}")
                status, cache_key = assets.prepare_cache(
                    cache_root,
                    self.source_lock,
                    True,
                    1.0,
                )
                self.assertEqual(status, "offline-verified")
                self.assertEqual(cache_key, f"pythia-14m/{assets.REVISION}")

    def test_invalid_existing_cache_is_not_repaired(self) -> None:
        self.populate_cache()
        target = self.cache / assets.EXPECTED_FILES[0]
        before = b"x" * len(self.payloads[target.name])
        target.write_bytes(before)
        os.chmod(target, 0o600)
        with self.assertRaisesRegex(assets.AssetError, "will not be repaired"):
            assets.install_cache(self.cache, self.source_lock, 1.0)
        self.assertEqual(target.read_bytes(), before)


if __name__ == "__main__":
    unittest.main()
