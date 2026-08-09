#!/usr/bin/env python3
"""Render a deterministic, data-driven v0.28 public-edge proof chart."""

from __future__ import annotations

import argparse
import html
import json
from pathlib import Path
from typing import Any


WIDTH = 1280
HEIGHT = 820


def load(directory: Path, name: str) -> dict[str, Any]:
    with (directory / name).open(encoding="utf-8") as source:
        value = json.load(source)
    if not isinstance(value, dict):
        raise SystemExit(f"{name} must contain one object")
    return value


def escape(value: Any) -> str:
    return html.escape(str(value), quote=True)


def text(
    parts: list[str],
    x: int,
    y: int,
    value: str,
    *,
    size: int = 15,
    fill: str = "#d8e7f2",
    weight: int = 400,
    anchor: str = "start",
) -> None:
    parts.append(
        f'<text x="{x}" y="{y}" font-size="{size}" fill="{fill}" '
        f'font-weight="{weight}" text-anchor="{anchor}">{escape(value)}</text>'
    )


def box(
    parts: list[str],
    x: int,
    y: int,
    width: int,
    height: int,
    *,
    fill: str = "#102738",
    stroke: str = "#2f5268",
    radius: int = 14,
) -> None:
    parts.append(
        f'<rect x="{x}" y="{y}" width="{width}" height="{height}" rx="{radius}" '
        f'fill="{fill}" stroke="{stroke}" stroke-width="1.5"/>'
    )


def card(parts: list[str], x: int, y: int, width: int, value: str, label: str) -> None:
    box(parts, x, y, width, 84)
    text(parts, x + 17, y + 34, value, size=24, fill="#76e0bd", weight=700)
    text(parts, x + 17, y + 61, label, size=13, fill="#9fb6c6")


def require_assertions(report: dict[str, Any], names: set[str]) -> None:
    if report.get("schema") != "inferlab.public-edge-assertions.v0.28":
        raise SystemExit("renderer requires the v0.28 assertion schema")
    assertions = report.get("assertions")
    if not isinstance(assertions, list):
        raise SystemExit("renderer requires an assertion list")
    passed = {
        item.get("name")
        for item in assertions
        if isinstance(item, dict) and item.get("passed") is True
    }
    if (
        report.get("all_passed") is not True
        or report.get("passed") != report.get("total")
        or report.get("total") != len(assertions)
        or not names.issubset(passed)
    ):
        raise SystemExit(f"renderer refuses incomplete proof: {sorted(names - passed)}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    report = load(args.evidence_dir, "assertions.json")
    routes = load(args.evidence_dir, "route-isolation.json")
    rate = load(args.evidence_dir, "rate-limit.json")
    status = load(args.evidence_dir, "operator-status-final.json")
    json_completion = load(args.evidence_dir, "json-completion.json")
    sse = load(args.evidence_dir, "sse-completion.json")
    disconnect = load(args.evidence_dir, "sse-disconnect.json")
    during = load(args.evidence_dir, "sse-disconnect-during-status.json")
    after = load(args.evidence_dir, "sse-disconnect-after-status.json")
    processes = load(args.evidence_dir, "process-continuity.json")
    production = load(args.evidence_dir, "production-tests.json")

    required = {
        "the hosted public router returns exact route absence under missing authentication and both credential classes",
        "the operator listener accepts only the operator credential",
        "all finite authentication body and input rejections produce zero compute-boundary delta",
        "credential A spends exactly the configured two-request burst before a finite 429",
        "the second public credential has an isolated bucket",
        "a real CPU JSON completion crosses the hosted edge",
        "a real CPU SSE is observed incrementally and reaches DONE",
        "disconnect drops SSE ownership back to idle without restarting the gateway",
        "the hosted scalar rejection metric equals the fixed status-counter sum without labels",
        "every rate and admission rejection remains before compute while nine accepted requests reach the real worker",
    }
    require_assertions(report, required)

    internal = routes.get("public_internal")
    if not isinstance(internal, dict) or {
        key: value.get("status") for key, value in internal.items()
    } != {"missing": 404, "operator": 404, "public": 404}:
        raise SystemExit("renderer requires the exact three-way public 404 matrix")
    edge = status.get("body", {}).get("public_edge", {})
    rejections = edge.get("rejections")
    if not isinstance(rejections, dict) or sum(rejections.values()) != 18:
        raise SystemExit("renderer requires the exact finite rejection total")
    rate_cases = rate.get("cases", {})
    if rate.get("rate_burst") != 2 or rate_cases.get("a_limited", {}).get("status") != 429:
        raise SystemExit("renderer requires exact burst exhaustion")
    retry_after = rate_cases["a_limited"].get("headers", {}).get("retry-after")
    if retry_after != "1":
        raise SystemExit("renderer requires the exact rate Retry-After")
    if sse.get("done") is not True or sse.get("content_read_span_ms", 0) < 300:
        raise SystemExit("renderer requires incrementally observed terminal SSE")
    during_admission = during.get("body", {}).get("admission", {})
    after_admission = after.get("body", {}).get("admission", {})
    if (
        during_admission.get("outstanding") != 1
        or during_admission.get("executing") != 1
        or after_admission.get("outstanding") != 0
        or after_admission.get("executing") != 0
    ):
        raise SystemExit("renderer requires live and released SSE ownership")
    if len(processes.get("processes", {})) != 2 or production.get("test_count") != 5:
        raise SystemExit("renderer requires exact process and production-test evidence")

    assertion_total = report["total"]
    json_ms = json_completion.get("duration_ms")
    sse_ms = sse.get("duration_ms")
    span_ms = sse.get("content_read_span_ms")
    refill_ms = rate.get("refill_wait_ms")
    if not all(isinstance(value, (int, float)) for value in [json_ms, sse_ms, span_ms, refill_ms]):
        raise SystemExit("renderer requires finite timing observations")

    description = (
        f"InferLab v0.28 public-edge proof: {assertion_total} assertions pass; "
        "missing authentication plus both credential classes see the same absent internal route on the public listener; "
        "eighteen finite hosted-gate rejections produce zero hidden compute attempts; "
        "two independent request buckets, real CPU JSON and incremental terminal SSE, "
        "and disconnect permit cleanup are proven with two stable processes."
    )
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}" role="img" aria-labelledby="title desc">',
        '<title id="title">InferLab v0.28 public edge isolation proof</title>',
        f'<desc id="desc">{escape(description)}</desc>',
        '<rect width="1280" height="820" fill="#071520"/>',
        '<defs><linearGradient id="header" x1="0" x2="1"><stop stop-color="#12364a"/><stop offset="1" stop-color="#102131"/></linearGradient></defs>',
        '<rect x="34" y="28" width="1212" height="106" rx="18" fill="url(#header)" stroke="#35647b"/>',
    ]
    text(parts, 62, 68, "INFERLAB · RELEASE v0.28", size=15, fill="#76e0bd", weight=700)
    text(parts, 62, 104, "A smaller public route surface with bounded work before compute", size=27, fill="#f1f7fb", weight=700)
    text(parts, 1216, 70, f"{report['passed']} / {report['total']}", size=24, fill="#76e0bd", weight=700, anchor="end")
    text(parts, 1216, 100, "deterministic assertions", size=13, fill="#9fb6c6", anchor="end")

    card(parts, 34, 154, 184, "2 listeners", "public · operator")
    card(parts, 232, 154, 184, "3 × 404", "missing · public · op")
    card(parts, 430, 154, 184, "burst 2", f"429 · retry {retry_after}s")
    card(parts, 628, 154, 184, "18 → 0", "rejects · hidden attempts")
    card(parts, 826, 154, 184, "9 = 9", "gateway · CPU")
    card(parts, 1024, 154, 222, "256 max", "hosted series ceiling")

    box(parts, 34, 260, 1212, 204, fill="#0c202f")
    text(parts, 58, 293, "EXACT HOSTED COMPLETION GATE", size=15, fill="#76e0bd", weight=700)
    nodes = [
        ("AUTH", "exact Bearer", "401"),
        ("BODY", "≤ 65,536 B", "413"),
        ("INPUT", "finite bounds", "400/413"),
        ("BUCKET", "per credential", "429"),
        ("ADMIT", "shared permits", "429"),
        ("CPU", "attempt begins", "200"),
    ]
    start_x = 60
    width = 166
    gap = 30
    for index, (name, detail, failure) in enumerate(nodes):
        x = start_x + index * (width + gap)
        box(parts, x, 320, width, 88, fill="#112b3d", stroke="#315c72", radius=10)
        text(parts, x + 14, 346, name, size=13, fill="#76e0bd", weight=700)
        text(parts, x + 14, 371, detail, size=14, fill="#edf5fa")
        text(parts, x + 14, 393, f"reject {failure}" if index < 5 else "real worker", size=12, fill="#9fb6c6")
        if index < len(nodes) - 1:
            parts.append(
                f'<path d="M {x + width + 4} 364 H {x + width + gap - 4}" stroke="#76e0bd" stroke-width="2" marker-end="url(#arrow)"/>'
            )
    parts.insert(
        5,
        '<defs><marker id="arrow" markerWidth="7" markerHeight="7" refX="6" refY="3.5" orient="auto"><path d="M0,0 L7,3.5 L0,7 z" fill="#76e0bd"/></marker></defs>',
    )
    text(parts, 58, 444, "Finite edge reasons are counted; downstream routing and worker-schema errors keep their existing contracts.", size=13, fill="#9fb6c6")

    box(parts, 34, 484, 590, 302, fill="#0c202f")
    text(parts, 58, 518, "PER-CREDENTIAL WATER TANK", size=15, fill="#76e0bd", weight=700)
    text(parts, 58, 550, "A: request 1", size=14)
    text(parts, 182, 550, "request 2", size=14)
    text(parts, 294, 550, "request 3 → 429", size=14, fill="#ffbd6b")
    parts.append('<line x1="58" y1="570" x2="580" y2="570" stroke="#34576a" stroke-width="14" stroke-linecap="round"/>')
    parts.append('<line x1="58" y1="570" x2="318" y2="570" stroke="#76e0bd" stroke-width="14" stroke-linecap="round"/>')
    text(parts, 58, 606, f"A refills after {refill_ms:.1f} ms and succeeds", size=14, fill="#edf5fa")
    text(parts, 58, 636, "B succeeds while A is empty", size=14, fill="#76e0bd")
    text(parts, 58, 678, "Admission-full consumes one token", size=14)
    text(parts, 58, 706, "one post-release success; next request is still 429", size=14, fill="#ffbd6b")
    text(parts, 58, 754, "Buckets reset on process restart · no distributed fairness claim", size=12, fill="#9fb6c6")

    box(parts, 642, 484, 604, 302, fill="#0c202f")
    text(parts, 666, 518, "REAL RESPONSE + PERMIT LIFETIME", size=15, fill="#76e0bd", weight=700)
    text(parts, 666, 552, f"JSON {json_ms:.1f} ms", size=22, fill="#edf5fa", weight=700)
    text(parts, 904, 552, f"SSE {sse_ms:.1f} ms", size=22, fill="#edf5fa", weight=700)
    text(parts, 666, 582, "one CPU attempt · completion returned", size=13, fill="#9fb6c6")
    text(parts, 904, 582, f"{span_ms:.1f} ms read span · terminal DONE", size=13, fill="#9fb6c6")
    parts.append('<line x1="682" y1="646" x2="1198" y2="646" stroke="#466b7d" stroke-width="3"/>')
    for x, label in [(702, "open"), (866, "content"), (1030, "disconnect"), (1180, "idle")]:
        parts.append(f'<circle cx="{x}" cy="646" r="7" fill="#76e0bd"/>')
        text(parts, x, 674, label, size=12, fill="#d8e7f2", anchor="middle")
    text(parts, 666, 714, "during: outstanding 1 · executing 1 · worker in-flight 1", size=14)
    text(parts, 666, 744, "after:  outstanding 0 · executing 0 · worker in-flight 0", size=14, fill="#76e0bd")
    text(parts, 1218, 774, f"{len(processes['processes'])} stable PIDs · {production['test_count']} exact tests", size=12, fill="#9fb6c6", anchor="end")

    parts.append("</svg>")
    args.output.write_text("\n".join(parts) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
