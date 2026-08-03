#!/usr/bin/env python3
"""Render retained v0.10 processor, distribution, DFA, and validity evidence."""

from __future__ import annotations

import argparse
import html
import json
from pathlib import Path

WIDTH = 1280
HEIGHT = 1120


def label(x, y, value, css="label", anchor="start") -> str:
    return (
        f'<text x="{x:.1f}" y="{y:.1f}" class="{css}" '
        f'text-anchor="{anchor}">{html.escape(str(value))}</text>'
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", type=Path, required=True)
    parser.add_argument("--probe", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    check = json.loads(args.check.read_text())
    probe = json.loads(args.probe.read_text())
    cases = probe["processor_cases"]
    distributions = probe["temperature_distributions"]
    structured = probe["structured"]

    items = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}" role="img" aria-labelledby="title description">',
        '<title id="title">InferLab v0.10 sampling and structured decoding proof</title>',
        '<desc id="description">Golden logit processor candidate counts, observed versus theoretical temperature distributions, JSON grammar automaton states, ten-thousand-sample validity, and enum output counts.</desc>',
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
        .grid{stroke:var(--grid);stroke-width:1} .arrow{stroke:var(--muted);stroke-width:1.5;fill:none}
        .blue{fill:var(--blue)} .green{fill:var(--green)} .orange{fill:var(--orange)}
        .purple{fill:var(--purple)} .red{fill:var(--red)}
        .expected{fill:none;stroke:var(--fg);stroke-width:1.5}
        .state{fill:var(--panel);stroke:var(--blue);stroke-width:2}
        </style>""",
        f'<rect class="background" width="{WIDTH}" height="{HEIGHT}"/>',
        label(52, 48, "v0.10 · sampling and structured decoding", "title"),
        label(
            52,
            74,
            f"{check['assertions_passed']}/{check['assertions_total']} checks · deterministic processors, seeded distributions, token DFA, and gateway JSON",
            "subtitle",
        ),
        '<rect class="panel" x="42" y="98" width="1196" height="225" rx="10"/>',
        label(66, 131, "Each processor changes the candidate support", "heading"),
        label(66, 152, "Synthetic logits [1, 4, 3, 2]; bar length is the number of tokens still selectable.", "small"),
    ]

    max_candidates = max(case["selection"]["candidate_count"] for case in cases)
    for index, case in enumerate(cases):
        column = index % 3
        row = index // 3
        x = 82 + column * 385
        y = 184 + row * 64
        candidates = case["selection"]["candidate_count"]
        width = candidates / max_candidates * 215
        items.append(label(x, y, case["name"], "label"))
        items.append(f'<rect class="blue" x="{x}" y="{y + 12}" width="{width:.1f}" height="20" rx="3"/>')
        items.append(label(x + width + 8, y + 27, f"{candidates} → token {case['selection']['token_id']}", "value"))

    items.extend(
        [
            '<rect class="panel" x="42" y="343" width="1196" height="300" rx="10"/>',
            label(66, 376, "Temperature changes probability—not validity", "heading"),
            label(66, 397, "Filled bars are 10,000 seeded samples; outlines are exact softmax probabilities.", "small"),
        ]
    )
    chart_top = 432
    chart_height = 155
    for panel, distribution in enumerate(distributions):
        left = 100 + panel * 385
        items.append(label(left + 130, 420, f"temperature {distribution['temperature']:.1f}", "label", "middle"))
        for token, (expected, observed) in enumerate(
            zip(
                distribution["expected_probability"],
                distribution["observed_probability"],
            )
        ):
            x = left + token * 82
            observed_height = observed * chart_height
            expected_height = expected * chart_height
            items.append(f'<rect class="green" x="{x}" y="{chart_top + chart_height - observed_height:.1f}" width="42" height="{observed_height:.1f}" rx="2"/>')
            items.append(f'<rect class="expected" x="{x - 4}" y="{chart_top + chart_height - expected_height:.1f}" width="50" height="{expected_height:.1f}"/>')
            items.append(label(x + 21, chart_top + chart_height + 20, f"token {token}", "small", "middle"))
            items.append(label(x + 21, chart_top + chart_height - observed_height - 8, f"{observed * 100:.1f}%", "value", "middle"))
        items.append(label(left + 105, 625, f"max error {distribution['maximum_absolute_probability_error'] * 100:.2f} pp", "small", "middle"))

    items.extend(
        [
            '<rect class="panel" x="42" y="663" width="1196" height="190" rx="10"/>',
            label(66, 696, "The JSON schema compiles to seven deterministic states", "heading"),
            label(66, 717, "Allowed token counts are 1, 3, 1, 3, 1, 1; EOS reaches the accepting state.", "small"),
        ]
    )
    state_names = ["start", "answer", "separator", "confidence", "close", "EOS", "accept"]
    allowed_counts = [1, 3, 1, 3, 1, 1, 0]
    for index, (name, count) in enumerate(zip(state_names, allowed_counts)):
        x = 98 + index * 162
        if index < len(state_names) - 1:
            items.append(f'<path class="arrow" d="M {x + 94} 779 L {x + 150} 779"/>')
            items.append(f'<path class="arrow" d="M {x + 143} 773 L {x + 150} 779 L {x + 143} 785"/>')
        items.append(f'<circle class="state" cx="{x + 47}" cy="779" r="38"/>')
        items.append(label(x + 47, 775, f"q{index}", "value", "middle"))
        items.append(label(x + 47, 792, name, "small", "middle"))
        items.append(label(x + 47, 832, f"allowed {count}", "small", "middle"))

    items.extend(
        [
            '<rect class="panel" x="42" y="873" width="1196" height="205" rx="10"/>',
            label(66, 906, "Grammar guarantees validity; the model still shapes content", "heading"),
            label(66, 927, "All samples parse and satisfy the schema, while answer preferences remain highly skewed.", "small"),
        ]
    )
    validity_width = structured["schema_valid"] / structured["samples"] * 370
    items.append(label(82, 960, "schema-valid", "label"))
    items.append(f'<rect class="green" x="190" y="944" width="{validity_width:.1f}" height="26" rx="3"/>')
    items.append(label(570, 963, f"{structured['schema_valid']:,} / {structured['samples']:,}", "value", "end"))

    answer_max = max(structured["answer_counts"].values())
    x = 675
    items.append(label(x, 960, "answer enum counts", "label"))
    for index, (answer, count) in enumerate(structured["answer_counts"].items()):
        y = 979 + index * 27
        width = count / answer_max * 330
        items.append(label(x, y + 15, answer, "small"))
        items.append(f'<rect class="purple" x="{x + 76}" y="{y}" width="{width:.1f}" height="18" rx="2"/>')
        items.append(label(x + 84 + width, y + 14, f"{count:,}", "value"))
    items.append(label(82, 1010, f"distinct valid objects: {structured['distinct_outputs']}", "small"))
    items.append(label(82, 1034, f"seed replay: {structured['replay_matches']} / {structured['replay_checks']}", "small"))
    items.append(label(82, 1058, "Guarantee: syntax and enum membership. Non-guarantee: balanced semantic choices.", "small"))
    items.append("</svg>")
    args.output.write_text("\n".join(items) + "\n")


if __name__ == "__main__":
    main()
