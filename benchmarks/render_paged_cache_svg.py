#!/usr/bin/env python3
"""Render v0.9 capacity, fragmentation, sharing, and ownership evidence."""

from __future__ import annotations

import argparse
import html
import json
from pathlib import Path

WIDTH = 1280
HEIGHT = 1060


def text(x, y, value, css_class="label", anchor="start"):
    return (
        f'<text x="{x:.1f}" y="{y:.1f}" class="{css_class}" '
        f'text-anchor="{anchor}">{html.escape(str(value))}</text>'
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", type=Path, required=True)
    parser.add_argument("--page-probe", type=Path, required=True)
    parser.add_argument("--prefix-probe", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    check = json.loads(args.check.read_text())
    pages = json.loads(args.page_probe.read_text())
    prefixes = json.loads(args.prefix_probe.read_text())
    capacity = pages["capacity"]
    fragmentation = pages["fragmentation"]
    sharing_stages = [
        ("cold retained", pages["sharing"]["after_cold_release"]),
        ("two warm share", pages["sharing"]["two_warm_sessions_before_decode"]),
        ("after COW", pages["sharing"]["after_copy_on_write"]),
    ]
    topology = prefixes["topology"]
    cold_kv = sum(pair["cold"]["generation"]["kv_tokens"] for pair in prefixes["prefix_pairs"])
    warm_kv = sum(pair["warm"]["generation"]["kv_tokens"] for pair in prefixes["prefix_pairs"])

    elements = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" '
        f'viewBox="0 0 {WIDTH} {HEIGHT}" role="img" aria-labelledby="title description">',
        '<title id="title">InferLab v0.9 paged KV cache proof</title>',
        '<desc id="description">Concurrent sequence capacity, internal fragmentation by page size, shared-prefix memory before and after copy-on-write, warm prefix work, and consistent-hash topology remapping.</desc>',
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
        .grid{stroke:var(--grid);stroke-width:1}
        .blue{fill:var(--blue)} .green{fill:var(--green)} .orange{fill:var(--orange)}
        .purple{fill:var(--purple)} .red{fill:var(--red)}
        .frag-line{fill:none;stroke:var(--purple);stroke-width:2.5}
        .frag-dot{fill:var(--purple);stroke:var(--panel);stroke-width:2}
        </style>""",
        f'<rect class="background" width="{WIDTH}" height="{HEIGHT}"/>',
        text(52, 48, "v0.9 · paged KV cache and prefix ownership", "title"),
        text(
            52,
            74,
            f"{check['assertions_passed']}/{check['assertions_total']} checks · layout parity, bounded allocation, sharing, copy-on-write, eviction, and affinity",
            "subtitle",
        ),
        '<rect class="panel" x="42" y="98" width="1196" height="300" rx="10"/>',
        text(66, 130, "Capacity and page-size tradeoff", "heading"),
        text(66, 151, "Same 64 token slots; short sessions use only the pages their prefixes need.", "small"),
        text(82, 188, "Concurrent 8-token sessions", "label"),
    ]

    capacity_values = [
        ("max-context reservation", capacity["contiguous_max_context_reservation_sessions"], "orange"),
        ("4-token pages", capacity["paged_concurrent_sessions"], "green"),
    ]
    for index, (label, value, css) in enumerate(capacity_values):
        y = 220 + index * 70
        width = value / capacity["paged_concurrent_sessions"] * 390
        elements.append(text(82, y, label, "small"))
        elements.append(f'<rect class="{css}" x="82" y="{y + 12}" width="{width:.1f}" height="28" rx="4"/>')
        elements.append(text(82 + width + 10, y + 33, f"{value} sessions", "value"))
    elements.append(
        text(
            82,
            365,
            f"{capacity['capacity_gain']:.1f}× capacity under the declared short-sequence workload",
            "small",
        )
    )

    frag_left, frag_top, frag_width, frag_height = 710, 190, 455, 145
    elements.append(text(frag_left, 172, "Internal fragmentation", "label"))
    for tick in [0, 10, 20, 30, 40]:
        y = frag_top + frag_height - tick / 40 * frag_height
        elements.append(f'<line class="grid" x1="{frag_left}" y1="{y:.1f}" x2="{frag_left + frag_width}" y2="{y:.1f}"/>')
        elements.append(text(frag_left - 10, y + 4, f"{tick}%", "small", "end"))
    points = []
    for index, item in enumerate(fragmentation):
        x = frag_left + index / (len(fragmentation) - 1) * frag_width
        y = frag_top + frag_height - item["internal_fragmentation_percent"] / 40 * frag_height
        points.append((x, y, item))
        elements.append(text(x, frag_top + frag_height + 23, item["page_tokens"], "small", "middle"))
    path = " ".join(("M" if i == 0 else "L") + f" {x:.1f} {y:.1f}" for i, (x, y, _) in enumerate(points))
    elements.append(f'<path class="frag-line" d="{path}"/>')
    for x, y, item in points:
        elements.append(f'<circle class="frag-dot" cx="{x:.1f}" cy="{y:.1f}" r="6"/>')
        elements.append(text(x, y - 12, f"{item['internal_fragmentation_percent']:.1f}%", "value", "middle"))
    elements.append(text(frag_left + frag_width / 2, 377, "tokens per page", "small", "middle"))

    elements.extend([
        '<rect class="panel" x="42" y="418" width="1196" height="270" rx="10"/>',
        text(66, 451, "Shared prefix, then copy-on-write", "heading"),
        text(66, 472, "Logical referenced bytes count each owner; physical bytes count stored rows once.", "small"),
    ])
    max_memory = max(stage[1]["logical_referenced_bytes"] for stage in sharing_stages)
    group_left, group_width = 120, 1020 / len(sharing_stages)
    for index, (label, stats) in enumerate(sharing_stages):
        center = group_left + group_width * (index + 0.5)
        physical_height = stats["physical_used_bytes"] / max_memory * 135
        logical_height = stats["logical_referenced_bytes"] / max_memory * 135
        baseline = 635
        elements.append(f'<rect class="blue" x="{center - 58:.1f}" y="{baseline - physical_height:.1f}" width="48" height="{physical_height:.1f}" rx="3"/>')
        elements.append(f'<rect class="green" x="{center + 10:.1f}" y="{baseline - logical_height:.1f}" width="48" height="{logical_height:.1f}" rx="3"/>')
        elements.append(text(center - 34, baseline - physical_height - 9, stats["physical_used_bytes"], "value", "middle"))
        elements.append(text(center + 34, baseline - logical_height - 9, stats["logical_referenced_bytes"], "value", "middle"))
        elements.append(text(center, 661, label, "label", "middle"))
        elements.append(text(center, 678, f"pages {stats['allocated_pages']} · refs {stats['live_references']}", "small", "middle"))
    elements.append(text(1070, 466, "physical", "small"))
    elements.append('<rect class="blue" x="1045" y="456" width="14" height="10"/>')
    elements.append(text(1165, 466, "logical", "small"))
    elements.append('<rect class="green" x="1140" y="456" width="14" height="10"/>')

    elements.extend([
        '<rect class="panel" x="42" y="708" width="1196" height="300" rx="10"/>',
        text(66, 741, "Prefix work and stable ownership", "heading"),
        text(66, 762, "Six cold/warm prompt pairs through the two-worker gateway; 256 keys before and after adding worker C.", "small"),
        text(82, 797, "K/V token projections across six pairs", "label"),
    ])
    maximum_kv = max(cold_kv, warm_kv)
    for index, (label, value, css) in enumerate([
        ("cold", cold_kv, "orange"),
        ("warm prefix hit", warm_kv, "green"),
    ]):
        y = 825 + index * 58
        width = value / maximum_kv * 420
        elements.append(text(82, y + 17, label, "small"))
        elements.append(f'<rect class="{css}" x="195" y="{y}" width="{width:.1f}" height="24" rx="3"/>')
        elements.append(text(205 + width, y + 17, value, "value"))

    remapped = topology["remapped_keys"]
    unchanged = topology["keys"] - remapped
    total_width = 440
    unchanged_width = unchanged / topology["keys"] * total_width
    remapped_width = remapped / topology["keys"] * total_width
    elements.append(text(710, 797, "Ownership after adding worker C", "label"))
    elements.append(f'<rect class="blue" x="710" y="825" width="{unchanged_width:.1f}" height="32" rx="3"/>')
    elements.append(f'<rect class="purple" x="{710 + unchanged_width:.1f}" y="825" width="{remapped_width:.1f}" height="32" rx="3"/>')
    elements.append(text(710 + unchanged_width / 2, 846, f"unchanged {unchanged}", "value", "middle"))
    elements.append(text(710 + unchanged_width + remapped_width / 2, 846, f"to C {remapped}", "value", "middle"))
    elements.append(text(710, 880, f"remapped {topology['remapped_fraction'] * 100:.1f}% · every moved key went only to the added worker", "small"))
    count_x = 710
    for worker, count in sorted(topology["three_worker_counts"].items()):
        elements.append(text(count_x, 923, f"{worker}: {count}", "value"))
        count_x += 150
    elements.append(text(710, 963, "Same key → same owner before change; A/B never exchange ownership when C joins.", "small"))
    elements.append("</svg>")
    args.output.write_text("\n".join(elements) + "\n")


if __name__ == "__main__":
    main()
