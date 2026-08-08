#!/usr/bin/env python3
"""Zero-dependency probes for the InferLab v0.26 OpenMetrics proof."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import re
import signal
import sys
import threading
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any


OPENMETRICS_CONTENT_TYPE = "application/openmetrics-text; version=1.0.0; charset=utf-8"
MAX_RESPONSE_BYTES = 2 * 1024 * 1024
METRIC_NAME = re.compile(r"[A-Za-z_:][A-Za-z0-9_:]*")
LABEL_NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
REQUEST_ID = re.compile(r"[A-Za-z0-9._:-]{1,64}")
NUMERIC_VALUE = re.compile(
    r"[+-]?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?"
)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, file_pointer, code, message, headers, new_url):  # type: ignore[no-untyped-def]
        return None


HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())


@dataclass(frozen=True)
class MetricSample:
    name: str
    labels: tuple[tuple[str, str], ...]
    value: float

    def as_json(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "labels": dict(self.labels),
            "value": self.value,
        }


def _parse_quoted(value: str, index: int) -> tuple[str, int]:
    if index >= len(value) or value[index] != '"':
        raise ValueError("label value must begin with a quote")
    index += 1
    result: list[str] = []
    while index < len(value):
        character = value[index]
        index += 1
        if character == '"':
            return "".join(result), index
        if character != "\\":
            result.append(character)
            continue
        if index >= len(value):
            raise ValueError("label value ends with an incomplete escape")
        escaped = value[index]
        index += 1
        mapping = {"\\": "\\", '"': '"', "n": "\n"}
        if escaped not in mapping:
            raise ValueError(f"unsupported label escape \\{escaped}")
        result.append(mapping[escaped])
    raise ValueError("unterminated label value")


def parse_labels(encoded: str) -> tuple[tuple[str, str], ...]:
    if not (encoded.startswith("{") and encoded.endswith("}")):
        raise ValueError("sample labels must be enclosed in braces")
    inner = encoded[1:-1]
    if not inner:
        return ()
    labels: dict[str, str] = {}
    index = 0
    while index < len(inner):
        match = LABEL_NAME.match(inner, index)
        if match is None:
            raise ValueError(f"invalid label name near {inner[index:]!r}")
        name = match.group(0)
        index = match.end()
        if name in labels:
            raise ValueError(f"duplicate label {name!r}")
        if index >= len(inner) or inner[index] != "=":
            raise ValueError(f"label {name!r} is missing '='")
        index += 1
        decoded, index = _parse_quoted(inner, index)
        labels[name] = decoded
        if index == len(inner):
            break
        if inner[index] != ",":
            raise ValueError(f"label {name!r} is not followed by a comma")
        index += 1
        if index == len(inner):
            raise ValueError("labels end with a trailing comma")
    return tuple(sorted(labels.items()))


def parse_openmetrics(text: str) -> dict[str, Any]:
    """Parse the deliberately small OpenMetrics surface InferLab emits.

    The proof rejects timestamps, exemplars, duplicate series, non-finite values,
    and malformed metadata. That keeps the retained contract strict without
    adding a package or network dependency to the $0 proof.
    """

    if "\r" in text:
        raise ValueError("OpenMetrics evidence must use LF line endings")
    if not text.endswith("# EOF\n"):
        raise ValueError("OpenMetrics evidence must end with exactly '# EOF\\n'")
    lines = text.splitlines()
    if lines.count("# EOF") != 1 or lines[-1] != "# EOF":
        raise ValueError("OpenMetrics EOF marker must occur exactly once at the end")

    help_text: dict[str, str] = {}
    metric_types: dict[str, str] = {}
    units: dict[str, str] = {}
    samples: list[MetricSample] = []
    seen_series: set[tuple[str, tuple[tuple[str, str], ...]]] = set()

    for line_number, line in enumerate(lines[:-1], start=1):
        if not line:
            raise ValueError(f"blank OpenMetrics line at {line_number}")
        if line.startswith("# HELP "):
            parts = line.split(" ", 3)
            if len(parts) != 4 or METRIC_NAME.fullmatch(parts[2]) is None or not parts[3]:
                raise ValueError(f"malformed HELP line at {line_number}")
            if parts[2] in help_text:
                raise ValueError(f"duplicate HELP metadata for {parts[2]}")
            help_text[parts[2]] = parts[3]
            continue
        if line.startswith("# TYPE "):
            parts = line.split()
            if (
                len(parts) != 4
                or METRIC_NAME.fullmatch(parts[2]) is None
                or parts[3] not in {"counter", "gauge", "histogram"}
            ):
                raise ValueError(f"malformed TYPE line at {line_number}")
            if parts[2] in metric_types:
                raise ValueError(f"duplicate TYPE metadata for {parts[2]}")
            metric_types[parts[2]] = parts[3]
            continue
        if line.startswith("# UNIT "):
            parts = line.split()
            if len(parts) != 4 or METRIC_NAME.fullmatch(parts[2]) is None:
                raise ValueError(f"malformed UNIT line at {line_number}")
            if parts[2] in units:
                raise ValueError(f"duplicate UNIT metadata for {parts[2]}")
            units[parts[2]] = parts[3]
            continue
        if line.startswith("#"):
            raise ValueError(f"unsupported OpenMetrics comment at line {line_number}: {line!r}")

        match = METRIC_NAME.match(line)
        if match is None:
            raise ValueError(f"invalid sample name at line {line_number}")
        name = match.group(0)
        index = match.end()
        labels: tuple[tuple[str, str], ...] = ()
        if index < len(line) and line[index] == "{":
            in_quote = False
            escaped = False
            end = None
            for cursor in range(index + 1, len(line)):
                character = line[cursor]
                if escaped:
                    escaped = False
                elif character == "\\" and in_quote:
                    escaped = True
                elif character == '"':
                    in_quote = not in_quote
                elif character == "}" and not in_quote:
                    end = cursor
                    break
            if end is None:
                raise ValueError(f"unterminated label set at line {line_number}")
            labels = parse_labels(line[index : end + 1])
            index = end + 1
        if index >= len(line) or line[index] != " ":
            raise ValueError(f"sample {name!r} is missing its value separator")
        encoded_value = line[index + 1 :]
        if NUMERIC_VALUE.fullmatch(encoded_value) is None:
            raise ValueError(f"sample {name!r} contains a timestamp or malformed value")
        try:
            numeric_value = float(encoded_value)
        except ValueError as error:
            raise ValueError(f"sample {name!r} has a non-numeric value") from error
        if not math.isfinite(numeric_value):
            raise ValueError(f"sample {name!r} has a non-finite value")
        identity = (name, labels)
        if identity in seen_series:
            raise ValueError(f"duplicate series for {name!r} and labels {dict(labels)!r}")
        seen_series.add(identity)
        samples.append(MetricSample(name=name, labels=labels, value=numeric_value))

    if set(help_text) != set(metric_types):
        raise ValueError("every metric family must have exactly one HELP and TYPE line")
    if set(units) - set(metric_types):
        raise ValueError("UNIT metadata references an unknown metric family")
    return {
        "help": help_text,
        "types": metric_types,
        "units": units,
        "samples": samples,
        "sample_count": len(samples),
        "family_count": len(metric_types),
    }


def _request(
    url: str,
    *,
    method: str = "GET",
    data: bytes | None = None,
    headers: dict[str, str] | None = None,
    timeout: float = 3.0,
) -> dict[str, Any]:
    request = urllib.request.Request(url, data=data, headers=headers or {}, method=method)
    started = time.perf_counter()
    try:
        response = HTTP.open(request, timeout=timeout)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        body = response.read(MAX_RESPONSE_BYTES + 1)
        if len(body) > MAX_RESPONSE_BYTES:
            raise ValueError(f"response from {url} exceeds {MAX_RESPONSE_BYTES} bytes")
        return {
            "status": response.status,
            "headers": {key.lower(): value for key, value in response.headers.items()},
            "body": body,
            "duration_ms": round((time.perf_counter() - started) * 1000.0, 3),
        }


def _decode_body(body: bytes, content_type: str | None) -> Any:
    text = body.decode("utf-8", errors="strict")
    if content_type and "json" in content_type.lower():
        try:
            return json.loads(text)
        except json.JSONDecodeError:
            pass
    return text


def _load_targets(path: Path) -> list[dict[str, Any]]:
    value = json.loads(path.read_text(encoding="utf-8"))
    targets = value.get("targets")
    if not isinstance(targets, list) or not targets:
        raise ValueError("target inventory must contain a non-empty targets array")
    names = [target.get("name") for target in targets]
    if any(not isinstance(name, str) or not name for name in names) or len(set(names)) != len(names):
        raise ValueError("target inventory names must be non-empty and unique")
    return targets


def wait_endpoint(args: argparse.Namespace) -> None:
    deadline = time.monotonic() + args.timeout
    last: dict[str, Any] | None = None
    while time.monotonic() < deadline:
        try:
            last = _request(args.url, timeout=min(args.request_timeout, max(0.05, deadline - time.monotonic())))
            if last["status"] == args.status:
                print(json.dumps({
                    "schema": "inferlab.observability-wait.v0.26",
                    "url": args.url,
                    "status": last["status"],
                }, indent=2, sort_keys=True))
                return
        except (OSError, ValueError, urllib.error.URLError):
            pass
        time.sleep(0.05)
    raise SystemExit(f"timed out waiting for {args.url}; last observation={last}")


def scrape_set(args: argparse.Namespace) -> None:
    targets = _load_targets(args.targets_file)
    args.raw_dir.mkdir(parents=True, exist_ok=True)
    observations: dict[str, Any] = {}
    total_series = 0
    for target in targets:
        observed = _request(target["metrics_url"], timeout=args.timeout)
        content_type = observed["headers"].get("content-type")
        body = observed["body"].decode("utf-8", errors="strict")
        parsed = parse_openmetrics(body)
        raw_name = f"{args.checkpoint}-{target['name']}.prom"
        raw_path = args.raw_dir / raw_name
        if raw_path.exists():
            raise ValueError(f"refusing to replace existing raw scrape {raw_path}")
        raw_path.write_text(body, encoding="utf-8")
        total_series += parsed["sample_count"]
        observations[target["name"]] = {
            "service": target["service"],
            "metrics_url": target["metrics_url"],
            "status": observed["status"],
            "content_type": content_type,
            "duration_ms": observed["duration_ms"],
            "bytes": len(observed["body"]),
            "sha256": hashlib.sha256(observed["body"]).hexdigest(),
            "raw_file": raw_name,
            "family_count": parsed["family_count"],
            "sample_count": parsed["sample_count"],
            "families": sorted(parsed["types"]),
        }
    print(json.dumps({
        "schema": "inferlab.openmetrics-scrape-set.v0.26",
        "checkpoint": args.checkpoint,
        "target_count": len(targets),
        "series_total": total_series,
        "targets": observations,
    }, indent=2, sort_keys=True))


def capture_json_set(args: argparse.Namespace) -> None:
    targets = _load_targets(args.targets_file)
    observations: dict[str, Any] = {}
    bearer = os.environ.get("INFERLAB_PROBE_BEARER") if args.use_bearer_env else None
    for target in targets:
        url = target.get(args.url_field)
        if not url:
            continue
        headers = {}
        if target.get("status_requires_bearer"):
            if not bearer:
                raise ValueError(f"target {target['name']} requires INFERLAB_PROBE_BEARER")
            headers["authorization"] = f"Bearer {bearer}"
        observed = _request(url, headers=headers, timeout=args.timeout)
        content_type = observed["headers"].get("content-type")
        observations[target["name"]] = {
            "service": target["service"],
            "url": url,
            "status": observed["status"],
            "content_type": content_type,
            "body": _decode_body(observed["body"], content_type),
            "duration_ms": observed["duration_ms"],
        }
    print(json.dumps({
        "schema": "inferlab.observability-status-set.v0.26",
        "url_field": args.url_field,
        "targets": observations,
    }, indent=2, sort_keys=True))


def request_capture(args: argparse.Namespace) -> None:
    headers = {"content-type": args.content_type}
    if args.request_id is not None:
        headers["x-inferlab-request-id"] = args.request_id
    bearer = os.environ.get("INFERLAB_PROBE_BEARER") if args.use_bearer_env else None
    if bearer:
        headers["authorization"] = f"Bearer {bearer}"
    data = args.body.read_bytes() if args.body else None
    observed = _request(args.url, method=args.method, data=data, headers=headers, timeout=args.timeout)
    if args.expect_status is not None and observed["status"] != args.expect_status:
        raise SystemExit(f"expected HTTP {args.expect_status}, observed {observed['status']}")
    response_content_type = observed["headers"].get("content-type")
    selected_headers = {
        name: observed["headers"].get(name)
        for name in [
            "content-type",
            "x-inferlab-request-id",
            "x-inferlab-worker",
            "x-inferlab-attempts",
            "x-inferlab-config-revision",
            "x-inferlab-control-cluster",
            "x-inferlab-config-term",
            "etag",
        ]
        if observed["headers"].get(name) is not None
    }
    print(json.dumps({
        "schema": "inferlab.observability-http-capture.v0.26",
        "request": {
            "method": args.method,
            "url": args.url,
            "request_id": args.request_id,
            "bearer_configured": bool(bearer),
            "body_bytes": len(data or b""),
        },
        "response": {
            "status": observed["status"],
            "headers": selected_headers,
            "body": _decode_body(observed["body"], response_content_type),
            "duration_ms": observed["duration_ms"],
        },
    }, indent=2, sort_keys=True))


def _json_request(
    url: str,
    method: str,
    payload: Any | None = None,
    *,
    headers: dict[str, str] | None = None,
    timeout: float = 3.0,
) -> tuple[dict[str, Any], Any]:
    encoded = None
    merged = dict(headers or {})
    if payload is not None:
        encoded = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        merged["content-type"] = "application/json"
    observation = _request(url, method=method, data=encoded, headers=merged, timeout=timeout)
    content_type = observation["headers"].get("content-type")
    return observation, _decode_body(observation["body"], content_type)


def batch_scenario(args: argparse.Namespace) -> None:
    base = args.base_url.rstrip("/")
    statuses: list[int] = []

    first, first_body = _json_request(
        f"{base}/v1/batch/jobs",
        "POST",
        {
            "idempotency_key": "observability-complete",
            "payload": {"kind": "bounded-proof", "sequence": 1},
            "max_attempts": 2,
        },
        timeout=args.timeout,
    )
    statuses.append(first["status"])
    first_job_id = first_body.get("job", {}).get("id") if isinstance(first_body, dict) else None
    claim_one, claim_one_body = _json_request(
        f"{base}/v1/batch/claim",
        "POST",
        {"consumer_id": "observability-consumer", "visibility_timeout_ms": 30_000},
        timeout=args.timeout,
    )
    statuses.append(claim_one["status"])
    claimed_first_id = claim_one_body.get("job_id") if isinstance(claim_one_body, dict) else None
    claim_one_token = claim_one_body.get("claim_token") if isinstance(claim_one_body, dict) else None
    ack, ack_body = _json_request(
        f"{base}/v1/batch/jobs/{claimed_first_id}/ack",
        "POST",
        {"consumer_id": "observability-consumer", "claim_token": claim_one_token},
        timeout=args.timeout,
    )
    statuses.append(ack["status"])

    second, second_body = _json_request(
        f"{base}/v1/batch/jobs",
        "POST",
        {
            "idempotency_key": "observability-dead-letter",
            "payload": {"kind": "bounded-proof", "sequence": 2},
            "max_attempts": 1,
        },
        timeout=args.timeout,
    )
    statuses.append(second["status"])
    second_job_id = second_body.get("job", {}).get("id") if isinstance(second_body, dict) else None
    claim_two, claim_two_body = _json_request(
        f"{base}/v1/batch/claim",
        "POST",
        {"consumer_id": "observability-consumer", "visibility_timeout_ms": 30_000},
        timeout=args.timeout,
    )
    statuses.append(claim_two["status"])
    claimed_second_id = claim_two_body.get("job_id") if isinstance(claim_two_body, dict) else None
    claim_two_token = claim_two_body.get("claim_token") if isinstance(claim_two_body, dict) else None
    failed, failed_body = _json_request(
        f"{base}/v1/batch/jobs/{claimed_second_id}/fail",
        "POST",
        {
            "consumer_id": "observability-consumer",
            "claim_token": claim_two_token,
            "error": "controlled observability proof failure",
        },
        timeout=args.timeout,
    )
    statuses.append(failed["status"])

    expected = [201, 200, 200, 201, 200, 200]
    completed = ack_body.get("status") if isinstance(ack_body, dict) else None
    dead_letter = failed_body.get("status") if isinstance(failed_body, dict) else None
    all_expected = (
        statuses == expected
        and first_job_id == claimed_first_id
        and second_job_id == claimed_second_id
        and completed == "completed"
        and dead_letter == "dead_letter"
    )
    if not all_expected:
        raise SystemExit(
            "batch proof scenario did not reach the expected completed/dead-letter states: "
            f"statuses={statuses!r} completed={completed!r} dead_letter={dead_letter!r}"
        )
    print(json.dumps({
        "schema": "inferlab.observability-batch-scenario.v0.26",
        "statuses": statuses,
        "expected_statuses": expected,
        "all_expected_statuses": True,
        "completed_job_id": first_job_id,
        "dead_letter_job_id": second_job_id,
        "final_states": [completed, dead_letter],
    }, indent=2, sort_keys=True))


def trust_scenario(args: argparse.Namespace) -> None:
    base = args.base_url.rstrip("/")
    unavailable, unavailable_body = _json_request(
        f"{base}/v1/service-trust/snapshot", "GET", timeout=args.timeout
    )
    snapshot_bytes = args.snapshot.read_bytes()
    published = _request(
        f"{base}/v1/service-trust/snapshot",
        method="POST",
        data=snapshot_bytes,
        headers={"content-type": "application/json"},
        timeout=args.timeout,
    )
    unchanged = _request(
        f"{base}/v1/service-trust/snapshot",
        method="POST",
        data=snapshot_bytes,
        headers={"content-type": "application/json"},
        timeout=args.timeout,
    )
    rejected = _request(
        f"{base}/v1/service-trust/snapshot",
        method="POST",
        data=args.tampered_snapshot.read_bytes(),
        headers={"content-type": "application/json"},
        timeout=args.timeout,
    )
    rejected_body = _decode_body(
        rejected["body"], rejected["headers"].get("content-type")
    )
    served = _request(f"{base}/v1/service-trust/snapshot", timeout=args.timeout)
    etag = served["headers"].get("etag")
    not_modified = _request(
        f"{base}/v1/service-trust/snapshot",
        headers={"if-none-match": etag or ""},
        timeout=args.timeout,
    )
    receipt_rejected, receipt_body = _json_request(
        f"{base}/v1/service-trust/receipts", "POST", {}, timeout=args.timeout
    )
    outcomes = {
        "snapshot_unavailable": int(unavailable["status"] == 404),
        "snapshot_published": int(published["status"] == 201),
        "snapshot_unchanged": int(unchanged["status"] == 200),
        "snapshot_rejected": int(rejected["status"] == 400),
        "snapshot_served": int(served["status"] == 200),
        "snapshot_not_modified": int(not_modified["status"] == 304),
        "receipt_rejected": int(receipt_rejected["status"] == 400),
    }
    if any(value != 1 for value in outcomes.values()) or not etag:
        raise SystemExit(f"trust proof scenario failed: outcomes={outcomes!r} etag={etag!r}")
    print(json.dumps({
        "schema": "inferlab.observability-trust-scenario.v0.26",
        "outcomes": outcomes,
        "statuses": {
            "unavailable": unavailable["status"],
            "published": published["status"],
            "unchanged": unchanged["status"],
            "rejected": rejected["status"],
            "served": served["status"],
            "not_modified": not_modified["status"],
            "receipt_rejected": receipt_rejected["status"],
        },
        "error_codes": {
            "unavailable": (unavailable_body.get("error", {}) or {}).get("code")
            if isinstance(unavailable_body, dict) else None,
            "snapshot_rejected": (rejected_body.get("error", {}) or {}).get("code")
            if isinstance(rejected_body, dict) else None,
            "receipt_rejected": (receipt_body.get("error", {}) or {}).get("code")
            if isinstance(receipt_body, dict) else None,
        },
        "etag_observed": True,
    }, indent=2, sort_keys=True))


def unique_prompts(args: argparse.Namespace) -> None:
    bearer = os.environ.get("INFERLAB_PROBE_BEARER") if args.use_bearer_env else None
    observations: list[dict[str, Any]] = []
    prompts: list[str] = []
    request_ids: list[str] = []
    for index in range(args.count):
        prompt = f"{args.prompt_prefix}-{index:03d}"
        request_id = f"{args.request_id_prefix}.{index:03d}"
        payload = json.dumps({
            "model": "inferlab-tiny",
            "stream": False,
            "temperature": 0,
            "max_tokens": args.max_tokens,
            "messages": [{"role": "user", "content": prompt}],
        }, separators=(",", ":")).encode("utf-8")
        headers = {
            "content-type": "application/json",
            "x-inferlab-request-id": request_id,
        }
        if bearer:
            headers["authorization"] = f"Bearer {bearer}"
        observed = _request(args.url, method="POST", data=payload, headers=headers, timeout=args.timeout)
        echoed = observed["headers"].get("x-inferlab-request-id")
        if observed["status"] != 200 or echoed != request_id:
            raise SystemExit(
                f"unique prompt {index} failed: status={observed['status']} echoed={echoed!r}"
            )
        prompts.append(prompt)
        request_ids.append(request_id)
        observations.append({
            "index": index,
            "status": observed["status"],
            "request_id": request_id,
            "echoed_request_id": echoed,
            "worker": observed["headers"].get("x-inferlab-worker"),
            "duration_ms": observed["duration_ms"],
        })
    print(json.dumps({
        "schema": "inferlab.observability-unique-prompts.v0.26",
        "requested": args.count,
        "succeeded": len(observations),
        "prompts": prompts,
        "request_ids": request_ids,
        "observations": observations,
    }, indent=2, sort_keys=True))


def stream_capture(args: argparse.Namespace) -> None:
    payload = json.dumps({
        "model": "inferlab-tiny",
        "stream": True,
        "temperature": 0,
        "speculative_tokens": args.speculative_tokens,
        "max_tokens": args.max_tokens,
        "messages": [{"role": "user", "content": args.prompt}],
    }, separators=(",", ":")).encode("utf-8")
    headers = {
        "content-type": "application/json",
        "x-inferlab-request-id": args.request_id,
    }
    bearer = os.environ.get("INFERLAB_PROBE_BEARER") if args.use_bearer_env else None
    if bearer:
        headers["authorization"] = f"Bearer {bearer}"
    observed = _request(args.url, method="POST", data=payload, headers=headers, timeout=args.timeout)
    body = observed["body"].decode("utf-8", errors="strict")
    print(json.dumps({
        "schema": "inferlab.observability-stream.v0.26",
        "status": observed["status"],
        "request_id": args.request_id,
        "echoed_request_id": observed["headers"].get("x-inferlab-request-id"),
        "worker": observed["headers"].get("x-inferlab-worker"),
        "config_revision": observed["headers"].get("x-inferlab-config-revision"),
        "done_received": "data: [DONE]" in body,
        "event_count": sum(1 for line in body.splitlines() if line.startswith("data: ")),
        "duration_ms": observed["duration_ms"],
    }, indent=2, sort_keys=True))


class RetryEventJournal:
    def __init__(self, path: Path) -> None:
        self._source = path.open("x", encoding="utf-8")
        self._lock = threading.Lock()
        self._sequence = 0

    def record(self, endpoint: str, path: str, request_id: str | None, status: int) -> None:
        with self._lock:
            self._sequence += 1
            self._source.write(json.dumps({
                "schema": "inferlab.request-id-retry-event.v0.26",
                "sequence": self._sequence,
                "endpoint": endpoint,
                "path": path,
                "request_id": request_id,
                "response_status": status,
            }, sort_keys=True) + "\n")
            self._source.flush()

    def close(self) -> None:
        self._source.close()


def _parse_bind(value: str) -> tuple[str, int]:
    host, separator, port = value.rpartition(":")
    if not separator or not host:
        raise ValueError(f"invalid bind address {value!r}")
    return host, int(port)


def retry_servers(args: argparse.Namespace) -> None:
    journal = RetryEventJournal(args.events)
    stopped = threading.Event()

    def handler(endpoint: str, response_status: int, delay_ms: int):
        class Handler(BaseHTTPRequestHandler):
            def do_GET(self) -> None:  # noqa: N802
                if self.path == "/health":
                    body = b'{"status":"ok"}\n'
                    self.send_response(200)
                    self.send_header("content-type", "application/json")
                    self.send_header("content-length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                    return
                self.send_error(404)

            def do_POST(self) -> None:  # noqa: N802
                length = int(self.headers.get("content-length", "0"))
                if length > 64 * 1024:
                    self.send_error(413)
                    return
                self.rfile.read(length)
                request_id = self.headers.get("x-inferlab-request-id")
                if delay_ms:
                    time.sleep(delay_ms / 1000.0)
                journal.record(endpoint, self.path, request_id, response_status)
                if response_status == 200:
                    body = json.dumps({
                        "id": "chatcmpl-retry-proof",
                        "object": "chat.completion",
                        "model": "inferlab-tiny",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "retry proof success"},
                            "finish_reason": "stop",
                        }],
                    }, separators=(",", ":")).encode("utf-8")
                else:
                    body = b'{"error":{"type":"proof_transient","message":"retry me"}}'
                self.send_response(response_status)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(body)))
                if request_id:
                    self.send_header("x-inferlab-request-id", request_id)
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, format: str, *values: object) -> None:
                return

        return Handler

    first = ThreadingHTTPServer(_parse_bind(args.first_bind), handler("first", 503, 0))
    second = ThreadingHTTPServer(
        _parse_bind(args.second_bind), handler("second", 200, args.second_delay_ms)
    )
    threads = [
        threading.Thread(target=first.serve_forever, daemon=True),
        threading.Thread(target=second.serve_forever, daemon=True),
    ]
    for thread in threads:
        thread.start()

    def stop(_signal: int, _frame: object) -> None:
        stopped.set()

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    try:
        while not stopped.wait(0.1):
            pass
    finally:
        first.shutdown()
        second.shutdown()
        first.server_close()
        second.server_close()
        journal.close()


def capture_retry_events(args: argparse.Namespace) -> None:
    records = [json.loads(line) for line in args.events.read_text(encoding="utf-8").splitlines()]
    sequences = [record.get("sequence") for record in records]
    print(json.dumps({
        "schema": "inferlab.request-id-retry-events.v0.26",
        "event_count": len(records),
        "sequences_contiguous": sequences == list(range(1, len(records) + 1)),
        "records": records,
    }, indent=2, sort_keys=True))


def extract_log_events(args: argparse.Namespace) -> None:
    requested = set(args.request_ids.split(","))
    events: list[dict[str, Any]] = []
    for line_number, raw in enumerate(args.log_file.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
        try:
            value = json.loads(raw)
        except json.JSONDecodeError:
            continue
        if not isinstance(value, dict):
            continue
        nested_fields = value.get("fields")
        event_fields = nested_fields if isinstance(nested_fields, dict) else value
        request_id = event_fields.get("request_id")
        if not isinstance(request_id, str) or request_id not in requested:
            continue
        retained = {
            "line": line_number,
            "level": value.get("level"),
            "target": value.get("target"),
        }
        for field in [
            "service",
            "event",
            "request_id",
            "worker_id",
            "request_number",
            "mode",
            "outcome",
            "duration_ms",
            "route",
            "method",
            "status",
            "message",
        ]:
            retained[field] = event_fields.get(field)
        events.append(retained)
    print(json.dumps({
        "schema": "inferlab.request-id-log-evidence.v0.26",
        "requested_ids": sorted(requested),
        "observed_ids": sorted({event["request_id"] for event in events}),
        "events": events,
    }, indent=2, sort_keys=True))


def sanitize_evidence(args: argparse.Namespace) -> None:
    evidence = args.evidence_dir.resolve()
    exact_paths = {
        args.proof_root,
        os.path.normpath(args.proof_root),
        str(Path(args.proof_root).resolve()),
        args.project_root,
        os.path.normpath(args.project_root),
        str(Path(args.project_root).resolve()),
    }
    exact_paths = {value for value in exact_paths if value and value != os.path.sep}
    sensitive_keys = {
        "data_directory",
        "event_path",
        "state_path",
        "snapshot_path",
        "wal_path",
        "model_path",
        "log_file",
    }
    host_path = re.compile(r"(?:/Users|/home|/private/var|/tmp)/[^\s\"'<>]+")
    private_markers = ("-----BEGIN", "PRIVATE KEY", "CERTIFICATE-----", "authorization: Bearer")
    replacements = 0

    def redact(value: Any, key: str | None = None) -> Any:
        nonlocal replacements
        if isinstance(value, dict):
            return {item_key: redact(item_value, item_key) for item_key, item_value in value.items()}
        if isinstance(value, list):
            return [redact(item) for item in value]
        if not isinstance(value, str):
            return value
        if key in sensitive_keys:
            replacements += 1
            return "<redacted-sensitive-path>"
        result = value
        for path in sorted(exact_paths, key=len, reverse=True):
            if path in result:
                replacements += result.count(path)
                result = result.replace(path, "<redacted-host-path>")
        result, count = host_path.subn("<redacted-host-path>", result)
        replacements += count
        return result

    json_files = sorted(evidence.glob("*.json"))
    for path in json_files:
        value = json.loads(path.read_text(encoding="utf-8"))
        path.write_text(json.dumps(redact(value), indent=2, sort_keys=True) + "\n", encoding="utf-8")
    retained_files = sorted(path for path in evidence.iterdir() if path.is_file())
    retained = "\n".join(path.read_text(encoding="utf-8", errors="replace") for path in retained_files)
    leaked_markers = [marker for marker in private_markers if marker in retained]
    remaining_paths = [path for path in exact_paths if path in retained]
    if leaked_markers or remaining_paths or host_path.search(retained):
        raise SystemExit("evidence sanitizer left private material or host paths")
    print(json.dumps({
        "schema": "inferlab.evidence-sanitizer.v0.26",
        "files_sanitized": [path.name for path in json_files],
        "replacement_count": replacements,
        "private_material_markers": 0,
        "remaining_host_paths": 0,
    }, indent=2, sort_keys=True))


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)

    wait = commands.add_parser("wait-endpoint")
    wait.add_argument("--url", required=True)
    wait.add_argument("--status", type=int, default=200)
    wait.add_argument("--timeout", type=float, default=60.0)
    wait.add_argument("--request-timeout", type=float, default=0.2)
    wait.set_defaults(handler=wait_endpoint)

    scrape = commands.add_parser("scrape-set")
    scrape.add_argument("--targets-file", type=Path, required=True)
    scrape.add_argument("--checkpoint", required=True)
    scrape.add_argument("--raw-dir", type=Path, required=True)
    scrape.add_argument("--timeout", type=float, default=3.0)
    scrape.set_defaults(handler=scrape_set)

    statuses = commands.add_parser("capture-json-set")
    statuses.add_argument("--targets-file", type=Path, required=True)
    statuses.add_argument("--url-field", default="status_url")
    statuses.add_argument("--use-bearer-env", action="store_true")
    statuses.add_argument("--timeout", type=float, default=3.0)
    statuses.set_defaults(handler=capture_json_set)

    request = commands.add_parser("request")
    request.add_argument("--url", required=True)
    request.add_argument("--method", default="GET")
    request.add_argument("--body", type=Path)
    request.add_argument("--content-type", default="application/json")
    request.add_argument("--request-id")
    request.add_argument("--use-bearer-env", action="store_true")
    request.add_argument("--expect-status", type=int)
    request.add_argument("--timeout", type=float, default=10.0)
    request.set_defaults(handler=request_capture)

    batch = commands.add_parser("batch-scenario")
    batch.add_argument("--base-url", required=True)
    batch.add_argument("--timeout", type=float, default=3.0)
    batch.set_defaults(handler=batch_scenario)

    trust = commands.add_parser("trust-scenario")
    trust.add_argument("--base-url", required=True)
    trust.add_argument("--snapshot", type=Path, required=True)
    trust.add_argument("--tampered-snapshot", type=Path, required=True)
    trust.add_argument("--timeout", type=float, default=3.0)
    trust.set_defaults(handler=trust_scenario)

    prompts = commands.add_parser("unique-prompts")
    prompts.add_argument("--url", required=True)
    prompts.add_argument("--count", type=int, default=24)
    prompts.add_argument("--prompt-prefix", default="observability-cardinality-canary")
    prompts.add_argument("--request-id-prefix", default="obs.cardinality")
    prompts.add_argument("--max-tokens", type=int, default=4)
    prompts.add_argument("--use-bearer-env", action="store_true")
    prompts.add_argument("--timeout", type=float, default=10.0)
    prompts.set_defaults(handler=unique_prompts)

    stream = commands.add_parser("stream")
    stream.add_argument("--url", required=True)
    stream.add_argument("--prompt", required=True)
    stream.add_argument("--request-id", required=True)
    stream.add_argument("--max-tokens", type=int, default=8)
    stream.add_argument("--speculative-tokens", type=int, default=2)
    stream.add_argument("--use-bearer-env", action="store_true")
    stream.add_argument("--timeout", type=float, default=15.0)
    stream.set_defaults(handler=stream_capture)

    servers = commands.add_parser("retry-servers")
    servers.add_argument("--first-bind", required=True)
    servers.add_argument("--second-bind", required=True)
    servers.add_argument("--second-delay-ms", type=int, default=25)
    servers.add_argument("--events", type=Path, required=True)
    servers.set_defaults(handler=retry_servers)

    retry_events = commands.add_parser("capture-retry-events")
    retry_events.add_argument("--events", type=Path, required=True)
    retry_events.set_defaults(handler=capture_retry_events)

    logs = commands.add_parser("extract-log-events")
    logs.add_argument("--log-file", type=Path, required=True)
    logs.add_argument("--request-ids", required=True)
    logs.set_defaults(handler=extract_log_events)

    sanitizer = commands.add_parser("sanitize-evidence")
    sanitizer.add_argument("--evidence-dir", type=Path, required=True)
    sanitizer.add_argument("--proof-root", required=True)
    sanitizer.add_argument("--project-root", required=True)
    sanitizer.set_defaults(handler=sanitize_evidence)
    return parser


def main() -> None:
    args = build_parser().parse_args()
    args.handler(args)


if __name__ == "__main__":
    main()
