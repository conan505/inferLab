#!/usr/bin/env python3
"""Render retained v0.12 causal-attention evidence as a data-driven SVG."""

from __future__ import annotations

import argparse
import html
import json
import math
from pathlib import Path

WIDTH = 1280
HEIGHT = 1120


def label(x, y, value, css="label", anchor="start") -> str:
    return (
        f'<text x="{x:.1f}" y="{y:.1f}" class="{css}" '
        f'text-anchor="{anchor}">{html.escape(str(value))}</text>'
    )


def line(points: list[tuple[float, float]], css: str) -> str:
    coordinates = " ".join(f"{x:.1f},{y:.1f}" for x, y in points)
    return f'<polyline class="series {css}" points="{coordinates}"/>'


def circle(x: float, y: float, css: str) -> str:
    return f'<circle class="point {css}" cx="{x:.1f}" cy="{y:.1f}" r="4.5"/>'


def bar(x: float, y: float, width: float, height: float, css: str) -> str:
    return (
        f'<rect class="{css}" x="{x:.1f}" y="{y:.1f}" '
        f'width="{max(width, 0):.1f}" height="{height:.1f}" rx="3"/>'
    )


def profile_map(observation: dict) -> dict:
    return {profile["algorithm"]: profile for profile in observation["profiles"]}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", type=Path, required=True)
    parser.add_argument("--probe", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    check = json.loads(args.check.read_text())
    probe = json.loads(args.probe.read_text())
    scaling = sorted(probe["sequence_scaling"], key=lambda item: item["tokens"])
    tokens = [item["tokens"] for item in scaling]
    profiles = {item["tokens"]: profile_map(item) for item in scaling}
    variants = {
        (item["algorithm"], item["precision"]): item
        for item in probe["fixture"]["variants"]
    }

    items = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}" role="img" aria-labelledby="title description">',
        '<title id="title">InferLab v0.12 tiled online-softmax attention proof</title>',
        '<desc id="description">Four charts compare score scratch, modeled external traffic, measured CPU wall time, and storage-precision drift for materialized and tiled online-softmax causal attention.</desc>',
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
        .axis{stroke:var(--grid);stroke-width:1} .guide{stroke:var(--grid);stroke-width:1}
        .series{fill:none;stroke-width:2.5;stroke-linejoin:round;stroke-linecap:round}
        .point{stroke:var(--panel);stroke-width:1.5}
        .blue-stroke{stroke:var(--blue)} .green-stroke{stroke:var(--green)}
        .blue-fill{fill:var(--blue)} .green-fill{fill:var(--green)}
        .orange-fill{fill:var(--orange)} .purple-fill{fill:var(--purple)}
        </style>""",
        f'<rect class="background" width="{WIDTH}" height="{HEIGHT}"/>',
        label(50, 46, "v0.12 · tiled online-softmax causal attention", "title"),
        label(
            50,
            72,
            f"{check['assertions_passed']}/{check['assertions_total']} checks · independent PyTorch oracle · FP32 accumulation",
            "subtitle",
        ),
    ]

    panels = [(42, 96), (650, 96), (42, 590), (650, 590)]
    for x, y in panels:
        items.append(f'<rect class="panel" x="{x}" y="{y}" width="588" height="456" rx="10"/>')

    # Panel 1: log2 score scratch.
    x0, y0, plot_w, plot_h = 105.0, 180.0, 465.0, 285.0
    items.extend(
        [
            label(66, 130, "Score scratch: quadratic matrix versus fixed tile", "heading"),
            label(66, 151, "Bytes, log₂ scale · tile = 32 scores", "small"),
        ]
    )
    low_log, high_log = 7.0, 20.0
    for exponent in [7, 11, 15, 19]:
        y = y0 + plot_h - (exponent - low_log) / (high_log - low_log) * plot_h
        items.append(f'<path class="guide" d="M {x0:.1f} {y:.1f} H {x0 + plot_w:.1f}"/>')
        value = {7: "128 B", 11: "2 KiB", 15: "32 KiB", 19: "512 KiB"}[exponent]
        items.append(label(x0 - 10, y + 4, value, "small", "end"))
    scratch_points = {"materialized": [], "online-tiled": []}
    for index, token_count in enumerate(tokens):
        x = x0 + index / (len(tokens) - 1) * plot_w
        items.append(label(x, y0 + plot_h + 24, token_count, "small", "middle"))
        for algorithm in scratch_points:
            value = profiles[token_count][algorithm]["stats"]["score_buffer_bytes"]
            y = y0 + plot_h - (math.log2(value) - low_log) / (high_log - low_log) * plot_h
            scratch_points[algorithm].append((x, y))
    items.append(line(scratch_points["materialized"], "blue-stroke"))
    items.append(line(scratch_points["online-tiled"], "green-stroke"))
    for point in scratch_points["materialized"]:
        items.append(circle(*point, "blue-fill"))
    for point in scratch_points["online-tiled"]:
        items.append(circle(*point, "green-fill"))
    items.append(label(x0, y0 - 9, "materialized", "label"))
    items.append(label(x0 + 116, y0 - 9, "online tiled", "label"))
    items.append(label(x0 + plot_w / 2, y0 + plot_h + 46, "sequence tokens", "small", "middle"))
    items.append(
        label(
            66,
            526,
            f"At 256 tokens: {check['score_scratch_reduction_at_256x']:,.0f}× less score scratch (1 MiB → 128 B).",
            "value",
        )
    )

    # Panel 2: modeled traffic.
    x0, y0, plot_w, plot_h = 714.0, 180.0, 465.0, 285.0
    items.extend(
        [
            label(674, 130, "Modeled external traffic", "heading"),
            label(674, 151, "MiB moved by the algorithmic byte model, not hardware counters", "small"),
        ]
    )
    max_traffic = max(
        profiles[token_count][algorithm]["stats"]["modeled_external_total_bytes"]
        for token_count in tokens
        for algorithm in ["materialized", "online-tiled"]
    ) / (1024 * 1024)
    traffic_ceiling = math.ceil(max_traffic)
    for tick in range(traffic_ceiling + 1):
        y = y0 + plot_h - tick / traffic_ceiling * plot_h
        items.append(f'<path class="guide" d="M {x0:.1f} {y:.1f} H {x0 + plot_w:.1f}"/>')
        items.append(label(x0 - 10, y + 4, tick, "small", "end"))
    traffic_points = {"materialized": [], "online-tiled": []}
    for index, token_count in enumerate(tokens):
        x = x0 + index / (len(tokens) - 1) * plot_w
        items.append(label(x, y0 + plot_h + 24, token_count, "small", "middle"))
        for algorithm in traffic_points:
            value = profiles[token_count][algorithm]["stats"]["modeled_external_total_bytes"] / (1024 * 1024)
            y = y0 + plot_h - value / traffic_ceiling * plot_h
            traffic_points[algorithm].append((x, y))
    items.append(line(traffic_points["materialized"], "blue-stroke"))
    items.append(line(traffic_points["online-tiled"], "green-stroke"))
    for point in traffic_points["materialized"]:
        items.append(circle(*point, "blue-fill"))
    for point in traffic_points["online-tiled"]:
        items.append(circle(*point, "green-fill"))
    items.append(label(x0, y0 - 9, "materialized", "label"))
    items.append(label(x0 + 116, y0 - 9, "online tiled", "label"))
    items.append(label(x0 + plot_w / 2, y0 + plot_h + 46, "sequence tokens", "small", "middle"))
    items.append(
        label(
            674,
            526,
            f"At 256 tokens: {check['modeled_traffic_reduction_at_256x']:.1f}× less modeled traffic (4.5 → 2.25 MiB).",
            "value",
        )
    )

    # Panel 3: measured median wall time.
    x0, y0, plot_w, plot_h = 105.0, 674.0, 465.0, 285.0
    items.extend(
        [
            label(66, 624, "Measured scalar CPU wall time", "heading"),
            label(66, 645, "Median microseconds · observational, host-specific", "small"),
        ]
    )
    max_latency = max(
        profiles[token_count][algorithm]["median_us"]
        for token_count in tokens
        for algorithm in ["materialized", "online-tiled"]
    )
    latency_ceiling = math.ceil(max_latency / 1000) * 1000
    for tick in range(5):
        value = latency_ceiling * tick / 4
        y = y0 + plot_h - tick / 4 * plot_h
        items.append(f'<path class="guide" d="M {x0:.1f} {y:.1f} H {x0 + plot_w:.1f}"/>')
        items.append(label(x0 - 10, y + 4, f"{value:,.0f}", "small", "end"))
    latency_points = {"materialized": [], "online-tiled": []}
    for index, token_count in enumerate(tokens):
        x = x0 + index / (len(tokens) - 1) * plot_w
        items.append(label(x, y0 + plot_h + 24, token_count, "small", "middle"))
        for algorithm in latency_points:
            value = profiles[token_count][algorithm]["median_us"]
            y = y0 + plot_h - value / latency_ceiling * plot_h
            latency_points[algorithm].append((x, y))
    items.append(line(latency_points["materialized"], "blue-stroke"))
    items.append(line(latency_points["online-tiled"], "green-stroke"))
    for point in latency_points["materialized"]:
        items.append(circle(*point, "blue-fill"))
    for point in latency_points["online-tiled"]:
        items.append(circle(*point, "green-fill"))
    items.append(label(x0, y0 - 9, "materialized", "label"))
    items.append(label(x0 + 116, y0 - 9, "online tiled", "label"))
    items.append(label(x0 + plot_w / 2, y0 + plot_h + 46, "sequence tokens", "small", "middle"))
    items.append(
        label(
            66,
            1020,
            f"Observed 256-token speedup: {check['observed_wall_time_speedup_at_256x']:.2f}×; no GPU claim is made.",
            "value",
        )
    )

    # Panel 4: precision drift.
    items.extend(
        [
            label(674, 624, "Storage precision drift from FP32", "heading"),
            label(674, 645, "Maximum absolute fixture output error · FP32 accumulation", "small"),
        ]
    )
    drift_values = [
        ("FP32", variants[("online-tiled", "fp32")]["maximum_absolute_error_to_materialized_fp32"], "blue-fill"),
        ("FP16", variants[("online-tiled", "fp16")]["maximum_absolute_error_to_materialized_fp32"], "orange-fill"),
        ("BF16", variants[("online-tiled", "bf16")]["maximum_absolute_error_to_materialized_fp32"], "purple-fill"),
    ]
    max_drift = max(value for _, value, _ in drift_values) * 1.12
    chart_x, chart_y, chart_w = 736.0, 704.0, 420.0
    for index, (name, value, css) in enumerate(drift_values):
        y = chart_y + index * 76
        items.append(label(704, y + 20, name, "label"))
        width = value / max_drift * chart_w if max_drift else 0
        items.append(bar(chart_x, y, width, 30, css))
        items.append(label(chart_x + width + 8, y + 20, f"{value:.7f}", "value"))
    items.append(label(674, 956, f"Maximum C++ ↔ PyTorch oracle error: {check['maximum_oracle_error']:.2e}", "value"))
    items.append(label(674, 978, f"Maximum online ↔ materialized error: {check['maximum_algorithm_error']:.2e}", "value"))
    items.append(label(674, 1000, "FP16/BF16 are storage simulations here, not accelerator throughput modes.", "small"))
    items.append(label(50, 1090, "Boundary: exact causal algorithm and byte model on CPU now; CUDA kernel, shared-memory tiling, and profiler counters remain v1.0.", "subtitle"))
    items.append("</svg>")
    args.output.write_text("\n".join(items) + "\n")


if __name__ == "__main__":
    main()
