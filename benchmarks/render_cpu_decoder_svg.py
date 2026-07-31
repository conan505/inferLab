#!/usr/bin/env python3
"""Render the retained v0.7 parity, latency, and streaming chart."""

from __future__ import annotations

import argparse
import html
import json
import math
from pathlib import Path

WIDTH = 1240
HEIGHT = 760


def text(x, y, value, css_class="label", anchor="start"):
    return (
        f'<text x="{x:.1f}" y="{y:.1f}" class="{css_class}" '
        f'text-anchor="{anchor}">{html.escape(str(value))}</text>'
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", type=Path, required=True)
    parser.add_argument("--parity", type=Path, nargs="+", required=True)
    parser.add_argument("--stream", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    check = json.loads(args.check.read_text())
    parities = [json.loads(path.read_text()) for path in args.parity]
    stream = json.loads(args.stream.read_text())

    elements = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" '
        f'height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}" '
        'role="img" aria-labelledby="title description">',
        '<title id="title">InferLab v0.7 CPU decoder proof</title>',
        '<desc id="description">Three PyTorch logit parity traces, C++ and '
        'PyTorch median generation latency, and the gateway SSE token timeline.'
        '</desc>',
        """<style>
        :root { color-scheme: light dark; }
        svg { --bg: #f8fafc; --fg: #172033; --muted: #596579;
              --grid: #d6dce5; --panel: #ffffff; --blue: #2563eb;
              --green: #16835b; --orange: #d97706; --red: #c2413b; }
        @media (prefers-color-scheme: dark) {
          svg { --bg: #111827; --fg: #eef2f7; --muted: #a9b4c4;
                --grid: #374151; --panel: #182235; --blue: #77a7ff;
                --green: #55d6a2; --orange: #ffb454; --red: #ff827b; }
        }
        .background { fill: var(--bg); }
        .panel { fill: var(--panel); stroke: var(--grid); stroke-width: 1; }
        .title { fill: var(--fg); font: 600 26px system-ui, sans-serif; }
        .subtitle { fill: var(--muted); font: 14px system-ui, sans-serif; }
        .heading { fill: var(--fg); font: 600 17px system-ui, sans-serif; }
        .label { fill: var(--fg); font: 13px system-ui, sans-serif; }
        .small { fill: var(--muted); font: 11px system-ui, sans-serif; }
        .value { fill: var(--fg); font: 600 13px ui-monospace, monospace; }
        .grid { stroke: var(--grid); stroke-width: 1; }
        .threshold { stroke: var(--red); stroke-width: 2; stroke-dasharray: 7 5; }
        .series-a { fill: none; stroke: var(--blue); stroke-width: 2; }
        .series-b { fill: none; stroke: var(--green); stroke-width: 2; }
        .series-c { fill: none; stroke: var(--orange); stroke-width: 2; }
        .dot-a { fill: var(--blue); }
        .dot-b { fill: var(--green); }
        .dot-c { fill: var(--orange); }
        .cpp { fill: var(--blue); }
        .torch { fill: var(--green); }
        .timeline { stroke: var(--grid); stroke-width: 3; }
        .token { fill: var(--blue); stroke: var(--panel); stroke-width: 2; }
        </style>""",
        f'<rect class="background" width="{WIDTH}" height="{HEIGHT}"/>',
        text(52, 48, "v0.7 · tiny C++ CPU decoder", "title"),
        text(
            52,
            74,
            (
                f"{check['assertions_passed']}/{check['assertions_total']} "
                "checks · full logits and greedy tokens compared independently"
            ),
            "subtitle",
        ),
        '<rect class="panel" x="42" y="100" width="730" height="360" rx="10"/>',
        '<rect class="panel" x="792" y="100" width="406" height="360" rx="10"/>',
        text(66, 132, "Absolute logit error by generation step", "heading"),
        text(66, 153, "Lower is better · red line is the 1e-4 acceptance limit", "small"),
    ]

    plot_left, plot_top, plot_width, plot_height = 92, 180, 650, 220
    for exponent in [-7, -6, -5, -4]:
        y = plot_top + (-4 - exponent) / 3 * plot_height
        elements.append(
            f'<line class="grid" x1="{plot_left}" y1="{y:.1f}" '
            f'x2="{plot_left + plot_width}" y2="{y:.1f}"/>'
        )
        elements.append(text(plot_left - 10, y + 4, f"1e{exponent}", "small", "end"))
    threshold_y = plot_top
    elements.append(
        f'<line class="threshold" x1="{plot_left}" y1="{threshold_y}" '
        f'x2="{plot_left + plot_width}" y2="{threshold_y}"/>'
    )
    colors = ["a", "b", "c"]
    for series_index, parity in enumerate(parities):
        values = [
            max(step["max_abs_logit_error"], 1.0e-7)
            for step in parity["per_step"]
        ]
        points = []
        for index, value in enumerate(values):
            x = plot_left + index / max(1, len(values) - 1) * plot_width
            normalized = (math.log10(value) + 7) / 3
            y = plot_top + plot_height - normalized * plot_height
            points.append((x, y))
        path = " ".join(
            ("M" if index == 0 else "L") + f" {x:.1f} {y:.1f}"
            for index, (x, y) in enumerate(points)
        )
        color = colors[series_index]
        elements.append(f'<path class="series-{color}" d="{path}"/>')
        for x, y in points:
            elements.append(
                f'<circle class="dot-{color}" cx="{x:.1f}" cy="{y:.1f}" r="4"/>'
            )
        elements.append(
            text(
                106 + series_index * 215,
                432,
                (
                    f"{parity['prompt']} · max "
                    f"{parity['max_abs_logit_error']:.2e}"
                ),
                "small",
            )
        )
    for index in range(8):
        x = plot_left + index / 7 * plot_width
        elements.append(text(x, 418, index + 1, "small", "middle"))

    elements.extend(
        [
            text(816, 132, "Median full generation latency", "heading"),
            text(816, 153, "Eight decoding steps · warm single-process runs", "small"),
        ]
    )
    cpp_latency = sum(
        parity["cpp_median_generation_us"] for parity in parities
    ) / len(parities)
    torch_latency = sum(
        parity["torch_median_generation_us"] for parity in parities
    ) / len(parities)
    maximum_latency = max(cpp_latency, torch_latency)
    bar_origin, bar_width = 1138, 260
    for row, (label, value, css) in enumerate(
        [
            ("C++ loops", cpp_latency, "cpp"),
            ("PyTorch oracle", torch_latency, "torch"),
        ]
    ):
        y = 215 + row * 105
        width = value / maximum_latency * bar_width
        elements.append(text(816, y, label, "label"))
        elements.append(
            f'<rect class="{css}" x="816" y="{y + 14}" '
            f'width="{width:.1f}" height="30" rx="4"/>'
        )
        elements.append(
            text(
                min(816 + width + 10, bar_origin),
                y + 35,
                f"{value:.1f} µs",
                "value",
            )
        )
    elements.append(
        text(
            816,
            418,
            "Educational micro-model: latency is not a production throughput claim.",
            "small",
        )
    )

    elements.extend(
        [
            '<rect class="panel" x="42" y="480" width="1156" height="230" rx="10"/>',
            text(66, 514, "Real SSE token timeline through the Rust gateway", "heading"),
            text(
                66,
                536,
                (
                    f"Injected 12 ms pacing · observed stream span "
                    f"{stream['stream_span_ms']:.3f} ms"
                ),
                "small",
            ),
        ]
    )
    content_events = stream["content_events"]
    start = content_events[0]["at_ms"]
    end = content_events[-1]["at_ms"]
    timeline_left, timeline_right, timeline_y = 94, 1148, 622
    elements.append(
        f'<line class="timeline" x1="{timeline_left}" y1="{timeline_y}" '
        f'x2="{timeline_right}" y2="{timeline_y}"/>'
    )
    for index, event in enumerate(content_events):
        fraction = (event["at_ms"] - start) / max(end - start, 0.001)
        x = timeline_left + fraction * (timeline_right - timeline_left)
        label_y = 582 if index % 2 == 0 else 676
        elements.append(
            f'<circle class="token" cx="{x:.1f}" cy="{timeline_y}" r="7"/>'
        )
        elements.append(text(x, label_y, event["content"], "label", "middle"))
        elements.append(
            text(
                x,
                label_y + (16 if index % 2 == 0 else -18),
                f"+{event['at_ms'] - start:.1f} ms",
                "small",
                "middle",
            )
        )
    elements.append(text(66, 696, "client receives", "small"))
    elements.append(
        text(
            1170,
            696,
            "[DONE]",
            "value",
            "end",
        )
    )
    elements.append("</svg>")
    args.output.write_text("\n".join(elements) + "\n")


if __name__ == "__main__":
    main()
