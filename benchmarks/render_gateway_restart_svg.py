#!/usr/bin/env python3
"""Render retained v0.14 gateway restart/reconciliation evidence."""

from __future__ import annotations

import argparse
import html
import json
from pathlib import Path

WIDTH = 1280
HEIGHT = 1110


def load(directory: Path, name: str) -> dict:
    return json.loads((directory / name).read_text())


def label(x, y, value, css="label", anchor="start") -> str:
    return (
        f'<text x="{x:.1f}" y="{y:.1f}" class="{css}" '
        f'text-anchor="{anchor}">{html.escape(str(value))}</text>'
    )


def bar(x, y, width, height, css) -> str:
    return (
        f'<rect class="{css}" x="{x:.1f}" y="{y:.1f}" '
        f'width="{max(width, 0):.1f}" height="{height:.1f}" rx="3"/>'
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

    live = load(evidence, "gateway-live.json")
    offline = load(evidence, "gateway-offline.json")
    reconciled = load(evidence, "gateway-reconciled.json")
    stale = load(evidence, "gateway-stale-control.json")
    live_requests = load(evidence, "requests-live.json")
    offline_requests = load(evidence, "requests-offline.json")
    weighted_requests = load(evidence, "requests-weighted.json")
    stream = load(evidence, "stream-final.json")

    initial_revision = check["initial_revision"]
    updated_revision = check["updated_revision"]
    stale_revision = check["stale_control_revision"]

    items = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}" role="img" aria-labelledby="title description">',
        '<title id="title">InferLab v0.14 restart-safe gateway routing proof</title>',
        '<desc id="description">Four-phase sequence showing a live control-plane bootstrap, disk bootstrap during complete outage, monotonic reconciliation to a newer revision, and rejection of a stale control-plane rollback. Supporting panels compare boot latency, revision identity, request continuity, and weighted routing.</desc>',
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
        .axis{stroke:var(--grid);stroke-width:1} .arrow{stroke:var(--grid);stroke-width:2;fill:none}
        .blue{fill:var(--blue)} .green{fill:var(--green)} .orange{fill:var(--orange)}
        .purple{fill:var(--purple)} .red{fill:var(--red)}
        .blue-stroke{stroke:var(--blue);fill:none;stroke-width:2.5}
        .purple-stroke{stroke:var(--purple);fill:none;stroke-width:2.5}
        .orange-stroke{stroke:var(--orange);fill:none;stroke-width:2.5;stroke-dasharray:5 4}
        </style>""",
        '<defs><marker id="arrowhead" markerWidth="8" markerHeight="8" refX="7" refY="3" orient="auto"><path d="M0,0 L0,6 L8,3 z" class="grid-fill" style="fill:var(--grid)"/></marker></defs>',
        f'<rect class="background" width="{WIDTH}" height="{HEIGHT}"/>',
        label(50, 46, "v0.14 · restart-safe routing snapshots", "title"),
        label(
            50,
            72,
            f"{check['assertions_passed']}/{check['assertions_total']} checks · durable apply-before-publish + offline bootstrap + monotonic reconciliation",
            "subtitle",
        ),
        panel(42, 96, 1196, 325),
        label(66, 130, "One recovery sequence: memory may restart, committed routing identity does not", "heading"),
        label(66, 151, "Each column is a phase; arrows show the configuration source used by the gateway.", "small"),
    ]

    phase_x = [190, 470, 750, 1030]
    phase_titles = [
        "1 · live bootstrap",
        "2 · full outage",
        "3 · reconciliation",
        "4 · rollback guard",
    ]
    phase_subtitles = [
        f"Raft r{initial_revision} reachable",
        "all 3 Raft nodes stopped",
        f"Raft commits r{updated_revision}",
        f"live control stuck at r{stale_revision}",
    ]
    for x, title_text, subtitle in zip(phase_x, phase_titles, phase_subtitles):
        items.append(label(x, 185, title_text, "value", "middle"))
        items.append(label(x, 203, subtitle, "small", "middle"))
    for x in [330, 610, 890]:
        items.append(
            f'<path class="arrow" marker-end="url(#arrowhead)" d="M {x-22} 286 H {x+22}"/>'
        )

    sequence_nodes = [
        (
            phase_x[0],
            [("control", f"control r{initial_revision}", "blue"), ("disk", f"save r{initial_revision}", "purple"), ("gateway", "serve 2/2", "green")],
        ),
        (
            phase_x[1],
            [("control", "control unavailable", "red"), ("disk", f"load r{initial_revision}", "purple"), ("gateway", "serve 4/4", "green")],
        ),
        (
            phase_x[2],
            [("control", f"control r{updated_revision}", "blue"), ("disk", f"replace with r{updated_revision}", "purple"), ("gateway", "serve 8/8", "green")],
        ),
        (
            phase_x[3],
            [("control", f"reject r{stale_revision}", "orange"), ("disk", f"retain r{updated_revision}", "purple"), ("gateway", "SSE DONE", "green")],
        ),
    ]
    row_y = {"control": 235, "disk": 291, "gateway": 347}
    for x, nodes in sequence_nodes:
        for row, text_value, css in nodes:
            y = row_y[row]
            items.append(f'<circle class="{css}" cx="{x}" cy="{y}" r="6"/>')
            items.append(label(x, y + 24, text_value, "small", "middle"))
        items.append(f'<path class="arrow" marker-end="url(#arrowhead)" d="M {x} 244 V 279"/>')
        items.append(f'<path class="arrow" marker-end="url(#arrowhead)" d="M {x} 300 V 335"/>')
    items.extend(
        [
            label(76, 239, "control", "small"),
            label(76, 295, "durable file", "small"),
            label(76, 351, "gateway + requests", "small"),
        ]
    )

    items.extend(
        [
            panel(42, 441, 575, 282),
            label(66, 475, "Observed gateway boot latency", "heading"),
            label(66, 496, "Process start to matching /internal/workers state (milliseconds)", "small"),
        ]
    )
    boot_rows = [
        ("live control", live["boot_latency_ms"], "blue"),
        ("control offline", offline["boot_latency_ms"], "purple"),
        ("stale guard", stale["boot_latency_ms"], "orange"),
    ]
    max_boot = max(value for _, value, _ in boot_rows)
    for index, (name, value, css) in enumerate(boot_rows):
        y = 530 + index * 55
        items.append(label(66, y + 18, name, "small"))
        width = value / max_boot * 310
        items.append(bar(180, y, width, 24, css))
        items.append(label(190 + width, y + 17, f"{value:.1f} ms", "value"))
    items.append(label(66, 700, "Offline boot intentionally includes the configured 150 ms live-control wait.", "small"))

    items.extend(
        [
            panel(637, 441, 601, 282),
            label(661, 475, "Revision identity by recovery phase", "heading"),
            label(661, 496, "Control observation may be absent or stale; disk and gateway never regress.", "small"),
        ]
    )
    phases = ["live", "offline", "reconciled", "stale"]
    control_values = [initial_revision, None, updated_revision, stale_revision]
    disk_values = [initial_revision, initial_revision, updated_revision, updated_revision]
    gateway_values = [initial_revision, initial_revision, updated_revision, updated_revision]
    min_revision = min(initial_revision, stale_revision)
    max_revision = max(updated_revision, initial_revision)
    revision_span = max(max_revision - min_revision, 1)
    plot_x = [700, 855, 1010, 1165]

    def revision_y(value: int) -> float:
        return 655 - (value - min_revision) / revision_span * 105

    for x, phase in zip(plot_x, phases):
        items.append(label(x, 705, phase, "small", "middle"))
    for values, css, offset, name, label_offset in [
        (control_values, "blue", -10, "control", -14),
        (disk_values, "purple", 0, "disk", 20),
        (gateway_values, "orange", 10, "gateway", -30),
    ]:
        previous = None
        for x, value in zip(plot_x, values):
            if value is None:
                previous = None
                continue
            px = x + offset
            py = revision_y(value)
            items.append(f'<circle class="{css}" cx="{px:.1f}" cy="{py:.1f}" r="5"/>')
            items.append(label(px, py + label_offset, f"r{value}", "value", "middle"))
            if previous is not None:
                items.append(
                    f'<path class="{css}-stroke" d="M {previous[0]:.1f} {previous[1]:.1f} L {px:.1f} {py:.1f}"/>'
                )
            previous = (px, py)
        legend_x = {"control": 700, "disk": 820, "gateway": 910}[name]
        items.append(f'<circle class="{css}" cx="{legend_x}" cy="518" r="4"/>')
        items.append(label(legend_x + 10, 522, name, "small"))

    items.extend(
        [
            panel(42, 743, 1196, 315),
            label(66, 777, "Real-model continuity and final weighted behavior", "heading"),
            label(66, 798, "All request counts are successful / attempted; final stream is a separate SSE completion.", "small"),
        ]
    )
    request_rows = [
        ("live boot", live_requests["succeeded"], live_requests["requested"]),
        ("control outage", offline_requests["succeeded"], offline_requests["requested"]),
        ("after reconcile", weighted_requests["succeeded"], weighted_requests["requested"]),
        ("final SSE", int(stream["status"] == 200 and stream["done_received"]), 1),
    ]
    max_requests = max(total for _, _, total in request_rows)
    for index, (name, succeeded, total) in enumerate(request_rows):
        y = 832 + index * 47
        items.append(label(76, y + 18, name, "small"))
        width = total / max_requests * 390
        items.append(bar(190, y, width, 25, "green"))
        items.append(label(200 + width, y + 18, f"{succeeded}/{total}", "value"))

    counts = sorted(
        weighted_requests["worker_counts"].items(),
        key=lambda item: item[1],
        reverse=True,
    )
    max_count = max(count for _, count in counts)
    items.append(label(700, 835, "3:1 routing after durable reconciliation", "label"))
    for index, (worker, count) in enumerate(counts):
        y = 865 + index * 58
        items.append(label(700, y + 21, worker, "small"))
        width = count / max_count * 260
        items.append(bar(810, y, width, 30, "purple" if index == 0 else "orange"))
        items.append(label(820 + width, y + 21, f"{count} requests", "value"))
    items.append(
        label(
            66,
            1030,
            f"Boundary: disk is a restart cache of committed routing state—not a new consensus authority. Final SSE revision r{stream['config_revision']} · DONE.",
            "value",
        )
    )
    items.append(label(50, 1089, "The chart reports one loopback proof run; boot times are observations, not service-level objectives.", "subtitle"))
    items.append("</svg>")
    args.output.write_text("\n".join(items) + "\n")


if __name__ == "__main__":
    main()
