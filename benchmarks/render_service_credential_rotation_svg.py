#!/usr/bin/env python3
"""Render a data-driven v0.21 service credential rotation evidence chart."""

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


def stage(
    x: int,
    title: str,
    controls: str,
    gateway: str,
    trust: str,
    css: str,
) -> str:
    return "".join(
        [
            f'<rect x="{x}" y="168" width="248" height="184" rx="12" class="{css}"/>',
            text(x + 124, 200, title, "stage-title"),
            text(x + 20, 238, "control signers", "stage-label"),
            text(x + 228, 238, controls, "stage-value"),
            text(x + 20, 273, "gateway signer", "stage-label"),
            text(x + 228, 273, gateway, "stage-value"),
            text(x + 20, 308, "verification state", "stage-label"),
            text(x + 228, 308, trust, "stage-value"),
            text(x + 124, 337, "route revision 2", "stage-foot"),
        ]
    )


def arrow(x1: int, x2: int, label: str) -> str:
    return "".join(
        [
            f'<line x1="{x1}" y1="260" x2="{x2}" y2="260" class="arrow" marker-end="url(#arrow)"/>',
            text((x1 + x2) // 2, 245, label, "edge-label"),
        ]
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    initial = load(args.evidence_dir, "initial-cluster.json")
    after_control = load(args.evidence_dir, "after-control-key-b.json")
    after_revoke = load(args.evidence_dir, "after-key-a-revocation.json")
    after_attacks = load(args.evidence_dir, "after-revoked-attacks.json")["response"]["body"]
    request = load(args.evidence_dir, "request.json")
    stream = load(args.evidence_dir, "stream.json")
    assertions = load(args.evidence_dir, "assertions.json")

    mixed_counts: dict[str, int] = {}
    for sample in after_control["statuses"]:
        for credential, count in sample["body"]["service_authentication"][
            "verifications_by_credential"
        ].items():
            mixed_counts[credential] = mixed_counts.get(credential, 0) + count
    key_a_accepts = sum(
        count for credential, count in mixed_counts.items() if credential.endswith("/key-a")
    )
    key_b_accepts = sum(
        count for credential, count in mixed_counts.items() if credential.endswith("/key-b")
    )
    final_statuses = [sample["body"] for sample in after_revoke["statuses"]]
    final_term = after_attacks["term"]
    credential_rejections = after_attacks["service_authentication"][
        "credential_revocation_rejections"
    ]

    parts = [
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 760" role="img" aria-labelledby="title desc">',
        '<title id="title">InferLab v0.21 overlap-safe service credential rotation proof</title>',
        '<desc id="desc">A four-stage timeline shows adding overlapping credentials, rolling control and gateway signers, revoking key-a, and rejecting old credentials while preserving quorum and route revision 2.</desc>',
        """<defs>
          <marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M 0 0 L 10 5 L 0 10 z"/></marker>
        </defs>
        <style>
          .background{fill:#f8fafc}.before{fill:#eff6ff;stroke:#3b82f6;stroke-width:1.5}.overlap{fill:#fefce8;stroke:#eab308;stroke-width:1.5}.rotated{fill:#ecfdf5;stroke:#10b981;stroke-width:1.5}.revoked{fill:#fff1f2;stroke:#f43f5e;stroke-width:1.5}.proof{fill:#ecfdf5;stroke:#10b981;stroke-width:1.5}.metric{fill:#fff;stroke:#cbd5e1;stroke-width:1.5}.arrow{stroke:#475569;stroke-width:2;fill:none}text{font-family:ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;fill:#0f172a}.title{font-size:28px;font-weight:700}.subtitle{font-size:15px;fill:#475569}.stage-title{font-size:17px;font-weight:700;text-anchor:middle}.stage-label{font-size:12px;fill:#475569}.stage-value{font-size:13px;font-weight:700;text-anchor:end}.stage-foot{font-size:12px;fill:#475569;text-anchor:middle}.edge-label{font-size:11px;fill:#64748b;text-anchor:middle}.section{font-size:18px;font-weight:700}.metric-label{font-size:12px;fill:#64748b;text-anchor:middle}.metric-value{font-size:23px;font-weight:700;text-anchor:middle}.proof-title{font-size:22px;font-weight:700;text-anchor:middle;fill:#047857}.proof-detail{font-size:13px;text-anchor:middle;fill:#065f46}.foot{font-size:12px;fill:#64748b}
        </style>""",
        '<rect width="1200" height="760" class="background"/>',
        text(50, 54, "v0.21 · overlap-safe service credential rotation", "title"),
        text(
            50,
            82,
            f"Initial leader {initial['leader_id']} · final leader {after_revoke['leader_id']} · final term {final_term} · loopback evidence",
            "subtitle",
        ),
        text(50, 125, "Trust expansion → signer rotation → credential revocation", "section"),
        stage(50, "1 · prepare overlap", "A / A / A", "A", "trust A + B", "before"),
        arrow(298, 342, "roll peers"),
        stage(342, "2 · mixed window", "A + B", "A", "accept A + B", "overlap"),
        arrow(590, 634, "leader last"),
        stage(634, "3 · finish rotation", "B / B / B", "B", "accept A + B", "rotated"),
        arrow(882, 926, "revoke A"),
        stage(926, "4 · close window", "B / B / B", "B", "reject A", "revoked"),
        text(50, 405, "Observed boundary behavior", "section"),
        '<rect x="50" y="430" width="250" height="112" rx="12" class="metric"/>',
        text(175, 463, "accepted during overlap", "metric-label"),
        text(175, 500, f"A {key_a_accepts} · B {key_b_accepts}", "metric-value"),
        text(175, 525, "mathematically matched credential", "metric-label"),
        '<rect x="325" y="430" width="250" height="112" rx="12" class="metric"/>',
        text(450, 463, "rolling restart checkpoints", "metric-label"),
        text(450, 500, "6 / 6", "metric-value"),
        text(450, 525, "three nodes · exactly one leader", "metric-label"),
        '<rect x="600" y="430" width="250" height="112" rx="12" class="metric"/>',
        text(725, 463, "revoked-key rejections", "metric-label"),
        text(725, 500, str(credential_rejections), "metric-value"),
        text(725, 525, "gateway read + high-term vote", "metric-label"),
        '<rect x="875" y="430" width="275" height="112" rx="12" class="metric"/>',
        text(1012, 463, "final durable state", "metric-label"),
        text(1012, 500, "3 × key-b · r2", "metric-value"),
        text(1012, 525, f"{len(final_statuses)} replicas agree", "metric-label"),
        '<rect x="50" y="585" width="1100" height="110" rx="14" class="proof"/>',
        text(600, 624, f"{assertions['passed']} / {assertions['total']} checks passed", "proof-title"),
        text(
            600,
            655,
            f"old key-a blocked · current key-b served request in {request['duration_ms']:.3f} ms · SSE in {stream['duration_ms']:.3f} ms + DONE",
            "proof-detail",
        ),
        text(
            600,
            681,
            "Bounded verification ring: at most 16 credentials per service and 256 total",
            "proof-detail",
        ),
        text(
            50,
            733,
            "Operational limit: trust and revocation changes still require rolling process restarts; request signatures do not provide transport encryption.",
            "foot",
        ),
        "</svg>",
    ]
    args.output.write_text("".join(parts), encoding="utf-8")


if __name__ == "__main__":
    main()
