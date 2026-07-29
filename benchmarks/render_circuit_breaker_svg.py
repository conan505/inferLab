#!/usr/bin/env python3
"""Render the retained circuit-breaker state transition proof as SVG."""

import argparse
import json


WIDTH = 1100
HEIGHT = 390


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--analysis", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()

    with open(args.analysis, encoding="utf-8") as source:
        analysis = json.load(source)

    nodes = [
        (80, "closed", "4 failures", "#38bdf8"),
        (330, "open", f"{analysis['worker_a_open_route_rejections']} routes skipped", "#fb7185"),
        (580, "half-open", "1 probe", "#fbbf24"),
        (830, "closed", "worker healed", "#4ade80"),
    ]
    node_markup = []
    for x, state, detail, color in nodes:
        node_markup.append(
            f"""  <rect x="{x}" y="135" width="190" height="92" rx="14" fill="#111c33" stroke="{color}" stroke-width="3"/>
  <text x="{x + 95}" y="173" text-anchor="middle" font-size="20" font-weight="700" fill="{color}">{state}</text>
  <text x="{x + 95}" y="202" text-anchor="middle" class="muted">{detail}</text>"""
        )

    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}">
  <rect width="100%" height="100%" fill="#0f172a"/>
  <defs>
    <marker id="arrow" markerWidth="10" markerHeight="10" refX="8" refY="3" orient="auto">
      <path d="M0,0 L0,6 L9,3 z" fill="#64748b"/>
    </marker>
  </defs>
  <style>
    text {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace; fill: #e2e8f0; }}
    .muted {{ fill: #94a3b8; font-size: 13px; }}
  </style>
  <text x="80" y="42" font-size="24" font-weight="700">InferLab v0.0.8 — circuit recovery</text>
  <text x="80" y="72" class="muted">sliding window → cooldown → one controlled probe → automatic re-entry</text>
  <line x1="270" y1="181" x2="322" y2="181" stroke="#64748b" stroke-width="3" marker-end="url(#arrow)"/>
  <line x1="520" y1="181" x2="572" y2="181" stroke="#64748b" stroke-width="3" marker-end="url(#arrow)"/>
  <line x1="770" y1="181" x2="822" y2="181" stroke="#64748b" stroke-width="3" marker-end="url(#arrow)"/>
{chr(10).join(node_markup)}
  <text x="80" y="288" class="muted">worker A openings</text>
  <text x="285" y="288" font-size="16">{analysis['worker_a_opened_total']}</text>
  <text x="390" y="288" class="muted">half-open probes</text>
  <text x="610" y="288" font-size="16">{analysis['half_open_probes']}</text>
  <text x="705" y="288" class="muted">recoveries</text>
  <text x="870" y="288" font-size="16">{analysis['recoveries']}</text>
  <text x="80" y="335" class="muted">request accounting</text>
  <text x="285" y="335" font-size="16">{analysis['upstream_attempts']} attempts = {analysis['original_requests']} originals + {analysis['retries_granted']} retries</text>
</svg>
"""
    with open(args.output, "w", encoding="utf-8") as destination:
        destination.write(svg)


if __name__ == "__main__":
    main()
