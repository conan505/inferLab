#!/usr/bin/env python3
"""Render the checked InferLab v0.25 partition/Figure-8 proof as SVG."""

from __future__ import annotations

import argparse
import html
import json
from pathlib import Path
from typing import Any


def load(directory: Path, name: str) -> dict[str, Any]:
    with (directory / name).open(encoding="utf-8") as source:
        return json.load(source)


def text(x: int, y: int, value: Any, css: str = "", anchor: str = "start") -> str:
    return (
        f'<text x="{x}" y="{y}" class="{css}" text-anchor="{anchor}">'
        f"{html.escape(str(value))}</text>"
    )


def card(x: int, y: int, width: int, title: str, value: str, detail: str, css: str) -> str:
    return "".join(
        [
            f'<rect x="{x}" y="{y}" width="{width}" height="116" rx="14" class="{css}"/>',
            text(x + 18, y + 29, title, "card-title"),
            text(x + 18, y + 65, value, "card-value"),
            text(x + 18, y + 91, detail, "card-detail"),
        ]
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    assertions = load(args.evidence_dir, "assertions.json")
    partition_links = load(args.evidence_dir, "partition-links.json")
    partition_cluster = load(args.evidence_dir, "partition-cluster.json")
    healed_cluster = load(args.evidence_dir, "healed-cluster.json")
    healed_state = load(args.evidence_dir, "healed-state.json")
    continuity = load(args.evidence_dir, "process-continuity.json")
    figure = load(args.evidence_dir, "figure-eight.json")
    request = load(args.evidence_dir, "request.json")
    stream = load(args.evidence_dir, "stream.json")

    if (
        assertions.get("all_passed") is not True
        or assertions.get("passed") != assertions.get("total")
        or not assertions.get("assertions")
        or not all(item.get("passed") is True for item in assertions["assertions"])
    ):
        raise SystemExit("refusing to render unchecked or failed evidence")

    link_modes = {
        name: entry.get("observation", {}).get("body", {}).get("mode")
        for name, entry in partition_links.get("statuses", {}).items()
    }
    cut_links = sorted(name for name, mode in link_modes.items() if mode == "drop")
    allowed_links = sorted(name for name, mode in link_modes.items() if mode == "allow")
    if len(cut_links) != 4 or allowed_links != ["b-to-c", "c-to-b"]:
        raise SystemExit("partition evidence does not contain the required four-link A cut")

    partition_statuses = {
        name: entry["observation"]["body"]
        for name, entry in partition_cluster["statuses"].items()
    }
    healed_statuses = {
        name: entry["observation"]["body"]
        for name, entry in healed_cluster["statuses"].items()
    }
    old = partition_statuses["node-a"]
    majority = [partition_statuses[name] for name in ["node-b", "node-c"]]
    old_term_live = old.get("term")
    majority_term_live = majority[0].get("term")
    if not (
        old.get("role") == "leader"
        and old.get("commit_index") == 2
        and isinstance(old_term_live, int)
        and all(status.get("term") == majority_term_live > old_term_live for status in majority)
        and all(status.get("commit_index") == 4 for status in majority)
        and healed_statuses["node-a"].get("role") == "follower"
        and all(status.get("commit_index") == 4 for status in healed_statuses.values())
    ):
        raise SystemExit("partition/healing status evidence is inconsistent")

    durable_nodes = healed_state.get("nodes", {})
    durable_logs = [entry.get("state", {}).get("log") for entry in durable_nodes.values()]
    durable_commits = [entry.get("state", {}).get("commit_index") for entry in durable_nodes.values()]
    if (
        len(durable_nodes) != 3
        or any(log != durable_logs[0] for log in durable_logs[1:])
        or durable_commits != [4, 4, 4]
    ):
        raise SystemExit("durable convergence evidence is inconsistent")
    final_log = next(iter(durable_nodes.values()))["state"]["log"]

    processes = continuity.get("processes", {})
    stable_processes = sum(
        item.get("same_pid") is True and item.get("alive") is True
        for item in processes.values()
    )
    if stable_processes != len(processes) or stable_processes != 11:
        raise SystemExit("process-continuity evidence is inconsistent")

    old_term = figure.get("old_term_majority", {})
    safe = figure.get("safe_branch", {})
    figure_checks = sum(value is True for value in figure.get("assertions", {}).values())
    if not (
        figure.get("passed") is True
        and figure_checks == len(figure.get("assertions", {})) == 11
        and old_term.get("replica_count") == 3
        and old_term.get("current_term_rule_candidate") is None
        and safe.get("current_term_rule_candidate") == 3
        and safe.get("prior_entry_committed_indirectly") is True
    ):
        raise SystemExit("Figure-8 evidence is inconsistent")

    json_ok = request.get("succeeded") == request.get("requested") == 1
    sse_ok = stream.get("status") == 200 and stream.get("done_received") is True
    if not (json_ok and sse_ok):
        raise SystemExit("real inference evidence is incomplete")

    desc = (
        f"A controlled three-process Raft cluster uses six directed loopback proxies. "
        f"{len(cut_links)} links isolate old leader A while B and C commit index 4; "
        f"healing makes A a follower and all three durable logs converge. "
        f"The five-server Figure-8 replay passes {figure_checks} algorithmic checks, "
        f"and real CPU JSON plus SSE ending in DONE succeed."
    )
    parts = [
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 900" role="img" aria-labelledby="title desc">',
        '<title id="title">InferLab v0.25 directed Raft partition and Figure-8 proof</title>',
        f'<desc id="desc">{html.escape(desc)}</desc>',
        """<style>
          .bg{fill:#f8fafc}.node{fill:#eff6ff;stroke:#2563eb;stroke-width:1.5}.minority{fill:#fff7ed;stroke:#ea580c;stroke-width:1.5}.majority{fill:#ecfdf5;stroke:#059669;stroke-width:1.5}.model{fill:#f5f3ff;stroke:#7c3aed;stroke-width:1.5}.proof{fill:#ecfdf5;stroke:#059669;stroke-width:1.5}.line{stroke:#64748b;stroke-width:2;fill:none}.cut{stroke:#dc2626;stroke-width:3;stroke-dasharray:8 6}.heal{stroke:#059669;stroke-width:3}text{font-family:ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;fill:#0f172a}.title{font-size:28px;font-weight:750}.subtitle{font-size:15px;fill:#475569}.section{font-size:18px;font-weight:700}.card-title{font-size:14px;font-weight:700}.card-value{font-size:21px;font-weight:750}.card-detail{font-size:12px;fill:#475569}.edge{font-size:12px;fill:#64748b}.proof-value{font-size:24px;font-weight:750;fill:#047857}.proof-detail{font-size:13px;fill:#065f46}.foot{font-size:12px;fill:#64748b}
        </style>""",
        '<rect width="1200" height="900" class="bg"/>',
        text(50, 54, "v0.25 · Directed Raft partition, suffix repair, and Figure 8", "title"),
        text(50, 84, "Three live OS processes + six Raft-only link proxies · five-server algorithmic replay is separate", "subtitle"),
        text(50, 128, "Controlled live schedule", "section"),
        card(50, 160, 250, f"old leader A · term {old_term_live}", f"commit {old['commit_index']}", "uncommitted conflicting index 3", "minority"),
        card(475, 160, 250, "connected majority B + C", f"commit {majority[0]['commit_index']}", f"term {majority_term_live} no-op + different config", "majority"),
        card(900, 160, 250, "healed three-node cluster", f"commit {healed_statuses['node-a']['commit_index']}", "identical durable logs", "node"),
        '<line x1="300" y1="218" x2="475" y2="218" class="cut"/>',
        text(388, 203, f"{len(cut_links)} directed drops", "edge", "middle"),
        '<line x1="725" y1="218" x2="900" y2="218" class="heal"/>',
        text(812, 203, "heal → truncate + append", "edge", "middle"),
        text(50, 340, "Why counting old-term replicas is unsafe", "section"),
        card(50, 375, 250, "Figure 8(c)", f"{old_term['replica_count']} / 5 copies", "index 2 is from term 2", "model"),
        card(350, 375, 250, "naive count", f"candidate {old_term['majority_only_candidate']}", "future S5 can overwrite it", "minority"),
        card(650, 375, 250, "Raft current-term rule", "no candidate", "leader is in term 4", "majority"),
        card(950, 375, 200, "Figure 8(e)", f"commit {safe['current_term_rule_candidate']}", "prior index 2 follows", "majority"),
        '<line x1="300" y1="433" x2="350" y2="433" class="line"/>',
        '<line x1="600" y1="433" x2="650" y2="433" class="line"/>',
        '<line x1="900" y1="433" x2="950" y2="433" class="line"/>',
        '<rect x="50" y="575" width="1100" height="170" rx="16" class="proof"/>',
        text(600, 625, f"{assertions['passed']} / {assertions['total']} checks passed", "proof-value", "middle"),
        text(600, 662, f"{stable_processes} exact owned PIDs remain live · final log has {len(final_log)} entries · {figure_checks} Figure-8 predicates hold", "proof-detail", "middle"),
        text(600, 696, f"real JSON {request['duration_ms']:.3f} ms · SSE {stream['duration_ms']:.3f} ms + [DONE]", "proof-detail", "middle"),
        text(50, 812, "Scope: one controlled single-host symmetric A-vs-{B,C} cut; whole HTTP RPC drop with deterministic 503, not packet-level chaos or Jepsen.", "foot"),
        text(50, 839, "Figure 8 is a deterministic five-server algorithmic replay over production commit/vote predicates, not a live five-node runtime.", "foot"),
        text(50, 866, "No latency/reorder/half-open model, arbitrary partitions, membership changes, multi-host network, formal verification, or proxy-management authentication.", "foot"),
        "</svg>",
    ]
    args.output.write_text("".join(parts), encoding="utf-8")


if __name__ == "__main__":
    main()
