#!/usr/bin/env python3
"""Render a deterministic, data-driven v0.24 mTLS trust evidence chart."""

from __future__ import annotations

import argparse
import json
from html import escape
from pathlib import Path
from typing import Any


def load(directory: Path, name: str) -> dict[str, Any]:
    with (directory / name).open(encoding="utf-8") as source:
        return json.load(source)


def text(x: int, y: int, value: str, css: str = "body", anchor: str = "start") -> str:
    return f'<text x="{x}" y="{y}" class="{css}" text-anchor="{anchor}">{escape(value)}</text>'


def card(x: int, y: int, width: int, title: str, value: str, detail: str, css: str) -> str:
    return "".join(
        [
            f'<rect x="{x}" y="{y}" width="{width}" height="125" rx="14" class="{css}"/>',
            text(x + width // 2, y + 31, title, "card-title", "middle"),
            text(x + width // 2, y + 70, value, "card-value", "middle"),
            text(x + width // 2, y + 101, detail, "card-detail", "middle"),
        ]
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    handshake = load(args.evidence_dir, "tls-handshake.json")
    g1 = load(args.evidence_dir, "generation-1-receipts.json")
    g2_controls = load(args.evidence_dir, "generation-2-convergence.json")
    g2 = load(args.evidence_dir, "generation-2-receipts.json")
    transport_attempts = [
        load(args.evidence_dir, name)
        for name in [
            "plaintext-downgrade.json",
            "no-client-certificate.json",
            "rogue-client-ca.json",
            "wrong-server-ca.json",
            "wrong-server-hostname.json",
        ]
    ]
    application_attempts = [
        load(args.evidence_dir, "tampered-snapshot.json"),
        load(args.evidence_dir, "forged-receipt.json"),
    ]
    restart = load(args.evidence_dir, "cache-restart.json")
    request = load(args.evidence_dir, "request.json")
    stream = load(args.evidence_dir, "stream.json")
    assertions = load(args.evidence_dir, "assertions.json")

    g1_body = g1["status"]["body"]
    g2_body = g2["status"]["body"]
    transport_failures = sum(
        item.get("failed_before_http_response") is True for item in transport_attempts
    )
    application_codes = [
        item.get("observation", {}).get("body", {}).get("error", {}).get("code")
        for item in application_attempts
    ]
    application_rejections = sum(
        observed == expected
        for observed, expected in zip(
            application_codes,
            ["invalid_snapshot", "invalid_receipt_signature"],
        )
    )
    parts = [
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 850" role="img" aria-labelledby="title desc">',
        '<title id="title">InferLab v0.24 mutual-TLS trust distribution proof</title>',
        f'<desc id="desc">TLS 1.3 mutual authentication protects the trust-distributor channel. {transport_failures} of {len(transport_attempts)} transport attacks fail before HTTP and {application_rejections} of {len(application_attempts)} application attacks receive the expected signature rejection; generation two converges with three receipts, a receiver restarts from cache during distributor outage, and real JSON and SSE inference continue.</desc>',
        """<style>
          .bg{fill:#f8fafc}.channel{fill:#eff6ff;stroke:#2563eb;stroke-width:1.5}.safe{fill:#ecfdf5;stroke:#059669;stroke-width:1.5}.attack{fill:#fff7ed;stroke:#ea580c;stroke-width:1.5}.outage{fill:#f1f5f9;stroke:#64748b;stroke-width:1.5}.proof{fill:#ecfdf5;stroke:#059669;stroke-width:1.5}.line{stroke:#64748b;stroke-width:2;fill:none}.dash{stroke-dasharray:7 5}text{font-family:ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;fill:#0f172a}.title{font-size:28px;font-weight:750}.subtitle{font-size:15px;fill:#475569}.section{font-size:18px;font-weight:700}.card-title{font-size:15px;font-weight:700}.card-value{font-size:21px;font-weight:750}.card-detail{font-size:12px;fill:#475569}.edge{font-size:12px;fill:#64748b}.proof-value{font-size:24px;font-weight:750;fill:#047857}.proof-detail{font-size:13px;fill:#065f46}.foot{font-size:12px;fill:#64748b}
        </style>""",
        '<rect width="1200" height="850" class="bg"/>',
        text(50, 54, "v0.24 · TLS 1.3 mutual authentication for trust distribution", "title"),
        text(
            50,
            84,
            f"{handshake['tls_version']} · server SAN localhost · client certificate required · application signatures remain authoritative",
            "subtitle",
        ),
        text(50, 132, "Authenticated channel and independently authorized policy", "section"),
        card(50, 165, 245, "trust-root signer", "root-signed g1", "authorizes policy meaning", "safe"),
        card(345, 165, 245, "mTLS distributor", handshake["tls_version"], "server + client identity", "channel"),
        card(640, 165, 245, "three controls", f"{g1_body['receipt_count']} / 3 receipts", "verify · persist · activate", "safe"),
        card(
            935,
            165,
            215,
            "transport attacks",
            f"{transport_failures} / {len(transport_attempts)} fail",
            "before HTTP response",
            "attack",
        ),
        '<line x1="295" y1="227" x2="345" y2="227" class="line"/>',
        text(320, 214, "signed bytes", "edge", "middle"),
        '<line x1="590" y1="227" x2="640" y2="227" class="line"/>',
        text(615, 214, "mTLS", "edge", "middle"),
        '<line x1="935" y1="227" x2="885" y2="227" class="line dash"/>',
        text(910, 214, "blocked", "edge", "middle"),
        text(50, 345, "Safety and recovery timeline", "section"),
        card(
            50,
            380,
            250,
            "application attacks",
            f"{application_rejections} / {len(application_attempts)} rejected",
            "tampered snapshot · forged receipt",
            "attack",
        ),
        card(
            350,
            380,
            250,
            "valid generation 2",
            f"{g2_body['receipt_count']} / 3 receipts",
            f"status observation {g2_controls['duration_ms']:.3f} ms",
            "safe",
        ),
        card(
            650,
            380,
            250,
            "distributor outage",
            "cache bootstrap",
            f"PID {restart['old_pid']} → {restart['new_pid']}",
            "outage",
        ),
        card(
            950,
            380,
            200,
            "real inference",
            "JSON + SSE",
            "SSE reaches [DONE]",
            "safe",
        ),
        '<line x1="300" y1="442" x2="350" y2="442" class="line"/>',
        '<line x1="600" y1="442" x2="650" y2="442" class="line"/>',
        '<line x1="900" y1="442" x2="950" y2="442" class="line"/>',
        '<rect x="50" y="565" width="1100" height="155" rx="16" class="proof"/>',
        text(600, 612, f"{assertions['passed']} / {assertions['total']} checks passed", "proof-value", "middle"),
        text(
            600,
            651,
            f"real JSON {request['duration_ms']:.3f} ms · SSE {stream['duration_ms']:.3f} ms + [DONE]",
            "proof-detail",
            "middle",
        ),
        text(
            600,
            683,
            "TLS authenticates the channel; Ed25519 root/service signatures still authorize policy and receipts",
            "proof-detail",
            "middle",
        ),
        text(
            50,
            778,
            "Limits: trust-distribution link only; ephemeral proof CA; no global service mTLS, certificate rotation/revocation, ACME/HSM, policy expiry, or distributor HA.",
            "foot",
        ),
        text(50, 807, "All endpoints are loopback; private keys and certificate PEM are excluded from retained evidence.", "foot"),
        "</svg>",
    ]
    args.output.write_text("".join(parts), encoding="utf-8")


if __name__ == "__main__":
    main()
