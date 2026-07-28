#!/usr/bin/env python3
"""Render synchronized and full-jitter retry timelines as an SVG."""

import argparse
import json


WIDTH = 1100
HEIGHT = 430
LEFT = 74
RIGHT = 48
TOP = 82
BOTTOM = 360


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--analysis", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()

    with open(args.analysis, encoding="utf-8") as source:
        analysis = json.load(source)
    synchronized = {
        item["time_ms"]: item["retries"]
        for item in analysis["synchronized_backoff"]["timeline"]
    }
    jittered = {
        item["time_ms"]: item["retries"]
        for item in analysis["full_jitter"]["timeline"]
    }
    bucket_ms = analysis["config"]["bucket_ms"]
    max_event_time = max(max(synchronized), max(jittered))
    # Leave one empty bucket after the final event so the last spike visibly
    # returns to zero instead of ending against the SVG boundary.
    max_time = max_event_time + bucket_ms
    max_retries = max(max(synchronized.values()), max(jittered.values()))
    times = range(0, max_time + bucket_ms, bucket_ms)

    def x(time_ms: int) -> float:
        return LEFT + time_ms / max_time * (WIDTH - LEFT - RIGHT)

    def y(retries: int) -> float:
        return BOTTOM - retries / max_retries * (BOTTOM - TOP)

    synchronized_points = " ".join(
        f"{x(time_ms):.1f},{y(synchronized.get(time_ms, 0)):.1f}"
        for time_ms in times
    )
    jitter_points = " ".join(
        f"{x(time_ms):.1f},{y(jittered.get(time_ms, 0)):.1f}"
        for time_ms in times
    )

    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}">
  <rect width="100%" height="100%" fill="#0f172a"/>
  <style>
    text {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace; fill: #e2e8f0; }}
    .muted {{ fill: #94a3b8; font-size: 12px; }}
    .grid {{ stroke: #334155; stroke-width: 1; }}
    .synchronized {{ fill: #fb7185; }}
    .jittered {{ fill: #38bdf8; }}
  </style>
  <text x="{LEFT}" y="28" font-size="18" font-weight="700">InferLab v0.0.7 — retry synchronization</text>
  <text x="{LEFT}" y="49" class="muted">1,000 clients · 3 retries · 25 ms buckets · exponential cap 100/200/400 ms</text>
  <line x1="{LEFT}" y1="{BOTTOM}" x2="{WIDTH - RIGHT}" y2="{BOTTOM}" class="grid"/>
  <line x1="{LEFT}" y1="{TOP}" x2="{LEFT}" y2="{BOTTOM}" class="grid"/>
  <text x="12" y="{TOP + 5}" class="muted">{max_retries}</text>
  <text x="38" y="{BOTTOM + 4}" class="muted">0</text>
  <text x="{LEFT}" y="{BOTTOM + 28}" class="muted">0 ms</text>
  <text x="{WIDTH - RIGHT - 65}" y="{BOTTOM + 28}" class="muted">{max_time} ms</text>
  <polyline points="{synchronized_points}" fill="none" stroke="#fb7185" stroke-width="3"/>
  <polyline points="{jitter_points}" fill="none" stroke="#38bdf8" stroke-width="3"/>
  <text x="720" y="96" class="synchronized" font-size="13">no jitter peak: {analysis['synchronized_backoff']['peak_retries_in_one_bucket']}</text>
  <text x="720" y="116" class="jittered" font-size="13">full jitter peak: {analysis['full_jitter']['peak_retries_in_one_bucket']}</text>
  <text x="720" y="136" class="muted">peak reduction: {analysis['peak_reduction_percent']:.1f}%</text>
</svg>
"""
    with open(args.output, "w", encoding="utf-8") as destination:
        destination.write(svg)


if __name__ == "__main__":
    main()
