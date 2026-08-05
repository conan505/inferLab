#!/usr/bin/env python3
"""Render retained v0.13 full-stack integration evidence."""

from __future__ import annotations

import argparse
import html
import json
from pathlib import Path

WIDTH = 1280
HEIGHT = 1160


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


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--check", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    evidence = args.evidence_dir
    check = json.loads(args.check.read_text())

    initial_config = load(evidence, "config-initial.json")
    initial_gateway = load(evidence, "gateway-initial.json")
    affinity = load(evidence, "affinity.json")
    worker_fault = load(evidence, "worker-fault.json")
    failover = load(evidence, "failover.json")
    live_config = load(evidence, "config-live.json")
    live_gateway = load(evidence, "gateway-live.json")
    post = load(evidence, "post-reconfigure.json")
    control_fault = load(evidence, "control-fault.json")
    election = load(evidence, "election-continuity.json")
    re_election = load(evidence, "re-election.json")
    weighted_config = load(evidence, "config-weighted.json")
    weighted_gateway = load(evidence, "gateway-weighted.json")
    weighted = load(evidence, "weighted.json")
    streamed = load(evidence, "stream.json")

    timeline = [
        (initial_config["written_at_ms"], "control", "commit affinity config"),
        (initial_gateway["observed_at_ms"], "control", "gateway applies initial revision"),
        (affinity["started_at_ms"], "request", "affinity + prefix reuse"),
        (worker_fault["at_ms"], "fault", f"kill {worker_fault['target']}"),
        (failover["observed_at_ms"], "request", "retry completes"),
        (live_config["written_at_ms"], "control", "commit live-worker config"),
        (live_gateway["observed_at_ms"], "control", "gateway removes failed worker"),
        (post["observed_at_ms"], "request", "one-attempt requests"),
        (control_fault["at_ms"], "fault", f"kill {control_fault['target']}"),
        (election["started_at_ms"], "request", "serve during election"),
        (re_election["observed_at_ms"], "recovery", "new Raft leader"),
        (weighted_config["written_at_ms"], "control", "commit weighted config"),
        (weighted_gateway["observed_at_ms"], "control", "gateway applies final revision"),
        (weighted["observed_at_ms"], "request", "3:1 routing observed"),
        (streamed["observed_at_ms"], "request", "speculative SSE done"),
    ]
    first_time = min(item[0] for item in timeline)
    last_time = max(item[0] for item in timeline)
    duration = max(last_time - first_time, 1)

    items = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}" role="img" aria-labelledby="title description">',
        '<title id="title">InferLab v0.13 real-worker full-stack integration proof</title>',
        '<desc id="description">Timeline of committed routing revisions, worker and leader faults, uninterrupted real-model requests, retry behavior, weighted routing, and final speculative SSE streaming.</desc>',
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
        .blue{fill:var(--blue)} .green{fill:var(--green)} .orange{fill:var(--orange)}
        .purple{fill:var(--purple)} .red{fill:var(--red)}
        .blue-stroke{stroke:var(--blue);fill:none;stroke-width:2.5}
        .green-stroke{stroke:var(--green);fill:none;stroke-width:2.5}
        </style>""",
        f'<rect class="background" width="{WIDTH}" height="{HEIGHT}"/>',
        label(50, 46, "v0.13 · real-worker full-stack integration", "title"),
        label(
            50,
            72,
            f"{check['assertions_passed']}/{check['assertions_total']} checks · Raft revisions + real online-attention workers + failover + SSE",
            "subtitle",
        ),
        '<rect class="panel" x="42" y="96" width="1196" height="398" rx="10"/>',
        label(66, 130, "One timeline: configuration changes and requests remain separate", "heading"),
        label(66, 151, "Faults are exact owned child processes; request headers retain their start revision.", "small"),
    ]

    lanes = {"control": 205, "request": 295, "fault": 385, "recovery": 385}
    lane_names = {"control": "control plane", "request": "data plane", "fault": "fault / recovery"}
    for lane, y in [("control", 205), ("request", 295), ("fault", 385)]:
        items.append(label(72, y + 4, lane_names[lane], "small"))
        items.append(f'<path class="axis" d="M 168 {y} H 1190"/>')
    for tick in range(6):
        x = 168 + tick / 5 * 1022
        elapsed = duration * tick / 5 / 1000
        items.append(f'<path class="guide" d="M {x:.1f} 178 V 432"/>')
        items.append(label(x, 454, f"{elapsed:.2f}s", "small", "middle"))

    category_css = {
        "control": "blue",
        "request": "green",
        "fault": "red",
        "recovery": "purple",
    }
    lane_offsets = {"control": 0, "request": 0, "fault": -12, "recovery": 12}
    category_indexes = {category: 0 for category in category_css}
    for timestamp, category, text_value in timeline:
        x = 168 + (timestamp - first_time) / duration * 1022
        y = lanes[category] + lane_offsets[category]
        items.append(f'<circle class="{category_css[category]}" cx="{x:.1f}" cy="{y:.1f}" r="5"/>')
        # Alternate within each lane, not across the global event list. Closely
        # spaced request and control-plane events then land on different rows.
        category_index = category_indexes[category]
        category_indexes[category] += 1
        label_y = y - 12 if category_index % 2 == 0 else y + 24
        anchor = "start" if x < 1040 else "end"
        items.append(label(x, label_y, text_value, "small", anchor))

    phase_rows = [
        ("affinity", affinity["succeeded"], affinity["requested"]),
        ("worker failover", failover["succeeded"], failover["requested"]),
        ("after reconfig", post["succeeded"], post["requested"]),
        ("leader election", election["succeeded"], election["requested"]),
        ("weighted", weighted["succeeded"], weighted["requested"]),
        ("SSE", int(streamed["status"] == 200 and streamed["done_received"]), 1),
    ]
    items.extend(
        [
            '<rect class="panel" x="42" y="514" width="575" height="310" rx="10"/>',
            label(66, 548, "Real-model request continuity", "heading"),
            label(66, 569, "Successful / attempted requests by experiment phase", "small"),
        ]
    )
    maximum_phase = max(row[2] for row in phase_rows)
    for index, (name, succeeded, requested) in enumerate(phase_rows):
        y = 598 + index * 34
        items.append(label(66, y + 17, name, "small"))
        width = requested / maximum_phase * 320
        items.append(bar(185, y, width, 22, "green"))
        items.append(label(193 + width, y + 16, f"{succeeded}/{requested}", "value"))

    revisions = [
        ("affinity", initial_config["committed"]["revision"]),
        ("failover", failover["requests"][0]["config_revision"]),
        ("reconfigured", live_config["committed"]["revision"]),
        ("election", election["requests"][0]["config_revision"]),
        ("weighted", weighted_config["committed"]["revision"]),
        ("SSE", streamed["config_revision"]),
    ]
    items.extend(
        [
            '<rect class="panel" x="637" y="514" width="601" height="310" rx="10"/>',
            label(661, 548, "Configuration revision carried by each request", "heading"),
            label(661, 569, "Failover and leader election keep the last committed snapshot.", "small"),
        ]
    )
    min_revision = min(revision for _, revision in revisions)
    max_revision = max(revision for _, revision in revisions)
    revision_span = max(max_revision - min_revision, 1)
    revision_points = []
    for index, (name, revision) in enumerate(revisions):
        x = 690 + index / (len(revisions) - 1) * 500
        y = 760 - (revision - min_revision) / revision_span * 130
        revision_points.append((x, y))
        items.append(label(x, 788, name, "small", "middle"))
        items.append(f'<circle class="blue" cx="{x:.1f}" cy="{y:.1f}" r="5"/>')
        items.append(label(x, y - 12, f"rev {revision}", "value", "middle"))
    path = " ".join(
        ("M" if index == 0 else "L") + f" {x:.1f} {y:.1f}"
        for index, (x, y) in enumerate(revision_points)
    )
    items.append(f'<path class="blue-stroke" d="{path}"/>')

    items.extend(
        [
            '<rect class="panel" x="42" y="844" width="1196" height="252" rx="10"/>',
            label(66, 878, "Final three-to-one weighted routing across real workers", "heading"),
            label(66, 899, "Eight non-stream requests after the new leader commits the final revision", "small"),
        ]
    )
    counts = list(weighted["worker_counts"].items())
    max_count = max(count for _, count in counts)
    for index, (worker, count) in enumerate(counts):
        y = 935 + index * 58
        items.append(label(76, y + 24, worker, "label"))
        width = count / max_count * 760
        items.append(bar(205, y, width, 34, "purple" if index == 0 else "orange"))
        items.append(label(215 + width, y + 23, f"{count} requests", "value"))
    items.append(
        label(
            66,
            1078,
            f"Worker failure: {check['failed_worker']} · failover attempts: {check['failover_attempts']} · leader re-election: {check['re_election_latency_ms']:.1f} ms · final SSE: DONE",
            "value",
        )
    )
    items.append(label(50, 1132, "Boundary: the proof integrates existing mechanisms; it does not turn control-plane consensus into a per-token dependency.", "subtitle"))
    items.append("</svg>")
    args.output.write_text("\n".join(items) + "\n")


if __name__ == "__main__":
    main()
