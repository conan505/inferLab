#!/usr/bin/env python3
"""Render retained v0.11 quantization and speculative-decoding evidence."""

from __future__ import annotations

import argparse
import html
import json
from pathlib import Path

WIDTH = 1280
HEIGHT = 1220


def label(x, y, value, css="label", anchor="start") -> str:
    return (
        f'<text x="{x:.1f}" y="{y:.1f}" class="{css}" '
        f'text-anchor="{anchor}">{html.escape(str(value))}</text>'
    )


def bar(x, y, width, height, css, rx=3) -> str:
    return (
        f'<rect class="{css}" x="{x:.1f}" y="{y:.1f}" '
        f'width="{max(width, 0):.1f}" height="{height:.1f}" rx="{rx}"/>'
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", type=Path, required=True)
    parser.add_argument("--probe", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    check = json.loads(args.check.read_text())
    probe = json.loads(args.probe.read_text())
    quantization = {item["mode"]: item for item in probe["quantization"]}
    profiles = probe["greedy_speculation"]["profiles"]
    int8_profiles = {
        item["draft_tokens_per_cycle"]: item
        for item in profiles
        if item["draft_quantization"] == "int8"
    }
    quality = probe["draft_quality"]
    sampled = probe["sampled_speculation"]

    items = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}" role="img" aria-labelledby="title description">',
        '<title id="title">InferLab v0.11 quantization and speculative decoding proof</title>',
        '<desc id="description">Comparison of active tensor bytes and logit error for FP32, per-row INT8, and groupwise INT4; target calls and measured wall time by speculative draft window; and acceptance versus corrected output error as draft quality changes.</desc>',
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
        .axis{stroke:var(--grid);stroke-width:1} .guide{stroke:var(--muted);stroke-width:1;stroke-dasharray:4 4}
        .blue{fill:var(--blue)} .green{fill:var(--green)} .orange{fill:var(--orange)}
        .purple{fill:var(--purple)} .red{fill:var(--red)} .outline{fill:none;stroke:var(--fg);stroke-width:1.5}
        </style>""",
        f'<rect class="background" width="{WIDTH}" height="{HEIGHT}"/>',
        label(52, 48, "v0.11 · quantization and speculative decoding", "title"),
        label(
            52,
            74,
            f"{check['assertions_passed']}/{check['assertions_total']} checks · measured memory, correctness, distributions, target calls, and wall time",
            "subtitle",
        ),
        '<rect class="panel" x="42" y="98" width="1196" height="300" rx="10"/>',
        label(66, 132, "Quantization removes linear-weight bytes; FP32 islands remain", "heading"),
        label(66, 153, "Active tensor payload includes all model tensors. Linear-only compression is shown below each bar.", "small"),
    ]

    memory_max = quantization["fp32"]["memory"]["active_tensor_bytes"]
    memory_colors = {"fp32": "blue", "int8": "green", "int4": "purple"}
    for index, mode in enumerate(["fp32", "int8", "int4"]):
        item = quantization[mode]
        memory = item["memory"]
        x = 86 + index * 380
        width = memory["active_tensor_bytes"] / memory_max * 285
        items.append(label(x, 190, mode.upper(), "label"))
        items.append(bar(x, 207, width, 36, memory_colors[mode]))
        items.append(label(x + width + 8, 231, f"{memory['active_tensor_bytes']:,} B", "value"))
        items.append(label(x, 267, f"tensor payload {memory['tensor_compression_ratio']:.2f}x", "small"))
        items.append(label(x, 285, f"linear weights {memory['linear_compression_ratio']:.2f}x", "small"))
        if mode == "fp32":
            detail = "9,600 B linear · no metadata"
        elif mode == "int8":
            detail = f"{memory['scale_count']} row scales · no zero points"
        else:
            detail = f"{memory['scale_count']} group scales + zero points"
        items.append(label(x, 303, detail, "small"))
        items.append(label(x, 337, f"max logit error {item['maximum_absolute_logit_error']:.6f}", "value"))
        items.append(label(x, 356, f"greedy mismatches {item['greedy_token_mismatches']} / {item['steps']}", "small"))
    items.append(label(66, 382, "Important boundary: embeddings, normalization parameters, biases, and runtime state stay FP32.", "small"))

    baseline_us = probe["greedy_speculation"]["baseline_median_us"]
    baseline_calls = probe["greedy_speculation"]["baseline_target_forward_calls"]
    items.extend(
        [
            '<rect class="panel" x="42" y="418" width="1196" height="355" rx="10"/>',
            label(66, 452, "Fewer target calls did not mean lower latency in this scalar prototype", "heading"),
            label(66, 473, "INT8 draft, eight generated tokens. Bars share a per-axis scale, not a common unit.", "small"),
            label(90, 510, "target forward calls", "label"),
            label(675, 510, "median wall time (microseconds)", "label"),
        ]
    )
    call_rows = [(0, baseline_calls, baseline_us)] + [
        (window, int8_profiles[window]["target_forward_calls"], int8_profiles[window]["median_generation_us"])
        for window in [1, 2, 3]
    ]
    max_time = max(row[2] for row in call_rows)
    for index, (window, calls, wall_us) in enumerate(call_rows):
        y = 545 + index * 52
        name = "baseline" if window == 0 else f"draft window {window}"
        items.append(label(90, y + 18, name, "small"))
        call_width = calls / baseline_calls * 300
        items.append(bar(190, y, call_width, 25, "blue" if window == 0 else "green"))
        items.append(label(198 + call_width, y + 18, calls, "value"))
        wall_width = wall_us / max_time * 390
        items.append(bar(675, y, wall_width, 25, "blue" if window == 0 else "orange"))
        items.append(label(683 + wall_width, y + 18, f"{wall_us:.1f}", "value"))
    items.append(label(90, 742, "Window 3: target calls 8 → 2 (−75%), but measured latency 27.5 → 112.8 microseconds (0.24x).", "small"))
    items.append(label(90, 759, "Cause: the draft is the same architecture in lower precision and verification recomputes the sequence.", "small"))

    items.extend(
        [
            '<rect class="panel" x="42" y="793" width="1196" height="385" rx="10"/>',
            label(66, 827, "Rejection correction preserves the target distribution as draft quality falls", "heading"),
            label(66, 848, "10,000 seeded one-step samples per synthetic draft. Right-side bars show max absolute probability error.", "small"),
            label(90, 886, "proposal acceptance", "label"),
            label(675, 886, "corrected output error", "label"),
        ]
    )
    for index, item in enumerate(quality):
        y = 918 + index * 66
        acceptance = item["acceptance_rate_percent"]
        error_pp = item["maximum_target_probability_error"] * 100
        items.append(label(90, y + 20, item["name"], "small"))
        items.append(bar(180, y, acceptance / 100 * 370, 28, "green"))
        items.append(label(188 + acceptance / 100 * 370, y + 20, f"{acceptance:.2f}%", "value"))
        items.append(bar(675, y, error_pp / 1.0 * 370, 28, "purple"))
        items.append(label(683 + error_pp / 1.0 * 370, y + 20, f"{error_pp:.3f} pp", "value"))
        items.append(label(90, y + 42, f"accepted {item['accepted']:,} · rejected {item['rejected']:,}", "small"))
    items.append('<path class="guide" d="M 1045 903 L 1045 1095"/>')
    items.append(label(1045, 1114, "1 pp bound", "small", "middle"))
    real_errors = ", ".join(
        f"{item['draft_quantization'].upper()} {item['target_vs_speculative_maximum_error'] * 100:.2f} pp"
        for item in sampled
    )
    items.append(label(66, 1140, f"Real quantized-draft target-vs-speculative max error: {real_errors}; replay 4/4 for each.", "small"))
    items.append(label(66, 1159, "Correctness result: worse proposals lower acceptance; rejection sampling repairs the output law.", "small"))
    items.append("</svg>")
    args.output.write_text("\n".join(items) + "\n")


if __name__ == "__main__":
    main()
