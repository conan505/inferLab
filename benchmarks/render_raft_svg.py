#!/usr/bin/env python3
"""Render the retained v0.6 Raft election and commit timeline."""

import argparse
import html
import json


WIDTH = 1240
HEIGHT = 760
LEFT = 150
RIGHT = 50
TOP = 150
BOTTOM = 620
LANES = {
    "node-a": 220,
    "node-b": 340,
    "node-c": 460,
    "gateway": 580,
}
COLORS = {
    "election_started": "#fbbf24",
    "leader_elected": "#4ade80",
    "leader_killed": "#fb7185",
    "entry_committed": "#60a5fa",
    "node_started": "#a78bfa",
    "log_repaired": "#fb923c",
}
def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--analysis", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    with open(args.analysis, encoding="utf-8") as source:
        analysis = json.load(source)

    events = [
        event
        for event in analysis["timeline"]
        if event["event"] in COLORS
    ]
    gateway_events = [
        {
            "elapsed_ms": phase["observed_at_ms"]
            - analysis["start_epoch_ms"],
            "event": "gateway",
            "policy": phase["applied_policy"],
            "revision": phase["applied_revision"],
        }
        for phase in analysis["gateway"].values()
    ]
    duration = max(
        [event["elapsed_ms"] for event in events]
        + [event["elapsed_ms"] for event in gateway_events]
    )
    plot_width = WIDTH - LEFT - RIGHT

    def x_position(elapsed_ms):
        return LEFT + elapsed_ms / max(duration, 1) * plot_width

    lane_markup = []
    for node_id, y in LANES.items():
        lane_markup.append(
            f'<text x="{LEFT - 16}" y="{y + 5}" text-anchor="end" '
            f'class="lane">{html.escape(node_id)}</text>'
        )
        lane_markup.append(
            f'<line x1="{LEFT}" y1="{y}" x2="{WIDTH - RIGHT}" y2="{y}" '
            'stroke="#475569" stroke-width="2"/>'
        )

    event_markup = []
    for event in events:
        x = x_position(event["elapsed_ms"])
        y = LANES[event["node_id"]]
        color = COLORS[event["event"]]
        event_markup.append(
            f'<circle cx="{x:.2f}" cy="{y}" r="10" fill="{color}"/>'
        )
        label = None
        label_y = y - 18
        if event["event"] == "leader_elected":
            label = f"leader t{event['term']}"
        elif event["event"] == "leader_killed":
            label = "killed"
        elif event["event"] == "node_started":
            label = (
                "start"
                if "replayed 0 log entries" in event["detail"]
                else "restart"
            )
            label_y = y + 32
        elif event["event"] == "log_repaired":
            label = "repair"
            label_y = y + 32
        if label is not None:
            event_markup.append(
                f'<text x="{x:.2f}" y="{label_y}" text-anchor="middle" '
                f'class="event">{html.escape(label)}</text>'
            )
        if event["event"] == "leader_killed":
            event_markup.append(
                f'<line x1="{x:.2f}" y1="{TOP - 20}" x2="{x:.2f}" '
                f'y2="{BOTTOM + 20}" stroke="{color}" stroke-width="1.5" '
                'stroke-dasharray="6 5"/>'
            )

    gateway_markup = []
    seen = set()
    for event in gateway_events:
        key = (event["policy"], event["revision"])
        if key in seen:
            continue
        seen.add(key)
        x = x_position(event["elapsed_ms"])
        y = LANES["gateway"]
        text_anchor = "end" if x > WIDTH - RIGHT - 180 else "middle"
        text_x = x - 2 if text_anchor == "end" else x
        gateway_markup.append(
            f'<rect x="{x - 8:.2f}" y="{y - 8}" width="16" height="16" '
            'fill="#38bdf8"/>'
            f'<text x="{text_x:.2f}" y="{y - 18}" text-anchor="{text_anchor}" '
            f'class="event">{html.escape(event["policy"])} · r{event["revision"]}</text>'
        )

    ticks = []
    tick_count = 8
    for tick in range(tick_count + 1):
        elapsed = duration * tick / tick_count
        x = x_position(elapsed)
        ticks.append(
            f'<line x1="{x:.2f}" y1="{BOTTOM + 35}" x2="{x:.2f}" '
            f'y2="{BOTTOM + 42}" stroke="#64748b"/>'
            f'<text x="{x:.2f}" y="{BOTTOM + 62}" text-anchor="middle" '
            f'class="muted">{elapsed:.0f} ms</text>'
        )

    reelections = analysis["reelections"]
    final = analysis["convergence"]["final"]
    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}" role="img" aria-labelledby="title description">
  <title id="title">InferLab v0.6 Raft election and commit timeline</title>
  <desc id="description">Three Raft node lanes show elections, leader kills, commits, restarts, and log repairs. The gateway lane shows committed routing snapshots applied while request serving continues.</desc>
  <rect width="100%" height="100%" fill="#0f172a"/>
  <style>
    text {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace; fill: #e2e8f0; }}
    .muted {{ fill: #94a3b8; font-size: 13px; }}
    .event {{ fill: #cbd5e1; font-size: 11px; }}
    .lane {{ fill: #e2e8f0; font-size: 15px; }}
    .metric {{ fill: #e2e8f0; font-size: 15px; }}
  </style>
  <text x="55" y="38" font-size="24" font-weight="500">InferLab v0.6 — Raft election and commit timeline</text>
  <text x="55" y="64" class="muted">Actual persisted node events; two exact leader PIDs are killed and restarted.</text>
  {''.join(lane_markup)}
  {''.join(event_markup)}
  {''.join(gateway_markup)}
  <line x1="{LEFT}" y1="{BOTTOM + 35}" x2="{WIDTH - RIGHT}" y2="{BOTTOM + 35}" stroke="#64748b"/>
  {''.join(ticks)}
  <text x="55" y="704" class="metric">Re-election: {reelections[0]['latency_ms']:.1f} ms · {reelections[1]['latency_ms']:.1f} ms · final revision {final['revision']} converged on 3/3 nodes</text>
  <text x="55" y="733" class="muted">candidate · leader · killed · commit · restart · repair · gateway revision</text>
</svg>
"""
    with open(args.output, "w", encoding="utf-8") as destination:
        destination.write(svg)


if __name__ == "__main__":
    main()
