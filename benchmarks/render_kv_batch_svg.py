#!/usr/bin/env python3
"""Render the retained v0.8 work, load, and backfill evidence."""

from __future__ import annotations

import argparse
import html
import json
from pathlib import Path

WIDTH = 1280
HEIGHT = 1160


def text(x, y, value, css_class="label", anchor="start"):
    return (
        f'<text x="{x:.1f}" y="{y:.1f}" class="{css_class}" '
        f'text-anchor="{anchor}">{html.escape(str(value))}</text>'
    )


def configuration(load: dict, name: str) -> dict:
    return next(item for item in load["configurations"] if item["name"] == name)


def line_chart(
    elements: list[str],
    levels: list[int],
    series: list[tuple[str, list[float], str]],
    left: float,
    top: float,
    width: float,
    height: float,
    title: str,
    unit: str,
) -> None:
    elements.append(text(left, top - 26, title, "heading"))
    maximum = max(value for _, values, _ in series for value in values) * 1.12
    for tick in range(5):
        value = maximum * tick / 4
        y = top + height - tick / 4 * height
        elements.append(
            f'<line class="grid" x1="{left}" y1="{y:.1f}" '
            f'x2="{left + width}" y2="{y:.1f}"/>'
        )
        elements.append(text(left - 10, y + 4, f"{value:.0f}", "small", "end"))
    for index, concurrency in enumerate(levels):
        x = left + index / max(1, len(levels) - 1) * width
        elements.append(text(x, top + height + 22, concurrency, "small", "middle"))
    elements.append(text(left + width / 2, top + height + 43, "concurrency", "small", "middle"))
    elements.append(text(left - 42, top - 8, unit, "small"))
    for series_index, (label, values, css_class) in enumerate(series):
        points = []
        for index, value in enumerate(values):
            x = left + index / max(1, len(values) - 1) * width
            y = top + height - value / maximum * height
            points.append((x, y, value))
        path = " ".join(
            ("M" if index == 0 else "L") + f" {x:.1f} {y:.1f}"
            for index, (x, y, _) in enumerate(points)
        )
        elements.append(f'<path class="{css_class}-line" d="{path}"/>')
        value_offset = -10 if series_index == 0 else 18
        for x, y, value in points:
            elements.append(
                f'<circle class="{css_class}-dot" cx="{x:.1f}" cy="{y:.1f}" r="5"/>'
            )
            elements.append(
                text(x, y + value_offset, f"{value:.1f}", "tiny", "middle")
            )
        end_x, end_y, _ = points[-1]
        elements.append(text(end_x - 5, end_y + 21, label, "small", "end"))


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", type=Path, required=True)
    parser.add_argument("--kv-parity", type=Path, required=True)
    parser.add_argument("--load", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    check = json.loads(args.check.read_text())
    parity = json.loads(args.kv_parity.read_text())
    load = json.loads(args.load.read_text())
    serial = configuration(load, "one-slot")
    continuous = configuration(load, "continuous-four-slot")
    levels = load["workload"]["concurrency_levels"]

    elements = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" '
        f'height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}" '
        'role="img" aria-labelledby="title description">',
        '<title id="title">InferLab v0.8 KV cache and continuous batching proof</title>',
        '<desc id="description">Deterministic decoder work before and after KV caching, request throughput and p95 latency across concurrency, and aligned sequence lanes showing continuous scheduler backfill.</desc>',
        """<style>
        :root { color-scheme: light dark; }
        svg { --bg: #f8fafc; --fg: #172033; --muted: #596579;
              --grid: #d6dce5; --panel: #ffffff; --blue: #2563eb;
              --green: #16835b; --orange: #d97706; --purple: #7c3aed; }
        @media (prefers-color-scheme: dark) {
          svg { --bg: #111827; --fg: #eef2f7; --muted: #a9b4c4;
                --grid: #374151; --panel: #182235; --blue: #77a7ff;
                --green: #55d6a2; --orange: #ffb454; --purple: #b794f6; }
        }
        .background { fill: var(--bg); }
        .panel { fill: var(--panel); stroke: var(--grid); stroke-width: 1; }
        .title { fill: var(--fg); font: 600 26px system-ui, sans-serif; }
        .subtitle { fill: var(--muted); font: 14px system-ui, sans-serif; }
        .heading { fill: var(--fg); font: 600 17px system-ui, sans-serif; }
        .label { fill: var(--fg); font: 13px system-ui, sans-serif; }
        .small { fill: var(--muted); font: 11px system-ui, sans-serif; }
        .tiny { fill: var(--muted); font: 10px system-ui, sans-serif; }
        .value { fill: var(--fg); font: 600 12px ui-monospace, monospace; }
        .grid { stroke: var(--grid); stroke-width: 1; }
        .baseline { fill: var(--orange); opacity: .78; }
        .cached { fill: var(--green); }
        .serial-line { fill: none; stroke: var(--orange); stroke-width: 2.5; }
        .continuous-line { fill: none; stroke: var(--blue); stroke-width: 2.5; }
        .serial-dot { fill: var(--orange); }
        .continuous-dot { fill: var(--blue); }
        .lane { stroke: var(--grid); stroke-width: 2; }
        .active { stroke: var(--blue); stroke-width: 6; stroke-linecap: round; opacity: .65; }
        .admit { fill: var(--green); stroke: var(--panel); stroke-width: 2; }
        .token { stroke: var(--purple); stroke-width: 2; }
        .complete { fill: var(--orange); stroke: var(--panel); stroke-width: 2; }
        </style>""",
        f'<rect class="background" width="{WIDTH}" height="{HEIGHT}"/>',
        text(52, 48, "v0.8 · KV cache and continuous batching", "title"),
        text(
            52,
            74,
            f"{check['assertions_passed']}/{check['assertions_total']} checks · exact token parity, deterministic work counters, and HTTP load",
            "subtitle",
        ),
        '<rect class="panel" x="42" y="98" width="1196" height="260" rx="10"/>',
        text(66, 130, "Decoder work for ‘teach me streaming’", "heading"),
        text(66, 151, "Each row is normalized to its recomputation baseline; labels show exact work units.", "small"),
    ]

    before = parity["recompute_metrics"]
    after = parity["cached_metrics"]
    work = [
        ("Query token projections", before["query_tokens"], after["query_tokens"]),
        ("K/V token projections", before["kv_tokens"], after["kv_tokens"]),
        (
            "Attention score elements",
            before["attention_score_elements"],
            after["attention_score_elements"],
        ),
    ]
    bar_left, bar_width = 310, 680
    for row, (label, baseline, cached) in enumerate(work):
        y = 190 + row * 54
        cached_width = bar_width * cached / baseline
        elements.append(text(72, y + 18, label, "label"))
        elements.append(
            f'<rect class="baseline" x="{bar_left}" y="{y}" width="{bar_width}" height="16" rx="3"/>'
        )
        elements.append(
            f'<rect class="cached" x="{bar_left}" y="{y + 20}" width="{cached_width:.1f}" height="16" rx="3"/>'
        )
        elements.append(text(bar_left + bar_width + 12, y + 13, f"recompute {baseline:,}", "value"))
        reduction = (baseline - cached) / baseline * 100
        elements.append(text(bar_left + cached_width + 8, y + 33, f"cache {cached:,} · −{reduction:.1f}%", "value"))

    elements.extend(
        [
            '<rect class="panel" x="42" y="378" width="1196" height="376" rx="10"/>',
            text(66, 411, "HTTP behavior as concurrency rises", "heading"),
            text(66, 432, "Mixed 2/4/6/8-token limits · 24 requests per point · 3 ms delay once per scheduler batch", "small"),
        ]
    )
    line_chart(
        elements,
        levels,
        [
            (
                "one slot",
                [level["request_throughput_per_second"] for level in serial["levels"]],
                "serial",
            ),
            (
                "four slots",
                [level["request_throughput_per_second"] for level in continuous["levels"]],
                "continuous",
            ),
        ],
        105,
        488,
        465,
        190,
        "Request throughput",
        "requests/s",
    )
    line_chart(
        elements,
        levels,
        [
            (
                "one slot",
                [level["latency_ms"]["p95"] for level in serial["levels"]],
                "serial",
            ),
            (
                "four slots",
                [level["latency_ms"]["p95"] for level in continuous["levels"]],
                "continuous",
            ),
        ],
        710,
        488,
        465,
        190,
        "End-to-end p95 latency",
        "milliseconds",
    )

    elements.extend(
        [
            '<rect class="panel" x="42" y="774" width="1196" height="338" rx="10"/>',
            text(66, 807, "Continuous backfill: aligned request lanes", "heading"),
            text(66, 828, "Circle = admitted · purple tick = one token step · diamond = completed; later circles appear before long lanes finish.", "small"),
        ]
    )
    trace = sorted(load["backfill"]["trace"], key=lambda event: event["sequence"])
    requests = load["backfill"]["requests"]
    request_by_id = {item["request_id"]: item for item in requests}
    admission_order = [
        event["request_id"] for event in trace if event["event"] == "admitted"
    ]
    unique_order = list(dict.fromkeys(admission_order))
    batches = [event["batch"] for event in trace]
    minimum_batch, maximum_batch = min(batches), max(batches)
    plot_left, plot_right = 245, 1182
    plot_top, lane_gap = 866, 28
    for batch in range(minimum_batch, maximum_batch + 1):
        fraction = (batch - minimum_batch) / max(1, maximum_batch - minimum_batch)
        x = plot_left + fraction * (plot_right - plot_left)
        elements.append(
            f'<line class="grid" x1="{x:.1f}" y1="850" x2="{x:.1f}" y2="1080"/>'
        )
        elements.append(text(x, 1097, batch - minimum_batch + 1, "small", "middle"))
    elements.append(text((plot_left + plot_right) / 2, 1107, "scheduler batch", "small", "middle"))

    for lane, request_id in enumerate(unique_order):
        y = plot_top + lane * lane_gap
        request = request_by_id[request_id]
        events = [event for event in trace if event["request_id"] == request_id]
        admitted = next(event for event in events if event["event"] == "admitted")
        completed = next(event for event in events if event["event"] == "completed")

        def x_for(batch):
            return plot_left + (batch - minimum_batch) / max(1, maximum_batch - minimum_batch) * (plot_right - plot_left)

        start_x, end_x = x_for(admitted["batch"]), x_for(completed["batch"])
        elements.append(text(70, y + 4, f"R{lane + 1}", "value"))
        elements.append(text(104, y + 4, f"limit {request['requested_max_tokens']}", "small"))
        elements.append(f'<line class="lane" x1="{plot_left}" y1="{y}" x2="{plot_right}" y2="{y}"/>')
        elements.append(f'<line class="active" x1="{start_x:.1f}" y1="{y}" x2="{end_x:.1f}" y2="{y}"/>')
        elements.append(f'<circle class="admit" cx="{start_x:.1f}" cy="{y}" r="6"/>')
        for event in events:
            if event["event"] == "token":
                x = x_for(event["batch"])
                elements.append(f'<line class="token" x1="{x:.1f}" y1="{y - 7}" x2="{x:.1f}" y2="{y + 7}"/>')
        elements.append(
            f'<rect class="complete" x="{end_x - 5:.1f}" y="{y - 5:.1f}" width="10" height="10" transform="rotate(45 {end_x:.1f} {y:.1f})"/>'
        )

    elements.append("</svg>")
    args.output.write_text("\n".join(elements) + "\n")


if __name__ == "__main__":
    main()
