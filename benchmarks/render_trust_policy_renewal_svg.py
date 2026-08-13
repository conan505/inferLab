#!/usr/bin/env python3
"""Render a deterministic v0.31 trust-policy renewal evidence summary."""

from __future__ import annotations

import argparse
import html
import json
from pathlib import Path
from typing import Any


def load(path: Path) -> Any:
    with path.open(encoding="utf-8") as source:
        return json.load(source)


def escape(value: Any) -> str:
    return html.escape(str(value), quote=True)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    evidence = args.evidence_dir
    assertions = load(evidence / "assertions.json")
    authority = load(evidence / "authority.json")
    generations = load(evidence / "automatic-generations.json")["generations"]
    normal = load(evidence / "normal-renewal.json")
    ambiguous = load(evidence / "ambiguous-retry.json")
    expiry = load(evidence / "expiry-outage-recovery.json")
    processes = load(evidence / "process-continuity.json")
    startup = load(evidence / "renewer-startup-rejections.json")
    production = load(evidence / "production-tests.json")
    json_result = load(evidence / "final-json.json")
    sse = load(evidence / "final-sse.json")

    passed = assertions.get("passed", 0)
    total = assertions.get("total", 0)
    template = authority["template_fingerprint"][:12]
    generation_labels = [item["label"] for item in generations]
    receipt_counts = [item["distributor"]["result"]["body"]["receipt_count"] for item in generations]
    json_duration = json_result["observation"]["duration_ms"]
    sse_duration = sse["duration_ms"]
    stable_count = len(processes["stable_runtime_processes"])

    green = "#27c281"
    blue = "#5aa9ff"
    amber = "#f6c85f"
    red = "#ff7b86"
    ink = "#dce8ff"
    muted = "#94a8c7"
    panel = "#15233a"
    background = "#091321"

    def box(
        x: int,
        y: int,
        width: int,
        height: int,
        title: str,
        lines: list[str],
        color: str,
    ) -> str:
        body = "".join(
            f'<text x="{x + 22}" y="{y + 59 + index * 23}" class="body">{escape(line)}</text>'
            for index, line in enumerate(lines)
        )
        return (
            f'<rect x="{x}" y="{y}" width="{width}" height="{height}" rx="14" '
            f'fill="{panel}" stroke="{color}" stroke-width="2"/>'
            f'<text x="{x + 22}" y="{y + 32}" class="title" fill="{color}">{escape(title)}</text>'
            f"{body}"
        )

    svg = f'''<svg xmlns="http://www.w3.org/2000/svg" width="1200" height="820" viewBox="0 0 1200 820" role="img" aria-labelledby="title description">
<title id="title">InferLab v0.31 deadline-safe automated signed trust-policy renewal</title>
<desc id="description">Deterministic retained-evidence summary of four automatic signed generations, normal deadline renewal, ambiguous response reconciliation, expiry without grace, recovery, receipts, process identity, and application traffic.</desc>
<style>
  .heading {{ font: 700 28px -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif; fill: {ink}; }}
  .subtitle {{ font: 15px -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif; fill: {muted}; }}
  .title {{ font: 700 17px -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif; }}
  .body {{ font: 14px ui-monospace,SFMono-Regular,Menlo,monospace; fill: {ink}; }}
  .small {{ font: 12px ui-monospace,SFMono-Regular,Menlo,monospace; fill: {muted}; }}
</style>
<rect width="1200" height="820" fill="{background}"/>
<text x="50" y="58" class="heading">v0.31 · deadline-safe automated signed trust-policy renewal</text>
<text x="50" y="86" class="subtitle">manifest-last retained evidence · {passed}/{total} offline assertions passed</text>
{box(50, 118, 1100, 150, "Four automatic generations · one fixed meaning", [
    f"generation path: {' → '.join(str(item['generation']) for item in generations)}",
    f"cycle labels: {' · '.join(generation_labels)}",
    f"verified receiver receipts: {' / '.join(str(value) for value in receipt_counts)}",
    f"semantic template fingerprint: {template}…",
], blue)}
{box(50, 298, 350, 178, "Normal deadline path", [
    f"generation: {normal['from_generation']} → {normal['to_generation']}",
    "all three receipts precede old expiry",
    "expiry rejection counters unchanged",
    "no protected-request authorization gap",
], green)}
{box(425, 298, 350, 178, "Ambiguous response", [
    f"committed response dropped at g{ambiguous['target_generation']}",
    "exact pending bytes survive restart",
    "GET equality reconciles the outbox",
    "no fork · duplicate · skipped generation",
], amber)}
{box(800, 298, 350, 178, "Outage across expiry", [
    f"generation {expiry['expired_generation']} expires without grace",
    f"higher generation {expiry['recovery_generation']} restores trust",
    f"late-recovery counter delta: {expiry['late_recovery_count_delta']}",
    "signed + missing auth share redacted 401",
], red)}
{box(50, 506, 530, 154, "Exact process and regression evidence", [
    f"unchanged runtime services: {stable_count}",
    "renewer: one exact expected replacement",
    "fault gate: explicit proof-only process",
    f"startup rejections / exact tests: {len(startup['cases'])} / {production['test_count']}",
], blue)}
{box(620, 506, 530, 154, "Real traffic after late recovery", [
    f"CPU JSON: 200 in {json_duration} ms",
    f"incremental SSE: DONE + EOF in {sse_duration} ms",
    f"SSE events / content pieces: {sse['event_count']} / {sse['content_event_count']}",
    "Raft route remains revision 2",
], green)}
{box(50, 690, 1100, 78, "Authority boundary", [
    "Only the separately supervised renewer receives the root seed; distributor and controls retain verification roles.",
], amber)}
<text x="50" y="798" class="small">One loopback schedule: no HA, semantic policy rollout, certificate automation, secure time, cancellation, or fleet-atomic claim.</text>
</svg>
'''
    args.output.write_text(svg, encoding="utf-8")


if __name__ == "__main__":
    main()
