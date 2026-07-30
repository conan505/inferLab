#!/usr/bin/env python3
"""Render the v0.0.9 continuous recovery curve as deterministic SVG."""

import argparse
import html
import json


WIDTH = 1200
HEIGHT = 760
LEFT = 90
RIGHT = 45
PLOT_WIDTH = WIDTH - LEFT - RIGHT
OUTCOME_TOP = 125
OUTCOME_BOTTOM = 335
LANE_START = 415
LANE_HEIGHT = 34
STATE_COLORS = {
    "closed": "#4ade80",
    "open": "#fb7185",
    "half-open": "#fbbf24",
}
EVENT_LABELS = {
    "worker_a_killed": "kill A",
    "worker_a_restarted": "heal A",
    "worker_b_slowed": "slow B",
    "worker_b_restored": "heal B",
    "worker_c_disconnected": "disconnect C",
    "worker_c_reconnected": "reconnect C",
}


def x_position(elapsed_ms: float, duration_ms: float) -> float:
    return LEFT + elapsed_ms / duration_ms * PLOT_WIDTH


def y_latency(value: float, maximum: float) -> float:
    return OUTCOME_BOTTOM - value / maximum * (
        OUTCOME_BOTTOM - OUTCOME_TOP
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--analysis", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    with open(args.analysis, encoding="utf-8") as source:
        analysis = json.load(source)

    duration_ms = analysis["duration_ms"]
    bins = analysis["timeline_bins"]
    max_requests = max(
        bin_["successful"] + bin_["errors"] for bin_ in bins
    )
    latency_ceiling = max(
        analysis["bounds"]["request_deadline_ms"],
        max(bin_["p95_latency_ms"] or 0 for bin_ in bins),
    )

    outcome_bars = []
    latency_points = []
    for bin_ in bins:
        x1 = x_position(bin_["start_ms"], duration_ms)
        x2 = x_position(bin_["end_ms"], duration_ms)
        width = max(1.0, x2 - x1 - 2)
        scale = (OUTCOME_BOTTOM - OUTCOME_TOP) / max_requests
        success_height = bin_["successful"] * scale
        error_height = bin_["errors"] * scale
        outcome_bars.append(
            f'<rect x="{x1 + 1:.2f}" y="{OUTCOME_BOTTOM - success_height:.2f}" '
            f'width="{width:.2f}" height="{success_height:.2f}" fill="#4ade80" opacity="0.72"/>'
        )
        if error_height > 0:
            outcome_bars.append(
                f'<rect x="{x1 + 1:.2f}" '
                f'y="{OUTCOME_BOTTOM - success_height - error_height:.2f}" '
                f'width="{width:.2f}" height="{error_height:.2f}" fill="#fb7185" opacity="0.92"/>'
            )
        if bin_["p95_latency_ms"] is not None:
            midpoint = (x1 + x2) / 2
            latency_points.append(
                (
                    midpoint,
                    y_latency(bin_["p95_latency_ms"], latency_ceiling),
                )
            )

    latency_path = " ".join(
        f"{'M' if index == 0 else 'L'} {x:.2f} {y:.2f}"
        for index, (x, y) in enumerate(latency_points)
    )

    event_markup = []
    for index, event in enumerate(analysis["events"]):
        if event["event"] not in EVENT_LABELS:
            continue
        x = x_position(event["elapsed_ms"], duration_ms)
        label_y = 70 if index % 2 == 0 else 91
        event_markup.append(
            f'<line x1="{x:.2f}" y1="58" x2="{x:.2f}" y2="565" '
            'stroke="#64748b" stroke-width="1" stroke-dasharray="4 4"/>'
        )
        event_markup.append(
            f'<text x="{x + 4:.2f}" y="{label_y}" class="event">'
            f'{html.escape(EVENT_LABELS[event["event"]])}</text>'
        )

    lane_markup = []
    for lane_index, worker_id in enumerate(
        ["worker-a", "worker-b", "worker-c"]
    ):
        y = LANE_START + lane_index * 52
        lane_markup.append(
            f'<text x="{LEFT - 12}" y="{y + 22}" text-anchor="end" '
            f'class="muted">{worker_id}</text>'
        )
        for segment in analysis["circuit_segments"][worker_id]:
            x1 = x_position(segment["start_ms"], duration_ms)
            x2 = x_position(segment["end_ms"], duration_ms)
            color = STATE_COLORS[segment["state"]]
            lane_markup.append(
                f'<rect x="{x1:.2f}" y="{y}" width="{max(1, x2 - x1):.2f}" '
                f'height="{LANE_HEIGHT}" fill="{color}" opacity="0.82"/>'
            )

    time_ticks = []
    tick_seconds = 2
    final_second = int(duration_ms / 1000)
    for second in range(0, final_second + 1, tick_seconds):
        x = x_position(second * 1000, duration_ms)
        time_ticks.append(
            f'<line x1="{x:.2f}" y1="{OUTCOME_BOTTOM}" x2="{x:.2f}" '
            f'y2="{OUTCOME_BOTTOM + 7}" stroke="#64748b"/>'
        )
        time_ticks.append(
            f'<text x="{x:.2f}" y="{OUTCOME_BOTTOM + 26}" '
            f'text-anchor="middle" class="muted">{second}s</text>'
        )

    recovery = analysis["recovery"]
    retry = analysis["retry_accounting"]
    bounds = analysis["bounds"]
    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}">
  <rect width="100%" height="100%" fill="#0f172a"/>
  <style>
    text {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace; fill: #e2e8f0; }}
    .muted {{ fill: #94a3b8; font-size: 13px; }}
    .event {{ fill: #cbd5e1; font-size: 12px; }}
    .metric {{ fill: #e2e8f0; font-size: 15px; }}
  </style>
  <text x="{LEFT}" y="34" font-size="24" font-weight="700">InferLab v0.0.9 — continuous chaos recovery</text>
  <text x="{LEFT}" y="56" class="muted">open-loop requests continue while workers die, slow down, disconnect, and recover</text>
  {''.join(event_markup)}
  <line x1="{LEFT}" y1="{OUTCOME_BOTTOM}" x2="{WIDTH - RIGHT}" y2="{OUTCOME_BOTTOM}" stroke="#64748b"/>
  <line x1="{LEFT}" y1="{OUTCOME_TOP}" x2="{LEFT}" y2="{OUTCOME_BOTTOM}" stroke="#64748b"/>
  <text x="{LEFT - 12}" y="{OUTCOME_TOP + 4}" text-anchor="end" class="muted">{max_requests}</text>
  <text x="{LEFT - 12}" y="{OUTCOME_BOTTOM + 4}" text-anchor="end" class="muted">0</text>
  <text x="{LEFT}" y="{OUTCOME_TOP - 14}" class="muted">requests / 500 ms</text>
  {''.join(outcome_bars)}
  <path d="{latency_path}" fill="none" stroke="#38bdf8" stroke-width="2.5"/>
  <text x="{WIDTH - RIGHT}" y="{OUTCOME_TOP - 14}" text-anchor="end" class="muted">blue line: p95 latency · ceiling {latency_ceiling:.0f} ms</text>
  {''.join(time_ticks)}
  <rect x="{LEFT}" y="370" width="12" height="12" fill="#4ade80" opacity="0.72"/>
  <text x="{LEFT + 18}" y="381" class="muted">success</text>
  <rect x="{LEFT + 95}" y="370" width="12" height="12" fill="#fb7185" opacity="0.92"/>
  <text x="{LEFT + 113}" y="381" class="muted">error</text>
  <line x1="{LEFT + 180}" y1="376" x2="{LEFT + 204}" y2="376" stroke="#38bdf8" stroke-width="2.5"/>
  <text x="{LEFT + 212}" y="381" class="muted">p95 latency</text>
  {''.join(lane_markup)}
  <text x="{LEFT}" y="600" class="metric">Detection: A {recovery['worker-a']['detection_ms']:.0f} ms · B {recovery['worker-b']['detection_ms']:.0f} ms · C {recovery['worker-c']['detection_ms']:.0f} ms</text>
  <text x="{LEFT}" y="628" class="metric">Recovery after heal: A {recovery['worker-a']['recovery_ms']:.0f} ms · B {recovery['worker-b']['recovery_ms']:.0f} ms · C {recovery['worker-c']['recovery_ms']:.0f} ms · mean MTTR {analysis['mean_mttr_ms']:.0f} ms</text>
  <text x="{LEFT}" y="656" class="metric">Attempts: {retry['upstream_attempts']} = {retry['original_requests']} originals + {retry['retries_granted']} retries · amplification {retry['amplification']:.3f}×</text>
  <text x="{LEFT}" y="684" class="metric">Bounds: queue {bounds['max_observed_queued']}/{bounds['configured_queue_capacity']} · executing {bounds['max_observed_executing']}/{bounds['configured_execution_capacity']} · max latency {bounds['maximum_client_latency_ms']:.1f}/{bounds['request_deadline_ms']} ms deadline</text>
  <text x="{LEFT}" y="724" class="muted">Circuit lanes: green closed · red open · yellow half-open. Event lines are actual harness timestamps.</text>
</svg>
"""
    with open(args.output, "w", encoding="utf-8") as destination:
        destination.write(svg)


if __name__ == "__main__":
    main()
