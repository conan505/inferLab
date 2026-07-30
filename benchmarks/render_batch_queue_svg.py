#!/usr/bin/env python3
"""Render the verified v0.5 WAL as a deterministic queue lifecycle chart."""

import argparse
import html
import json


WIDTH = 1240
HEIGHT = 720
LEFT = 185
RIGHT = 55
TOP = 150
STEP = 76
LANES = {
    "batch-00000001": 230,
    "batch-00000002": 390,
    "batch-00000003": 550,
}
COLORS = {
    "enqueued": "#60a5fa",
    "claimed": "#fbbf24",
    "released": "#fb923c",
    "acknowledged": "#4ade80",
    "dead_lettered": "#fb7185",
}
LABELS = {
    "enqueued": "pending",
    "claimed": "claimed",
    "released": "pending",
    "acknowledged": "completed",
    "dead_lettered": "dead letter",
}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", required=True)
    parser.add_argument("--wal", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    with open(args.check, encoding="utf-8") as source:
        check = json.load(source)
    with open(args.wal, encoding="utf-8") as source:
        events = [json.loads(line) for line in source if line.strip()]

    lane_names = {
        check["jobs"]["crash_job"]: "crashed consumer → redelivery",
        check["jobs"]["pending_job"]: "pending across restart",
        check["jobs"]["poison_job"]: "bounded failures → DLQ",
    }
    marks = []
    connectors = []
    last_position = {}
    for index, event in enumerate(events):
        job_id = event["job_id"]
        x = LEFT + index * STEP
        y = LANES[job_id]
        if job_id in last_position:
            connectors.append(
                f'<line x1="{last_position[job_id]}" y1="{y}" '
                f'x2="{x}" y2="{y}" stroke="#64748b" stroke-width="3"/>'
            )
        last_position[job_id] = x
        color = COLORS[event["type"]]
        detail = LABELS[event["type"]]
        if event["type"] == "claimed":
            detail = f"claim #{event['attempt']}"
        if event["type"] == "released" and event["expired"]:
            detail = "timeout"
        marks.append(
            f'<circle cx="{x}" cy="{y}" r="13" fill="{color}"/>'
            f'<text x="{x}" y="{y + 32}" text-anchor="middle" class="event">'
            f'{html.escape(detail)}</text>'
        )

    restart_x = LEFT + 2.5 * STEP
    final = check["final_status"]
    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}">
  <rect width="100%" height="100%" fill="#0f172a"/>
  <style>
    text {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace; fill: #e2e8f0; }}
    .muted {{ fill: #94a3b8; font-size: 13px; }}
    .event {{ fill: #cbd5e1; font-size: 11px; }}
    .metric {{ fill: #e2e8f0; font-size: 15px; }}
  </style>
  <text x="55" y="38" font-size="24" font-weight="700">InferLab v0.5 — durable batch lifecycle</text>
  <text x="55" y="64" class="muted">Each dot is one fsync-backed WAL transition; horizontal position is WAL order.</text>
  <line x1="{restart_x}" y1="100" x2="{restart_x}" y2="630" stroke="#a78bfa" stroke-width="2" stroke-dasharray="7 6"/>
  <text x="{restart_x + 8}" y="118" fill="#c4b5fd" font-size="13">queue process killed + restarted</text>
  {''.join(connectors)}
  <text x="55" y="{LANES[check['jobs']['crash_job']] + 5}" class="metric">{html.escape(lane_names[check['jobs']['crash_job']])}</text>
  <text x="55" y="{LANES[check['jobs']['pending_job']] + 5}" class="metric">{html.escape(lane_names[check['jobs']['pending_job']])}</text>
  <text x="55" y="{LANES[check['jobs']['poison_job']] + 5}" class="metric">{html.escape(lane_names[check['jobs']['poison_job']])}</text>
  {''.join(marks)}
  <text x="55" y="650" class="metric">Final: {final['completed']} completed · {final['dead_letter']} dead letter · {final['pending']} pending · {final['claimed']} claimed</text>
  <text x="55" y="680" class="muted">Blue pending · yellow claim · orange release/timeout · green completion · red dead letter</text>
</svg>
"""
    with open(args.output, "w", encoding="utf-8") as destination:
        destination.write(svg)


if __name__ == "__main__":
    main()
