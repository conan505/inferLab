#!/usr/bin/env python3
"""Render the v0.0.6 admission and RSS time series as a dependency-free SVG."""

import argparse
import json


WIDTH = 1000
HEIGHT = 500
LEFT = 76
RIGHT = 24
PLOT_WIDTH = WIDTH - LEFT - RIGHT


def points(samples: list[dict], field: str, top: int, bottom: int, low: float, high: float):
    # Leave a small right margin after the final drained sample so the drop to zero is visible.
    max_elapsed = max(sample["elapsed_ms"] for sample in samples) * 1.05
    span = max(high - low, 1)
    return " ".join(
        f"{LEFT + sample['elapsed_ms'] / max_elapsed * PLOT_WIDTH:.1f},"
        f"{bottom - (sample[field] - low) / span * (bottom - top):.1f}"
        for sample in samples
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--analysis", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()

    with open(args.analysis, encoding="utf-8") as source:
        analysis = json.load(source)
    samples = analysis["resource_samples"]
    if len(samples) < 2:
        raise SystemExit("at least two resource samples are required")

    rss_values = [sample["rss_kib"] for sample in samples]
    rss_low = min(rss_values) - 128
    rss_high = max(rss_values) + 128
    elapsed_seconds = max(sample["elapsed_ms"] for sample in samples) / 1000
    admission = analysis["gateway_status_after"]["admission"]

    admission_top, admission_bottom = 74, 270
    rss_top, rss_bottom = 340, 460
    executing_points = points(
        samples, "executing", admission_top, admission_bottom, 0, 6
    )
    queued_points = points(samples, "queued", admission_top, admission_bottom, 0, 6)
    outstanding_points = points(
        samples, "outstanding", admission_top, admission_bottom, 0, 6
    )
    rss_points = points(
        samples, "rss_kib", rss_top, rss_bottom, rss_low, rss_high
    )

    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}">
  <rect width="100%" height="100%" fill="#0f172a"/>
  <style>
    text {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace; fill: #e2e8f0; }}
    .muted {{ fill: #94a3b8; font-size: 12px; }}
    .grid {{ stroke: #334155; stroke-width: 1; }}
    .limit {{ stroke: #64748b; stroke-width: 1; stroke-dasharray: 5 5; }}
  </style>
  <text x="{LEFT}" y="28" font-size="18" font-weight="700">InferLab v0.0.6 — 5× open-loop overload</text>
  <text x="{LEFT}" y="49" class="muted">40 req/s offered · 8 req/s estimated capacity · 2 executing · 4 queued</text>

  <line x1="{LEFT}" y1="{admission_bottom}" x2="{WIDTH - RIGHT}" y2="{admission_bottom}" class="grid"/>
  <line x1="{LEFT}" y1="{admission_top}" x2="{LEFT}" y2="{admission_bottom}" class="grid"/>
  <line x1="{LEFT}" y1="{admission_bottom - 2 / 6 * (admission_bottom - admission_top):.1f}" x2="{WIDTH - RIGHT}" y2="{admission_bottom - 2 / 6 * (admission_bottom - admission_top):.1f}" class="limit"/>
  <line x1="{LEFT}" y1="{admission_bottom - 4 / 6 * (admission_bottom - admission_top):.1f}" x2="{WIDTH - RIGHT}" y2="{admission_bottom - 4 / 6 * (admission_bottom - admission_top):.1f}" class="limit"/>
  <line x1="{LEFT}" y1="{admission_top}" x2="{WIDTH - RIGHT}" y2="{admission_top}" class="limit"/>
  <text x="25" y="{admission_top + 8}" class="muted">6</text>
  <text x="25" y="{admission_bottom + 4}" class="muted">0</text>
  <text x="{LEFT}" y="{admission_top - 10}" class="muted">request count over time</text>
  <polyline points="{outstanding_points}" fill="none" stroke="#c084fc" stroke-width="3"/>
  <polyline points="{queued_points}" fill="none" stroke="#fb923c" stroke-width="2"/>
  <polyline points="{executing_points}" fill="none" stroke="#38bdf8" stroke-width="2"/>
  <text x="650" y="92" fill="#38bdf8" font-size="12">executing ≤ {admission['worker_execution_capacity']}</text>
  <text x="650" y="110" fill="#fb923c" font-size="12">queued ≤ {admission['queue_capacity']}</text>
  <text x="650" y="128" fill="#c084fc" font-size="12">outstanding ≤ {admission['outstanding_capacity']}</text>

  <line x1="{LEFT}" y1="{rss_bottom}" x2="{WIDTH - RIGHT}" y2="{rss_bottom}" class="grid"/>
  <line x1="{LEFT}" y1="{rss_top}" x2="{LEFT}" y2="{rss_bottom}" class="grid"/>
  <text x="{LEFT}" y="{rss_top - 12}" class="muted">gateway RSS (KiB)</text>
  <text x="8" y="{rss_top + 5}" class="muted">{rss_high:.0f}</text>
  <text x="8" y="{rss_bottom + 4}" class="muted">{rss_low:.0f}</text>
  <polyline points="{rss_points}" fill="none" stroke="#4ade80" stroke-width="2"/>

  <text x="{LEFT}" y="486" class="muted">0 s</text>
  <text x="{WIDTH - RIGHT - 55}" y="486" class="muted">{elapsed_seconds:.2f} s</text>
</svg>
"""
    with open(args.output, "w", encoding="utf-8") as destination:
        destination.write(svg)


if __name__ == "__main__":
    main()
