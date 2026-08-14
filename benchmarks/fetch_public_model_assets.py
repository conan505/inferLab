#!/usr/bin/env python3
"""Fetch or verify the immutable v0.32 public-model asset cache.

The checked-in lock is the sole source of asset identities.  Network access is
used only when the complete cache directory is absent; an existing invalid
cache is never repaired implicitly.
"""

from __future__ import annotations

import argparse
import errno
import fcntl
import hashlib
import json
import math
import os
import re
import shutil
import signal
import stat
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Any, BinaryIO


SCHEMA = "inferlab.public-model-lock.v1"
REPOSITORY = "EleutherAI/pythia-14m"
LICENSE = "Apache-2.0"
REVISION = "cf967c0a9a04383db6f7b1108d86b2962634b4ac"
CANONICAL_FILES = (
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
    (
        "tokenizer.json",
        2_114_042,
        "870f4e2baa6b683221fa52004d5d6f40ab8c9d31961617304b78c910c2c3caf2",
    ),
    (
        "tokenizer_config.json",
        4_834,
        "eee017c5bd133137f45907bd0a6e781e2ccd1a533734b7ed2a2f2f4446659809",
    ),
)
EXPECTED_FILES = tuple(item[0] for item in CANONICAL_FILES)
REVISION_PATTERN = re.compile(r"[0-9a-f]{40}\Z")
SHA256_PATTERN = re.compile(r"[0-9a-f]{64}\Z")
MAX_LOCK_BYTES = 256 * 1024
MAX_ASSET_BYTES = 64 * 1024 * 1024
MAX_TOTAL_BYTES = 64 * 1024 * 1024
CHUNK_BYTES = 1024 * 1024
MAX_REDIRECTS = 5
LOCK_RETRY_SECONDS = 0.05

CANONICAL_CHECKPOINT = {
    "file": "model.safetensors",
    "format": "safetensors",
    "header_bytes": 8_488,
    "header_sha256": "da85647d12efa36759dba812776603f6989559e6bf75446d3273c5fd0fe0e11d",
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


class AssetError(RuntimeError):
    """A finite public-asset preparation failure."""


class AcquisitionTimeout(AssetError):
    """The configured end-to-end acquisition budget expired."""


class PublicationOutcomeError(AssetError):
    """Rename committed, but durability/final verification was not confirmed."""


@dataclass(frozen=True)
class FileSpec:
    name: str
    bytes: int
    sha256: str


@dataclass(frozen=True)
class SourceLock:
    repository: str
    revision: str
    license: str
    files: tuple[FileSpec, ...]

    @property
    def total_bytes(self) -> int:
        return sum(item.bytes for item in self.files)


def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise AssetError(f"source lock contains duplicate key {key!r}")
        result[key] = value
    return result


def reject_json_constant(value: str) -> Any:
    raise AssetError(f"source lock contains non-finite JSON constant {value}")


def exact_keys(document: dict[str, Any], expected: set[str], label: str) -> None:
    observed = set(document)
    if observed != expected:
        missing = sorted(expected - observed)
        extra = sorted(observed - expected)
        raise AssetError(f"{label} keys differ: missing={missing}, extra={extra}")


def exact_canonical_object(
    document: Any,
    expected: dict[str, Any],
    label: str,
) -> None:
    if not isinstance(document, dict):
        raise AssetError(f"{label} contract must be an object")
    exact_keys(document, set(expected), f"{label} contract")
    for key, expected_value in expected.items():
        observed = document[key]
        if type(observed) is not type(expected_value) or observed != expected_value:
            raise AssetError(f"{label} contract value differs for {key}")


def reject_non_finite_numbers(value: Any, label: str = "source lock") -> None:
    if isinstance(value, float) and not math.isfinite(value):
        raise AssetError(f"{label} contains a non-finite JSON number")
    if isinstance(value, dict):
        for key, child in value.items():
            reject_non_finite_numbers(child, f"{label}.{key}")
    elif isinstance(value, list):
        for index, child in enumerate(value):
            reject_non_finite_numbers(child, f"{label}[{index}]")


def open_regular_readonly(path: Path) -> tuple[int, os.stat_result]:
    flags = os.O_RDONLY | os.O_NONBLOCK
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        if error.errno == errno.ELOOP:
            raise AssetError(f"refusing symlink {path}") from error
        raise
    metadata = os.fstat(descriptor)
    if not stat.S_ISREG(metadata.st_mode):
        os.close(descriptor)
        raise AssetError(f"expected regular file {path}")
    return descriptor, metadata


def load_source_lock(path: Path) -> SourceLock:
    try:
        descriptor, metadata = open_regular_readonly(path)
    except OSError as error:
        raise AssetError(f"cannot open source lock {path}: {error}") from error
    try:
        if metadata.st_size <= 0 or metadata.st_size > MAX_LOCK_BYTES:
            raise AssetError("source lock size is outside the accepted bound")
        with os.fdopen(descriptor, "rb", closefd=False) as source:
            raw = source.read(MAX_LOCK_BYTES + 1)
        if len(raw) != metadata.st_size:
            raise AssetError("source lock changed while it was read")
        final_metadata = os.fstat(descriptor)
        if (
            final_metadata.st_dev,
            final_metadata.st_ino,
            final_metadata.st_size,
            final_metadata.st_mtime_ns,
            final_metadata.st_ctime_ns,
        ) != (
            metadata.st_dev,
            metadata.st_ino,
            metadata.st_size,
            metadata.st_mtime_ns,
            metadata.st_ctime_ns,
        ):
            raise AssetError("source lock changed while it was read")
    finally:
        os.close(descriptor)

    try:
        document = json.loads(
            raw,
            object_pairs_hook=unique_object,
            parse_constant=reject_json_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise AssetError(f"source lock is not strict UTF-8 JSON: {error}") from error
    if not isinstance(document, dict):
        raise AssetError("source lock root must be an object")
    reject_non_finite_numbers(document)
    exact_keys(
        document,
        {"schema", "source", "files", "checkpoint", "architecture"},
        "source lock",
    )
    if document["schema"] != SCHEMA:
        raise AssetError("source lock schema is unsupported")
    exact_canonical_object(document["checkpoint"], CANONICAL_CHECKPOINT, "checkpoint")
    exact_canonical_object(
        document["architecture"],
        CANONICAL_ARCHITECTURE,
        "architecture",
    )

    source = document["source"]
    if not isinstance(source, dict):
        raise AssetError("source contract must be an object")
    exact_keys(source, {"repository", "revision", "license"}, "source contract")
    repository = source["repository"]
    revision = source["revision"]
    license_name = source["license"]
    if repository != REPOSITORY:
        raise AssetError(f"source repository must be exactly {REPOSITORY}")
    if not isinstance(revision, str) or REVISION_PATTERN.fullmatch(revision) is None:
        raise AssetError("source revision must be a full lowercase immutable commit")
    if revision != REVISION:
        raise AssetError(f"source revision must be exactly {REVISION}")
    if license_name != LICENSE:
        raise AssetError(f"source license must be exactly {LICENSE}")

    files = document["files"]
    if not isinstance(files, list) or len(files) != len(EXPECTED_FILES):
        raise AssetError(f"source lock must contain exactly {len(EXPECTED_FILES)} files")
    parsed: list[FileSpec] = []
    for index, item in enumerate(files):
        if not isinstance(item, dict):
            raise AssetError(f"file entry {index} must be an object")
        exact_keys(item, {"name", "bytes", "sha256"}, f"file entry {index}")
        name = item["name"]
        byte_count = item["bytes"]
        digest = item["sha256"]
        if not isinstance(name, str):
            raise AssetError(f"file entry {index} has a non-string name")
        if type(byte_count) is not int or not 0 < byte_count <= MAX_ASSET_BYTES:
            raise AssetError(f"file entry {name!r} has an invalid byte bound")
        if not isinstance(digest, str) or SHA256_PATTERN.fullmatch(digest) is None:
            raise AssetError(f"file entry {name!r} has an invalid SHA-256")
        parsed.append(FileSpec(name=name, bytes=byte_count, sha256=digest))

    names = tuple(item.name for item in parsed)
    if names != EXPECTED_FILES:
        raise AssetError(
            "source lock file inventory must be sorted and exactly "
            + ", ".join(EXPECTED_FILES)
        )
    identities = tuple((item.name, item.bytes, item.sha256) for item in parsed)
    if identities != CANONICAL_FILES:
        raise AssetError("source lock asset sizes or SHA-256 identities differ from v0.32")
    result = SourceLock(
        repository=repository,
        revision=revision,
        license=license_name,
        files=tuple(parsed),
    )
    if result.total_bytes > MAX_TOTAL_BYTES:
        raise AssetError("source lock total byte bound is excessive")
    return result


def absolute_without_resolving(path: Path) -> Path:
    if path.is_absolute():
        return Path(os.path.normpath(path))
    return Path(os.path.abspath(path))


def normalize_cache_root(path: Path) -> Path:
    """Canonicalize platform aliases while refusing a symlink cache root."""
    absolute = absolute_without_resolving(path)
    metadata = lstat_or_none(absolute)
    if metadata is not None:
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
            raise AssetError(f"cache root is not a real directory: {absolute}")
    # macOS exposes /var as a system-owned alias of /private/var. Resolve such
    # ancestors before the component-by-component creation checks below; the
    # caller-selected root itself was checked without following symlinks.
    return Path(os.path.realpath(absolute))


def ensure_directory_tree(path: Path) -> None:
    path = absolute_without_resolving(path)
    current = Path(path.anchor)
    for component in path.parts[1:]:
        current /= component
        try:
            metadata = os.lstat(current)
        except FileNotFoundError:
            try:
                os.mkdir(current, 0o700)
            except FileExistsError:
                metadata = os.lstat(current)
            else:
                metadata = os.lstat(current)
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
            raise AssetError(f"cache path component is not a real directory: {current}")


def lstat_or_none(path: Path) -> os.stat_result | None:
    try:
        return os.lstat(path)
    except FileNotFoundError:
        return None


def open_cache_directory(path: Path) -> tuple[int, os.stat_result]:
    flags = os.O_RDONLY | os.O_NONBLOCK
    if hasattr(os, "O_DIRECTORY"):
        flags |= os.O_DIRECTORY
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        if error.errno == errno.ELOOP:
            raise AssetError(f"refusing symlink asset cache {path}") from error
        raise AssetError(f"cannot open asset cache {path}: {error}") from error
    metadata = os.fstat(descriptor)
    if not stat.S_ISDIR(metadata.st_mode):
        os.close(descriptor)
        raise AssetError(f"asset cache is not a directory: {path}")
    return descriptor, metadata


def sha256_file_at(
    directory_descriptor: int,
    item: FileSpec,
) -> tuple[int, str]:
    flags = os.O_RDONLY | os.O_NONBLOCK
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(item.name, flags, dir_fd=directory_descriptor)
    except OSError as error:
        if error.errno == errno.ELOOP:
            raise AssetError(f"refusing symlink cache file {item.name}") from error
        raise AssetError(f"cannot open cache file {item.name}: {error}") from error
    metadata = os.fstat(descriptor)
    try:
        if not stat.S_ISREG(metadata.st_mode):
            raise AssetError(f"cache entry {item.name} is not a regular file")
        if metadata.st_size != item.bytes:
            raise AssetError(
                f"cache file {item.name} has {metadata.st_size} bytes, expected {item.bytes}"
            )
        if stat.S_IMODE(metadata.st_mode) != 0o600:
            raise AssetError(f"cache file {item.name} must have mode 0600")
        digest = hashlib.sha256()
        total = 0
        with os.fdopen(descriptor, "rb", closefd=False) as source:
            while True:
                chunk = source.read(CHUNK_BYTES)
                if not chunk:
                    break
                total += len(chunk)
                if total > item.bytes:
                    raise AssetError(f"cache file {item.name} exceeds its byte bound")
                digest.update(chunk)
        final_metadata = os.fstat(descriptor)
        identity = lambda value: (
            value.st_dev,
            value.st_ino,
            value.st_size,
            value.st_mtime_ns,
            value.st_ctime_ns,
        )
        if identity(final_metadata) != identity(metadata):
            raise AssetError(f"cache file {item.name} changed while it was verified")
        return total, digest.hexdigest()
    finally:
        os.close(descriptor)


def verify_cache(directory: Path, source_lock: SourceLock) -> None:
    try:
        descriptor, metadata = open_cache_directory(directory)
    except FileNotFoundError as error:
        raise AssetError(f"asset cache is missing: {directory}") from error
    try:
        if stat.S_IMODE(metadata.st_mode) != 0o700:
            raise AssetError("asset cache directory must have mode 0700")
        try:
            entries = sorted(os.listdir(descriptor))
        except OSError as error:
            raise AssetError(f"cannot list asset cache {directory}: {error}") from error
        expected = [item.name for item in source_lock.files]
        if entries != expected:
            raise AssetError(
                f"asset cache inventory differs: expected={expected}, observed={entries}"
            )
        for item in source_lock.files:
            total, digest = sha256_file_at(descriptor, item)
            if total != item.bytes or digest != item.sha256:
                raise AssetError(f"asset cache identity mismatch for {item.name}")

        final_metadata = os.fstat(descriptor)
        if (
            final_metadata.st_dev,
            final_metadata.st_ino,
            final_metadata.st_mtime_ns,
            final_metadata.st_ctime_ns,
        ) != (
            metadata.st_dev,
            metadata.st_ino,
            metadata.st_mtime_ns,
            metadata.st_ctime_ns,
        ):
            raise AssetError("asset cache directory changed while it was verified")
        try:
            path_metadata = os.stat(directory, follow_symlinks=False)
        except OSError as error:
            raise AssetError("asset cache path changed while it was verified") from error
        if (path_metadata.st_dev, path_metadata.st_ino) != (
            metadata.st_dev,
            metadata.st_ino,
        ):
            raise AssetError("asset cache generation changed while it was verified")
    finally:
        os.close(descriptor)


class HttpsOnlyRedirectHandler(urllib.request.HTTPRedirectHandler):
    def redirect_request(
        self,
        request: urllib.request.Request,
        file_pointer: BinaryIO,
        code: int,
        message: str,
        headers: Any,
        new_url: str,
    ) -> urllib.request.Request | None:
        if urllib.parse.urlsplit(new_url).scheme != "https":
            raise AssetError("refusing a non-HTTPS asset redirect")
        redirect_count = getattr(request, "_inferlab_redirect_count", 0) + 1
        if redirect_count > MAX_REDIRECTS:
            raise AssetError(f"asset redirect count exceeds {MAX_REDIRECTS}")
        redirected = super().redirect_request(
            request, file_pointer, code, message, headers, new_url
        )
        if redirected is not None:
            setattr(redirected, "_inferlab_redirect_count", redirect_count)
        return redirected


def immutable_url(source_lock: SourceLock, item: FileSpec) -> str:
    repository = "/".join(
        urllib.parse.quote(part, safe="")
        for part in source_lock.repository.split("/")
    )
    name = urllib.parse.quote(item.name, safe="")
    return f"https://huggingface.co/{repository}/resolve/{source_lock.revision}/{name}"


def write_all(descriptor: int, chunk: bytes) -> None:
    view = memoryview(chunk)
    while view:
        written = os.write(descriptor, view)
        if written <= 0:
            raise AssetError("asset staging write made no progress")
        view = view[written:]


@contextmanager
def acquisition_deadline(seconds: float) -> Any:
    """Bound the complete six-file network acquisition, including trickles."""
    if not hasattr(signal, "setitimer") or not hasattr(signal, "SIGALRM"):
        raise AssetError("this platform cannot enforce the acquisition deadline")
    previous_handler = signal.getsignal(signal.SIGALRM)
    previous_timer = signal.getitimer(signal.ITIMER_REAL)
    if previous_timer[0] > 0.0:
        raise AssetError("cannot nest asset acquisition inside an active real-time timer")
    deadline = time.monotonic() + seconds

    def expired(_signal_number: int, _frame: Any) -> None:
        raise acquisition_timeout_error(seconds)

    signal.signal(signal.SIGALRM, expired)
    signal.setitimer(signal.ITIMER_REAL, seconds)
    try:
        yield deadline
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0.0)
        signal.signal(signal.SIGALRM, previous_handler)


def download_file(
    opener: urllib.request.OpenerDirector,
    source_lock: SourceLock,
    item: FileSpec,
    destination: Path,
    timeout_seconds: float,
) -> None:
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(destination, flags, 0o600)
    try:
        os.fchmod(descriptor, 0o600)
        request = urllib.request.Request(
            immutable_url(source_lock, item),
            headers={
                "Accept-Encoding": "identity",
                "User-Agent": "inferlab-v0.32-asset-fetcher/1",
            },
        )
        try:
            response = opener.open(request, timeout=timeout_seconds)
        except (OSError, urllib.error.URLError) as error:
            raise AssetError(f"asset download failed for {item.name}: {error}") from error
        with response:
            if response.getcode() != 200:
                raise AssetError(
                    f"asset response for {item.name} returned HTTP {response.getcode()}"
                )
            final_url = response.geturl()
            if urllib.parse.urlsplit(final_url).scheme != "https":
                raise AssetError(f"asset response for {item.name} is not HTTPS")
            content_length = response.headers.get("Content-Length")
            if content_length is not None:
                try:
                    declared_length = int(content_length)
                except ValueError as error:
                    raise AssetError(
                        f"asset response for {item.name} has invalid length"
                    ) from error
                if declared_length > item.bytes:
                    raise AssetError(f"asset response for {item.name} exceeds its byte bound")

            digest = hashlib.sha256()
            total = 0
            while True:
                remaining = item.bytes - total
                chunk = response.read(min(CHUNK_BYTES, remaining + 1))
                if not chunk:
                    break
                total += len(chunk)
                if total > item.bytes:
                    raise AssetError(f"asset {item.name} exceeds its exact byte bound")
                digest.update(chunk)
                write_all(descriptor, chunk)
            if total != item.bytes:
                raise AssetError(f"asset {item.name} is truncated: received {total} bytes")
            if digest.hexdigest() != item.sha256:
                raise AssetError(f"asset {item.name} has the wrong SHA-256")
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def fsync_directory(path: Path) -> None:
    flags = os.O_RDONLY
    if hasattr(os, "O_DIRECTORY"):
        flags |= os.O_DIRECTORY
    descriptor = os.open(path, flags)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def acquisition_timeout_error(seconds: float) -> AcquisitionTimeout:
    return AcquisitionTimeout(
        f"asset acquisition exceeded its {seconds:g}-second deadline"
    )


def acquire_fetch_lock(parent: Path, deadline: float, timeout_seconds: float) -> int:
    lock_path = parent / ".fetch.lock"
    flags = os.O_RDWR | os.O_CREAT | getattr(os, "O_CLOEXEC", 0)
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(lock_path, flags, 0o600)
    except OSError as error:
        raise AssetError(f"cannot open fetch lock {lock_path}: {error}") from error
    metadata = os.fstat(descriptor)
    if not stat.S_ISREG(metadata.st_mode):
        os.close(descriptor)
        raise AssetError("fetch lock is not a regular file")
    os.fchmod(descriptor, 0o600)
    try:
        while True:
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return descriptor
            except OSError as error:
                if error.errno not in {errno.EACCES, errno.EAGAIN}:
                    raise AssetError(
                        f"cannot acquire fetch lock {lock_path}: {error}"
                    ) from error
            remaining = deadline - time.monotonic()
            if remaining <= 0.0:
                raise acquisition_timeout_error(timeout_seconds)
            time.sleep(min(LOCK_RETRY_SECONDS, remaining))
    except BaseException:
        os.close(descriptor)
        raise


def install_cache(
    cache_directory: Path,
    source_lock: SourceLock,
    acquisition_timeout_seconds: float,
) -> str:
    cache_directory = normalize_cache_root(cache_directory)
    parent = cache_directory.parent
    lock_descriptor: int | None = None
    staging: Path | None = None
    publication_started = False
    with acquisition_deadline(acquisition_timeout_seconds) as deadline:
        try:
            ensure_directory_tree(parent)
            lock_descriptor = acquire_fetch_lock(
                parent,
                deadline,
                acquisition_timeout_seconds,
            )
            observed = lstat_or_none(cache_directory)
            if observed is not None:
                try:
                    verify_cache(cache_directory, source_lock)
                except AssetError as error:
                    raise AssetError(
                        "existing cache is invalid and will not be repaired "
                        f"automatically: {error}"
                    ) from error
                return "warm-cache"

            staging = Path(
                tempfile.mkdtemp(
                    prefix=f".{source_lock.revision}.staging-",
                    dir=parent,
                )
            )
            os.chmod(staging, 0o700)
            opener = urllib.request.build_opener(HttpsOnlyRedirectHandler())
            for item in source_lock.files:
                remaining = deadline - time.monotonic()
                if remaining <= 0.0:
                    raise acquisition_timeout_error(acquisition_timeout_seconds)
                download_file(
                    opener,
                    source_lock,
                    item,
                    staging / item.name,
                    min(30.0, remaining),
                )
            verify_cache(staging, source_lock)
            fsync_directory(staging)

            if lstat_or_none(cache_directory) is not None:
                raise AssetError(
                    "asset cache appeared during installation; refusing to replace it"
                )
            publication_started = True
            os.rename(staging, cache_directory)
            staging = None
            fsync_directory(parent)
            verify_cache(cache_directory, source_lock)
            return "downloaded"
        except (AssetError, OSError) as error:
            try:
                final_generation = lstat_or_none(cache_directory)
                staging_generation = (
                    lstat_or_none(staging) if staging is not None else None
                )
            except OSError:
                final_generation = None
                staging_generation = None
            publication_committed = publication_started and (
                staging is None
                or (final_generation is not None and staging_generation is None)
            )
            if publication_committed:
                raise PublicationOutcomeError(
                    "asset cache was atomically published, but durability and final "
                    "verification are indeterminate; rerun explicit verification"
                ) from error
            if isinstance(error, OSError):
                raise AssetError(
                    "asset cache installation failed before atomic publication"
                ) from error
            raise
        finally:
            if staging is not None and lstat_or_none(staging) is not None:
                shutil.rmtree(staging)
            if lock_descriptor is not None:
                os.close(lock_descriptor)


def parse_offline_environment() -> bool:
    value = os.environ.get("INFERLAB_V32_OFFLINE", "0")
    if value not in {"0", "1"}:
        raise AssetError("INFERLAB_V32_OFFLINE must be exactly 0 or 1")
    return value == "1"


def prepare_cache(
    cache_root: Path,
    source_lock: SourceLock,
    offline: bool,
    acquisition_timeout_seconds: float,
) -> tuple[str, str]:
    normalized_root = normalize_cache_root(cache_root)
    cache_key = f"pythia-14m/{source_lock.revision}"
    cache_directory = normalized_root / cache_key

    existing = lstat_or_none(cache_directory)
    if existing is not None:
        try:
            verify_cache(cache_directory, source_lock)
        except AssetError as error:
            raise AssetError(
                f"existing cache is invalid and will not be repaired automatically: {error}"
            ) from error
        return ("offline-verified" if offline else "warm-cache"), cache_key
    if offline:
        raise AssetError(f"offline verification requires cache key {cache_key}")
    status = install_cache(
        cache_directory,
        source_lock,
        acquisition_timeout_seconds,
    )
    return status, cache_key


def main() -> int:
    project_root = Path(__file__).resolve().parent.parent
    cache_root_environment = os.environ.get("INFERLAB_V32_CACHE_ROOT")
    default_cache_root = (
        Path(cache_root_environment) if cache_root_environment is not None else None
    )
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--lock",
        type=Path,
        default=project_root / "models" / "public" / "pythia-14m-v0.32.lock.json",
    )
    parser.add_argument(
        "--cache-root",
        type=Path,
        default=default_cache_root,
        required=default_cache_root is None,
        help="explicit cache root (or set INFERLAB_V32_CACHE_ROOT)",
    )
    parser.add_argument(
        "--offline",
        action="store_true",
        default=parse_offline_environment(),
    )
    parser.add_argument("--acquisition-timeout-seconds", type=float, default=180.0)
    arguments = parser.parse_args()
    if not 1.0 <= arguments.acquisition_timeout_seconds <= 600.0:
        raise AssetError("acquisition timeout must be between 1 and 600 seconds")

    source_lock = load_source_lock(absolute_without_resolving(arguments.lock))
    status, cache_key = prepare_cache(
        arguments.cache_root,
        source_lock,
        arguments.offline,
        arguments.acquisition_timeout_seconds,
    )

    print(
        json.dumps(
            {
                "schema": "inferlab.public-model-cache-result.v0.32",
                "status": status,
                "repository": source_lock.repository,
                "revision": source_lock.revision,
                "cache_key": cache_key,
                "file_count": len(source_lock.files),
                "total_bytes": source_lock.total_bytes,
            },
            indent=2,
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except AssetError as error:
        print(f"v0.32 asset preparation failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
