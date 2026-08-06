#!/usr/bin/env python3
"""Render retained v0.17 control-cluster identity evidence as SVG."""

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
    primary = load(evidence, "gateway-primary-fresh.json")
    stream = load(evidence, "stream-crossing-foreign-cluster.json")
    outage = load(evidence, "primary-control-outage.json")
    foreign = load(evidence, "gateway-foreign-rejected.json")
    rejected = load(evidence, "request-foreign-rejected.json")
    renewed = load(evidence, "gateway-primary-renewed.json")
    disk_rejected = load(evidence, "foreign-disk-bootstrap-rejected.json")
    repair = load(evidence, "gateway-live-repair.json")
    final_stream = load(evidence, "stream-final.json")

    events = [
        (primary["observed_at_ms"], "primary accepted", "primary"),
        (stream["started_at_ms"], "SSE owns primary", "stream"),
        (outage["at_ms"], "primary stops", "fault"),
        (foreign["observed_at_ms"], "foreign rejected", "foreign"),
        (rejected["observed_at_ms"], "new request 503", "foreign"),
        (stream["observed_at_ms"], "existing SSE DONE", "stream"),
        (renewed["observed_at_ms"], "primary renews", "primary"),
        (repair["observed_at_ms"], "live repairs disk", "primary"),
        (final_stream["observed_at_ms"], "final SSE DONE", "stream"),
    ]
    start = min(event[0] for event in events)
    end = max(event[0] for event in events)
    span = max(end - start, 1)
    x0, x1 = 112, 1208

    def scale(value: float) -> float:
        return x0 + (value - start) / span * (x1 - x0)

    items = [
        '<svg xmlns="http://www.w3.org/2000/svg" width="1280" height="880" viewBox="0 0 1280 880" role="img" aria-labelledby="title description">',
        '<title id="title">InferLab v0.17 control-cluster identity fencing proof</title>',
        '<desc id="description">A timeline and identity comparison showing that equal revision numbers from different Raft clusters are not interchangeable, a foreign cluster cannot renew the gateway lease, existing primary work completes, primary recovery renews service, and valid live primary control repairs a foreign disk identity.</desc>',
        """<style>
            .bg{fill:#07111f}.panel{fill:#0d1b2d;stroke:#334155;stroke-width:1}
            .axis{stroke:#94a3b8;stroke-width:1.5}.grid{stroke:#334155;stroke-width:1}
            .title{fill:#f8fafc;font:700 25px system-ui,sans-serif}.subtitle{fill:#94a3b8;font:14px system-ui,sans-serif}
            .heading{fill:#f8fafc;font:700 16px system-ui,sans-serif}.label{fill:#e2e8f0;font:13px system-ui,sans-serif}
            .small{fill:#94a3b8;font:12px system-ui,sans-serif}.value{fill:#f8fafc;font:700 15px ui-monospace,monospace}
            .primary{fill:#22c55e;stroke:#bbf7d0;stroke-width:2}.foreign{fill:#ef4444;stroke:#fecaca;stroke-width:2}
            .stream{fill:#38bdf8;stroke:#bae6fd;stroke-width:2}.fault{fill:#f59e0b;stroke:#fde68a;stroke-width:2}
            .primaryText{fill:#86efac;font:700 13px system-ui,sans-serif}.foreignText{fill:#fca5a5;font:700 13px system-ui,sans-serif}
            .streamText{fill:#7dd3fc;font:700 13px system-ui,sans-serif}.neutral{fill:#64748b}
        </style>""",
        '<rect width="1280" height="880" class="bg"/>',
        label(64, 52, "v0.17 · Control-cluster identity fencing", "title"),
        label(
            64,
            78,
            f"{check['assertions_passed']}/{check['assertions_total']} assertions · expected {check['expected_cluster_id']} · rejected {check['rejected_cluster_id']}",
            "subtitle",
        ),
        label(64, 122, "Revision is unique only inside a cluster namespace", "heading"),
        '<rect x="64" y="142" width="540" height="126" rx="8" class="panel"/>',
        '<rect x="676" y="142" width="540" height="126" rx="8" class="panel"/>',
        label(88, 176, "PRIMARY CLUSTER", "primaryText"),
        label(700, 176, "FOREIGN CLUSTER", "foreignText"),
        label(88, 208, "cluster = inferlab-primary", "value"),
        label(700, 208, "cluster = inferlab-foreign", "value"),
        label(88, 238, f"routing identity = r{check['revision']} / t1 · cpu-primary", "label"),
        label(700, 238, f"routing identity = r{check['revision']} / t1 · cpu-foreign", "label"),
        label(640, 208, "≠", "title", "middle"),
        label(64, 310, "Runtime sequence", "heading"),
        '<line x1="112" y1="390" x2="1208" y2="390" class="axis"/>',
    ]

    for tick in range(6):
        x = x0 + tick / 5 * (x1 - x0)
        elapsed = span * tick / 5 / 1000
        items.append(f'<line x1="{x:.1f}" y1="376" x2="{x:.1f}" y2="404" class="grid"/>')
        items.append(label(x, 426, f"+{elapsed:.1f}s", "small", "middle"))

    rows = [348, 468, 326, 490, 348, 468, 326, 490, 348]
    for (at_ms, text_value, css), row in zip(events, rows):
        x = scale(at_ms)
        marker = 390
        items.append(
            f'<line x1="{x:.1f}" y1="{marker}" x2="{x:.1f}" y2="{row + (-10 if row > marker else 8)}" class="grid"/>'
        )
        if css == "fault":
            items.append(f'<path d="M {x:.1f} 380 l 10 18 h -20 z" class="{css}"/>')
        elif css == "stream":
            items.append(f'<rect x="{x - 6:.1f}" y="384" width="12" height="12" class="{css}"/>')
        else:
            items.append(f'<circle cx="{x:.1f}" cy="390" r="7" class="{css}"/>')
        anchor = "start" if x < 150 else "end" if x > 1130 else "middle"
        items.append(label(x, row, text_value, "label", anchor))

    stream_x0 = scale(stream["started_at_ms"])
    stream_x1 = scale(stream["observed_at_ms"])
    items.extend(
        [
            f'<line x1="{stream_x0:.1f}" y1="532" x2="{stream_x1:.1f}" y2="532" class="stream" stroke-width="7"/>',
            f'<circle cx="{stream_x0:.1f}" cy="532" r="6" class="stream"/>',
            f'<circle cx="{stream_x1:.1f}" cy="532" r="6" class="stream"/>',
            label(64, 536, "primary SSE", "label"),
            label(
                (stream_x0 + stream_x1) / 2,
                556,
                f"{stream['duration_ms']:.0f} ms · foreign rejected · primary ownership preserved",
                "small",
                "middle",
            ),
            label(64, 606, "Fence outcomes", "heading"),
        ]
    )

    panels = [
        (64, 350, "LIVE FOREIGN", "foreignText", f"{check['cluster_mismatch_rejections']} responses rejected", "lease not renewed · readiness 503"),
        (464, 350, "FOREIGN DISK", "foreignText", "bootstrap refused", "same route bytes do not override namespace"),
        (864, 352, "PRIMARY RETURNS", "primaryText", "same primary r2 accepted", "lease renews · live repairs disk identity"),
    ]
    for x, width, heading, heading_css, line1, line2 in panels:
        items.append(f'<rect x="{x}" y="628" width="{width}" height="126" rx="8" class="panel"/>')
        items.append(label(x + 20, 660, heading, heading_css))
        items.append(label(x + 20, 696, line1, "label"))
        items.append(label(x + 20, 726, line2, "small"))

    items.extend(
        [
            label(64, 802, "Workers reached by rejected request", "heading"),
            label(410, 802, "cpu-primary", "label", "end"),
            '<rect x="430" y="786" width="240" height="20" rx="4" class="neutral"/>',
            label(686, 802, "0 attempts", "value"),
            label(930, 802, "cpu-foreign", "label", "end"),
            '<rect x="950" y="786" width="160" height="20" rx="4" class="neutral"/>',
            label(1126, 802, "0 attempts", "value"),
            label(
                64,
                850,
                f"Proof: two independent 3-node Raft clusters · two real CPU workers · r{check['revision']} exists in both namespaces · exact child PIDs",
                "subtitle",
            ),
            "</svg>",
        ]
    )
    args.output.write_text("\n".join(items) + "\n")


if __name__ == "__main__":
    main()
