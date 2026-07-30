#!/usr/bin/env python3
"""Merge v0.6 Raft snapshots, persisted events, and gateway observations."""

import argparse
import json
from collections import Counter, defaultdict
from pathlib import Path


def read_json(path: Path):
    with path.open(encoding="utf-8") as source:
        return json.load(source)


def read_jsonl(path: Path) -> list[dict]:
    with path.open(encoding="utf-8") as source:
        return [json.loads(line) for line in source if line.strip()]


def gateway_summary(snapshot: dict) -> dict:
    requests = snapshot["requests"]
    status = snapshot["gateway_status"]["body"]
    return {
        "observed_at_ms": snapshot["observed_at_ms"],
        "expected_policy": snapshot["expected_policy"],
        "minimum_revision": snapshot["minimum_revision"],
        "request_count": len(requests),
        "successful": sum(result["status"] == 200 for result in requests),
        "status_counts": dict(
            Counter(
                str(result["status"])
                if result["status"] is not None
                else "transport"
                for result in requests
            )
        ),
        "worker_counts": dict(
            Counter(
                result["worker"]
                for result in requests
                if result.get("worker")
            )
        ),
        "applied_policy": status["routing_policy"],
        "applied_revision": status["control_plane"]["revision"],
        "applied_term": status["control_plane"]["term"],
        "control_source_url": status["control_plane"]["source_url"],
        "control_error": status["control_plane"]["last_error"],
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--results", required=True)
    args = parser.parse_args()
    root = Path(args.results)

    node_events = {}
    timeline = []
    for node_id in ["node-a", "node-b", "node-c"]:
        events = read_jsonl(root / f"{node_id}-events.jsonl")
        node_events[node_id] = events
        timeline.extend(events)
    faults = read_jsonl(root / "fault-events.jsonl")
    timeline.extend(
        {
            **fault,
            "role": "stopped",
            "leader_id": None,
            "log_index": None,
            "detail": f"killed exact owned PID {fault['target_pid']}",
        }
        for fault in faults
    )
    timeline.sort(key=lambda event: (event["at_ms"], event["node_id"]))
    start_ms = min(event["at_ms"] for event in timeline)
    for event in timeline:
        event["elapsed_ms"] = round(event["at_ms"] - start_ms, 3)

    leadership = [
        event for event in timeline if event["event"] == "leader_elected"
    ]
    leaders_by_term = defaultdict(set)
    for event in leadership:
        leaders_by_term[event["term"]].add(event["node_id"])

    initial = read_json(root / "initial-election.json")
    reelections = [
        read_json(root / "re-election-1.json"),
        read_json(root / "re-election-2.json"),
    ]
    writes = [
        read_json(root / "config-1-write.json"),
        read_json(root / "config-2-write.json"),
        read_json(root / "config-3-write.json"),
    ]
    convergence = {
        "initial": read_json(root / "config-1-convergence.json"),
        "majority_after_first_kill": read_json(
            root / "config-2-majority.json"
        ),
        "restarted_first_node": read_json(
            root / "restarted-node-1-caught-up.json"
        ),
        "majority_after_second_kill": read_json(
            root / "config-3-majority.json"
        ),
        "final": read_json(root / "final-convergence.json"),
    }
    gateway = {
        "before": gateway_summary(read_json(root / "gateway-before.json")),
        "election_1": gateway_summary(
            read_json(root / "gateway-election-1.json")
        ),
        "after_config_2": gateway_summary(
            read_json(root / "gateway-after-config-2.json")
        ),
        "election_2": gateway_summary(
            read_json(root / "gateway-election-2.json")
        ),
        "final": gateway_summary(read_json(root / "gateway-final.json")),
    }
    persistent_states = {
        node_id: read_json(root / f"{node_id}-state.json")
        for node_id in ["node-a", "node-b", "node-c"]
    }
    final_statuses = {
        entry["body"]["node_id"]: entry["body"]
        for entry in convergence["final"]["statuses"]
        if entry["status"] == 200
    }

    analysis = {
        "schema": "inferlab.raft-analysis.v0.6",
        "start_epoch_ms": start_ms,
        "timeline": timeline,
        "node_events": node_events,
        "faults": faults,
        "initial_election": initial,
        "reelections": reelections,
        "leadership": leadership,
        "leaders_by_term": {
            str(term): sorted(nodes)
            for term, nodes in sorted(leaders_by_term.items())
        },
        "writes": writes,
        "convergence": convergence,
        "gateway": gateway,
        "persistent_states": persistent_states,
        "final_statuses": final_statuses,
    }
    print(json.dumps(analysis, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
