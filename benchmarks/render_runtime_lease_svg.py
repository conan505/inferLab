#!/usr/bin/env python3
"""Render retained v0.16 runtime routing-lease evidence as an SVG chart."""

from __future__ import annotations

import argparse
import html
import json
from pathlib import Path


def load(directory: Path, name: str) -> dict:
    return json.loads((directory / name).read_text())


def text(x: float, y: float, value: object, css: str = "label", anchor: str = "start") -> str:
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
    live = load(evidence, "lease-live-fresh.json")
    stream = load(evidence, "stream-crossing-expiry.json")
    outage = load(evidence, "control-outage.json")
    expired = load(evidence, "lease-expired-rejecting.json")
    rejected = load(evidence, "request-rejected.json")
    election = load(evidence, "recovered-election.json")
    renewed = load(evidence, "lease-renewed.json")
    second_outage = load(evidence, "control-second-outage.json")
    stale = load(evidence, "lease-expired-serving-stale.json")
    final_stream = load(evidence, "stream-final.json")

    events = [
        (live["observed_at_ms"], "fresh", "live verified", "ok"),
        (stream["started_at_ms"], "stream", "stream admitted", "stream"),
        (outage["at_ms"], "fault", "all control stopped", "fault"),
        (expired["observed_at_ms"], "expired", "lease expired", "bad"),
        (rejected["observed_at_ms"], "reject", "new request: 503", "bad"),
        (election["observed_at_ms"], "recover", "control elected", "fault"),
        (renewed["observed_at_ms"], "renew", "same r renewed", "ok"),
        (second_outage["at_ms"], "fault", "control stopped", "fault"),
        (stale["observed_at_ms"], "stale", "serve-stale ready", "stale"),
        (final_stream["observed_at_ms"], "done", "final SSE DONE", "stream"),
    ]
    start = min(event[0] for event in events)
    end = max(event[0] for event in events)
    span = max(end - start, 1)
    x0, x1 = 112, 1208

    def scale(value: float) -> float:
        return x0 + (value - start) / span * (x1 - x0)

    items = [
        '<svg xmlns="http://www.w3.org/2000/svg" width="1280" height="850" viewBox="0 0 1280 850" role="img" aria-labelledby="title description">',
        '<title id="title">InferLab v0.16 runtime routing lease proof</title>',
        '<desc id="description">Timeline showing live lease renewal, total control outage, completion of an existing stream, rejection of a new request after lease expiry, recovery by equal-revision control, and explicit serve-stale behavior.</desc>',
        """<style>
            .bg{fill:#07111f}.panel{fill:#0d1b2d;stroke:#334155;stroke-width:1}
            .grid{stroke:#334155;stroke-width:1}.axis{stroke:#94a3b8;stroke-width:1.5}
            .title{fill:#f8fafc;font:700 25px system-ui,sans-serif}.subtitle{fill:#94a3b8;font:14px system-ui,sans-serif}
            .heading{fill:#f8fafc;font:700 16px system-ui,sans-serif}.label{fill:#e2e8f0;font:13px system-ui,sans-serif}
            .small{fill:#94a3b8;font:12px system-ui,sans-serif}.value{fill:#f8fafc;font:700 15px ui-monospace,monospace}
            .ok{fill:#22c55e;stroke:#bbf7d0;stroke-width:2}.bad{fill:#ef4444;stroke:#fecaca;stroke-width:2}
            .fault{fill:#f59e0b;stroke:#fde68a;stroke-width:2}.stream{fill:#38bdf8;stroke:#bae6fd;stroke-width:2}
            .stale{fill:#a78bfa;stroke:#ddd6fe;stroke-width:2}.neutral{fill:#64748b}
            .okText{fill:#86efac;font:700 13px system-ui,sans-serif}.badText{fill:#fca5a5;font:700 13px system-ui,sans-serif}
            .staleText{fill:#c4b5fd;font:700 13px system-ui,sans-serif}
        </style>""",
        '<rect width="1280" height="850" class="bg"/>',
        text(64, 54, "v0.16 · Runtime routing lease", "title"),
        text(
            64,
            80,
            f"r{check['revision']}/t{check['term']} · {check['lease_duration_ms']} ms lease · {check['assertions_passed']}/{check['assertions_total']} assertions",
            "subtitle",
        ),
        text(64, 126, "One request is checked once; live verification renews the gate for future requests", "heading"),
        '<line x1="112" y1="230" x2="1208" y2="230" class="axis"/>',
    ]

    for tick in range(6):
        x = x0 + tick / 5 * (x1 - x0)
        elapsed = span * tick / 5 / 1000
        items.append(f'<line x1="{x:.1f}" y1="215" x2="{x:.1f}" y2="245" class="grid"/>')
        items.append(text(x, 265, f"+{elapsed:.1f}s", "small", "middle"))

    label_rows = [188, 310, 164, 334, 188, 310, 164, 334, 188, 310]
    for (at_ms, _, label, css), label_y in zip(events, label_rows):
        x = scale(at_ms)
        marker_y = 230
        items.append(f'<line x1="{x:.1f}" y1="{marker_y}" x2="{x:.1f}" y2="{label_y + (-10 if label_y > marker_y else 8)}" class="grid"/>')
        if css == "stream":
            items.append(f'<rect x="{x - 6:.1f}" y="224" width="12" height="12" class="{css}"/>')
        elif css == "fault":
            items.append(f'<path d="M {x:.1f} 220 l 10 18 h -20 z" class="{css}"/>')
        else:
            items.append(f'<circle cx="{x:.1f}" cy="230" r="7" class="{css}"/>')
        anchor = "start" if x < 150 else "end" if x > 1130 else "middle"
        items.append(text(x, label_y, label, "label", anchor))

    stream_x0 = scale(stream["started_at_ms"])
    stream_x1 = scale(stream["observed_at_ms"])
    items.extend(
        [
            f'<line x1="{stream_x0:.1f}" y1="378" x2="{stream_x1:.1f}" y2="378" class="stream" stroke-width="7"/>',
            f'<circle cx="{stream_x0:.1f}" cy="378" r="6" class="stream"/>',
            f'<circle cx="{stream_x1:.1f}" cy="378" r="6" class="stream"/>',
            text(64, 382, "existing SSE", "label"),
            text(
                (stream_x0 + stream_x1) / 2,
                402,
                f"{stream['duration_ms']:.0f} ms · crosses expiry · DONE",
                "small",
                "middle",
            ),
            text(64, 454, "Expiry policy changes admission and readiness—not ownership already granted", "heading"),
        ]
    )

    columns = [64, 472, 880]
    widths = [360, 360, 336]
    panels = [
        (
            "FRESH",
            "okText",
            "GET /readyz → 200",
            "new request → worker",
            "valid live r renews deadline",
        ),
        (
            "EXPIRED · REJECT-NEW",
            "badText",
            "GET /readyz → 503",
            "new request → 503 · attempts 0",
            "existing stream → continues",
        ),
        (
            "EXPIRED · SERVE-STALE",
            "staleText",
            "GET /readyz → 200",
            "new request → worker",
            "operator accepts stale-route risk",
        ),
    ]
    for x, width, panel in zip(columns, widths, panels):
        items.append(f'<rect x="{x}" y="478" width="{width}" height="146" rx="8" class="panel"/>')
        items.append(text(x + 20, 510, panel[0], panel[1]))
        items.append(text(x + 20, 546, panel[2], "label"))
        items.append(text(x + 20, 574, panel[3], "label"))
        items.append(text(x + 20, 604, panel[4], "small"))

    items.extend(
        [
            text(64, 672, "Observed request outcomes", "heading"),
            text(64, 706, "existing stream", "label"),
            '<rect x="220" y="691" width="250" height="20" rx="4" class="neutral"/>',
            '<rect x="220" y="691" width="250" height="20" rx="4" class="stream"/>',
            text(486, 706, "1 / 1 DONE", "value"),
            text(64, 744, "expired reject-new", "label"),
            '<rect x="220" y="729" width="250" height="20" rx="4" class="neutral"/>',
            text(486, 744, "0 worker attempts", "value"),
            text(650, 706, "after equal-r renewal", "label"),
            '<rect x="850" y="691" width="190" height="20" rx="4" class="neutral"/>',
            '<rect x="850" y="691" width="190" height="20" rx="4" class="ok"/>',
            text(1056, 706, "1 / 1", "value"),
            text(650, 744, "serve-stale traffic", "label"),
            '<rect x="850" y="729" width="190" height="20" rx="4" class="neutral"/>',
            '<rect x="850" y="729" width="190" height="20" rx="4" class="stale"/>',
            text(1056, 744, "request + SSE", "value"),
            text(
                64,
                810,
                f"Proof: three Raft processes · one real CPU worker · exact child PIDs · {check['rejected_worker_attempts']} rejected worker attempts",
                "subtitle",
            ),
            "</svg>",
        ]
    )
    args.output.write_text("\n".join(items) + "\n")


if __name__ == "__main__":
    main()
