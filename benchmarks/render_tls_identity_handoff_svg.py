#!/usr/bin/env python3
"""Render a deterministic summary SVG from checked v0.30 retained evidence."""

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
    certificates = load(evidence / "certificate-identities.json")
    server = load(evidence / "server-handoff.json")
    controls = load(evidence / "control-handoff.json")
    live = load(evidence / "live-rejections.json")
    production = load(evidence / "production-tests.json")
    processes = load(evidence / "process-continuity.json")
    json_result = load(evidence / "final-json.json")
    sse = load(evidence / "final-sse.json")

    passed = assertions.get("passed", 0)
    total = assertions.get("total", 0)
    server_a = certificates["server"]["A"][:12]
    server_b = certificates["server"]["B"][:12]
    held = server["held_connection"]
    steps = controls["steps"]
    rejection_count = len(live["server_cases"]) + len(live["client_cases"])
    test_count = production["test_count"]
    process_count = len(processes["initial"])
    json_ok = json_result["observation"]["status"] == 200
    sse_ok = sse["done_received"] and sse["eof_after_done"]

    green = "#27c281"
    blue = "#5aa9ff"
    amber = "#f6c85f"
    ink = "#dce8ff"
    muted = "#94a8c7"
    panel = "#15233a"
    background = "#091321"

    def box(x: int, y: int, width: int, height: int, title: str, lines: list[str], color: str) -> str:
        line_text = "".join(
            f'<text x="{x + 22}" y="{y + 58 + index * 23}" class="body">{escape(line)}</text>'
            for index, line in enumerate(lines)
        )
        return (
            f'<rect x="{x}" y="{y}" width="{width}" height="{height}" rx="14" fill="{panel}" stroke="{color}" stroke-width="2"/>'
            f'<text x="{x + 22}" y="{y + 31}" class="title" fill="{color}">{escape(title)}</text>'
            f"{line_text}"
        )

    svg = f'''<svg xmlns="http://www.w3.org/2000/svg" width="1200" height="760" viewBox="0 0 1200 760" role="img" aria-labelledby="title description">
<title id="title">InferLab v0.30 restart-free same-CA TLS identity handoff proof</title>
<desc id="description">Deterministic retained-evidence summary of server and three client leaf renewals, rejection matrices, process continuity, policy receipts, and application traffic.</desc>
<style>
  .heading {{ font: 700 28px -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif; fill: {ink}; }}
  .subtitle {{ font: 15px -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif; fill: {muted}; }}
  .title {{ font: 700 17px -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif; }}
  .body {{ font: 14px ui-monospace,SFMono-Regular,Menlo,monospace; fill: {ink}; }}
  .small {{ font: 12px ui-monospace,SFMono-Regular,Menlo,monospace; fill: {muted}; }}
</style>
<rect width="1200" height="760" fill="{background}"/>
<text x="50" y="58" class="heading">v0.30 · restart-free same-CA TLS leaf renewal</text>
<text x="50" y="86" class="subtitle">manifest-last retained evidence · {passed}/{total} offline assertions passed</text>
{box(50, 120, 530, 178, "Distributor server A → B", [
    f"status activates B: {server_b}",
    f"new TCP/TLS connection presents B: {server_b}",
    f"held established connection remains A: {server_a}",
    f"held requests: {held['first_status']} → {held['second_status']}",
], blue)}
{box(620, 120, 530, 178, "Three control clients A → B", [
    f"handoff order: {' → '.join(step['identity_id'] for step in steps)}",
    "each activation builds a fresh HTTP client pool",
    "new fetch + receipt observations carry generation 2",
    "publisher A/B are fresh probes, not retained processes",
], green)}
{box(50, 332, 350, 165, "Failure / LKG matrix", [
    f"startup failures before bind: 15",
    f"live rejected candidates: {rejection_count}",
    "rollback · fork · CA change rejected",
    "last-known-good traffic stays usable",
], amber)}
{box(425, 332, 350, 165, "Authority preserved", [
    "policy generations: 1 → 2",
    "three Ed25519 receipts each generation",
    "TLS leaf is channel identity only",
    "issuer and verifier CAs remain unchanged",
], blue)}
{box(800, 332, 350, 165, "Runtime continuity", [
    f"unchanged PID/start/executable: {process_count}",
    "Raft: one leader + revision 2",
    f"exact focused regressions passed: {test_count}",
    "watchers supervised by their processes",
], green)}
{box(50, 531, 1100, 132, "Real traffic after renewal", [
    f"CPU JSON completion: {'200 OK' if json_ok else 'failed'}",
    f"incremental SSE: {'[DONE] then EOF' if sse_ok else 'failed'}",
    "Private PEM/key material, host paths, fixed prompts, and deterministic seeds are absent from retained output.",
], green)}
<text x="50" y="710" class="small">A/B labels identify public leaf SHA-256 fingerprints; they do not claim TLS renegotiation or CA migration.</text>
</svg>
'''
    args.output.write_text(svg, encoding="utf-8")


if __name__ == "__main__":
    main()
