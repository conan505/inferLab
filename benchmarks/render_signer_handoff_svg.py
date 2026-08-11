#!/usr/bin/env python3
"""Render the deterministic v0.29 signer handoff evidence chart."""

from __future__ import annotations

import argparse
import html
import json
from pathlib import Path
from typing import Any


REQUIRED_ASSERTIONS = {
    "proof contract fixes six processes, four services and isolated ports",
    "g1 and g2 are exact root-signed overlap and revocation policies",
    "invalid startup bundles fail closed before a listener or state file",
    "generation one starts on A with one healthy Raft leader and revision two",
    "generation one converges by service ID with three cryptographic A receipts",
    "every deterministic live source failure retains and recovers the exact LKG",
    "sequential follower follower leader gateway handoff keeps exact process identities",
    "authenticated peer and gateway traffic continues through the sequential handoff",
    "signer-only handoff leaves all generation-one A receipts byte-identical",
    "generation two activates B and preserves revision-two quorum",
    "generation two converges by service ID with three cryptographic B receipts",
    "revoked A cannot read vote or reactivate while valid B still reads",
    "revision three remains a healthy B-authenticated three-control commit",
    "one leader term and monotonic quorum state span g1 handoff g2 and r3",
    "final gateway is bundle generation two on B and routing revision three",
    "real CPU JSON completes through revision three with one attempt",
    "real CPU SSE is incremental and ends with DONE followed by EOF",
    "all exact nonce handoff LKG watcher and convergence regressions run one test",
    "six owned process identities are unchanged from A through B and r3",
    "manifest is exact hash size schema bound when required",
}


def load(directory: Path, name: str) -> dict[str, Any]:
    with (directory / name).open(encoding="utf-8") as source:
        return json.load(source)


def esc(value: Any) -> str:
    return html.escape(str(value), quote=True)


def text(x: int, y: int, value: Any, *, size: int = 14, weight: int = 400, fill: str = "#182230", anchor: str = "start") -> str:
    return (
        f'<text x="{x}" y="{y}" font-family="Inter, ui-sans-serif, system-ui, sans-serif" '
        f'font-size="{size}" font-weight="{weight}" fill="{fill}" text-anchor="{anchor}">{esc(value)}</text>'
    )


def box(x: int, y: int, width: int, height: int, *, fill: str, stroke: str = "#cbd5e1", radius: int = 12) -> str:
    return f'<rect x="{x}" y="{y}" width="{width}" height="{height}" rx="{radius}" fill="{fill}" stroke="{stroke}"/>'


def arrow(x1: int, y1: int, x2: int, y2: int, color: str = "#64748b") -> str:
    return f'<path d="M{x1} {y1} L{x2} {y2}" fill="none" stroke="{color}" stroke-width="2" marker-end="url(#arrow)"/>'


def card(x: int, value: str, label_top: str, label_bottom: str = "") -> list[str]:
    parts = [box(x, 96, 205, 94, fill="#ffffff")]
    parts.append(text(x + 102, 132, value, size=26, weight=500, fill="#0f766e", anchor="middle"))
    parts.append(text(x + 102, 159, label_top, size=13, fill="#475569", anchor="middle"))
    if label_bottom:
        parts.append(text(x + 102, 178, label_bottom, size=13, fill="#475569", anchor="middle"))
    return parts


def assertion_observation(report: dict[str, Any], name: str) -> dict[str, Any]:
    for item in report["assertions"]:
        if item["name"] == name:
            return item.get("observations", {})
    raise ValueError(f"missing assertion {name}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    report = load(args.evidence_dir, "assertions.json")
    handoff = load(args.evidence_dir, "handoff-sequence.json")
    receipts_one = load(args.evidence_dir, "generation-1-receipts.json")
    receipts_two = load(args.evidence_dir, "generation-2-receipts.json")
    tests = load(args.evidence_dir, "production-tests.json")
    continuity = load(args.evidence_dir, "process-continuity.json")

    if report.get("failed") != 0 or report.get("passed") != report.get("total"):
        raise SystemExit("renderer requires an all-passing assertion report")
    names = {item.get("name") for item in report.get("assertions", []) if item.get("passed") is True}
    missing = REQUIRED_ASSERTIONS - names
    if missing:
        raise SystemExit(f"renderer is missing required passing assertions: {sorted(missing)}")
    if len(handoff.get("steps", [])) != 4:
        raise SystemExit("renderer requires four exact handoff steps")
    if receipts_one["result"]["body"]["receipt_count"] != 3 or receipts_two["result"]["body"]["receipt_count"] != 3:
        raise SystemExit("renderer requires exact three-receipt convergence")
    if tests.get("test_count") != 11:
        raise SystemExit("renderer requires eleven exact production regressions")
    if continuity.get("unchanged") is not True:
        raise SystemExit("renderer requires exact process continuity")

    json_observation = assertion_observation(report, "real CPU JSON completes through revision three with one attempt")
    sse_observation = assertion_observation(report, "real CPU SSE is incremental and ends with DONE followed by EOF")
    json_duration = json_observation.get("duration_ms")
    sse_duration = sse_observation.get("duration_ms")
    sse_pieces = sse_observation.get("piece_count")

    svg: list[str] = [
        '<svg xmlns="http://www.w3.org/2000/svg" width="1200" height="840" viewBox="0 0 1200 840" role="img" aria-labelledby="title desc">',
        '<title id="title">InferLab v0.29 restart-free service signer handoff proof</title>',
        '<desc id="desc">Six unchanged processes switch four service signers from credential A to B, converge through service-scoped receipts, reject revoked A, and complete real CPU JSON and SSE at revision three.</desc>',
        '<defs><marker id="arrow" markerWidth="8" markerHeight="8" refX="7" refY="4" orient="auto"><path d="M0,0 L8,4 L0,8 z" fill="#64748b"/></marker></defs>',
        '<rect width="1200" height="840" fill="#f8fafc"/>',
        text(48, 48, "v0.29 · restart-free service-signing handoff", size=26, weight=500),
        text(48, 75, "Exact local proof · real distributor, 3-control Raft quorum, gateway and CPU worker", size=14, fill="#475569"),
    ]

    for item in card(48, f"{report['passed']}/{report['total']}", "exact assertions"):
        svg.append(item)
    for item in card(277, "6", "unchanged OS processes"):
        svg.append(item)
    for item in card(506, "4", "service signers", "A → B sequentially"):
        svg.append(item)
    for item in card(735, "3 + 3", "service-scoped receipts", "g1 A · g2 B"):
        svg.append(item)
    for item in card(964, "11", "exact regressions"):
        svg.append(item)

    svg.extend([
        text(48, 230, "Handoff timeline", size=18, weight=500),
        text(48, 253, "Every step preserves the same PID, parent, start token, command, leader, quorum and revision 2.", size=13, fill="#475569"),
    ])
    x_positions = [65, 280, 495, 710, 925]
    labels = [
        ("g1 · bundle 1", "all services use A"),
        ("follower 1", "A → B"),
        ("follower 2", "A → B"),
        ("leader", "A → B"),
        ("gateway", "A → B"),
    ]
    for index, (x, (top, bottom)) in enumerate(zip(x_positions, labels)):
        svg.append(box(x, 275, 180, 78, fill="#ecfeff" if index else "#f1f5f9", stroke="#94a3b8"))
        svg.append(text(x + 90, 307, top, size=15, weight=500, anchor="middle"))
        svg.append(text(x + 90, 332, bottom, size=13, fill="#475569", anchor="middle"))
        if index < len(labels) - 1:
            svg.append(arrow(x + 180, 314, x_positions[index + 1] - 8, 314))

    svg.extend([
        text(48, 398, "Authority and rejection boundary", size=18, weight=500),
        box(48, 420, 310, 138, fill="#ffffff"),
        text(68, 451, "Trust generation 1", size=16, weight=500),
        text(68, 479, "A + B trusted for 4 services", size=13, fill="#475569"),
        text(68, 503, "3 control receipts signed by A", size=13, fill="#475569"),
        text(68, 527, "Signer-only switch creates no receipt", size=13, fill="#0f766e"),
        arrow(366, 489, 430, 489),
        box(438, 420, 310, 138, fill="#ecfeff", stroke="#5eead4"),
        text(458, 451, "Trust generation 2", size=16, weight=500),
        text(458, 479, "A + B remain signature-bound", size=13, fill="#475569"),
        text(458, 503, "all */key-a credentials revoked", size=13, fill="#475569"),
        text(458, 527, "3 control receipts signed by B", size=13, fill="#0f766e"),
        arrow(756, 489, 820, 489),
        box(828, 420, 324, 138, fill="#fff7ed", stroke="#fdba74"),
        text(848, 451, "Fail-closed after revocation", size=16, weight=500),
        text(848, 479, "old-A gateway read → 401", size=13, fill="#7c2d12"),
        text(848, 503, "old-A high-term vote → 401", size=13, fill="#7c2d12"),
        text(848, 527, "revoked-A bundle → B LKG", size=13, fill="#7c2d12"),
    ])

    svg.extend([
        text(48, 608, "End-to-end completion after B", size=18, weight=500),
        box(48, 630, 1104, 116, fill="#ffffff"),
        text(72, 664, "revision 3", size=16, weight=500, fill="#0f766e"),
        text(72, 690, "B-authenticated control read", size=13, fill="#475569"),
        arrow(250, 681, 330, 681),
        text(350, 664, "gateway", size=16, weight=500),
        text(350, 690, "one attempt · CPU route", size=13, fill="#475569"),
        arrow(525, 681, 605, 681),
        text(625, 664, "real CPU worker", size=16, weight=500),
        text(625, 690, "JSON + incremental SSE", size=13, fill="#475569"),
        arrow(820, 681, 900, 681),
        text(920, 664, "DONE + EOF", size=16, weight=500, fill="#0f766e"),
        text(920, 690, f"JSON {json_duration} ms · SSE {sse_duration} ms", size=13, fill="#475569"),
        text(920, 715, f"{sse_pieces} content pieces", size=13, fill="#475569"),
        text(48, 790, "Boundary: this proves new request signing and convergence without process restart; it does not rotate mTLS certificates, CAs or in-flight signatures.", size=13, fill="#475569"),
        text(48, 818, "Evidence is redacted, checker-replayable, SVG-replayable and manifest-last SHA-256 bound.", size=13, fill="#475569"),
        "</svg>",
    ])
    args.output.write_text("\n".join(svg) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
