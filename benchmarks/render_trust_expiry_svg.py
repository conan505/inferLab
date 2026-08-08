#!/usr/bin/env python3
"""Render a deterministic, data-driven v0.27 trust-expiry proof chart."""

from __future__ import annotations

import argparse
import html
import json
from pathlib import Path
from typing import Any


WIDTH = 1280
HEIGHT = 820


def load(directory: Path, name: str) -> dict[str, Any]:
    with (directory / name).open(encoding="utf-8") as source:
        value = json.load(source)
    if not isinstance(value, dict):
        raise SystemExit(f"{name} must contain an object")
    return value


def escape(value: Any) -> str:
    return html.escape(str(value), quote=True)


def text(
    parts: list[str],
    x: int,
    y: int,
    value: str,
    *,
    size: int = 16,
    fill: str = "#d8e7f2",
    weight: int = 400,
    anchor: str = "start",
) -> None:
    parts.append(
        f'<text x="{x}" y="{y}" font-size="{size}" fill="{fill}" '
        f'font-weight="{weight}" text-anchor="{anchor}">{escape(value)}</text>'
    )


def rounded_box(
    parts: list[str],
    x: int,
    y: int,
    width: int,
    height: int,
    *,
    fill: str = "#102738",
    stroke: str = "#2f5268",
) -> None:
    parts.append(
        f'<rect x="{x}" y="{y}" width="{width}" height="{height}" rx="14" '
        f'fill="{fill}" stroke="{stroke}" stroke-width="1.5"/>'
    )


def metric_card(
    parts: list[str], x: int, y: int, width: int, value: str, label: str
) -> None:
    rounded_box(parts, x, y, width, 86)
    text(parts, x + 18, y + 35, value, size=25, fill="#7ce4c3", weight=700)
    text(parts, x + 18, y + 63, label, size=14, fill="#9fb6c6")


def require_assertions(report: dict[str, Any], names: set[str]) -> None:
    if report.get("schema") != "inferlab.trust-expiry-assertions.v0.27":
        raise SystemExit("renderer requires v0.27 assertion schema")
    assertions = report.get("assertions")
    if not isinstance(assertions, list):
        raise SystemExit("renderer requires an assertion list")
    passed = {
        item.get("name")
        for item in assertions
        if isinstance(item, dict) and item.get("passed") is True
    }
    if (
        report.get("all_passed") is not True
        or report.get("passed") != report.get("total")
        or report.get("total") != len(assertions)
        or not names.issubset(passed)
    ):
        missing = sorted(names - passed)
        raise SystemExit(f"renderer refuses incomplete proof assertions: {missing}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    report = load(args.evidence_dir, "assertions.json")
    cutoff = load(args.evidence_dir, "request-time-cutoff-and-admitted-stream.json")
    g1 = load(args.evidence_dir, "generation-1-receipts.json")
    g2 = load(args.evidence_dir, "generation-2-receipts.json")
    durable_g1 = load(args.evidence_dir, "durable-generation-1.json")
    durable_g2 = load(args.evidence_dir, "durable-generation-2.json")
    final_request = load(args.evidence_dir, "final-request.json")
    final_stream = load(args.evidence_dir, "final-stream.json")

    attack_names = {
        "changing the signed expiry is rejected as an invalid snapshot",
        "a v2 expiry not later than issue time is rejected structurally",
        "a different valid deadline at the same generation is a fork, not renewal",
        "a future-issued authentic v2 policy fails startup before listening",
        "an authentic v2 policy above the receiver lifetime cap fails startup",
        "policy v1 is default-rejected as a legacy unbounded downgrade",
    }
    rejection_names = {
        "the same protected route is rejected at or after the exclusive expiry",
        "expiry is checked before missing authentication headers",
    }
    boundary_names = rejection_names | {
        "a valid signed gateway request beginning before expiry is accepted",
        "a real CPU SSE admitted before expiry deliberately completes after expiry",
    }
    recovery_names = {
        "a restart with only expired cache fails before listening without changing durable state",
        "three controls recover to valid higher-generation policy v2",
        "generation 2 advances every durable cache and rollback floor without deletion",
        "real CPU JSON inference succeeds after valid generation-2 recovery",
        "real CPU SSE reaches the terminal DONE event after recovery",
        "seven exact production regressions pass activation, receipt, remote/local clock, retry, and 304 cases",
    }
    required = attack_names | boundary_names | recovery_names | {
        "three exact controls activate valid v2 generation 1 over mutual TLS",
        "the distributor retains three structurally signed generation-1 receipts",
        "a 304 Not Modified retains the exact generation-1 signed deadline",
        "all three live controls report expired generation 1 with zero remaining time",
    }
    require_assertions(report, required)

    expiry = cutoff.get("expires_at_ms")
    pre = cutoff.get("pre_expiry_signed_request", {})
    post = cutoff.get("post_expiry_signed_request", {})
    stream = cutoff.get("pre_expiry_stream", {})
    if not all(isinstance(value, int) for value in [
        expiry,
        pre.get("started_at_ms"),
        post.get("started_at_ms"),
        stream.get("started_at_ms"),
        stream.get("completed_at_ms"),
    ]):
        raise SystemExit("renderer requires integer cutoff wall-clock observations")

    pre_delta = expiry - pre["started_at_ms"]
    post_delta = post["started_at_ms"] - expiry
    stream_before = expiry - stream["started_at_ms"]
    stream_after = stream["completed_at_ms"] - expiry
    if min(pre_delta, post_delta, stream_before, stream_after) < 0:
        raise SystemExit("renderer refuses an inverted expiry timeline")

    g1_body = g1.get("status", {}).get("body", {})
    g2_body = g2.get("status", {}).get("body", {})
    g1_receipts = g1_body.get("receipt_count")
    g2_receipts = g2_body.get("receipt_count")
    if g1_receipts != 3 or g2_receipts != 3:
        raise SystemExit("renderer requires exact three-receiver g1/g2 evidence")

    g1_nodes = durable_g1.get("nodes", {})
    g2_nodes = durable_g2.get("nodes", {})
    if set(g1_nodes) != {"control-a", "control-b", "control-c"} or set(g2_nodes) != set(g1_nodes):
        raise SystemExit("renderer requires exact three-control durable evidence")
    g1_lifetime = next(iter(g1_nodes.values()))["expires_at_ms"] - next(iter(g1_nodes.values()))["issued_at_ms"]
    g2_lifetime = next(iter(g2_nodes.values()))["expires_at_ms"] - next(iter(g2_nodes.values()))["issued_at_ms"]

    json_ms = final_request.get("duration_ms")
    sse_ms = final_stream.get("duration_ms")
    if not isinstance(json_ms, (int, float)) or not isinstance(sse_ms, (int, float)):
        raise SystemExit("renderer requires measured final JSON and SSE durations")

    attack_count = len(attack_names)
    boundary_rejections = len(rejection_names)
    assertion_count = report["total"]
    description = (
        f"InferLab v0.27 proof: {assertion_count} assertions pass; "
        f"{attack_count} invalid or downgrade candidates are rejected; "
        f"{boundary_rejections} post-expiry authentication paths return 401; "
        "a pre-expiry stream completes after the deadline; seven exact production "
        "regressions cover activation, receipt, and clock edges; generation 2 restores service."
    )

    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}" role="img" aria-labelledby="title desc">',
        '<title id="title">InferLab v0.27 signed service-trust expiry proof</title>',
        f'<desc id="desc">{escape(description)}</desc>',
        '<rect width="1280" height="820" fill="#071520"/>',
        '<defs><linearGradient id="header" x1="0" x2="1"><stop stop-color="#12364a"/><stop offset="1" stop-color="#102131"/></linearGradient></defs>',
        '<rect x="34" y="28" width="1212" height="106" rx="18" fill="url(#header)" stroke="#35647b"/>',
    ]
    text(parts, 62, 69, "INFERLAB · RELEASE v0.27", size=15, fill="#7ce4c3", weight=700)
    text(parts, 62, 104, "Signed policy validity ends at admission, not mid-stream", size=27, fill="#f1f7fb", weight=700)
    text(parts, 1216, 70, f"{report['passed']} / {report['total']}", size=24, fill="#7ce4c3", weight=700, anchor="end")
    text(parts, 1216, 100, "deterministic assertions", size=13, fill="#9fb6c6", anchor="end")

    metric_card(parts, 34, 154, 224, f"g1 · {g1_receipts}/3", f"v2 · {g1_lifetime / 1000:.0f}s signed window")
    metric_card(parts, 274, 154, 224, f"{attack_count}/{attack_count}", "tamper · window · fork · downgrade")
    metric_card(
        parts,
        514,
        154,
        224,
        f"{boundary_rejections}/{boundary_rejections} → 401",
        "signed + missing after expiry",
    )
    metric_card(parts, 754, 154, 224, f"g2 · {g2_receipts}/3", f"recovered · {g2_lifetime / 1000:.0f}s window")
    metric_card(parts, 994, 154, 252, f"{json_ms:.1f} / {sse_ms:.1f} ms", "real CPU JSON / final SSE")

    rounded_box(parts, 34, 260, 1212, 224, fill="#0c202f")
    text(parts, 58, 293, "EXCLUSIVE DEADLINE E", size=15, fill="#7ce4c3", weight=700)
    axis_y = 369
    axis_x1, axis_x2 = 96, 1184
    expiry_x = 640
    parts.append(f'<line x1="{axis_x1}" y1="{axis_y}" x2="{axis_x2}" y2="{axis_y}" stroke="#5f7e91" stroke-width="3"/>')
    parts.append(f'<line x1="{expiry_x}" y1="318" x2="{expiry_x}" y2="440" stroke="#ffbd6b" stroke-width="3"/>')
    text(parts, expiry_x, 313, "E = signed expires_at_ms", size=14, fill="#ffcf8f", weight=700, anchor="middle")

    points = [
        (160, "SSE starts", f"E − {stream_before} ms", "#73b7ff"),
        (430, "signed read accepted", f"E − {pre_delta} ms", "#7ce4c3"),
        (780, "new reads rejected", f"E + {post_delta} ms", "#ff8b8b"),
        (1070, "same SSE gets [DONE]", f"E + {stream_after} ms", "#73b7ff"),
    ]
    for x, label, delta, color in points:
        parts.append(f'<circle cx="{x}" cy="{axis_y}" r="8" fill="{color}"/>')
        text(parts, x, axis_y + 35, label, size=14, fill=color, weight=700, anchor="middle")
        text(parts, x, axis_y + 57, delta, size=13, fill="#a9bfcc", anchor="middle")
    text(parts, 58, 462, "The stream crosses E because it was already admitted; expiry gates only a new protected request.", size=14, fill="#b8cad5")

    rounded_box(parts, 34, 506, 780, 270, fill="#0c202f")
    text(parts, 58, 540, "WITHHOLDING → FAIL CLOSED → RECOVERY", size=15, fill="#7ce4c3", weight=700)
    stages = [
        (82, "1", "g1 valid", "3 receipts"),
        (245, "2", "304", "deadline unchanged"),
        (408, "3", "g1 expired", "new auth → 401"),
        (571, "4", "cache restart", "fails before listener"),
        (734, "5", "g2 valid", "3 controls recover"),
    ]
    line_y = 615
    parts.append('<line x1="110" y1="615" x2="754" y2="615" stroke="#42677c" stroke-width="3"/>')
    for x, number, label, detail in stages:
        parts.append(f'<circle cx="{x}" cy="{line_y}" r="22" fill="#16394c" stroke="#68b99f" stroke-width="2"/>')
        text(parts, x, line_y + 6, number, size=15, fill="#e7f4f8", weight=700, anchor="middle")
        text(parts, x, 659, label, size=14, fill="#dceaf1", weight=700, anchor="middle")
        text(parts, x, 681, detail, size=12, fill="#9fb6c6", anchor="middle")
    text(parts, 58, 735, "Signature authenticity remains true; receiver authority changes with the signed wall-clock window.", size=14, fill="#b8cad5")

    rounded_box(parts, 836, 506, 410, 270, fill="#102738")
    text(parts, 860, 540, "PROVEN BOUNDARY", size=15, fill="#7ce4c3", weight=700)
    bullets = [
        "✓ v2 deadline is signature-bound",
        "✓ E is exclusive: now ≥ E rejects",
        "✓ 304 and download time do not renew",
        "✓ backward clock does not reopen in-process",
        "✓ expired activation emits no receipt",
        "✓ expired cache cannot bootstrap",
        "✓ higher generation restores authority",
        "○ no in-flight cancellation or cert revocation",
    ]
    for index, bullet in enumerate(bullets):
        fill = "#d9e8ef" if bullet.startswith("✓") else "#ffcf8f"
        text(parts, 862, 579 + index * 27, bullet, size=14, fill=fill)

    parts.append('</svg>')
    args.output.write_text("\n".join(parts) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
