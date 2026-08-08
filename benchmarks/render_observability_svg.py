#!/usr/bin/env python3
"""Render checked InferLab v0.26 observability evidence as a deterministic SVG."""

from __future__ import annotations

import argparse
import html
import json
from pathlib import Path
from typing import Any


REQUIRED_ASSERTIONS = {
    "inventory contains the exact nine metrics targets and six service classes",
    "every target exposes exactly its documented family and type catalog",
    "all metric labels use exact closed allowlists and route method pairs",
    "every target stays at or below 256 series",
    "every scrape set stays at or below 2500 topology series",
    "closed catalogs bound every target at 256 and this topology at 2500 series",
    "all histograms use the exact buckets and satisfy cumulative algebra",
    "unique prompts create no gateway or worker time series",
    "valid client request ID reaches the CPU worker and returns unchanged",
    "invalid client request ID is replaced once before gateway to worker forwarding",
    "one request ID remains identical across a failed attempt and successful retry",
    "request IDs prompts and runtime worker identities are absent from metrics",
    "nine service targets keep exact proof-owned process identities",
    "real CPU SSE preserves its request ID and reaches DONE",
}


def load(directory: Path, name: str) -> dict[str, Any]:
    with (directory / name).open(encoding="utf-8") as source:
        value = json.load(source)
    if not isinstance(value, dict):
        raise SystemExit(f"{name} is not a JSON object")
    return value


def escape(value: Any) -> str:
    return html.escape(str(value), quote=True)


def text(x: int, y: int, value: Any, css: str = "", anchor: str = "start") -> str:
    return (
        f'<text x="{x}" y="{y}" class="{escape(css)}" '
        f'text-anchor="{escape(anchor)}">{escape(value)}</text>'
    )


def card(
    x: int,
    y: int,
    width: int,
    title: str,
    value: str,
    detail: str,
    css: str,
) -> str:
    return "".join(
        [
            f'<rect x="{x}" y="{y}" width="{width}" height="116" rx="14" class="{css}"/>',
            text(x + 18, y + 29, title, "card-title"),
            text(x + 18, y + 65, value, "card-value"),
            text(x + 18, y + 91, detail, "card-detail"),
        ]
    )


def require_number(value: Any, label: str) -> float:
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        raise SystemExit(f"{label} is not numeric")
    return float(value)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    directory = args.evidence_dir

    assertions = load(directory, "assertions.json")
    cardinality = load(directory, "cardinality.json")
    histograms = load(directory, "histograms.json")
    deltas = load(directory, "deltas.json")
    inventory = load(directory, "target-inventory.json")
    valid = load(directory, "request-id-valid.json")
    invalid = load(directory, "request-id-invalid.json")
    stream = load(directory, "stream.json")
    continuity = load(directory, "process-continuity.json")

    assertion_rows = assertions.get("assertions")
    if not isinstance(assertion_rows, list) or not assertion_rows:
        raise SystemExit("refusing to render empty assertion evidence")
    assertion_names = {row.get("name") for row in assertion_rows}
    if (
        assertions.get("schema") != "inferlab.observability-assertions.v0.26"
        or assertions.get("all_passed") is not True
        or assertions.get("passed") != assertions.get("total")
        or assertions.get("total") != len(assertion_rows)
        or not all(row.get("passed") is True for row in assertion_rows)
        or not REQUIRED_ASSERTIONS.issubset(assertion_names)
    ):
        raise SystemExit("refusing to render unchecked, incomplete, or failed evidence")

    target_rows = inventory.get("targets")
    service_values = {row.get("service") for row in target_rows or []}
    if (
        inventory.get("schema") != "inferlab.observability-target-inventory.v0.26"
        or not isinstance(target_rows, list)
        or len(target_rows) != 9
        or service_values
        != {
            "gateway",
            "cpu-worker",
            "batch-queue",
            "control-plane",
            "trust-distributor",
            "raft-link-proxy",
        }
    ):
        raise SystemExit("target inventory is inconsistent")

    observations = cardinality.get("observations")
    if (
        cardinality.get("schema") != "inferlab.openmetrics-cardinality-audit.v0.26"
        or cardinality.get("all_within_caps") is not True
        or not isinstance(observations, list)
        or len(observations) != 40
        or not all(row.get("within_cap") is True for row in observations)
    ):
        raise SystemExit("cardinality evidence is inconsistent")
    per_target = [row for row in observations if row.get("target") != "<topology>"]
    topology = [row for row in observations if row.get("target") == "<topology>"]
    peak_target = max(int(row["series"]) for row in per_target)
    peak_topology = max(int(row["series"]) for row in topology)
    per_target_cap = int(cardinality.get("per_target_cap", 0))
    topology_cap = int(cardinality.get("topology_cap", 0))
    theoretical_targets = cardinality.get("theoretical_targets")
    theoretical_topology = cardinality.get("theoretical_topology")
    if (
        per_target_cap != 256
        or topology_cap != 2500
        or cardinality.get("theoretical_all_within_caps") is not True
        or not isinstance(theoretical_targets, dict)
        or max(theoretical_targets.values(), default=0) != 255
        or theoretical_topology != 1721
    ):
        raise SystemExit("cardinality caps are inconsistent")

    histogram_count = histograms.get("histograms_checked")
    if (
        histograms.get("schema") != "inferlab.openmetrics-histogram-audit.v0.26"
        or histograms.get("all_valid") is not True
        or not isinstance(histogram_count, int)
        or histogram_count <= 0
        or histograms.get("errors") != []
    ):
        raise SystemExit("histogram audit is inconsistent")

    unique_count = deltas.get("unique_prompt_requests")
    retry = deltas.get("retry_gateway", {})
    if (
        deltas.get("schema") != "inferlab.openmetrics-delta-report.v0.26"
        or not isinstance(unique_count, int)
        or unique_count < 20
        or require_number(deltas.get("gateway_unique_request_delta"), "gateway unique delta")
        != unique_count
        or require_number(deltas.get("worker_unique_request_delta"), "worker unique delta")
        != unique_count
        or retry
        != {
            "requests": 1.0,
            "attempts": 2.0,
            "transient_failures": 1.0,
            "retries_granted": 1.0,
            "completion_success_histogram": 1.0,
        }
    ):
        raise SystemExit("delta report is inconsistent")

    valid_id = valid.get("request", {}).get("request_id")
    valid_echo = valid.get("response", {}).get("headers", {}).get("x-inferlab-request-id")
    invalid_id = invalid.get("request", {}).get("request_id")
    replacement_id = invalid.get("response", {}).get("headers", {}).get("x-inferlab-request-id")
    if not (
        valid.get("response", {}).get("status") == 200
        and valid_id == valid_echo
        and isinstance(valid_id, str)
        and invalid.get("response", {}).get("status") == 200
        and isinstance(invalid_id, str)
        and isinstance(replacement_id, str)
        and invalid_id != replacement_id
    ):
        raise SystemExit("request-ID evidence is inconsistent")
    if not (
        stream.get("status") == 200
        and stream.get("done_received") is True
        and stream.get("request_id") == stream.get("echoed_request_id")
    ):
        raise SystemExit("stream evidence is inconsistent")

    processes = continuity.get("processes")
    if (
        continuity.get("schema") != "inferlab.observability-process-continuity.v0.26"
        or not isinstance(processes, dict)
        or len(processes) != 9
        or not all(
            row.get("initial_pid") == row.get("current_pid")
            and row.get("initial_start_token") == row.get("current_start_token")
            and bool(row.get("initial_start_token"))
            and row.get("initial_command") == row.get("current_command")
            and row.get("same_command") is True
            and bool(row.get("initial_command"))
            and row.get("owned_child") is True
            and row.get("alive") is True
            and row.get("non_zombie") is True
            for row in processes.values()
        )
    ):
        raise SystemExit("process-continuity evidence is inconsistent")

    passed = int(assertions["passed"])
    total = int(assertions["total"])
    sse_ms = require_number(stream.get("duration_ms"), "SSE duration")
    valid_ms = require_number(valid.get("response", {}).get("duration_ms"), "JSON duration")
    desc = (
        f"InferLab v0.26 checked {len(target_rows)} metrics targets across "
        f"{len(service_values)} service classes. Peak series were {peak_target} per target and "
        f"{peak_topology} across the topology, below caps {per_target_cap} and {topology_cap}. "
        f"Closed catalogs bound the same topology to {theoretical_topology} theoretical series. "
        f"All {histogram_count} histogram label sets satisfy the fixed-bucket algebra; "
        f"{unique_count} unique prompts created no new gateway or worker series. "
        f"A request ID survived gateway to CPU worker and a retry, invalid input was replaced, "
        f"and real JSON plus SSE ending in DONE succeeded. All {passed} checker assertions passed."
    )

    parts = [
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 940" role="img" aria-labelledby="title desc">',
        '<title id="title">InferLab v0.26 bounded-cardinality Prometheus observability proof</title>',
        f'<desc id="desc">{escape(desc)}</desc>',
        """<style>
          .bg{fill:#f8fafc}.target{fill:#eff6ff;stroke:#2563eb;stroke-width:1.5}.bounded{fill:#ecfdf5;stroke:#059669;stroke-width:1.5}.identity{fill:#f5f3ff;stroke:#7c3aed;stroke-width:1.5}.delta{fill:#fff7ed;stroke:#ea580c;stroke-width:1.5}.proof{fill:#ecfdf5;stroke:#059669;stroke-width:1.5}.line{stroke:#64748b;stroke-width:2;fill:none}.arrow{fill:#64748b}text{font-family:ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;fill:#0f172a}.title{font-size:28px;font-weight:750}.subtitle{font-size:15px;fill:#475569}.section{font-size:18px;font-weight:700}.card-title{font-size:14px;font-weight:700}.card-value{font-size:21px;font-weight:750}.card-detail{font-size:12px;fill:#475569}.edge{font-size:12px;fill:#64748b}.proof-value{font-size:24px;font-weight:750;fill:#047857}.proof-detail{font-size:13px;fill:#065f46}.foot{font-size:12px;fill:#64748b}
        </style>""",
        '<rect width="1200" height="940" class="bg"/>',
        text(50, 54, "v0.26 · Bounded-cardinality Prometheus observability", "title"),
        text(50, 84, "Separate loopback metrics listeners · raw OpenMetrics evidence · no request-derived labels", "subtitle"),
        text(50, 128, "Exact-process evidence topology", "section"),
        card(50, 158, 250, "service classes", str(len(service_values)), f"{len(target_rows)} scrape targets", "target"),
        card(350, 158, 250, "target bound", f"255 / {per_target_cap}", f"observed peak {peak_target}", "bounded"),
        card(650, 158, 250, "topology bound", f"{theoretical_topology} / {topology_cap}", f"observed peak {peak_topology}", "bounded"),
        card(950, 158, 200, "histograms", str(histogram_count), "label sets valid", "identity"),
        text(50, 324, "Cardinality and correlation invariants", "section"),
        card(50, 354, 250, "unique prompts", str(unique_count), "zero new gateway/worker series", "bounded"),
        card(350, 354, 250, "valid request ID", "preserved", "client → gateway → CPU worker", "identity"),
        card(
            650,
            354,
            250,
            "invalid request ID",
            "replaced once",
            "not forwarded as request_id",
            "identity",
        ),
        card(950, 354, 200, "retry path", "1 → 2", "one request, two attempts", "delta"),
        '<line x1="570" y1="528" x2="880" y2="528" class="line"/>',
        '<polygon points="880,528 868,521 868,535" class="arrow"/>',
        text(50, 536, "client", "section"),
        text(240, 536, "gateway", "section"),
        text(900, 536, "CPU worker", "section"),
        text(560, 512, "same bounded request ID; never a metric label", "edge", "middle"),
        '<line x1="105" y1="528" x2="215" y2="528" class="line"/>',
        '<polygon points="215,528 203,521 203,535" class="arrow"/>',
        '<line x1="300" y1="528" x2="570" y2="528" class="line"/>',
        '<rect x="50" y="610" width="1100" height="172" rx="16" class="proof"/>',
        text(600, 659, f"{passed} / {total} checks passed", "proof-value", "middle"),
        text(600, 696, f"{len(processes)} exact owned service PIDs · {histogram_count} histogram label sets · all final in-flight gauges drained", "proof-detail", "middle"),
        text(600, 729, f"real CPU JSON {valid_ms:.3f} ms · SSE {sse_ms:.3f} ms + [DONE]", "proof-detail", "middle"),
        text(600, 758, "exact deltas: retry requests 1 · attempts 2 · transient failures 1 · granted retries 1", "proof-detail", "middle"),
        text(50, 842, "Scope: controlled single-host proof and local Prometheus scrape demo; raw counters/gauges/histograms are cross-checked against service status.", "foot"),
        text(50, 869, "No Grafana, OpenTelemetry, cloud backend, remote write, long-term retention, alert/SLO claim, or globally secured metrics transport.", "foot"),
        text(50, 896, "Native paged-cache metrics are deferred: scrape-time allocator locking/page scans would put observability on the worker hot path.", "foot"),
        text(50, 923, "Request IDs are bounded correlation fields in headers and JSON logs; prompts, IDs, worker identities, URLs, and error text are not labels.", "foot"),
        "</svg>",
    ]
    args.output.write_text("".join(parts), encoding="utf-8")


if __name__ == "__main__":
    main()
