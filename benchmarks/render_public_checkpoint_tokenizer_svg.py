#!/usr/bin/env python3
"""Render deterministic v0.32 checkpoint/tokenizer retained evidence."""

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
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    evidence = args.evidence_dir
    assertions = load(evidence / "assertions.json")
    acquisition = load(evidence / "artifact-acquisition.json")
    checkpoint = load(evidence / "checkpoint-reference.json")
    tokenizer = load(evidence / "tokenizer-reference.json")
    production = load(evidence / "tokenizer-production.json")
    failures = load(evidence / "failure-matrix.json")
    tests = load(evidence / "production-tests.json")
    timings = load(evidence / "timings.json")

    passed = assertions["passed"]
    total = assertions["total"]
    run_id = assertions["run_id"]
    tensor_count = checkpoint["checkpoint"]["tensor_count"]
    element_count = checkpoint["checkpoint"]["element_count"]
    encode_cases = len(tokenizer["encode_cases"])
    decode_rejections = len(production["decode_rejections"])
    request_rejections = len(production["request_rejections"])
    failure_count = len(failures["direct_cases"]) + len(failures["regression_cases"])
    capture_ms = timings["total_capture_ms"]

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
            f'<text x="{x + 22}" y="{y + 59 + index * 23}" class="body">'
            f"{escape(line)}</text>"
            for index, line in enumerate(lines)
        )
        return (
            f'<rect x="{x}" y="{y}" width="{width}" height="{height}" rx="14" '
            f'fill="{panel}" stroke="{color}" stroke-width="2"/>'
            f'<text x="{x + 22}" y="{y + 32}" class="title" fill="{color}">'
            f"{escape(title)}</text>{body}"
        )

    svg = f'''<svg xmlns="http://www.w3.org/2000/svg" width="1200" height="860" viewBox="0 0 1200 860" role="img" aria-labelledby="title description">
<title id="title">InferLab v0.32 pinned checkpoint and production tokenizer</title>
<desc id="description">Deterministic retained-evidence summary for immutable Pythia-14M artifact verification and production tokenizer parity, explicitly without a public-model forward pass, generation, or public-model service.</desc>
<style>
  .heading {{ font: 700 28px -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif; fill: {ink}; }}
  .subtitle {{ font: 15px -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif; fill: {muted}; }}
  .title {{ font: 700 17px -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif; }}
  .body {{ font: 14px ui-monospace,SFMono-Regular,Menlo,monospace; fill: {ink}; }}
  .small {{ font: 12px ui-monospace,SFMono-Regular,Menlo,monospace; fill: {muted}; }}
</style>
<rect width="1200" height="860" fill="{background}"/>
<text x="50" y="58" class="heading">v0.32 · pinned checkpoint + production tokenizer</text>
<text x="50" y="86" class="subtitle">{escape(run_id)} · manifest-last evidence · {passed}/{total} assertions passed</text>
{box(50, 118, 1100, 142, "Immutable six-file acquisition", [
    f"repository: {acquisition['source']['repository']}",
    f"revision: {acquisition['source']['revision'][:16]}…",
    f"mode / exact bytes: {acquisition['mode']} / {acquisition['total_bytes']:,}",
    "online fetch authority is separate; public-model consumers are offline",
], blue)}
{box(50, 290, 350, 182, "Checkpoint inventory", [
    f"tensors / dtype: {tensor_count} / F16",
    f"parameters: {element_count:,}",
    f"tensor bytes: {checkpoint['checkpoint']['data_bytes']:,}",
    "exact names · shapes · offsets · finite payload",
], green)}
{box(425, 290, 350, 182, "Tokenizer parity", [
    f"maintained reference: tokenizers==0.23.1",
    f"encode / decode cases: {encode_cases} / {len(tokenizer['decode_cases'])}",
    "recognize_configured · encode_as_text",
    "NFC · ByteLevel · BPE · U+0000 · 2048 edge",
], blue)}
{box(800, 290, 350, 182, "Strict text boundary", [
    f"decode / request rejections: {decode_rejections} / {request_rejections}",
    "[127] rejects; [127,104] decodes to é",
    "50277..50303 are alignment-only model rows",
    "literal U+FFFD remains valid data",
], amber)}
{box(50, 502, 530, 160, "Fail-closed verification", [
    f"failure cases: {failure_count}",
    "missing · extra · symlink · FIFO · corrupt",
    "bounded fetch · atomic rename commit point",
    f"production regression tests: {tests['total_tests']}",
], red)}
{box(620, 502, 530, 160, "Deliberate Day-14 boundary", [
    "public-model forward passes: 0",
    "public-model generations: 0",
    "public-model services / retained weight bytes: 0 / 0",
    f"measured evidence capture: {capture_ms:,} ms",
], green)}
{box(50, 692, 1100, 92, "What this proves", [
    "The exact public bytes are structurally understood and text parity is production-grade; model execution remains a later milestone.",
], blue)}
<text x="50" y="824" class="small">No logits, sampling, quality, HTTP serving, worker integration, ambient Hub lookup, or arbitrary-model compatibility claim.</text>
</svg>
'''
    args.output.write_text(svg, encoding="utf-8")


if __name__ == "__main__":
    main()
