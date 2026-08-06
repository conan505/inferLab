#!/usr/bin/env python3
"""Render retained v0.15 bounded-age routing snapshot evidence."""

from __future__ import annotations

import argparse
import html
import json
from pathlib import Path

WIDTH = 1280
HEIGHT = 1020


def load(directory: Path, name: str) -> dict:
    return json.loads((directory / name).read_text())


def label(x, y, value, css="label", anchor="start") -> str:
    return (
        f'<text x="{x:.1f}" y="{y:.1f}" class="{css}" '
        f'text-anchor="{anchor}">{html.escape(str(value))}</text>'
    )


def panel(x, y, width, height) -> str:
    return (
        f'<rect class="panel" x="{x}" y="{y}" width="{width}" '
        f'height="{height}" rx="10"/>'
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--check", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    evidence = args.evidence_dir
    check = json.loads(args.check.read_text())

    fresh_gateway = load(evidence, "gateway-fresh-disk.json")
    expired = load(evidence, "expired-fixture.json")
    future = load(evidence, "future-fixture.json")
    requests_live = load(evidence, "requests-live.json")
    requests_fresh = load(evidence, "requests-fresh-disk.json")
    requests_repair = load(evidence, "requests-live-repair.json")
    stream = load(evidence, "stream-final.json")

    maximum_age_ms = check["maximum_age_ms"]
    maximum_future_skew_ms = check["maximum_future_skew_ms"]
    fresh_age_ms = check["fresh_disk_age_ms"]
    expired_age_ms = expired["observed_age_ms"]
    future_delta_ms = future["future_delta_ms"]

    items = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}" role="img" aria-labelledby="title description">',
        '<title id="title">InferLab v0.15 bounded-age routing snapshot proof</title>',
        '<desc id="description">A five-stage startup sequence, timestamp eligibility window, and real-model continuity results. Fresh disk state is accepted during control outage, expired and excessively future-dated state fail closed, and live control repairs the durable file.</desc>',
        """<style>
        :root { color-scheme: light dark; }
        svg { --bg:#f8fafc; --fg:#172033; --muted:#596579; --grid:#d6dce5;
              --panel:#ffffff; --blue:#2563eb; --green:#16835b; --orange:#d97706;
              --purple:#7c3aed; --red:#c2413b; }
        @media (prefers-color-scheme: dark) {
          svg { --bg:#111827; --fg:#eef2f7; --muted:#a9b4c4; --grid:#374151;
                --panel:#182235; --blue:#77a7ff; --green:#55d6a2; --orange:#ffb454;
                --purple:#b794f6; --red:#ff827b; }
        }
        .background{fill:var(--bg)} .panel{fill:var(--panel);stroke:var(--grid);stroke-width:1}
        .title{fill:var(--fg);font:600 26px system-ui,sans-serif}
        .subtitle{fill:var(--muted);font:14px system-ui,sans-serif}
        .heading{fill:var(--fg);font:600 17px system-ui,sans-serif}
        .label{fill:var(--fg);font:13px system-ui,sans-serif}
        .small{fill:var(--muted);font:11px system-ui,sans-serif}
        .value{fill:var(--fg);font:600 12px ui-monospace,monospace}
        .axis{stroke:var(--grid);stroke-width:2} .arrow{stroke:var(--grid);stroke-width:2;fill:none}
        .blue{fill:var(--blue)} .green{fill:var(--green)} .orange{fill:var(--orange)}
        .purple{fill:var(--purple)} .red{fill:var(--red)}
        .eligible{fill:var(--green);fill-opacity:.14;stroke:var(--green);stroke-width:1}
        </style>""",
        '<defs><marker id="arrowhead" markerWidth="8" markerHeight="8" refX="7" refY="3" orient="auto"><path d="M0,0 L0,6 L8,3 z" style="fill:var(--grid)"/></marker></defs>',
        f'<rect class="background" width="{WIDTH}" height="{HEIGHT}"/>',
        label(50, 46, "v0.15 · bounded-age routing fallback", "title"),
        label(
            50,
            72,
            f"{check['assertions_passed']}/{check['assertions_total']} checks · freshness window + clock-skew guard + live repair",
            "subtitle",
        ),
        panel(42, 96, 1196, 310),
        label(66, 130, "One startup rule, five observable outcomes", "heading"),
        label(66, 151, "Arrows show time and restart order; the gateway serves only when one identity source is eligible.", "small"),
    ]

    phase_x = [145, 380, 615, 850, 1085]
    phase_titles = [
        "1 · live save",
        "2 · fresh disk",
        "3 · expired disk",
        "4 · future clock",
        "5 · live repair",
    ]
    phase_subtitles = [
        f"persist r{check['revision']}",
        f"age {fresh_age_ms} ms",
        f"age {expired_age_ms} ms",
        f"ahead {future_delta_ms} ms",
        "overwrite timestamp",
    ]
    phase_outcomes = [
        ("SERVE 2/2", "green"),
        ("SERVE 3/3", "green"),
        ("FAIL CLOSED", "red"),
        ("FAIL CLOSED", "red"),
        ("SERVE 2/2 + SSE", "green"),
    ]
    phase_sources = [
        ("live control", "blue"),
        ("durable file", "purple"),
        ("durable file", "purple"),
        ("durable file", "purple"),
        ("live control", "blue"),
    ]
    for index, x in enumerate(phase_x):
        items.append(label(x, 193, phase_titles[index], "value", "middle"))
        items.append(label(x, 214, phase_subtitles[index], "small", "middle"))
        source_text, source_css = phase_sources[index]
        items.append(f'<circle class="{source_css}" cx="{x}" cy="252" r="7"/>')
        items.append(label(x, 277, source_text, "small", "middle"))
        outcome_text, outcome_css = phase_outcomes[index]
        items.append(f'<rect class="{outcome_css}" x="{x - 49}" y="315" width="98" height="28" rx="4"/>')
        items.append(label(x, 334, outcome_text, "value", "middle"))
        if index < len(phase_x) - 1:
            items.append(
                f'<path class="arrow" marker-end="url(#arrowhead)" d="M {x + 67} 285 H {phase_x[index + 1] - 67}"/>'
            )
    items.append(label(66, 380, "Safety boundary: a syntactically valid file can still be temporally ineligible.", "small"))

    items.extend(
        [
            panel(42, 426, 1196, 300),
            label(66, 460, "The eligibility window around ‘now’", "heading"),
            label(
                66,
                481,
                "Signed timestamp offset: past snapshots are negative; future-dated snapshots are positive.",
                "small",
            ),
        ]
    )
    domain_min = -max(expired_age_ms, maximum_age_ms) * 1.12
    domain_max = max(future_delta_ms, maximum_future_skew_ms) * 1.12
    axis_left = 100.0
    axis_right = 1180.0

    def scale(value: float) -> float:
        return axis_left + (value - domain_min) / (domain_max - domain_min) * (
            axis_right - axis_left
        )

    eligible_left = scale(-maximum_age_ms)
    eligible_right = scale(maximum_future_skew_ms)
    items.append(
        f'<rect class="eligible" x="{eligible_left:.1f}" y="510" width="{eligible_right - eligible_left:.1f}" height="150" rx="5"/>'
    )
    items.append(label((eligible_left + eligible_right) / 2, 532, "eligible fallback window", "small", "middle"))
    items.append(f'<path class="axis" d="M {axis_left:.1f} 670 H {axis_right:.1f}"/>')
    for value, text_value, text_y, anchor, offset in [
        (-maximum_age_ms, f"−{maximum_age_ms} ms age limit", 697, "middle", 0),
        (0, "now", 697, "end", -7),
        (
            maximum_future_skew_ms,
            f"+{maximum_future_skew_ms} ms skew limit",
            715,
            "start",
            7,
        ),
    ]:
        x = scale(value)
        items.append(f'<path class="axis" d="M {x:.1f} 663 V 677"/>')
        items.append(label(x + offset, text_y, text_value, "small", anchor))

    observations = [
        (-fresh_age_ms, 565, f"fresh disk: −{fresh_age_ms} ms · accept", "green"),
        (-expired_age_ms, 605, f"expired disk: −{expired_age_ms} ms · reject", "red"),
        (future_delta_ms, 645, f"future disk: +{future_delta_ms} ms · reject", "orange"),
    ]
    for value, y, text_value, css in observations:
        x = scale(value)
        items.append(f'<circle class="{css}" cx="{x:.1f}" cy="{y}" r="7"/>')
        anchor = "end" if x > 930 else "start"
        offset = -12 if anchor == "end" else 12
        items.append(label(x + offset, y + 4, text_value, "value", anchor))

    items.extend(
        [
            panel(42, 746, 1196, 218),
            label(66, 780, "Real-model continuity where startup is permitted", "heading"),
            label(66, 801, "Rejected phases start no listener and therefore intentionally serve zero traffic.", "small"),
        ]
    )
    request_rows = [
        ("live save", requests_live["succeeded"], requests_live["requested"]),
        ("fresh disk / control down", requests_fresh["succeeded"], requests_fresh["requested"]),
        ("live repair", requests_repair["succeeded"], requests_repair["requested"]),
        ("final SSE", int(stream["status"] == 200 and stream["done_received"]), 1),
    ]
    max_requests = max(total for _, _, total in request_rows)
    for index, (name, succeeded, total) in enumerate(request_rows):
        x = 75 + index * 290
        width = total / max_requests * 180
        items.append(label(x, 835, name, "small"))
        items.append(f'<rect class="green" x="{x}" y="852" width="{width:.1f}" height="28" rx="4"/>')
        items.append(label(x + width + 12, 871, f"{succeeded}/{total}", "value"))
    items.append(
        label(
            66,
            930,
            f"Boundary: maximum age {maximum_age_ms} ms · future skew {maximum_future_skew_ms} ms · final SSE r{stream['config_revision']} · DONE.",
            "value",
        )
    )
    items.append(label(50, 995, "One loopback run; timing values are observations, not service-level objectives.", "subtitle"))
    items.append("</svg>")
    args.output.write_text("\n".join(items) + "\n")


if __name__ == "__main__":
    main()
