#!/usr/bin/env python3
"""Render retained v0.18 signed-control evidence as SVG."""

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
    old_request = load(evidence, "request-old-key.json")
    stream = load(evidence, "stream-crossing-rogue-key.json")
    outage = load(evidence, "primary-control-outage.json")
    rogue = load(evidence, "gateway-rogue-rejected.json")
    rejected = load(evidence, "request-rogue-rejected.json")
    renewed = load(evidence, "gateway-new-key-renewed.json")
    downgrade = load(evidence, "gateway-key-downgrade-rejected.json")
    rerenewed = load(evidence, "gateway-new-key-rerenewed.json")
    disk = load(evidence, "gateway-new-key-disk.json")
    final_stream = load(evidence, "stream-final.json")

    events = [
        (old_request["observed_at_ms"], "old key accepted", "old"),
        (stream["started_at_ms"], "SSE owns old key", "stream"),
        (outage["at_ms"], "primary stops", "fault"),
        (rogue["observed_at_ms"], "unknown key rejected", "reject"),
        (rejected["observed_at_ms"], "new request 503", "reject"),
        (stream["observed_at_ms"], "existing SSE DONE", "stream"),
        (renewed["observed_at_ms"], "new key renews", "new"),
        (downgrade["observed_at_ms"], "old key downgrade", "reject"),
        (rerenewed["observed_at_ms"], "new key re-renews", "new"),
        (disk["observed_at_ms"], "new-key disk boots", "new"),
        (final_stream["observed_at_ms"], "final SSE DONE", "stream"),
    ]
    start = min(event[0] for event in events)
    end = max(event[0] for event in events)
    span = max(end - start, 1)
    x0, x1 = 112, 1208

    def scale(value: float) -> float:
        return x0 + (value - start) / span * (x1 - x0)

    items = [
        '<svg xmlns="http://www.w3.org/2000/svg" width="1280" height="920" viewBox="0 0 1280 920" role="img" aria-labelledby="title description">',
        '<title id="title">InferLab v0.18 signed-control and key-rotation proof</title>',
        '<desc id="description">A signing chain and timeline showing that an unknown key cannot replace an authenticated route or renew its lease, admitted streaming completes, trusted key B rotates the same route, later key A cannot downgrade it, tampered and revoked disk state fail, and the new-key disk remains usable.</desc>',
        """<style>
            .bg{fill:#07111f}.panel{fill:#0d1b2d;stroke:#334155;stroke-width:1}
            .axis{stroke:#94a3b8;stroke-width:1.5}.grid{stroke:#334155;stroke-width:1}
            .title{fill:#f8fafc;font:700 25px system-ui,sans-serif}.subtitle{fill:#94a3b8;font:14px system-ui,sans-serif}
            .heading{fill:#f8fafc;font:700 16px system-ui,sans-serif}.label{fill:#e2e8f0;font:13px system-ui,sans-serif}
            .small{fill:#94a3b8;font:12px system-ui,sans-serif}.value{fill:#f8fafc;font:700 15px ui-monospace,monospace}
            .old{fill:#a78bfa;stroke:#ddd6fe;stroke-width:2}.new{fill:#22c55e;stroke:#bbf7d0;stroke-width:2}
            .reject{fill:#ef4444;stroke:#fecaca;stroke-width:2}.stream{fill:#38bdf8;stroke:#bae6fd;stroke-width:2}
            .fault{fill:#f59e0b;stroke:#fde68a;stroke-width:2}.neutral{fill:#64748b}
            .oldText{fill:#c4b5fd;font:700 13px system-ui,sans-serif}.newText{fill:#86efac;font:700 13px system-ui,sans-serif}
            .rejectText{fill:#fca5a5;font:700 13px system-ui,sans-serif}.streamText{fill:#7dd3fc;font:700 13px system-ui,sans-serif}
        </style>""",
        '<rect width="1280" height="920" class="bg"/>',
        label(64, 52, "v0.18 · Signed control and key rotation", "title"),
        label(
            64,
            78,
            f"{check['assertions_passed']}/{check['assertions_total']} assertions · Ed25519 · {check['old_key_id']} → {check['new_key_id']}",
            "subtitle",
        ),
        label(64, 122, "What the signature binds", "heading"),
        '<rect x="64" y="142" width="252" height="118" rx="8" class="panel"/>',
        '<rect x="370" y="142" width="252" height="118" rx="8" class="panel"/>',
        '<rect x="676" y="142" width="252" height="118" rx="8" class="panel"/>',
        '<rect x="982" y="142" width="234" height="118" rx="8" class="panel"/>',
        label(88, 176, "CANONICAL PAYLOAD", "streamText"),
        label(88, 206, "cluster · revision · term", "label"),
        label(88, 232, "policy · ordered workers", "small"),
        label(394, 176, "SIGN", "oldText"),
        label(394, 206, "Ed25519 private seed", "label"),
        label(394, 232, "key ID is bound too", "small"),
        label(700, 176, "ENVELOPE", "oldText"),
        label(700, 206, "schema · algorithm · key ID", "label"),
        label(700, 232, "64-byte signature", "small"),
        label(1006, 176, "VERIFY", "newText"),
        label(1006, 206, "trusted public-key ring", "label"),
        label(1006, 232, "unknown/revoked → reject", "small"),
        label(343, 208, "→", "heading", "middle"),
        label(649, 208, "→", "heading", "middle"),
        label(955, 208, "→", "heading", "middle"),
        label(64, 306, "Runtime sequence", "heading"),
        '<line x1="112" y1="386" x2="1208" y2="386" class="axis"/>',
    ]

    for tick in range(6):
        x = x0 + tick / 5 * (x1 - x0)
        elapsed = span * tick / 5 / 1000
        items.append(f'<line x1="{x:.1f}" y1="372" x2="{x:.1f}" y2="400" class="grid"/>')
        items.append(label(x, 422, f"+{elapsed:.1f}s", "small", "middle"))

    rows = [346, 456, 318, 488, 374, 456, 318, 488, 346, 456, 318]
    anchors = ["end", "start", "start", "middle", "start", "middle", "middle", "end", "middle", "end", "end"]
    for (at_ms, text_value, css), row, preferred_anchor in zip(events, rows, anchors):
        x = scale(at_ms)
        marker = 386
        items.append(
            f'<line x1="{x:.1f}" y1="{marker}" x2="{x:.1f}" y2="{row + (-10 if row > marker else 8)}" class="grid"/>'
        )
        if css == "fault":
            items.append(f'<path d="M {x:.1f} 376 l 10 18 h -20 z" class="{css}"/>')
        elif css == "stream":
            items.append(f'<rect x="{x - 6:.1f}" y="380" width="12" height="12" class="{css}"/>')
        else:
            items.append(f'<circle cx="{x:.1f}" cy="386" r="7" class="{css}"/>')
        anchor = "end" if x > 1180 else preferred_anchor
        items.append(label(x, row, text_value, "label", anchor))

    stream_x0 = scale(stream["started_at_ms"])
    stream_x1 = scale(stream["observed_at_ms"])
    items.extend(
        [
            f'<line x1="{stream_x0:.1f}" y1="528" x2="{stream_x1:.1f}" y2="528" class="stream" stroke-width="7"/>',
            f'<circle cx="{stream_x0:.1f}" cy="528" r="6" class="stream"/>',
            f'<circle cx="{stream_x1:.1f}" cy="528" r="6" class="stream"/>',
            label(64, 532, "primary SSE", "label"),
            label(
                (stream_x0 + stream_x1) / 2,
                552,
                f"{stream['duration_ms']:.0f} ms · unknown key rejected · old-key ownership preserved",
                "small",
                "middle",
            ),
            label(64, 602, "Authentication outcomes", "heading"),
        ]
    )

    panels = [
        (64, 214, "UNKNOWN KEY", "rejectText", f"{check['signature_rejections_at_expiry']} rejected", "no publish or renewal"),
        (294, 214, "KEY DOWNGRADE", "rejectText", f"{check['key_downgrade_rejections']} rejected", "B cannot return to A"),
        (524, 214, "TAMPERED DISK", "rejectText", "verification failed", "changed worker detected"),
        (754, 214, "REVOKED A", "rejectText", "old disk refused", "deny wins over trust"),
        (984, 232, "TRUSTED B", "newText", "same r2 accepted", "new disk survives"),
    ]
    for x, width, heading, heading_css, line1, line2 in panels:
        items.append(f'<rect x="{x}" y="624" width="{width}" height="126" rx="8" class="panel"/>')
        items.append(label(x + 18, 656, heading, heading_css))
        items.append(label(x + 18, 692, line1, "label"))
        items.append(label(x + 18, 722, line2, "small"))

    items.extend(
        [
            label(64, 796, "Workers reached by rejected request", "heading"),
            label(410, 796, "cpu-primary", "label", "end"),
            '<rect x="430" y="780" width="240" height="20" rx="4" class="neutral"/>',
            label(686, 796, "0 attempts", "value"),
            label(930, 796, "cpu-rogue", "label", "end"),
            '<rect x="950" y="780" width="160" height="20" rx="4" class="neutral"/>',
            label(1126, 796, "0 attempts", "value"),
            label(
                64,
                850,
                f"Proof: same cluster and r{check['revision']} · unknown key · monotonic A→B · tamper · revocation · exact child PIDs",
                "subtitle",
            ),
            label(
                64,
                882,
                "Boundary: signatures authenticate route bytes to a provisioned public key; they do not provide secrecy or stop replay of still-eligible signed state.",
                "small",
            ),
            "</svg>",
        ]
    )
    args.output.write_text("\n".join(items) + "\n")


if __name__ == "__main__":
    main()
