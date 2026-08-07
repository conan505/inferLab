#!/usr/bin/env python3
"""Render a data-driven v0.23 distributed service-trust evidence chart."""

from __future__ import annotations

import argparse
import json
from html import escape
from pathlib import Path
from typing import Any


def load(directory: Path, name: str) -> dict[str, Any]:
    with (directory / name).open(encoding="utf-8") as source:
        return json.load(source)


def label(x: int, y: int, value: str, css: str = "label", anchor: str = "start") -> str:
    return (
        f'<text x="{x}" y="{y}" class="{css}" '
        f'text-anchor="{anchor}">{escape(value)}</text>'
    )


def panel(x: int, title: str, line_one: str, line_two: str, css: str) -> str:
    return "".join(
        [
            f'<rect x="{x}" y="180" width="250" height="160" rx="14" class="{css}"/>',
            label(x + 125, 215, title, "panel-title", "middle"),
            label(x + 125, 260, line_one, "panel-value", "middle"),
            label(x + 125, 298, line_two, "panel-detail", "middle"),
        ]
    )


def arrow(x1: int, x2: int, text: str) -> str:
    return "".join(
        [
            f'<line x1="{x1}" y1="260" x2="{x2}" y2="260" class="arrow" marker-end="url(#arrow)"/>',
            label((x1 + x2) // 2, 246, text, "edge", "middle"),
        ]
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    initial = load(args.evidence_dir, "initial-controls.json")
    partial = load(args.evidence_dir, "generation-2-partial-receipts.json")
    converged = load(args.evidence_dir, "generation-2-receipts.json")
    g2_controls = load(args.evidence_dir, "generation-2-convergence.json")
    generation_three = load(args.evidence_dir, "generation-3-receipts.json")
    g3_controls = load(args.evidence_dir, "generation-3-convergence.json")
    rollback = load(args.evidence_dir, "rollback-publication.json")
    fork = load(args.evidence_dir, "fork-publication.json")
    tamper = load(args.evidence_dir, "tamper-publication.json")
    cache_restart = load(args.evidence_dir, "cache-restart.json")
    request = load(args.evidence_dir, "request.json")
    stream = load(args.evidence_dir, "stream.json")
    assertions = load(args.evidence_dir, "assertions.json")

    initial_max = max(
        item["convergence_latency_ms"] for item in initial["observations"]
    )
    partial_body = partial["status"]["body"]
    converged_body = converged["status"]["body"]
    g3_body = generation_three["status"]["body"]

    parts = [
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 820" role="img" aria-labelledby="title desc">',
        '<title id="title">InferLab v0.23 distributed signed service-trust proof</title>',
        '<desc id="desc">Root authority is separated from distributor transport. Three controls remotely boot generation one, generation two first has two receipts and one pending receiver, controls later reach generation two and the complete receipt set is subsequently observed, generation three revokes key A, attacks are rejected, and a follower restarts from cache during distributor outage while real inference continues.</desc>',
        """<defs>
          <marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M 0 0 L 10 5 L 0 10 z"/></marker>
        </defs>
        <style>
          .background{fill:#f8fafc}.authority{fill:#eef2ff;stroke:#6366f1;stroke-width:1.5}.partial{fill:#fffbeb;stroke:#f59e0b;stroke-width:1.5}.converged{fill:#ecfdf5;stroke:#10b981;stroke-width:1.5}.outage{fill:#f1f5f9;stroke:#64748b;stroke-width:1.5}.metric{fill:#fff;stroke:#cbd5e1;stroke-width:1.5}.proof{fill:#ecfdf5;stroke:#10b981;stroke-width:1.5}.arrow{stroke:#475569;stroke-width:2;fill:none}text{font-family:ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;fill:#0f172a}.title{font-size:28px;font-weight:700}.subtitle{font-size:15px;fill:#475569}.section{font-size:18px;font-weight:700}.panel-title{font-size:17px;font-weight:700}.panel-value{font-size:20px;font-weight:700}.panel-detail{font-size:13px;fill:#475569}.edge{font-size:11px;fill:#64748b}.metric-label{font-size:12px;fill:#64748b}.metric-value{font-size:22px;font-weight:700}.proof-title{font-size:22px;font-weight:700;fill:#047857}.proof-detail{font-size:13px;fill:#065f46}.foot{font-size:12px;fill:#64748b}
        </style>""",
        '<rect width="1200" height="820" class="background"/>',
        label(50, 54, "v0.23 · distributed signed trust and activation receipts", "title"),
        label(
            50,
            83,
            f"Remote g1 boot ≤ {initial_max:.1f} ms observed · root authority separate from transport · exact loopback partition",
            "subtitle",
        ),
        label(50, 130, "Publication and convergence timeline", "section"),
        panel(50, "1 · remote g1 boot", "3 / 3 receipts", "root-signed policy", "authority"),
        arrow(300, 350, "publish overlap g2"),
        panel(
            350,
            "2 · C withheld",
            f"{partial_body['receipt_count']} ack · {len(partial_body['pending_receivers'])} pending",
            "A + B active · C on g1",
            "partial",
        ),
        arrow(600, 650, "heal partition"),
        panel(
            650,
            "3 · controls reach g2",
            f"controls {g2_controls['duration_ms']:.3f} ms",
            f"then {converged_body['receipt_count']} / 3 receipts",
            "converged",
        ),
        arrow(900, 950, "rotate B · g3"),
        panel(
            950,
            "4 · controls reach g3",
            f"controls {g3_controls['duration_ms']:.3f} ms",
            f"then {g3_body['receipt_count']} / 3 receipts",
            "converged",
        ),
        label(50, 390, "Safety attacks and outage recovery", "section"),
        '<rect x="50" y="420" width="250" height="112" rx="12" class="metric"/>',
        label(175, 454, "valid rollback g2", "metric-label", "middle"),
        label(175, 491, f"HTTP {rollback['status']}", "metric-value", "middle"),
        label(175, 516, "current g3 unchanged", "metric-label", "middle"),
        '<rect x="325" y="420" width="250" height="112" rx="12" class="metric"/>',
        label(450, 454, "different valid g3 fork", "metric-label", "middle"),
        label(450, 491, f"HTTP {fork['status']}", "metric-value", "middle"),
        label(450, 516, "snapshot identity fenced", "metric-label", "middle"),
        '<rect x="600" y="420" width="250" height="112" rx="12" class="metric"/>',
        label(725, 454, "tampered higher bytes", "metric-label", "middle"),
        label(725, 491, f"HTTP {tamper['status']}", "metric-value", "middle"),
        label(725, 516, "root signature rejected", "metric-label", "middle"),
        '<rect x="875" y="420" width="275" height="112" rx="12" class="outage"/>',
        label(1012, 454, "distributor outage restart", "metric-label", "middle"),
        label(1012, 491, "cached g3", "metric-value", "middle"),
        label(
            1012,
            516,
            f"PID {cache_restart['old_pid']} → {cache_restart['new_pid']}",
            "metric-label",
            "middle",
        ),
        '<rect x="50" y="580" width="1100" height="145" rx="14" class="proof"/>',
        label(
            600,
            625,
            f"{assertions['passed']} / {assertions['total']} checks passed",
            "proof-title",
            "middle",
        ),
        label(
            600,
            661,
            f"gateway B real request {request['duration_ms']:.3f} ms · SSE {stream['duration_ms']:.3f} ms + DONE",
            "proof-detail",
            "middle",
        ),
        label(
            600,
            692,
            "publish → verify → persist cache/floor → activate → signed receipt",
            "proof-detail",
            "middle",
        ),
        label(
            50,
            775,
            "Limits: one distributor, eventual per-receiver convergence, ambiguous missing receipts, trusted local disk/key custody, and no TLS/mTLS or multi-host evidence.",
            "foot",
        ),
        "</svg>",
    ]
    args.output.write_text("".join(parts), encoding="utf-8")


if __name__ == "__main__":
    main()
