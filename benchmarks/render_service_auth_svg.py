#!/usr/bin/env python3
"""Render a data-driven v0.20 service-authentication evidence diagram."""

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


def box(x: int, y: int, width: int, height: int, title: str, detail: str, css: str) -> str:
    return "".join(
        [
            f'<rect x="{x}" y="{y}" width="{width}" height="{height}" rx="12" class="{css}"/>',
            text(x + width // 2, y + 28, title, "box-title"),
            text(x + width // 2, y + 51, detail, "box-detail"),
        ]
    )


def arrow(x1: int, y: int, x2: int, label: str) -> str:
    return "".join(
        [
            f'<line x1="{x1}" y1="{y}" x2="{x2}" y2="{y}" class="arrow" marker-end="url(#arrow)"/>',
            text((x1 + x2) // 2, y - 10, label, "edge-label"),
        ]
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    election = load(args.evidence_dir, "election.json")
    final_cluster = load(args.evidence_dir, "final-cluster.json")
    leader = load(args.evidence_dir, "leader-after-rejections.json")["response"]["body"]
    gateway = load(args.evidence_dir, "gateway-ready.json")["status"]["body"]
    request = load(args.evidence_dir, "request.json")
    stream = load(args.evidence_dir, "stream.json")
    assertions = load(args.evidence_dir, "assertions.json")

    auth = leader["service_authentication"]
    final_statuses = [sample["body"] for sample in final_cluster["statuses"]]
    peer_rpcs = sum(
        status["service_authentication"]["authorized_peer_rpcs"]
        for status in final_statuses
    )
    gateway_reads = sum(
        status["service_authentication"]["authorized_gateway_reads"]
        for status in final_statuses
    )
    rejection_values = [
        ("authentication", auth["authentication_rejections"], "bar-auth"),
        ("freshness", auth["freshness_rejections"], "bar-fresh"),
        ("replay", auth["replay_rejections"], "bar-replay"),
        ("authorization", auth["authorization_rejections"], "bar-scope"),
    ]
    maximum = max(value for _, value, _ in rejection_values)

    parts = [
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 760" role="img" aria-labelledby="title desc">',
        '<title id="title">InferLab v0.20 cryptographic service identity proof</title>',
        '<desc id="desc">Two authenticated request lanes show signed Raft peer RPCs and signed gateway control reads. A rejection chart distinguishes authentication, freshness, replay, and authorization failures.</desc>',
        """<defs>
          <marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M 0 0 L 10 5 L 0 10 z"/></marker>
        </defs>
        <style>
          .background{fill:#f8fafc}.panel{fill:#fff;stroke:#cbd5e1;stroke-width:1.5}.trusted{fill:#ecfdf5;stroke:#10b981;stroke-width:1.5}.decision{fill:#eff6ff;stroke:#3b82f6;stroke-width:1.5}.durable{fill:#f5f3ff;stroke:#8b5cf6;stroke-width:1.5}.rejected{fill:#fff1f2;stroke:#f43f5e;stroke-width:1.5}.arrow{stroke:#475569;stroke-width:2;fill:none}.arrow marker{fill:#475569}text{font-family:ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;fill:#0f172a}.title{font-size:28px;font-weight:700}.subtitle{font-size:15px;fill:#475569}.lane{font-size:17px;font-weight:700}.box-title{font-size:15px;font-weight:700;text-anchor:middle}.box-detail{font-size:12px;fill:#475569;text-anchor:middle}.edge-label{font-size:11px;fill:#64748b;text-anchor:middle}.section{font-size:18px;font-weight:700}.label{font-size:13px}.value{font-size:13px;font-weight:700}.bar-auth{fill:#ef4444}.bar-fresh{fill:#f59e0b}.bar-replay{fill:#8b5cf6}.bar-scope{fill:#0ea5e9}.axis{stroke:#94a3b8;stroke-width:1}.proof{fill:#ecfdf5;stroke:#10b981;stroke-width:1.5}.proof-title{font-size:22px;font-weight:700;text-anchor:middle;fill:#047857}.proof-detail{font-size:13px;text-anchor:middle;fill:#065f46}.foot{font-size:12px;fill:#64748b}
        </style>""",
        '<rect width="1200" height="760" class="background"/>',
        text(50, 54, "v0.20 · cryptographic service identity", "title"),
        text(
            50,
            82,
            f"Leader {election['leader_id']} · term {election['term']} · committed revision 2 · loopback evidence",
            "subtitle",
        ),
        text(50, 130, "Raft peer lane", "lane"),
        box(50, 150, 200, 72, "Peer service", "node ID + private key", "trusted"),
        arrow(250, 186, 310, "signed"),
        box(310, 150, 220, 72, "Integrity gate", "method · path · audience · body", "decision"),
        arrow(530, 186, 590, "fresh"),
        box(590, 150, 220, 72, "Scope gate", "service ID = claimed peer ID", "decision"),
        arrow(810, 186, 870, "allowed"),
        box(870, 150, 280, 72, "Raft state transition", f"{peer_rpcs} authorized peer RPCs observed", "durable"),
        text(50, 275, "Gateway control-read lane", "lane"),
        box(50, 295, 200, 72, "Gateway service", "gateway-primary private key", "trusted"),
        arrow(250, 331, 310, "signed GET"),
        box(310, 295, 220, 72, "Request gate", "signature · time · nonce", "decision"),
        arrow(530, 331, 590, "scoped"),
        box(590, 295, 220, 72, "Audience + allowlist", "exact node + gateway role", "decision"),
        arrow(810, 331, 870, "route"),
        box(870, 295, 280, 72, "Signed route → worker", f"{gateway_reads} authorized reads · r2", "durable"),
        text(50, 424, "Rejected requests at the leader", "section"),
        '<line x1="250" y1="455" x2="250" y2="618" class="axis"/>',
    ]

    for index, (name, value, css) in enumerate(rejection_values):
        y = 468 + index * 38
        width = 0 if maximum == 0 else round(420 * value / maximum)
        parts.extend(
            [
                text(50, y + 16, name, "label"),
                f'<rect x="250" y="{y}" width="{width}" height="22" rx="4" class="{css}"/>',
                text(262 + width, y + 16, str(value), "value"),
            ]
        )

    control = gateway["control_plane"]
    parts.extend(
        [
            '<rect x="740" y="430" width="410" height="190" rx="14" class="proof"/>',
            text(
                945,
                476,
                f"{assertions['passed']} / {assertions['total']} checks passed",
                "proof-title",
            ),
            text(945, 510, "unknown · stale · replay · tamper · wrong scope rejected", "proof-detail"),
            text(
                945,
                540,
                f"gateway identity: {control['service_id']} · route key: route-2026-b",
                "proof-detail",
            ),
            text(
                945,
                570,
                f"real request {request['duration_ms']:.3f} ms · SSE {stream['duration_ms']:.3f} ms + DONE",
                "proof-detail",
            ),
            text(50, 676, "What signatures add", "section"),
            text(50, 704, "Identity · request integrity · bounded freshness · process-local replay defense · endpoint scope", "label"),
            text(50, 733, "What they do not add: encryption, hostname authentication, durable nonce history, or automatic key rotation.", "foot"),
            "</svg>",
        ]
    )
    args.output.write_text("".join(parts), encoding="utf-8")


if __name__ == "__main__":
    main()
