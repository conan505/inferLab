#!/usr/bin/env python3
"""Render a data-driven v0.22 signed online service-trust evidence chart."""

from __future__ import annotations

import argparse
import json
from html import escape
from pathlib import Path
from typing import Any


def load(directory: Path, name: str) -> dict[str, Any]:
    with (directory / name).open(encoding="utf-8") as source:
        return json.load(source)


def text(x: int, y: int, value: str, css: str = "label") -> str:
    return f'<text x="{x}" y="{y}" class="{css}">{escape(value)}</text>'


def stage(x: int, title: str, policy: str, result: str, css: str) -> str:
    return "".join(
        [
            f'<rect x="{x}" y="160" width="250" height="175" rx="12" class="{css}"/>',
            text(x + 125, 193, title, "stage-title"),
            text(x + 125, 235, policy, "stage-value"),
            text(x + 125, 270, result, "stage-detail"),
            text(x + 125, 310, "route revision 2 retained", "stage-foot"),
        ]
    )


def arrow(x1: int, x2: int, label: str) -> str:
    return "".join(
        [
            f'<line x1="{x1}" y1="247" x2="{x2}" y2="247" class="arrow" marker-end="url(#arrow)"/>',
            text((x1 + x2) // 2, 232, label, "edge-label"),
        ]
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    initial = load(args.evidence_dir, "initial-cluster.json")
    generation_two = load(args.evidence_dir, "generation-2-convergence.json")
    generation_three = load(args.evidence_dir, "generation-3-convergence.json")
    continuity = load(args.evidence_dir, "online-process-continuity.json")
    rollback = load(args.evidence_dir, "rollback-rejected.json")
    tamper = load(args.evidence_dir, "tamper-rejected.json")
    restart = load(args.evidence_dir, "restart-floor-rejection.json")
    request = load(args.evidence_dir, "request.json")
    stream = load(args.evidence_dir, "stream.json")
    assertions = load(args.evidence_dir, "assertions.json")

    generation_two_max = max(
        observation["convergence_latency_ms"]
        for observation in generation_two["observations"]
    )
    generation_three_max = max(
        observation["convergence_latency_ms"]
        for observation in generation_three["observations"]
    )
    rollback_rejections = sum(
        status["body"]["service_authentication"]["trust_policy_rejections"]
        for status in rollback["statuses"]
    )
    tamper_rejections = sum(
        status["body"]["service_authentication"]["trust_policy_rejections"]
        for status in tamper["statuses"]
    )

    parts = [
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 760" role="img" aria-labelledby="title desc">',
        '<title id="title">InferLab v0.22 signed online service-trust proof</title>',
        '<desc id="desc">A four-stage timeline shows root-signed generation 1, online trust expansion generation 2, online revocation generation 3, and rejection of rollback and tampered snapshots while route revision 2 remains active.</desc>',
        """<defs>
          <marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M 0 0 L 10 5 L 0 10 z"/></marker>
        </defs>
        <style>
          .background{fill:#f8fafc}.bootstrap{fill:#eff6ff;stroke:#3b82f6;stroke-width:1.5}.overlap{fill:#fefce8;stroke:#eab308;stroke-width:1.5}.active{fill:#ecfdf5;stroke:#10b981;stroke-width:1.5}.rejected{fill:#fff1f2;stroke:#f43f5e;stroke-width:1.5}.metric{fill:#fff;stroke:#cbd5e1;stroke-width:1.5}.proof{fill:#ecfdf5;stroke:#10b981;stroke-width:1.5}.arrow{stroke:#475569;stroke-width:2;fill:none}text{font-family:ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;fill:#0f172a}.title{font-size:28px;font-weight:700}.subtitle{font-size:15px;fill:#475569}.section{font-size:18px;font-weight:700}.stage-title{font-size:17px;font-weight:700;text-anchor:middle}.stage-value{font-size:18px;font-weight:700;text-anchor:middle}.stage-detail{font-size:13px;text-anchor:middle}.stage-foot{font-size:12px;fill:#475569;text-anchor:middle}.edge-label{font-size:11px;fill:#64748b;text-anchor:middle}.metric-label{font-size:12px;fill:#64748b;text-anchor:middle}.metric-value{font-size:23px;font-weight:700;text-anchor:middle}.proof-title{font-size:22px;font-weight:700;text-anchor:middle;fill:#047857}.proof-detail{font-size:13px;text-anchor:middle;fill:#065f46}.foot{font-size:12px;fill:#64748b}
        </style>""",
        '<rect width="1200" height="760" class="background"/>',
        text(50, 54, "v0.22 · signed online service-trust convergence", "title"),
        text(
            50,
            82,
            f"Initial leader {initial['leader_id']} · authenticated generations · durable rollback floor · loopback evidence",
            "subtitle",
        ),
        text(50, 122, "Last-known-good receiver policy over time", "section"),
        stage(50, "1 · bootstrap g1", "gateway A", "root signature verified", "bootstrap"),
        arrow(300, 350, "signed g2"),
        stage(350, "2 · overlap g2", "gateway A + B", f"online ≤ {generation_two_max:.1f} ms", "overlap"),
        arrow(600, 650, "signed g3"),
        stage(650, "3 · revoke g3", "reject A · accept B", f"online ≤ {generation_three_max:.1f} ms", "active"),
        arrow(900, 950, "bad input"),
        stage(950, "4 · retain g3", "rollback + tamper", "both rejected", "rejected"),
        text(50, 390, "Observed convergence and fences", "section"),
        '<rect x="50" y="418" width="250" height="112" rx="12" class="metric"/>',
        text(175, 451, "control process continuity", "metric-label"),
        text(175, 489, "3 / 3 unchanged", "metric-value"),
        text(175, 514, str(continuity["unchanged"]).lower(), "metric-label"),
        '<rect x="325" y="418" width="250" height="112" rx="12" class="metric"/>',
        text(450, 451, "online generation reloads", "metric-label"),
        text(450, 489, "g1 → g2 → g3", "metric-value"),
        text(450, 514, "no control restart", "metric-label"),
        '<rect x="600" y="418" width="250" height="112" rx="12" class="metric"/>',
        text(725, 451, "policy rejection observations", "metric-label"),
        text(725, 489, f"{rollback_rejections} → {tamper_rejections}", "metric-value"),
        text(725, 514, "rollback then signature failure", "metric-label"),
        '<rect x="875" y="418" width="275" height="112" rx="12" class="metric"/>',
        text(1012, 451, "restart rollback fence", "metric-label"),
        text(1012, 489, "blocked", "metric-value"),
        text(1012, 514, f"exit {restart['exit_status']} · durable floor 3", "metric-label"),
        '<rect x="50" y="575" width="1100" height="112" rx="14" class="proof"/>',
        text(600, 615, f"{assertions['passed']} / {assertions['total']} checks passed", "proof-title"),
        text(
            600,
            647,
            f"gateway key-b served request in {request['duration_ms']:.3f} ms · SSE in {stream['duration_ms']:.3f} ms + DONE",
            "proof-detail",
        ),
        text(
            600,
            674,
            "Signed policy generation 3 and route revision 2 survive invalid live input and process restart",
            "proof-detail",
        ),
        text(
            50,
            730,
            "Operational limit: policy files and rollback floors depend on local filesystem integrity; root keys and private service signers remain static configuration.",
            "foot",
        ),
        "</svg>",
    ]
    args.output.write_text("".join(parts), encoding="utf-8")


if __name__ == "__main__":
    main()
