#!/usr/bin/env python3
"""Validate the InferLab v0.6 three-node Raft evidence."""

import argparse
import json


def all_gateway_requests_succeeded(phase: dict) -> bool:
    return phase["successful"] == phase["request_count"]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--analysis", required=True)
    args = parser.parse_args()
    with open(args.analysis, encoding="utf-8") as source:
        analysis = json.load(source)

    initial = analysis["initial_election"]
    reelections = analysis["reelections"]
    writes = analysis["writes"]
    convergence = analysis["convergence"]
    gateway = analysis["gateway"]
    states = analysis["persistent_states"]
    final_statuses = analysis["final_statuses"]
    leadership = analysis["leadership"]
    final_revision = convergence["final"]["revision"]
    write_revisions = [write["committed"]["revision"] for write in writes]
    write_terms = [write["committed"]["term"] for write in writes]
    final_logs = [state["log"] for state in states.values()]
    final_commits = [state["commit_index"] for state in states.values()]
    killed_nodes = [fault["node_id"] for fault in analysis["faults"]]

    checks = {
        "schema_is_v0_6": analysis["schema"]
        == "inferlab.raft-analysis.v0.6",
        "initial_cluster_elected_exactly_one_leader": (
            initial["leader_id"] in {"node-a", "node-b", "node-c"}
            and len(
                [
                    status
                    for status in initial["statuses"]
                    if status["status"] == 200
                    and status["body"]["role"] == "leader"
                ]
            )
            == 1
        ),
        "each_term_has_at_most_one_observed_leader": all(
            len(nodes) == 1
            for nodes in analysis["leaders_by_term"].values()
        ),
        "two_leader_failures_targeted_owned_loopback_children": (
            len(analysis["faults"]) == 2
            and all(
                fault["event"] == "leader_killed"
                and fault["scope"] == "owned-child-process"
                and fault["bind"] == "127.0.0.1"
                and isinstance(fault["target_pid"], int)
                and fault["target_pid"] > 0
                for fault in analysis["faults"]
            )
        ),
        "different_leader_won_after_each_kill": (
            reelections[0]["leader_id"] != killed_nodes[0]
            and reelections[1]["leader_id"] != killed_nodes[1]
            and reelections[0]["term"] > initial["term"]
            and reelections[1]["term"] > reelections[0]["term"]
        ),
        "reelection_latency_was_bounded": all(
            0 < election["latency_ms"] < 1_000
            for election in reelections
        ),
        "all_three_configuration_writes_committed": (
            all(
                write["attempts"][-1]["response"]["status"] == 200
                for write in writes
            )
            and write_revisions == sorted(write_revisions)
            and len(set(write_revisions)) == 3
            and write_terms[0] < write_terms[1] < write_terms[2]
        ),
        "majority_progressed_with_one_node_down": (
            convergence["majority_after_first_kill"]["expected_nodes"] == 2
            and convergence["majority_after_second_kill"][
                "expected_nodes"
            ]
            == 2
        ),
        "restarted_nodes_caught_up": (
            convergence["restarted_first_node"]["expected_nodes"] == 3
            and convergence["restarted_first_node"]["revision"]
            >= write_revisions[1]
            and convergence["final"]["expected_nodes"] == 3
            and convergence["final"]["revision"] >= write_revisions[2]
        ),
        "all_final_logs_are_identical": (
            final_logs[0] == final_logs[1] == final_logs[2]
            and final_commits[0] == final_commits[1] == final_commits[2]
            and final_commits[0] == len(final_logs[0])
        ),
        "all_nodes_applied_same_final_configuration": (
            len(final_statuses) == 3
            and all(
                status["commit_index"] == final_revision
                and status["last_applied"] == final_revision
                and status["committed_configuration"]["revision"]
                == final_revision
                and status["committed_configuration"]["configuration"][
                    "routing_policy"
                ]
                == "weighted-round-robin"
                for status in final_statuses.values()
            )
        ),
        "gateway_served_through_both_elections": (
            all_gateway_requests_succeeded(gateway["election_1"])
            and all_gateway_requests_succeeded(gateway["election_2"])
            and gateway["election_1"]["applied_policy"] == "round-robin"
            and gateway["election_2"]["applied_policy"]
            == "least-in-flight"
        ),
        "gateway_applied_each_committed_snapshot": (
            gateway["before"]["applied_revision"] >= write_revisions[0]
            and gateway["after_config_2"]["applied_revision"]
            >= write_revisions[1]
            and gateway["final"]["applied_revision"] >= write_revisions[2]
            and gateway["before"]["applied_policy"] == "round-robin"
            and gateway["after_config_2"]["applied_policy"]
            == "least-in-flight"
            and gateway["final"]["applied_policy"]
            == "weighted-round-robin"
        ),
        "weighted_policy_changed_real_routing": gateway["final"][
            "worker_counts"
        ]
        == {"worker-a": 6, "worker-b": 2, "worker-c": 2},
        "gateway_control_refresh_ended_healthy": (
            gateway["final"]["control_error"] is None
            and all_gateway_requests_succeeded(gateway["final"])
        ),
        "persistent_restarts_replayed_committed_state": all(
            sum(
                event["event"] == "node_started"
                for event in analysis["node_events"][node_id]
            )
            >= 2
            for node_id in set(killed_nodes)
        ),
        "leadership_changed_at_least_three_times": len(leadership) >= 3,
    }
    report = {
        "schema": "inferlab.raft-check.v0.6",
        "terms_observed": len(analysis["leaders_by_term"]),
        "leaders": [
            {
                "node_id": event["node_id"],
                "term": event["term"],
                "elapsed_ms": event["elapsed_ms"],
            }
            for event in leadership
        ],
        "reelection_latencies_ms": [
            election["latency_ms"] for election in reelections
        ],
        "write_revisions": write_revisions,
        "write_terms": write_terms,
        "final_revision": final_revision,
        "final_log_entries": len(final_logs[0]),
        "gateway_final_worker_counts": gateway["final"]["worker_counts"],
        "checks": checks,
        "passed": all(checks.values()),
    }
    print(json.dumps(report, indent=2, sort_keys=True))
    if not report["passed"]:
        failed = [name for name, passed in checks.items() if not passed]
        raise SystemExit(
            "Raft evidence did not satisfy: " + ", ".join(failed)
        )


if __name__ == "__main__":
    main()
