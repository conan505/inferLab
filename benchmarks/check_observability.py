#!/usr/bin/env python3
"""Check the retained InferLab v0.26 bounded-cardinality evidence."""

from __future__ import annotations

import argparse
import json
import math
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from observability_probe import (
    OPENMETRICS_CONTENT_TYPE,
    REQUEST_ID,
    MetricSample,
    parse_openmetrics,
)


HISTOGRAM_BUCKETS = (0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0)
HISTOGRAM_BUCKET_LABELS = tuple(str(value) for value in HISTOGRAM_BUCKETS) + ("+Inf",)
SERVICES = {
    "gateway",
    "cpu-worker",
    "batch-queue",
    "control-plane",
    "trust-distributor",
    "raft-link-proxy",
}
EXPECTED_TARGETS = {
    "gateway-primary": "gateway",
    "gateway-retry": "gateway",
    "cpu-worker": "cpu-worker",
    "batch-queue": "batch-queue",
    "control-a": "control-plane",
    "control-b": "control-plane",
    "control-c": "control-plane",
    "trust-distributor": "trust-distributor",
    "raft-link-proxy": "raft-link-proxy",
}
ROUTE_METHOD_PAIRS = {
    "gateway": frozenset({
        ("/", "GET"),
        ("/assets/og-inferlab.png", "GET"),
        ("/health", "GET"),
        ("/readyz", "GET"),
        ("/showcase/status", "GET"),
        ("/internal/workers", "GET"),
        ("/v1/chat/completions", "POST"),
        ("unmatched", "other"),
    }),
    "cpu-worker": frozenset({
        ("/health", "GET"),
        ("/internal/scheduler", "GET"),
        ("/internal/cache", "GET"),
        ("/v1/chat/completions", "POST"),
        ("unmatched", "other"),
    }),
    "batch-queue": frozenset({
        ("/healthz", "GET"),
        ("/v1/batch/jobs", "POST"),
        ("/v1/batch/claim", "POST"),
        ("/v1/batch/jobs/{job_id}", "GET"),
        ("/v1/batch/jobs/{job_id}/ack", "POST"),
        ("/v1/batch/jobs/{job_id}/fail", "POST"),
        ("/v1/batch/dead-letter", "GET"),
        ("/internal/status", "GET"),
        ("unmatched", "other"),
    }),
    "control-plane": frozenset({
        ("/healthz", "GET"),
        ("/raft/request-vote", "POST"),
        ("/raft/append-entries", "POST"),
        ("/v1/control/status", "GET"),
        ("/v1/control/config", "GET"),
        ("/v1/control/config", "PUT"),
        ("unmatched", "other"),
    }),
    "trust-distributor": frozenset({
        ("/health", "GET"),
        ("/readyz", "GET"),
        ("/v1/service-trust/status", "GET"),
        ("/v1/service-trust/snapshot", "GET"),
        ("/v1/service-trust/snapshot", "POST"),
        ("/v1/service-trust/receipts", "POST"),
        ("unmatched", "other"),
    }),
    "raft-link-proxy": frozenset({
        ("/healthz", "GET"),
        ("/v1/link/status", "GET"),
        ("/v1/link/mode", "PUT"),
        ("/raft/request-vote", "POST"),
        ("/raft/append-entries", "POST"),
        ("unmatched", "other"),
    }),
}
ROUTES = {
    service: {route for route, _method in pairs}
    for service, pairs in ROUTE_METHOD_PAIRS.items()
}


@dataclass(frozen=True)
class FamilySpec:
    metric_type: str
    labels: dict[str, frozenset[str] | None]
    unit: str | None = None


def labels(**values: set[str] | None) -> dict[str, frozenset[str] | None]:
    return {key: None if value is None else frozenset(value) for key, value in values.items()}


COMMON = {
    "inferlab_http_requests_total": FamilySpec(
        "counter",
        labels(
            service=SERVICES,
            route=None,
            method={"GET", "POST", "PUT", "other"},
            status_class={"2xx", "3xx", "4xx", "5xx"},
        ),
    ),
    "inferlab_http_handler_duration_seconds": FamilySpec(
        "histogram",
        labels(service=SERVICES, route=None, method={"GET", "POST", "PUT", "other"}),
        "seconds",
    ),
    "inferlab_http_requests_in_flight": FamilySpec("gauge", labels(service=SERVICES)),
}


DOMAIN: dict[str, dict[str, FamilySpec]] = {
    "gateway": {
        "inferlab_gateway_admission_current": FamilySpec(
            "gauge", labels(state={"outstanding", "executing", "queued"})
        ),
        "inferlab_gateway_admission_rejections_total": FamilySpec("counter", labels()),
        "inferlab_gateway_requests_total": FamilySpec("counter", labels()),
        "inferlab_gateway_attempts_total": FamilySpec("counter", labels()),
        "inferlab_gateway_transient_failures_total": FamilySpec("counter", labels()),
        "inferlab_gateway_retries_total": FamilySpec(
            "counter", labels(decision={"granted", "budget_denied", "limit_exhausted"})
        ),
        "inferlab_gateway_deadlines_exceeded_total": FamilySpec("counter", labels()),
        "inferlab_gateway_workers": FamilySpec("gauge", labels()),
        "inferlab_gateway_worker_requests_in_flight": FamilySpec("gauge", labels()),
        "inferlab_gateway_worker_circuits": FamilySpec(
            "gauge", labels(state={"closed", "open", "half_open"})
        ),
        "inferlab_gateway_routing_lease_ready": FamilySpec("gauge", labels()),
        "inferlab_gateway_control_revision": FamilySpec("gauge", labels()),
        "inferlab_gateway_completion_duration_seconds": FamilySpec(
            "histogram",
            labels(outcome={"success", "error", "cancelled", "deadline"}),
            "seconds",
        ),
    },
    "cpu-worker": {
        "inferlab_worker_requests_total": FamilySpec("counter", labels()),
        "inferlab_worker_scheduler_current": FamilySpec(
            "gauge", labels(state={"queued", "active"})
        ),
        "inferlab_worker_scheduler_requests_total": FamilySpec(
            "counter", labels(outcome={"admitted", "completed", "cancelled", "failed"})
        ),
        "inferlab_worker_scheduler_batches_total": FamilySpec("counter", labels()),
        "inferlab_worker_tokens_total": FamilySpec("counter", labels()),
        "inferlab_worker_batch_slots_total": FamilySpec(
            "counter", labels(state={"used", "available"})
        ),
        "inferlab_worker_generation_duration_seconds": FamilySpec(
            "histogram", labels(outcome={"success", "error", "cancelled"}), "seconds"
        ),
        # Native paged-cache diagnostics deliberately remain on-demand JSON in
        # v0.26: exporting them would lock and scan the allocator on every scrape.
    },
    "batch-queue": {
        "inferlab_queue_jobs": FamilySpec(
            "gauge", labels(state={"pending", "claimed", "completed", "dead_letter"})
        ),
        "inferlab_queue_wal_bytes": FamilySpec("gauge", labels(), "bytes"),
        "inferlab_queue_wal_events_total": FamilySpec("counter", labels()),
        "inferlab_queue_claims_total": FamilySpec("counter", labels()),
        "inferlab_queue_acknowledgments_total": FamilySpec("counter", labels()),
        "inferlab_queue_redeliveries_total": FamilySpec("counter", labels()),
        "inferlab_queue_failures_total": FamilySpec(
            "counter", labels(kind={"explicit", "dead_lettered", "torn_tail"})
        ),
    },
    "control-plane": {
        "inferlab_control_role": FamilySpec(
            "gauge", labels(role={"follower", "candidate", "leader"})
        ),
        "inferlab_control_term": FamilySpec("gauge", labels()),
        "inferlab_control_commit_index": FamilySpec("gauge", labels()),
        "inferlab_control_last_applied": FamilySpec("gauge", labels()),
        "inferlab_control_last_log_index": FamilySpec("gauge", labels()),
        "inferlab_control_storage_healthy": FamilySpec("gauge", labels()),
        "inferlab_control_elections_total": FamilySpec("counter", labels()),
        "inferlab_control_leadership_terms_total": FamilySpec("counter", labels()),
        "inferlab_control_votes_granted_total": FamilySpec("counter", labels()),
        "inferlab_control_append_entries_total": FamilySpec(
            "counter", labels(result={"accepted", "rejected"})
        ),
        "inferlab_control_replication_total": FamilySpec(
            "counter", labels(result={"success", "failure"})
        ),
        "inferlab_control_write_authorization_total": FamilySpec(
            "counter",
            labels(
                result={
                    "verified",
                    "committed",
                    "auth_rejected",
                    "freshness_rejected",
                    "revision_conflict",
                }
            ),
        ),
        "inferlab_control_service_authentication_total": FamilySpec(
            "counter",
            labels(
                result={
                    "verified",
                    "auth_rejected",
                    "freshness_rejected",
                    "replay_rejected",
                    "authorization_rejected",
                    "credential_revoked",
                    "peer_authorized",
                    "gateway_authorized",
                }
            ),
        ),
        "inferlab_control_trust_policy_total": FamilySpec(
            "counter", labels(result={"reloaded", "rejected"})
        ),
        "inferlab_control_trust_fetch_consecutive_failures": FamilySpec("gauge", labels()),
        "inferlab_control_trust_receipts_total": FamilySpec(
            "counter", labels(result={"posted", "failed"})
        ),
    },
    "trust-distributor": {
        "inferlab_trust_snapshot_requests_total": FamilySpec(
            "counter", labels(outcome={"served", "not_modified", "unavailable"})
        ),
        "inferlab_trust_snapshot_publish_total": FamilySpec(
            "counter", labels(outcome={"published", "unchanged", "rejected", "storage_error"})
        ),
        "inferlab_trust_receipts_total": FamilySpec(
            "counter", labels(outcome={"recorded", "duplicate", "rejected", "storage_error"})
        ),
        "inferlab_trust_snapshot_generation": FamilySpec("gauge", labels()),
        "inferlab_trust_receivers": FamilySpec(
            "gauge", labels(state={"expected", "acked", "pending"})
        ),
        "inferlab_trust_storage_healthy": FamilySpec("gauge", labels()),
    },
    "raft-link-proxy": {
        "inferlab_raft_link_mode": FamilySpec("gauge", labels(mode={"allow", "drop"})),
        "inferlab_raft_link_mode_changes_total": FamilySpec("counter", labels()),
        "inferlab_raft_link_requests_total": FamilySpec(
            "counter", labels(outcome={"forwarded", "dropped", "upstream_failure"})
        ),
        "inferlab_raft_link_last_transition_timestamp_seconds": FamilySpec(
            "gauge", labels(), "seconds"
        ),
    },
}


def expected_families(service: str) -> dict[str, FamilySpec]:
    return {**COMMON, **DOMAIN[service]}


def theoretical_family_series(service: str, spec: FamilySpec) -> int:
    """Derive one family's maximum from its exact finite label domains.

    Route and method are a closed pair relation, not an independent Cartesian
    product. The service label is fixed for one scrape target. Every other
    label must have an explicit finite value set.
    """
    label_names = set(spec.labels)
    has_route_method = bool(label_names.intersection({"route", "method"}))
    if has_route_method and not {"route", "method"}.issubset(label_names):
        raise ValueError(f"{service}: route and method labels must appear together")

    combinations = len(ROUTE_METHOD_PAIRS[service]) if has_route_method else 1
    for label_name, allowed in spec.labels.items():
        if label_name in {"route", "method"}:
            continue
        if label_name == "service":
            combinations *= 1
            continue
        if allowed is None:
            raise ValueError(f"{service}: {label_name} has no finite domain")
        combinations *= len(allowed)

    samples_per_combination = len(HISTOGRAM_BUCKETS) + 3 if spec.metric_type == "histogram" else 1
    return combinations * samples_per_combination


def theoretical_series_by_service() -> dict[str, int]:
    return {
        service: sum(
            theoretical_family_series(service, spec)
            for spec in expected_families(service).values()
        )
        for service in sorted(SERVICES)
    }


def theoretical_family_series_by_service() -> dict[str, dict[str, int]]:
    return {
        service: {
            family: theoretical_family_series(service, spec)
            for family, spec in sorted(expected_families(service).items())
        }
        for service in sorted(SERVICES)
    }


def load(directory: Path, name: str) -> dict[str, Any]:
    return json.loads((directory / name).read_text(encoding="utf-8"))


def normalize_content_type(value: str | None) -> str:
    if value is None:
        return ""
    parts = [part.strip().lower() for part in value.split(";")]
    if not parts:
        return ""
    parameters = sorted(parts[1:])
    return "; ".join([parts[0], *parameters])


def expected_content_type() -> str:
    return normalize_content_type(OPENMETRICS_CONTENT_TYPE)


def family_sample_names(name: str, spec: FamilySpec) -> set[str]:
    if spec.metric_type == "histogram":
        return {f"{name}_bucket", f"{name}_sum", f"{name}_count"}
    return {name}


def metadata_family_name(name: str, spec: FamilySpec) -> str:
    """Return the OpenMetrics HELP/TYPE family name for a sample catalog name.

    OpenMetrics counter samples end in ``_total`` while their HELP/TYPE family
    name omits that suffix. The locked InferLab catalog names the sample that a
    PromQL user queries, so the checker binds both representations explicitly.
    """
    if spec.metric_type == "counter" and name.endswith("_total"):
        return name.removesuffix("_total")
    return name


def parse_scrape_set(directory: Path, name: str) -> tuple[dict[str, Any], dict[str, dict[str, Any]]]:
    capture = load(directory, name)
    parsed: dict[str, dict[str, Any]] = {}
    checkpoint = capture.get("checkpoint")
    expected_checkpoint = name.removesuffix("-scrapes.json")
    if checkpoint != expected_checkpoint:
        raise ValueError(
            f"scrape checkpoint {checkpoint!r} != {expected_checkpoint!r} for {name}"
        )
    for target, observation in capture.get("targets", {}).items():
        expected_raw_file = f"{checkpoint}-{target}.prom"
        if observation.get("raw_file") != expected_raw_file:
            raise ValueError(
                f"{target} raw file {observation.get('raw_file')!r} != {expected_raw_file!r}"
            )
        raw_path = directory / observation["raw_file"]
        raw = raw_path.read_text(encoding="utf-8")
        document = parse_openmetrics(raw)
        encoded = raw.encode("utf-8")
        if len(encoded) != observation.get("bytes"):
            raise ValueError(f"{target} byte count does not match {name}")
        import hashlib

        if hashlib.sha256(encoded).hexdigest() != observation.get("sha256"):
            raise ValueError(f"{target} SHA-256 does not match {name}")
        if document["sample_count"] != observation.get("sample_count"):
            raise ValueError(f"{target} sample count does not match {name}")
        if document["family_count"] != observation.get("family_count"):
            raise ValueError(f"{target} family count does not match {name}")
        if sorted(document["types"]) != observation.get("families"):
            raise ValueError(f"{target} family summary does not match {name}")
        parsed[target] = {"raw": raw, "document": document, "observation": observation}
    if capture.get("target_count") != len(parsed):
        raise ValueError(f"target count does not match {name}")
    if capture.get("series_total") != sum(
        entry["document"]["sample_count"] for entry in parsed.values()
    ):
        raise ValueError(f"series total does not match {name}")
    return capture, parsed


def samples(document: dict[str, Any], name: str) -> list[MetricSample]:
    return [sample for sample in document["samples"] if sample.name == name]


def sample_value(document: dict[str, Any], name: str, **wanted: str) -> float:
    matches = [
        sample.value
        for sample in document["samples"]
        if sample.name == name and dict(sample.labels) == wanted
    ]
    if len(matches) != 1:
        raise ValueError(f"expected one {name}{wanted}, observed {len(matches)}")
    return matches[0]


def histogram_count(document: dict[str, Any], name: str, **wanted: str) -> float:
    return sample_value(document, f"{name}_count", **wanted)


def series_identities(document: dict[str, Any]) -> set[tuple[str, tuple[tuple[str, str], ...]]]:
    return {(sample.name, sample.labels) for sample in document["samples"]}


def validate_catalog(
    target: str,
    service: str,
    document: dict[str, Any],
) -> list[str]:
    errors: list[str] = []
    catalog = expected_families(service)
    observed_types = document["types"]
    expected_metadata = {
        metadata_family_name(family, spec): (family, spec)
        for family, spec in catalog.items()
    }
    if set(observed_types) != set(expected_metadata):
        errors.append(
            f"{target}: metadata family set mismatch "
            f"missing={sorted(set(expected_metadata)-set(observed_types))} "
            f"extra={sorted(set(observed_types)-set(expected_metadata))}"
        )
    for family, spec in catalog.items():
        metadata_name = metadata_family_name(family, spec)
        if observed_types.get(metadata_name) != spec.metric_type:
            errors.append(
                f"{target}: {metadata_name} type is {observed_types.get(metadata_name)!r}"
            )
        if not document["help"].get(metadata_name):
            errors.append(f"{target}: {family} has no non-empty HELP text")
        unit = document["units"].get(metadata_name)
        if spec.unit is not None and unit != spec.unit:
            errors.append(f"{target}: {family} unit {unit!r} != required {spec.unit!r}")
        if spec.unit is None and unit is not None:
            errors.append(f"{target}: unitless family {family} unexpectedly declares {unit!r}")

        expected_names = family_sample_names(family, spec)
        family_samples = [sample for sample in document["samples"] if sample.name in expected_names]
        if not family_samples:
            errors.append(f"{target}: {family} has no samples")
            continue
        for sample in family_samples:
            observed_labels = dict(sample.labels)
            required = set(spec.labels)
            if spec.metric_type == "histogram" and sample.name.endswith("_bucket"):
                required.add("le")
            if set(observed_labels) != required:
                errors.append(
                    f"{target}: {sample.name} labels {sorted(observed_labels)} != {sorted(required)}"
                )
                continue
            if {"route", "method"}.issubset(observed_labels):
                route_method = (observed_labels["route"], observed_labels["method"])
                if route_method not in ROUTE_METHOD_PAIRS[service]:
                    errors.append(
                        f"{target}: {sample.name} route/method pair {route_method!r} is not allowed"
                    )
            for label_name, allowed in spec.labels.items():
                value = observed_labels[label_name]
                if label_name == "service" and value != service:
                    errors.append(f"{target}: service label {value!r} != {service!r}")
                elif label_name == "route" and value not in ROUTES[service]:
                    errors.append(f"{target}: unbounded/unknown route label {value!r}")
                elif allowed is not None and value not in allowed:
                    errors.append(
                        f"{target}: {sample.name} label {label_name}={value!r} is not allowed"
                    )
            if sample.value < 0 and spec.metric_type in {"counter", "histogram"}:
                errors.append(f"{target}: {sample.name} is negative")
    known_names = {
        sample_name
        for family, spec in catalog.items()
        for sample_name in family_sample_names(family, spec)
    }
    unknown_samples = sorted({sample.name for sample in document["samples"]} - known_names)
    if unknown_samples:
        errors.append(f"{target}: unknown samples {unknown_samples}")
    return errors


def audit_histograms(checkpoints: dict[str, dict[str, dict[str, Any]]]) -> dict[str, Any]:
    observations: list[dict[str, Any]] = []
    errors: list[str] = []
    for checkpoint, targets in checkpoints.items():
        for target, entry in targets.items():
            service = entry["observation"]["service"]
            document = entry["document"]
            for family, spec in expected_families(service).items():
                if spec.metric_type != "histogram":
                    continue
                buckets = samples(document, f"{family}_bucket")
                sums = samples(document, f"{family}_sum")
                counts = samples(document, f"{family}_count")
                grouped: dict[tuple[tuple[str, str], ...], list[MetricSample]] = {}
                for bucket in buckets:
                    base = tuple((name, value) for name, value in bucket.labels if name != "le")
                    grouped.setdefault(base, []).append(bucket)
                sum_map = {sample.labels: sample.value for sample in sums}
                count_map = {sample.labels: sample.value for sample in counts}
                bucket_bases = set(grouped)
                sum_bases = set(sum_map)
                count_bases = set(count_map)
                if not bucket_bases:
                    errors.append(
                        f"{checkpoint}/{target}/{family} has no histogram label sets"
                    )
                if not (bucket_bases == sum_bases == count_bases):
                    errors.append(
                        f"{checkpoint}/{target}/{family} bucket/sum/count label-set parity failed "
                        f"missing_sum={sorted(bucket_bases - sum_bases)!r} "
                        f"missing_count={sorted(bucket_bases - count_bases)!r} "
                        f"orphan_sum={sorted(sum_bases - bucket_bases)!r} "
                        f"orphan_count={sorted(count_bases - bucket_bases)!r}"
                    )
                for base in sorted(bucket_bases | sum_bases | count_bases):
                    group = grouped.get(base, [])
                    decoded: list[tuple[float, float, str]] = []
                    for bucket in group:
                        encoded = dict(bucket.labels)["le"]
                        if encoded == "+Inf":
                            boundary = math.inf
                        else:
                            try:
                                boundary = float(encoded)
                            except ValueError:
                                boundary = math.nan
                        decoded.append((boundary, bucket.value, encoded))
                    decoded.sort(key=lambda item: item[0])
                    boundaries = tuple(item[0] for item in decoded[:-1]) if decoded else ()
                    encoded_boundaries = tuple(item[2] for item in decoded)
                    values = [item[1] for item in decoded]
                    count = count_map.get(base)
                    total = sum_map.get(base)
                    valid = (
                        len(decoded) == len(HISTOGRAM_BUCKETS) + 1
                        and boundaries == HISTOGRAM_BUCKETS
                        and encoded_boundaries == HISTOGRAM_BUCKET_LABELS
                        and math.isinf(decoded[-1][0])
                        and all(left <= right for left, right in zip(values, values[1:]))
                        and count is not None
                        and values[-1] == count
                        and total is not None
                        and math.isfinite(total)
                        and total >= 0
                    )
                    if not valid:
                        errors.append(f"{checkpoint}/{target}/{family}{dict(base)} histogram algebra failed")
                    observations.append({
                        "checkpoint": checkpoint,
                        "target": target,
                        "family": family,
                        "labels": dict(base),
                        "bucket_count": len(decoded),
                        "count": count,
                        "sum": total,
                        "valid": valid,
                    })
    return {
        "schema": "inferlab.openmetrics-histogram-audit.v0.26",
        "bucket_boundaries_seconds": list(HISTOGRAM_BUCKETS),
        "histograms_checked": len(observations),
        "all_valid": not errors and bool(observations),
        "errors": errors,
        "observations": observations,
    }


def audit_cardinality(checkpoints: dict[str, dict[str, dict[str, Any]]]) -> dict[str, Any]:
    observations: list[dict[str, Any]] = []
    all_within = True
    for checkpoint, targets in checkpoints.items():
        topology = 0
        for target, entry in targets.items():
            count = entry["document"]["sample_count"]
            topology += count
            within = count <= 256
            all_within = all_within and within
            observations.append({
                "checkpoint": checkpoint,
                "target": target,
                "series": count,
                "cap": 256,
                "within_cap": within,
            })
        within = topology <= 2500
        all_within = all_within and within
        observations.append({
            "checkpoint": checkpoint,
            "target": "<topology>",
            "series": topology,
            "cap": 2500,
            "within_cap": within,
        })
    theoretical_by_service = theoretical_series_by_service()
    theoretical_families = theoretical_family_series_by_service()
    theoretical_targets = {
        target: theoretical_by_service[service]
        for target, service in EXPECTED_TARGETS.items()
    }
    theoretical_topology = sum(theoretical_targets.values())
    return {
        "schema": "inferlab.openmetrics-cardinality-audit.v0.26",
        "per_target_cap": 256,
        "topology_cap": 2500,
        "all_within_caps": all_within,
        "theoretical_targets": theoretical_targets,
        "theoretical_families_by_service": theoretical_families,
        "route_method_pair_counts": {
            service: len(pairs) for service, pairs in sorted(ROUTE_METHOD_PAIRS.items())
        },
        "theoretical_topology": theoretical_topology,
        "theoretical_all_within_caps": max(theoretical_targets.values()) <= 256
        and theoretical_topology <= 2500,
        "observations": observations,
    }


def contract_json() -> dict[str, Any]:
    theoretical_by_service = theoretical_series_by_service()
    theoretical_families = theoretical_family_series_by_service()
    return {
        "schema": "inferlab.openmetrics-contract.v0.26",
        "content_type": OPENMETRICS_CONTENT_TYPE,
        "histogram_buckets_seconds": list(HISTOGRAM_BUCKETS),
        "service_values": sorted(SERVICES),
        "routes": {service: sorted(values) for service, values in sorted(ROUTES.items())},
        "route_method_pairs": {
            service: [
                {"route": route, "method": method}
                for route, method in sorted(pairs)
            ]
            for service, pairs in sorted(ROUTE_METHOD_PAIRS.items())
        },
        "per_target_series_cap": 256,
        "topology_series_cap": 2500,
        "theoretical_series_by_service": theoretical_by_service,
        "theoretical_family_series_by_service": theoretical_families,
        "proof_topology_theoretical_series": sum(
            theoretical_by_service[service] for service in EXPECTED_TARGETS.values()
        ),
        "families": {
            service: {
                name: {
                    "type": spec.metric_type,
                    "labels": {
                        label: None if values is None else sorted(values)
                        for label, values in sorted(spec.labels.items())
                    },
                    "unit": spec.unit,
                }
                for name, spec in sorted(expected_families(service).items())
            }
            for service in sorted(SERVICES)
        },
        "worker_cache_export_deferred": [
            "inferlab_worker_kv_pages",
            "inferlab_worker_kv_bytes",
            "inferlab_worker_prefix_cache_requests_total",
            "inferlab_worker_prefix_cache_evictions_total",
            "inferlab_worker_kv_allocation_failures_total",
        ],
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--contract-output", type=Path, required=True)
    parser.add_argument("--cardinality-output", type=Path, required=True)
    parser.add_argument("--histogram-output", type=Path, required=True)
    parser.add_argument("--delta-output", type=Path, required=True)
    args = parser.parse_args()
    directory = args.evidence_dir

    inventory = load(directory, "target-inventory.json")
    statuses = load(directory, "final-statuses.json")
    valid_request = load(directory, "request-id-valid.json")
    invalid_request = load(directory, "request-id-invalid.json")
    stream = load(directory, "stream.json")
    worker_logs = load(directory, "worker-request-id-events.json")
    retry_request = load(directory, "request-id-retry.json")
    retry_events = load(directory, "retry-events.json")
    unique = load(directory, "unique-prompts.json")
    batch_scenario = load(directory, "batch-scenario.json")
    trust_scenario = load(directory, "trust-scenario.json")
    link_scenario = load(directory, "link-scenario.json")
    continuity = load(directory, "process-continuity.json")
    sanitizer = load(directory, "sanitizer.json")
    private_scan = load(directory, "private-material-scan.json")

    checkpoint_files = {
        "baseline": "baseline-scrapes.json",
        "before-cardinality": "before-cardinality-scrapes.json",
        "after-cardinality": "after-cardinality-scrapes.json",
        "final": "final-scrapes.json",
    }
    captures: dict[str, dict[str, Any]] = {}
    checkpoints: dict[str, dict[str, dict[str, Any]]] = {}
    for checkpoint, file_name in checkpoint_files.items():
        capture, parsed = parse_scrape_set(directory, file_name)
        captures[checkpoint] = capture
        checkpoints[checkpoint] = parsed

    assertions: list[dict[str, Any]] = []

    def check(name: str, condition: bool, detail: Any) -> None:
        assertions.append({"name": name, "passed": bool(condition), "detail": detail})

    inventory_targets = inventory.get("targets", [])
    inventory_rows_valid = isinstance(inventory_targets, list) and all(
        isinstance(target, dict) for target in inventory_targets
    )
    target_names = [target.get("name") for target in inventory_targets] if inventory_rows_valid else []
    target_map = (
        {target.get("name"): target.get("service") for target in inventory_targets}
        if inventory_rows_valid
        else {}
    )
    check(
        "inventory contains the exact nine metrics targets and six service classes",
        inventory.get("schema") == "inferlab.observability-target-inventory.v0.26"
        and inventory_rows_valid
        and len(inventory_targets) == 9
        and inventory.get("target_count") == 9
        and len(set(target_names)) == 9
        and target_map == EXPECTED_TARGETS
        and set(target_map.values()) == SERVICES,
        target_map,
    )

    for checkpoint, capture in captures.items():
        observations = capture.get("targets", {})
        check(
            f"{checkpoint} scrape set is complete OpenMetrics 1.0",
            capture.get("schema") == "inferlab.openmetrics-scrape-set.v0.26"
            and capture.get("checkpoint") == checkpoint
            and set(observations) == set(EXPECTED_TARGETS)
            and capture.get("target_count") == len(EXPECTED_TARGETS)
            and all(item.get("status") == 200 for item in observations.values())
            and all(
                normalize_content_type(item.get("content_type")) == expected_content_type()
                for item in observations.values()
            ),
            {
                target: {"status": item.get("status"), "content_type": item.get("content_type")}
                for target, item in observations.items()
            },
        )

    catalog_errors: list[str] = []
    for checkpoint, targets in checkpoints.items():
        for target, entry in targets.items():
            service = EXPECTED_TARGETS.get(target, "")
            if entry["observation"].get("service") != service:
                catalog_errors.append(f"{checkpoint}/{target}: service mismatch")
            catalog_errors.extend(
                f"{checkpoint}/{error}" for error in validate_catalog(target, service, entry["document"])
            )
    check(
        "every target exposes exactly its documented family and type catalog",
        not catalog_errors,
        catalog_errors,
    )
    check(
        "all metric labels use exact closed allowlists and route method pairs",
        not catalog_errors,
        {
            "allowlisted_services": sorted(SERVICES),
            "route_method_pair_counts": {
                key: len(value) for key, value in ROUTE_METHOD_PAIRS.items()
            },
        },
    )

    cardinality = audit_cardinality(checkpoints)
    check(
        "every target stays at or below 256 series",
        all(
            item["within_cap"]
            for item in cardinality["observations"]
            if item["target"] != "<topology>"
        ),
        [item for item in cardinality["observations"] if item["target"] != "<topology>"],
    )
    check(
        "every scrape set stays at or below 2500 topology series",
        all(
            item["within_cap"]
            for item in cardinality["observations"]
            if item["target"] == "<topology>"
        ),
        [item for item in cardinality["observations"] if item["target"] == "<topology>"],
    )
    check(
        "closed catalogs bound every target at 256 and this topology at 2500 series",
        cardinality["theoretical_all_within_caps"] is True
        and max(cardinality["theoretical_targets"].values()) == 255
        and cardinality["theoretical_topology"] == 1721
        and all(
            sum(cardinality["theoretical_families_by_service"][service].values())
            == theoretical_series_by_service()[service]
            for service in SERVICES
        ),
        {
            "targets": cardinality["theoretical_targets"],
            "topology": cardinality["theoretical_topology"],
            "route_method_pair_counts": cardinality["route_method_pair_counts"],
            "families": cardinality["theoretical_families_by_service"],
        },
    )

    histogram_audit = audit_histograms(checkpoints)
    check(
        "all histograms use the exact buckets and satisfy cumulative algebra",
        histogram_audit["all_valid"],
        {"checked": histogram_audit["histograms_checked"], "errors": histogram_audit["errors"]},
    )

    monotonic_errors: list[str] = []
    ordered = [checkpoints[name] for name in checkpoint_files]
    for target, service in EXPECTED_TARGETS.items():
        counter_names = {
            sample_name
            for family, spec in expected_families(service).items()
            if spec.metric_type in {"counter", "histogram"}
            for sample_name in family_sample_names(family, spec)
            if spec.metric_type == "counter" or sample_name.endswith("_count") or sample_name.endswith("_bucket") or sample_name.endswith("_sum")
        }
        previous: dict[tuple[str, tuple[tuple[str, str], ...]], float] = {}
        for targets in ordered:
            for sample in targets[target]["document"]["samples"]:
                if sample.name not in counter_names:
                    continue
                identity = (sample.name, sample.labels)
                if identity in previous and sample.value < previous[identity]:
                    monotonic_errors.append(f"{target} {sample.name}{dict(sample.labels)} decreased")
                previous[identity] = sample.value
    check("counters and histogram components never decrease without a restart", not monotonic_errors, monotonic_errors)

    before_gateway = checkpoints["before-cardinality"]["gateway-primary"]["document"]
    after_gateway = checkpoints["after-cardinality"]["gateway-primary"]["document"]
    before_worker = checkpoints["before-cardinality"]["cpu-worker"]["document"]
    after_worker = checkpoints["after-cardinality"]["cpu-worker"]["document"]
    check(
        "unique prompts create no gateway or worker time series",
        series_identities(before_gateway) == series_identities(after_gateway)
        and series_identities(before_worker) == series_identities(after_worker),
        {
            "gateway_before": len(series_identities(before_gateway)),
            "gateway_after": len(series_identities(after_gateway)),
            "worker_before": len(series_identities(before_worker)),
            "worker_after": len(series_identities(after_worker)),
        },
    )
    unique_count = unique.get("requested")
    unique_prompts = unique.get("prompts")
    unique_request_ids = unique.get("request_ids")
    unique_observations = unique.get("observations")
    check(
        "all unique-prompt requests succeed with stable valid request IDs",
        unique.get("schema") == "inferlab.observability-unique-prompts.v0.26"
        and type(unique_count) is int
        and unique_count >= 20
        and unique.get("succeeded") == unique_count
        and isinstance(unique_prompts, list)
        and len(unique_prompts) == unique_count
        and all(isinstance(value, str) for value in unique_prompts)
        and len(set(unique_prompts)) == unique_count
        and isinstance(unique_request_ids, list)
        and len(unique_request_ids) == unique_count
        and all(
            isinstance(value, str) and REQUEST_ID.fullmatch(value)
            for value in unique_request_ids
        )
        and len(set(unique_request_ids)) == unique_count
        and isinstance(unique_observations, list)
        and len(unique_observations) == unique_count
        and all(isinstance(item, dict) for item in unique_observations)
        and all(type(item.get("index")) is int for item in unique_observations)
        and {item["index"] for item in unique_observations} == set(range(unique_count))
        and all(item.get("status") == 200 for item in unique_observations)
        and all(
            item.get("request_id") == item.get("echoed_request_id")
            and item.get("request_id") == unique_request_ids[item["index"]]
            for item in unique_observations
        ),
        {"requested": unique_count, "succeeded": unique.get("succeeded")},
    )
    gateway_unique_delta = sample_value(after_gateway, "inferlab_gateway_requests_total") - sample_value(
        before_gateway, "inferlab_gateway_requests_total"
    )
    worker_unique_delta = sample_value(after_worker, "inferlab_worker_requests_total") - sample_value(
        before_worker, "inferlab_worker_requests_total"
    )
    check(
        "unique prompts increment gateway and worker request counters exactly",
        gateway_unique_delta == unique_count and worker_unique_delta == unique_count,
        {"gateway_delta": gateway_unique_delta, "worker_delta": worker_unique_delta, "requests": unique_count},
    )

    valid_supplied = valid_request.get("request", {}).get("request_id")
    valid_echoed = valid_request.get("response", {}).get("headers", {}).get("x-inferlab-request-id")
    invalid_supplied = invalid_request.get("request", {}).get("request_id")
    replacement = invalid_request.get("response", {}).get("headers", {}).get("x-inferlab-request-id")
    observed_log_ids = set(worker_logs.get("observed_ids", []))

    def generation_log_evidence(request_id: str | None, mode: str) -> tuple[bool, dict[str, Any]]:
        related = [
            event
            for event in worker_logs.get("events", [])
            if event.get("request_id") == request_id
        ]
        starts = [event for event in related if event.get("event") == "generation_started"]
        terminals = [event for event in related if event.get("event") == "generation_terminal"]
        responses = [event for event in related if event.get("event") == "http_response_headers"]
        valid = len(starts) == len(terminals) == len(responses) == 1
        if valid:
            start = starts[0]
            terminal = terminals[0]
            response = responses[0]
            request_number = start.get("request_number")
            valid = (
                start.get("target") == "cpu_worker::metrics"
                and terminal.get("target") == "cpu_worker::metrics"
                and response.get("target") == "observability::http"
                and start.get("service") == terminal.get("service") == response.get("service") == "cpu-worker"
                and start.get("worker_id") == terminal.get("worker_id") == "cpu-observability-canary"
                and isinstance(request_number, int)
                and request_number > 0
                and terminal.get("request_number") == request_number
                and start.get("mode") == terminal.get("mode") == mode
                and terminal.get("outcome") == "success"
                and isinstance(terminal.get("duration_ms"), (int, float))
                and terminal.get("duration_ms") >= 0
                and response.get("route") == "/v1/chat/completions"
                and response.get("method") == "POST"
                and response.get("status") == 200
            )
        return valid, {
            "request_id": request_id,
            "mode": mode,
            "start": starts,
            "terminal": terminals,
            "response": responses,
        }

    valid_log_ok, valid_log_detail = generation_log_evidence(valid_supplied, "json")
    replacement_log_ok, replacement_log_detail = generation_log_evidence(replacement, "json")
    stream_log_ok, stream_log_detail = generation_log_evidence(stream.get("request_id"), "stream")
    expected_observed_log_ids = {valid_supplied, replacement, stream.get("request_id")}
    expected_requested_log_ids = expected_observed_log_ids | {invalid_supplied}
    log_inventory_ok = (
        set(worker_logs.get("observed_ids", [])) == expected_observed_log_ids
        and set(worker_logs.get("requested_ids", [])) == expected_requested_log_ids
    )
    check(
        "valid client request ID reaches the CPU worker and returns unchanged",
        worker_logs.get("schema") == "inferlab.request-id-log-evidence.v0.26"
        and valid_request.get("response", {}).get("status") == 200
        and REQUEST_ID.fullmatch(valid_supplied or "") is not None
        and valid_echoed == valid_supplied
        and valid_supplied in observed_log_ids
        and valid_log_ok
        and log_inventory_ok,
        {"supplied": valid_supplied, "echoed": valid_echoed, "worker_log": valid_log_detail},
    )
    check(
        "invalid client request ID is replaced once before gateway to worker forwarding",
        invalid_request.get("response", {}).get("status") == 200
        and REQUEST_ID.fullmatch(invalid_supplied or "") is None
        and invalid_supplied != replacement
        and REQUEST_ID.fullmatch(replacement or "") is not None
        and invalid_supplied
        not in json.dumps(invalid_request.get("response", {}), sort_keys=True)
        and replacement in observed_log_ids
        and invalid_supplied not in observed_log_ids
        and not any(
            event.get("request_id") == invalid_supplied
            for event in worker_logs.get("events", [])
        )
        and replacement_log_ok,
        {"invalid": invalid_supplied, "replacement": replacement, "worker_log": replacement_log_detail},
    )
    check(
        "real CPU SSE preserves its request ID and reaches DONE",
        stream.get("status") == 200
        and stream.get("request_id") == stream.get("echoed_request_id")
        and REQUEST_ID.fullmatch(stream.get("request_id", "")) is not None
        and stream.get("done_received") is True
        and stream.get("event_count", 0) > 1
        and stream_log_ok,
        {"stream": stream, "worker_log": stream_log_detail},
    )

    retry_id = retry_request.get("request", {}).get("request_id")
    retry_echo = retry_request.get("response", {}).get("headers", {}).get("x-inferlab-request-id")
    retry_records = [
        record
        for record in retry_events.get("records", [])
        if record.get("path") == "/v1/chat/completions"
    ]
    check(
        "one request ID remains identical across a failed attempt and successful retry",
        retry_request.get("response", {}).get("status") == 200
        and retry_echo == retry_id
        and [(item.get("endpoint"), item.get("response_status")) for item in retry_records]
        == [("first", 503), ("second", 200)]
        and all(item.get("request_id") == retry_id for item in retry_records),
        {"request_id": retry_id, "echoed": retry_echo, "attempts": retry_records},
    )

    baseline_retry = checkpoints["baseline"]["gateway-retry"]["document"]
    final_retry = checkpoints["final"]["gateway-retry"]["document"]
    retry_deltas = {
        "requests": sample_value(final_retry, "inferlab_gateway_requests_total")
        - sample_value(baseline_retry, "inferlab_gateway_requests_total"),
        "attempts": sample_value(final_retry, "inferlab_gateway_attempts_total")
        - sample_value(baseline_retry, "inferlab_gateway_attempts_total"),
        "transient_failures": sample_value(final_retry, "inferlab_gateway_transient_failures_total")
        - sample_value(baseline_retry, "inferlab_gateway_transient_failures_total"),
        "retries_granted": sample_value(final_retry, "inferlab_gateway_retries_total", decision="granted")
        - sample_value(baseline_retry, "inferlab_gateway_retries_total", decision="granted"),
        "completion_success_histogram": histogram_count(
            final_retry,
            "inferlab_gateway_completion_duration_seconds",
            outcome="success",
        )
        - histogram_count(
            baseline_retry,
            "inferlab_gateway_completion_duration_seconds",
            outcome="success",
        ),
    }
    check(
        "retry failure produces exact gateway counter and histogram deltas",
        retry_deltas
        == {
            "requests": 1.0,
            "attempts": 2.0,
            "transient_failures": 1.0,
            "retries_granted": 1.0,
            "completion_success_histogram": 1.0,
        },
        retry_deltas,
    )

    all_raw = "\n".join(entry["raw"] for targets in checkpoints.values() for entry in targets.values())
    metric_canaries = [
        value
        for value in [
            valid_supplied,
            invalid_supplied,
            replacement,
            stream.get("request_id"),
            retry_id,
            *unique.get("prompts", []),
            *unique.get("request_ids", []),
            "cpu-observability-canary",
            "gateway-observability-canary",
        ]
        if isinstance(value, str) and value
    ]
    leaked_canaries = sorted({value for value in metric_canaries if value in all_raw})
    check(
        "request IDs prompts and runtime worker identities are absent from metrics",
        not leaked_canaries,
        leaked_canaries,
    )

    final_status_targets = statuses.get("targets", {})
    final_documents = checkpoints["final"]
    gateway_status = final_status_targets.get("gateway-primary", {}).get("body", {})
    gateway_metrics = final_documents["gateway-primary"]["document"]
    admission = gateway_status.get("admission", {})
    resilience = gateway_status.get("resilience", {})
    workers = gateway_status.get("workers", [])
    circuit_counts = {"closed": 0, "open": 0, "half_open": 0}
    for worker in workers:
        state = worker.get("circuit", {}).get("state")
        normalized = str(state).replace("-", "_")
        if normalized in circuit_counts:
            circuit_counts[normalized] += 1
    check(
        "gateway metrics equal its bounded JSON diagnostics",
        sample_value(gateway_metrics, "inferlab_gateway_admission_current", state="outstanding") == admission.get("outstanding")
        and sample_value(gateway_metrics, "inferlab_gateway_admission_current", state="executing") == admission.get("executing")
        and sample_value(gateway_metrics, "inferlab_gateway_admission_current", state="queued") == admission.get("queued")
        and sample_value(gateway_metrics, "inferlab_gateway_admission_rejections_total") == admission.get("rejected_total")
        and sample_value(gateway_metrics, "inferlab_gateway_requests_total") == resilience.get("original_requests")
        and sample_value(gateway_metrics, "inferlab_gateway_attempts_total") == resilience.get("attempts")
        and sample_value(gateway_metrics, "inferlab_gateway_transient_failures_total") == resilience.get("transient_failures")
        and sample_value(gateway_metrics, "inferlab_gateway_retries_total", decision="granted") == resilience.get("retries_granted")
        and sample_value(gateway_metrics, "inferlab_gateway_retries_total", decision="budget_denied") == resilience.get("retries_denied_budget")
        and sample_value(gateway_metrics, "inferlab_gateway_retries_total", decision="limit_exhausted") == resilience.get("retry_limit_exhausted")
        and sample_value(gateway_metrics, "inferlab_gateway_deadlines_exceeded_total") == resilience.get("deadline_exceeded")
        and sample_value(gateway_metrics, "inferlab_gateway_workers") == len(workers)
        and sample_value(gateway_metrics, "inferlab_gateway_worker_requests_in_flight")
        == sum(worker.get("in_flight", 0) for worker in workers)
        and all(
            sample_value(gateway_metrics, "inferlab_gateway_worker_circuits", state=state)
            == count
            for state, count in circuit_counts.items()
        )
        and sample_value(gateway_metrics, "inferlab_gateway_routing_lease_ready")
        == int(bool(gateway_status.get("routing_lease", {}).get("accepting_new_requests")))
        and sample_value(gateway_metrics, "inferlab_gateway_control_revision")
        == gateway_status.get("routing_snapshot", {}).get("control_revision")
        == 2,
        {"admission": admission, "resilience": resilience, "worker_count": len(workers), "circuits": circuit_counts},
    )

    retry_status = final_status_targets.get("gateway-retry", {}).get("body", {})
    retry_status_resilience = retry_status.get("resilience", {})
    check(
        "retry gateway counters equal its exact JSON diagnostics",
        sample_value(final_retry, "inferlab_gateway_requests_total")
        == retry_status_resilience.get("original_requests")
        and sample_value(final_retry, "inferlab_gateway_attempts_total")
        == retry_status_resilience.get("attempts")
        and sample_value(final_retry, "inferlab_gateway_transient_failures_total")
        == retry_status_resilience.get("transient_failures")
        and sample_value(final_retry, "inferlab_gateway_retries_total", decision="granted")
        == retry_status_resilience.get("retries_granted")
        and sample_value(final_retry, "inferlab_gateway_control_revision") == 0
        and sample_value(final_retry, "inferlab_gateway_routing_lease_ready") == 1,
        retry_status_resilience,
    )

    worker_status = final_status_targets.get("cpu-worker", {}).get("body", {})
    scheduler = worker_status.get("scheduler", {})
    worker_metrics = final_documents["cpu-worker"]["document"]
    check(
        "worker metrics equal scheduler JSON without scraping native cache state",
        sample_value(worker_metrics, "inferlab_worker_requests_total") == worker_status.get("requests")
        and sample_value(worker_metrics, "inferlab_worker_scheduler_current", state="queued") == scheduler.get("queued")
        and sample_value(worker_metrics, "inferlab_worker_scheduler_current", state="active") == scheduler.get("active")
        and sample_value(worker_metrics, "inferlab_worker_scheduler_requests_total", outcome="admitted") == scheduler.get("admitted")
        and sample_value(worker_metrics, "inferlab_worker_scheduler_requests_total", outcome="completed") == scheduler.get("completed")
        and sample_value(worker_metrics, "inferlab_worker_scheduler_requests_total", outcome="cancelled") == scheduler.get("cancelled")
        and sample_value(worker_metrics, "inferlab_worker_scheduler_requests_total", outcome="failed") == scheduler.get("failed")
        and sample_value(worker_metrics, "inferlab_worker_scheduler_batches_total") == scheduler.get("batches")
        and sample_value(worker_metrics, "inferlab_worker_tokens_total") == scheduler.get("token_steps")
        and sample_value(worker_metrics, "inferlab_worker_batch_slots_total", state="used") == scheduler.get("slots_used")
        and sample_value(worker_metrics, "inferlab_worker_batch_slots_total", state="available") == scheduler.get("slots_available")
        and not any(name.startswith("inferlab_worker_kv_") for name in worker_metrics["types"]),
        {"requests": worker_status.get("requests"), "scheduler": scheduler},
    )

    queue_status = final_status_targets.get("batch-queue", {}).get("body", {})
    queue_metrics = final_documents["batch-queue"]["document"]
    check(
        "batch scenario ends with one completed and one dead-letter job",
        batch_scenario.get("schema") == "inferlab.observability-batch-scenario.v0.26"
        and batch_scenario.get("all_expected_statuses") is True
        and queue_status.get("jobs_total") == 2
        and queue_status.get("completed") == 1
        and queue_status.get("dead_letter") == 1
        and queue_status.get("pending") == 0
        and queue_status.get("claimed") == 0,
        {"scenario": batch_scenario, "status": queue_status},
    )
    check(
        "queue gauges and exact failure counters equal durable status",
        all(
            sample_value(queue_metrics, "inferlab_queue_jobs", state=state) == queue_status.get(field)
            for state, field in [
                ("pending", "pending"),
                ("claimed", "claimed"),
                ("completed", "completed"),
                ("dead_letter", "dead_letter"),
            ]
        )
        and sample_value(queue_metrics, "inferlab_queue_wal_bytes") == queue_status.get("wal_bytes")
        and sample_value(queue_metrics, "inferlab_queue_wal_events_total") == queue_status.get("wal_events") == 6
        and sample_value(queue_metrics, "inferlab_queue_claims_total") == queue_status.get("claims_total") == 2
        and sample_value(queue_metrics, "inferlab_queue_acknowledgments_total") == queue_status.get("acknowledgments_total") == 1
        and sample_value(queue_metrics, "inferlab_queue_failures_total", kind="explicit") == queue_status.get("explicit_failures_total") == 1
        and sample_value(queue_metrics, "inferlab_queue_failures_total", kind="dead_lettered") == queue_status.get("dead_lettered_total") == 1,
        queue_status,
    )

    trust_status = final_status_targets.get("trust-distributor", {}).get("body", {})
    trust_metrics = final_documents["trust-distributor"]["document"]
    trust_expected = {
        "snapshot_unavailable": 1,
        "snapshot_published": 1,
        "snapshot_unchanged": 1,
        "snapshot_rejected": 1,
        "snapshot_served": 1,
        "snapshot_not_modified": 1,
        "receipt_rejected": 1,
    }
    check(
        "trust scenario exercises every retained success and rejection outcome once",
        trust_scenario.get("schema") == "inferlab.observability-trust-scenario.v0.26"
        and trust_scenario.get("outcomes") == trust_expected
        and trust_scenario.get("error_codes")
        == {
            "unavailable": "snapshot_unavailable",
            "snapshot_rejected": "invalid_snapshot",
            "receipt_rejected": "invalid_json",
        },
        trust_scenario,
    )
    check(
        "trust metrics equal exact scenario deltas and distributor status",
        sample_value(trust_metrics, "inferlab_trust_snapshot_requests_total", outcome="unavailable") == 1
        and sample_value(trust_metrics, "inferlab_trust_snapshot_requests_total", outcome="served") == 1
        and sample_value(trust_metrics, "inferlab_trust_snapshot_requests_total", outcome="not_modified") == 1
        and sample_value(trust_metrics, "inferlab_trust_snapshot_publish_total", outcome="published") == 1
        and sample_value(trust_metrics, "inferlab_trust_snapshot_publish_total", outcome="unchanged") == 1
        and sample_value(trust_metrics, "inferlab_trust_snapshot_publish_total", outcome="rejected") == 1
        and sample_value(trust_metrics, "inferlab_trust_receipts_total", outcome="rejected") == 1
        and sample_value(trust_metrics, "inferlab_trust_snapshot_generation") == trust_status.get("snapshot", {}).get("generation") == 1
        and sample_value(trust_metrics, "inferlab_trust_receivers", state="expected") == len(trust_status.get("expected_receivers", [])) == 1
        and sample_value(trust_metrics, "inferlab_trust_receivers", state="acked") == len(trust_status.get("acked_receivers", [])) == 0
        and sample_value(trust_metrics, "inferlab_trust_receivers", state="pending") == len(trust_status.get("pending_receivers", [])) == 1
        and sample_value(trust_metrics, "inferlab_trust_storage_healthy") == 1
        and trust_status.get("storage", {}).get("mutation_poisoned") is False,
        trust_status,
    )

    link_status = final_status_targets.get("raft-link-proxy", {}).get("body", {})
    link_metrics = final_documents["raft-link-proxy"]["document"]
    check(
        "link scenario performs exactly one forward drop and upstream failure",
        link_scenario.get("schema") == "inferlab.observability-link-scenario.v0.26"
        and link_scenario.get("forwarded_status") == 200
        and link_scenario.get("dropped_status") == 503
        and link_scenario.get("upstream_failure_status") in {502, 503}
        and link_scenario.get("mode_sequence") == ["allow", "drop", "allow"]
        and link_scenario.get("error_codes")
        == {"dropped": "link_dropped", "upstream_failure": "upstream_failure"},
        link_scenario,
    )
    check(
        "link metrics equal exact mode and request counters",
        sample_value(link_metrics, "inferlab_raft_link_mode", mode="allow") == 1
        and sample_value(link_metrics, "inferlab_raft_link_mode", mode="drop") == 0
        and sample_value(link_metrics, "inferlab_raft_link_mode_changes_total") == link_status.get("mode_changes") == 2
        and sample_value(link_metrics, "inferlab_raft_link_requests_total", outcome="forwarded") == link_status.get("forwarded_requests") == 1
        and sample_value(link_metrics, "inferlab_raft_link_requests_total", outcome="dropped") == link_status.get("dropped_requests") == 1
        and sample_value(link_metrics, "inferlab_raft_link_requests_total", outcome="upstream_failure") == link_status.get("upstream_failures") == 1
        and sample_value(link_metrics, "inferlab_raft_link_last_transition_timestamp_seconds")
        == link_status.get("last_transition_at_ms", 0) // 1000,
        link_status,
    )

    control_statuses = {
        name: final_status_targets.get(name, {}).get("body", {})
        for name in ["control-a", "control-b", "control-c"]
    }
    leaders = [name for name, body in control_statuses.items() if body.get("role") == "leader"]
    check(
        "three controls retain one leader and committed revision two",
        len(leaders) == 1
        and len({body.get("term") for body in control_statuses.values()}) == 1
        and all(body.get("commit_index") == 2 and body.get("last_applied") == 2 for body in control_statuses.values())
        and all((body.get("committed_configuration") or {}).get("revision") == 2 for body in control_statuses.values()),
        {name: {"role": body.get("role"), "term": body.get("term"), "commit": body.get("commit_index")} for name, body in control_statuses.items()},
    )
    control_cross_checks: dict[str, Any] = {}
    controls_valid = True
    for name, body in control_statuses.items():
        document = final_documents[name]["document"]
        role_values = {
            role: sample_value(document, "inferlab_control_role", role=role)
            for role in ["follower", "candidate", "leader"]
        }
        valid = (
            role_values.get(body.get("role")) == 1
            and sum(role_values.values()) == 1
            and sample_value(document, "inferlab_control_term") == body.get("term")
            and sample_value(document, "inferlab_control_commit_index") == body.get("commit_index")
            and sample_value(document, "inferlab_control_last_applied") == body.get("last_applied")
            and sample_value(document, "inferlab_control_last_log_index") == body.get("last_log_index")
            and sample_value(document, "inferlab_control_storage_healthy") == int(bool(body.get("storage_healthy")))
        )
        controls_valid = controls_valid and valid
        control_cross_checks[name] = {"role_metrics": role_values, "status_role": body.get("role"), "valid": valid}
    check("control gauges equal live Raft status on all three nodes", controls_valid, control_cross_checks)

    all_in_flight_zero = all(
        sample_value(entry["document"], "inferlab_http_requests_in_flight", service=entry["observation"]["service"])
        == 0
        for entry in final_documents.values()
    )
    check("all final HTTP in-flight gauges drain to zero", all_in_flight_zero, {target: sample_value(entry["document"], "inferlab_http_requests_in_flight", service=entry["observation"]["service"]) for target, entry in final_documents.items()})

    participants = continuity.get("processes", {})
    proof_shell_pid = continuity.get("proof_shell_pid")
    expected_commands = {
        "gateway-primary": "gateway",
        "gateway-retry": "gateway",
        "cpu-worker": "cpu-worker",
        "batch-queue": "batch-queue",
        "control-a": "control-plane",
        "control-b": "control-plane",
        "control-c": "control-plane",
        "trust-distributor": "trust-distributor",
        "raft-link-proxy": "raft-link-proxy",
    }
    check(
        "nine service targets keep exact proof-owned process identities",
        continuity.get("schema") == "inferlab.observability-process-continuity.v0.26"
        and set(participants) == set(EXPECTED_TARGETS)
        and isinstance(proof_shell_pid, int)
        and all(
            value.get("initial_pid") == value.get("current_pid")
            and value.get("initial_start_token") == value.get("current_start_token")
            and bool(value.get("initial_start_token"))
            and value.get("initial_command") == value.get("current_command")
            and value.get("same_command") is True
            and bool(value.get("initial_command"))
            and value.get("parent_pid") == proof_shell_pid
            and value.get("owned_child") is True
            and value.get("alive") is True
            and value.get("non_zombie") is True
            and "Z" not in value.get("process_state", "")
            and Path(str(value.get("initial_command", "")).split()[0]).name
            == expected_commands[name]
            and Path(str(value.get("current_command", "")).split()[0]).name
            == expected_commands[name]
            for name, value in participants.items()
        ),
        participants,
    )
    check(
        "retained evidence is sanitized",
        sanitizer.get("schema") == "inferlab.evidence-sanitizer.v0.26"
        and sanitizer.get("private_material_markers") == 0
        and sanitizer.get("remaining_host_paths") == 0,
        sanitizer,
    )
    check(
        "known proof private seeds are absent",
        private_scan.get("schema") == "inferlab.private-material-scan.v0.26"
        and private_scan.get("matches") == 0
        and private_scan.get("known_seed_count", 0) >= 3,
        private_scan,
    )

    delta_report = {
        "schema": "inferlab.openmetrics-delta-report.v0.26",
        "unique_prompt_requests": unique_count,
        "gateway_unique_request_delta": gateway_unique_delta,
        "worker_unique_request_delta": worker_unique_delta,
        "retry_gateway": retry_deltas,
        "batch": {
            "claims": sample_value(queue_metrics, "inferlab_queue_claims_total"),
            "acknowledgments": sample_value(queue_metrics, "inferlab_queue_acknowledgments_total"),
            "explicit_failures": sample_value(queue_metrics, "inferlab_queue_failures_total", kind="explicit"),
            "dead_lettered": sample_value(queue_metrics, "inferlab_queue_failures_total", kind="dead_lettered"),
        },
        "trust_outcomes": trust_expected,
        "link": {
            outcome: sample_value(link_metrics, "inferlab_raft_link_requests_total", outcome=outcome)
            for outcome in ["forwarded", "dropped", "upstream_failure"]
        },
    }

    passed = sum(item["passed"] for item in assertions)
    result = {
        "schema": "inferlab.observability-assertions.v0.26",
        "passed": passed,
        "total": len(assertions),
        "all_passed": passed == len(assertions),
        "assertions": assertions,
    }
    args.contract_output.write_text(json.dumps(contract_json(), indent=2, sort_keys=True) + "\n", encoding="utf-8")
    args.cardinality_output.write_text(json.dumps(cardinality, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    args.histogram_output.write_text(json.dumps(histogram_audit, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    args.delta_output.write_text(json.dumps(delta_report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    if not result["all_passed"]:
        failed = [item["name"] for item in assertions if not item["passed"]]
        raise SystemExit("failed v0.26 assertions: " + "; ".join(failed))
    print(f"v0.26 proof: {passed}/{len(assertions)} assertions passed")


if __name__ == "__main__":
    main()
