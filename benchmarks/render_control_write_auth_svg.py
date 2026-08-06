#!/usr/bin/env python3
"""Render retained v0.19 administrative-writer authorization evidence."""

from __future__ import annotations

import argparse
import html
import json
from pathlib import Path


def load(directory: Path, name: str) -> dict:
    return json.loads((directory / name).read_text())


def label(
    x: float,
    y: float,
    value: object,
    css: str = "label",
    anchor: str = "start",
) -> str:
    return (
        f'<text x="{x:.1f}" y="{y:.1f}" class="{css}" '
        f'text-anchor="{anchor}">{html.escape(str(value))}</text>'
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--check", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    evidence = args.evidence_dir
    check = json.loads(args.check.read_text())
    event_files = [
        ("write-unsigned-rejected.json", "unsigned", "reject"),
        ("write-unknown-rejected.json", "unknown key", "reject"),
        ("write-tampered-rejected.json", "tampered", "reject"),
        ("write-stale-rejected.json", "stale", "stale"),
        ("write-revoked-rejected.json", "revoked", "reject"),
        ("write-valid-committed.json", "commit r2", "commit"),
        ("write-replay-rejected.json", "replay fenced", "conflict"),
        ("request-revision-2.json", "r2 serves", "serve"),
        ("write-update-committed.json", "commit r3", "commit"),
        ("stream-final.json", "r3 SSE DONE", "serve"),
    ]
    events = []
    for name, event_label, css in event_files:
        document = load(evidence, name)
        events.append((document["observed_at_ms"], event_label, css))
    start_ms = min(event[0] for event in events)

    items = [
        '<svg xmlns="http://www.w3.org/2000/svg" width="1280" height="920" viewBox="0 0 1280 920" role="img" aria-labelledby="title description">',
        '<title id="title">InferLab v0.19 authorized control-writer proof</title>',
        '<desc id="description">A signed administrative intent passes writer trust, freshness, and expected-revision checks before Raft commit. Unsigned, unknown, tampered, stale, revoked, and replayed writes are rejected while two authorized writes reach the gateway and real worker.</desc>',
        """<style>
            .bg{fill:#07111f}.panel{fill:#0d1b2d;stroke:#334155;stroke-width:1}
            .axis{stroke:#94a3b8;stroke-width:1.5}.grid{stroke:#334155;stroke-width:1}
            .arrow{fill:#94a3b8;font:700 18px system-ui,sans-serif}
            .title{fill:#f8fafc;font:700 25px system-ui,sans-serif}.subtitle{fill:#94a3b8;font:14px system-ui,sans-serif}
            .heading{fill:#f8fafc;font:700 16px system-ui,sans-serif}.label{fill:#e2e8f0;font:13px system-ui,sans-serif}
            .small{fill:#94a3b8;font:12px system-ui,sans-serif}.value{fill:#f8fafc;font:700 15px ui-monospace,monospace}
            .reject{fill:#ef4444;stroke:#fecaca;stroke-width:2}.stale{fill:#f59e0b;stroke:#fde68a;stroke-width:2}
            .conflict{fill:#a78bfa;stroke:#ddd6fe;stroke-width:2}.commit{fill:#22c55e;stroke:#bbf7d0;stroke-width:2}
            .serve{fill:#38bdf8;stroke:#bae6fd;stroke-width:2}.neutral{fill:#64748b}
            .rejectText{fill:#fca5a5;font:700 13px system-ui,sans-serif}.staleText{fill:#fcd34d;font:700 13px system-ui,sans-serif}
            .conflictText{fill:#c4b5fd;font:700 13px system-ui,sans-serif}.commitText{fill:#86efac;font:700 13px system-ui,sans-serif}
            .serveText{fill:#7dd3fc;font:700 13px system-ui,sans-serif}
        </style>""",
        '<rect width="1280" height="920" class="bg"/>',
        label(64, 52, "v0.19 · Authorized control writers", "title"),
        label(
            64,
            78,
            f"{check['assertions_passed']}/{check['assertions_total']} assertions · writer {check['writer_id']} · route key {check['route_signing_key_id']}",
            "subtitle",
        ),
        label(64, 120, "Commit gate", "heading"),
    ]

    stages = [
        (64, 194, "SIGNED INTENT", "writer · cluster · route", "expected revision · time · nonce", "serveText"),
        (270, 194, "VERIFY WRITER", "trusted public key", "unknown/revoked/tampered → 401", "conflictText"),
        (476, 194, "CHECK FRESHNESS", "bounded age + future skew", "stale/future → 401", "staleText"),
        (682, 194, "FENCE REVISION", "expected = committed", "replay/conflict → 409", "conflictText"),
        (888, 194, "RAFT COMMIT", "replicate writer provenance", "only then publish route", "commitText"),
        (1094, 122, "GATEWAY", "verify route key", "serve real worker", "serveText"),
    ]
    for index, (x, width, heading, line1, line2, heading_css) in enumerate(stages):
        items.append(f'<rect x="{x}" y="142" width="{width}" height="118" rx="8" class="panel"/>')
        items.append(label(x + 16, 174, heading, heading_css))
        items.append(label(x + 16, 204, line1, "label"))
        items.append(label(x + 16, 232, line2, "small"))
        if index < len(stages) - 1:
            items.append(label(x + width + 6, 208, "→", "arrow"))

    items.extend(
        [
            label(64, 306, "Observed write sequence", "heading"),
            '<line x1="92" y1="386" x2="1188" y2="386" class="axis"/>',
        ]
    )
    x0, x1 = 110, 1170
    rows = [344, 454, 326, 472, 344, 454, 326, 472, 344, 454]
    revisions = [0, 0, 0, 0, 0, 2, 2, 2, 3, 3]
    for index, ((at_ms, text_value, css), row, revision) in enumerate(
        zip(events, rows, revisions)
    ):
        x = x0 + index * (x1 - x0) / (len(events) - 1)
        items.append(
            f'<line x1="{x:.1f}" y1="386" x2="{x:.1f}" y2="{row + (-10 if row > 386 else 8)}" class="grid"/>'
        )
        if css == "serve":
            items.append(f'<rect x="{x - 6:.1f}" y="380" width="12" height="12" class="{css}"/>')
        else:
            items.append(f'<circle cx="{x:.1f}" cy="386" r="7" class="{css}"/>')
        anchor = "start" if index == 0 else "end" if index == len(events) - 1 else "middle"
        items.append(label(x, row, text_value, "label", anchor))
        items.append(label(x, 416, f"r{revision}", "small", "middle"))
        items.append(label(x, 506, f"+{(at_ms - start_ms) / 1000:.1f}s", "small", "middle"))

    items.append(label(64, 558, "Decision outcomes", "heading"))
    panels = [
        (64, 260, "AUTHENTICATION", "rejectText", f"{check['authentication_rejections']} rejected", "unsigned · unknown · tampered · revoked"),
        (340, 260, "FRESHNESS", "staleText", f"{check['freshness_rejections']} rejected", "valid signature, expired intent"),
        (616, 260, "REVISION FENCE", "conflictText", f"{check['revision_conflicts']} rejected", "exact replay cannot create r3"),
        (892, 324, "AUTHORIZED COMMITS", "commitText", f"{check['committed_writes']} committed · final r{check['final_revision']}", "writer provenance replicated on 3 nodes"),
    ]
    for x, width, heading, heading_css, line1, line2 in panels:
        items.append(f'<rect x="{x}" y="582" width="{width}" height="126" rx="8" class="panel"/>')
        items.append(label(x + 18, 614, heading, heading_css))
        items.append(label(x + 18, 650, line1, "label"))
        items.append(label(x + 18, 680, line2, "small"))

    items.extend(
        [
            label(64, 758, "Identity separation", "heading"),
            label(242, 758, "deploy-bot", "value"),
            label(350, 758, "authorizes creation", "small"),
            label(574, 758, "→", "arrow"),
            label(612, 758, "route-2026-b", "value"),
            label(758, 758, "authenticates delivery", "small"),
            label(64, 814, "Protected: API mutation intent · route bytes · optimistic replay fence · durable writer audit", "subtitle"),
            label(64, 846, "Boundary: static writer trust is not mTLS, fine-grained RBAC, durable idempotency, HSM storage, or Raft peer authentication.", "small"),
            label(
                1216,
                886,
                f"final real SSE {check['final_stream_duration_ms']:.0f} ms · DONE",
                "serveText",
                "end",
            ),
            "</svg>",
        ]
    )
    args.output.write_text("\n".join(items) + "\n")


if __name__ == "__main__":
    main()
