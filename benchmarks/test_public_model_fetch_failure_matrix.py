#!/usr/bin/env python3
"""Exact network-boundary regressions required by the v0.32 failure matrix."""

from __future__ import annotations

import hashlib
import io
import os
import signal
import tempfile
import time
import unittest
import urllib.request
from pathlib import Path
from unittest import mock

import fetch_public_model_assets as assets


class FakeResponse:
    def __init__(
        self,
        payload: bytes,
        *,
        content_length: int | None = None,
        read_delay_seconds: float = 0.0,
    ) -> None:
        self.payload = payload
        self.position = 0
        self.read_delay_seconds = read_delay_seconds
        self.headers: dict[str, str] = {}
        if content_length is not None:
            self.headers["Content-Length"] = str(content_length)

    def __enter__(self) -> FakeResponse:
        return self

    def __exit__(self, *_arguments: object) -> None:
        return None

    def getcode(self) -> int:
        return 200

    def geturl(self) -> str:
        return "https://immutable.example/artifact"

    def read(self, byte_count: int) -> bytes:
        if self.read_delay_seconds:
            time.sleep(self.read_delay_seconds)
        start = self.position
        end = min(len(self.payload), start + byte_count)
        self.position = end
        return self.payload[start:end]


class FakeOpener:
    def __init__(self, response: FakeResponse) -> None:
        self.response = response

    def open(
        self,
        _request: urllib.request.Request,
        *,
        timeout: float,
    ) -> FakeResponse:
        if timeout <= 0:
            raise AssertionError("download timeout must be positive")
        return self.response


class PublicModelFetchFailureMatrixTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def file_spec(self, expected: bytes = b"abc") -> assets.FileSpec:
        return assets.FileSpec(
            name="README.md",
            bytes=len(expected),
            sha256=hashlib.sha256(expected).hexdigest(),
        )

    def source_lock(self) -> assets.SourceLock:
        payloads = {
            name: f"fixture:{name}\n".encode("utf-8")
            for name in assets.EXPECTED_FILES
        }
        return assets.SourceLock(
            repository=assets.REPOSITORY,
            revision=assets.REVISION,
            license=assets.LICENSE,
            files=tuple(
                assets.FileSpec(
                    name=name,
                    bytes=len(payloads[name]),
                    sha256=hashlib.sha256(payloads[name]).hexdigest(),
                )
                for name in assets.EXPECTED_FILES
            ),
        )

    def download(self, response: FakeResponse, expected: bytes = b"abc") -> None:
        assets.download_file(
            FakeOpener(response),
            self.source_lock(),
            self.file_spec(expected),
            self.root / "artifact",
            1.0,
        )

    def test_streamed_response_oversize_is_rejected_at_exact_bound(self) -> None:
        with self.assertRaisesRegex(assets.AssetError, "exceeds its exact byte bound"):
            self.download(FakeResponse(b"abcd"))

    def test_short_response_is_rejected_before_publication(self) -> None:
        with self.assertRaisesRegex(assets.AssetError, "truncated"):
            self.download(FakeResponse(b"ab"))

    def test_exact_length_hash_mismatch_is_rejected(self) -> None:
        with self.assertRaisesRegex(assets.AssetError, "wrong SHA-256"):
            self.download(FakeResponse(b"abd"))

    def test_redirect_overflow_is_rejected_at_the_configured_limit(self) -> None:
        request = urllib.request.Request("https://immutable.example/source")
        setattr(request, "_inferlab_redirect_count", assets.MAX_REDIRECTS)
        handler = assets.HttpsOnlyRedirectHandler()
        with self.assertRaisesRegex(
            assets.AssetError,
            f"redirect count exceeds {assets.MAX_REDIRECTS}",
        ):
            handler.redirect_request(
                request,
                io.BytesIO(),
                302,
                "redirect",
                {},
                "https://immutable.example/next",
            )

    @unittest.skipUnless(
        hasattr(signal, "setitimer") and hasattr(signal, "SIGALRM"),
        "total acquisition deadlines require POSIX interval timers",
    )
    def test_trickled_http_read_obeys_one_total_acquisition_deadline(self) -> None:
        source_lock = self.source_lock()
        response = FakeResponse(b"fixture:README.md\n", read_delay_seconds=1.0)
        started = time.monotonic()
        with mock.patch.object(
            urllib.request,
            "build_opener",
            return_value=FakeOpener(response),
        ):
            with self.assertRaisesRegex(
                assets.AcquisitionTimeout,
                "asset acquisition exceeded its 0.05-second deadline",
            ):
                assets.install_cache(
                    self.root / "cache" / "generation",
                    source_lock,
                    0.05,
                )
        elapsed = time.monotonic() - started
        self.assertGreaterEqual(elapsed, 0.04)
        self.assertLess(elapsed, 2.0)
        self.assertFalse((self.root / "cache" / "generation").exists())
        staging = list((self.root / "cache").glob(".*.staging-*"))
        self.assertEqual(staging, [])


if __name__ == "__main__":
    unittest.main()
