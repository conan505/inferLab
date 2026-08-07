#!/usr/bin/env python3
"""Check the retained InferLab v0.25 partition and Figure-8 evidence."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


EXPECTED_NODES = {"node-a", "node-b", "node-c"}
EXPECTED_LINKS = {
    "a-to-b",
    "a-to-c",
    "b-to-a",
    "b-to-c",
    "c-to-a",
    "c-to-b",
}
CUT_LINKS = {"a-to-b", "a-to-c", "b-to-a", "c-to-a"}
MAJORITY_LINKS = {"b-to-c", "c-to-b"}
EXPECTED_LINK_TOPOLOGY = {
    "a-to-b": ("node-a", "node-b", "http://127.0.0.1:9962"),
    "a-to-c": ("node-a", "node-c", "http://127.0.0.1:9963"),
    "b-to-a": ("node-b", "node-a", "http://127.0.0.1:9961"),
    "b-to-c": ("node-b", "node-c", "http://127.0.0.1:9963"),
    "c-to-a": ("node-c", "node-a", "http://127.0.0.1:9961"),
    "c-to-b": ("node-c", "node-b", "http://127.0.0.1:9962"),
}
EXPECTED_FIGURE_ASSERTIONS = {
    "term_three_election_reaches_majority",
    "term_four_election_reaches_majority",
    "old_term_entry_reaches_majority",
    "majority_only_rule_would_commit_old_term",
    "current_term_rule_rejects_old_term",
    "conflicting_future_leader_can_win_unsafe_branch",
    "allegedly_committed_entry_can_be_overwritten",
    "current_term_entry_waits_for_majority",
    "current_term_entry_commits_safe_branch",
    "prior_entry_commits_indirectly",
    "conflicting_future_leader_blocked_safe_branch",
}


def load(directory: Path, name: str) -> dict[str, Any]:
    with (directory / name).open(encoding="utf-8") as source:
        return json.load(source)


def status_bodies(evidence: dict[str, Any]) -> dict[str, dict[str, Any]]:
    return {
        node_id: entry["observation"]["body"]
        for node_id, entry in evidence["statuses"].items()
    }


def link_bodies(evidence: dict[str, Any]) -> dict[str, dict[str, Any]]:
    return {
        link_id: entry["observation"]["body"]
        for link_id, entry in evidence["statuses"].items()
    }


def durable_states(evidence: dict[str, Any]) -> dict[str, dict[str, Any]]:
    return {node_id: entry["state"] for node_id, entry in evidence["nodes"].items()}


def command_type(entry: dict[str, Any]) -> str | None:
    return (entry.get("command") or {}).get("type")


def command_policy(entry: dict[str, Any]) -> str | None:
    return ((entry.get("command") or {}).get("configuration") or {}).get("routing_policy")


def committed_policy(status: dict[str, Any]) -> str | None:
    return ((status.get("committed_configuration") or {}).get("configuration") or {}).get(
        "routing_policy"
    )


def valid_write_authorization(evidence: dict[str, Any], nonce: str) -> bool:
    authorization = evidence.get("request", {}).get("authorization", {})
    signature = authorization.get("signature")
    return (
        authorization.get("schema") == "inferlab.control-write-authorization.v1"
        and authorization.get("algorithm") == "ed25519"
        and authorization.get("writer_id") == "deploy-bot"
        and authorization.get("nonce") == nonce
        and isinstance(signature, str)
        and bool(signature)
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    baseline_cluster = load(args.evidence_dir, "baseline-cluster.json")
    baseline_write = load(args.evidence_dir, "baseline-write.json")
    baseline_state = load(args.evidence_dir, "baseline-state.json")
    baseline_links = load(args.evidence_dir, "baseline-links.json")
    partition_transition = load(args.evidence_dir, "partition-transition.json")
    partition_links = load(args.evidence_dir, "partition-links.json")
    isolated_write = load(args.evidence_dir, "isolated-write.json")
    majority_write = load(args.evidence_dir, "majority-write.json")
    majority_election = load(args.evidence_dir, "majority-election.json")
    majority_cluster = load(args.evidence_dir, "majority-cluster.json")
    partition_cluster = load(args.evidence_dir, "partition-cluster.json")
    partition_state = load(args.evidence_dir, "partition-state.json")
    healing_transition = load(args.evidence_dir, "healing-transition.json")
    healed_cluster = load(args.evidence_dir, "healed-cluster.json")
    healed_state = load(args.evidence_dir, "healed-state.json")
    healed_links = load(args.evidence_dir, "healed-links.json")
    link_events = load(args.evidence_dir, "link-events.json")
    continuity = load(args.evidence_dir, "process-continuity.json")
    figure_eight = load(args.evidence_dir, "figure-eight.json")
    figure_test = load(args.evidence_dir, "figure-eight-test.json")
    gateway = load(args.evidence_dir, "gateway-ready.json")
    request = load(args.evidence_dir, "request.json")
    stream = load(args.evidence_dir, "stream.json")
    sanitizer = load(args.evidence_dir, "sanitizer.json")
    private_scan = load(args.evidence_dir, "private-material-scan.json")

    assertions: list[dict[str, Any]] = []

    def check(name: str, condition: bool, detail: Any) -> None:
        assertions.append({"name": name, "passed": bool(condition), "detail": detail})

    baseline_statuses = status_bodies(baseline_cluster)
    baseline_term = baseline_statuses["node-a"].get("term")
    check(
        "baseline has one shared-term leader A",
        set(baseline_statuses) == EXPECTED_NODES
        and baseline_cluster.get("leader_id") == "node-a"
        and baseline_statuses["node-a"].get("role") == "leader"
        and isinstance(baseline_term, int)
        and baseline_term > 0
        and all(status.get("term") == baseline_term for status in baseline_statuses.values()),
        {node: {"role": value.get("role"), "term": value.get("term")} for node, value in baseline_statuses.items()},
    )
    check(
        "baseline is committed and applied at revision 2",
        all(
            status.get("commit_index") == 2
            and status.get("last_applied") == 2
            and (status.get("committed_configuration") or {}).get("revision") == 2
            and committed_policy(status) == "round-robin"
            for status in baseline_statuses.values()
        ),
        {node: {"commit": value.get("commit_index"), "policy": committed_policy(value)} for node, value in baseline_statuses.items()},
    )
    check(
        "baseline signed write commits as revision 2",
        baseline_write.get("response", {}).get("status") == 200
        and baseline_write.get("response", {}).get("body", {}).get("revision") == 2
        and baseline_write.get("response", {}).get("body", {}).get("configuration", {}).get("routing_policy")
        == "round-robin"
        and valid_write_authorization(baseline_write, "partition-baseline"),
        baseline_write.get("response"),
    )
    baseline_states = durable_states(baseline_state)
    baseline_logs = [state.get("log") for state in baseline_states.values()]
    check(
        "baseline durable logs are identical through index 2",
        set(baseline_states) == EXPECTED_NODES
        and all(state.get("commit_index") == 2 for state in baseline_states.values())
        and all(log == baseline_logs[0] for log in baseline_logs[1:])
        and [entry.get("index") for entry in baseline_logs[0]] == [1, 2]
        and [entry.get("term") for entry in baseline_logs[0]] == [baseline_term, baseline_term]
        and command_type(baseline_logs[0][0]) == "noop"
        and command_policy(baseline_logs[0][1]) == "round-robin",
        baseline_logs,
    )
    baseline_link_statuses = link_bodies(baseline_links)
    check(
        "six directed links begin in allow mode",
        set(baseline_link_statuses) == EXPECTED_LINKS
        and all(status.get("schema") == "inferlab.raft-link-status.v0.25" for status in baseline_link_statuses.values())
        and all(status.get("mode") == "allow" for status in baseline_link_statuses.values())
        and all(status.get("upstream_failures") == 0 for status in baseline_link_statuses.values()),
        {link: status.get("mode") for link, status in baseline_link_statuses.items()},
    )
    check(
        "six proxies expose the exact directed topology",
        all(
            status.get("link_id") == link
            and (
                status.get("source_id"),
                status.get("target_id"),
                status.get("upstream_base_url"),
            )
            == EXPECTED_LINK_TOPOLOGY[link]
            for link, status in baseline_link_statuses.items()
        ),
        {
            link: {
                "source": status.get("source_id"),
                "target": status.get("target_id"),
                "upstream": status.get("upstream_base_url"),
            }
            for link, status in baseline_link_statuses.items()
        },
    )
    check(
        "baseline leader traffic traverses both outgoing proxies",
        all(baseline_link_statuses[link].get("forwarded_requests", 0) > 0 for link in ["a-to-b", "a-to-c"]),
        {link: baseline_link_statuses[link].get("forwarded_requests") for link in ["a-to-b", "a-to-c"]},
    )

    check(
        "partition closes inbound links before outbound links",
        partition_transition.get("requested_mode") == "drop"
        and partition_transition.get("ordered_link_ids")
        == ["b-to-a", "c-to-a", "a-to-b", "a-to-c"]
        and all(item.get("observation", {}).get("status") == 200 for item in partition_transition.get("transitions", [])),
        partition_transition.get("ordered_link_ids"),
    )
    partition_link_statuses = link_bodies(partition_links)
    check(
        "four drops isolate A while B and C remain connected",
        all(partition_link_statuses[link].get("mode") == "drop" for link in CUT_LINKS)
        and all(partition_link_statuses[link].get("mode") == "allow" for link in MAJORITY_LINKS)
        and all(status.get("upstream_failures") == 0 for status in partition_link_statuses.values())
        and any(
            partition_link_statuses[link].get("forwarded_requests", 0)
            > baseline_link_statuses[link].get("forwarded_requests", 0)
            for link in MAJORITY_LINKS
        ),
        {link: status.get("mode") for link, status in partition_link_statuses.items()},
    )
    check(
        "dropped Raft RPCs are observed on both sides of the cut",
        sum(partition_link_statuses[link].get("dropped_requests", 0) for link in {"a-to-b", "a-to-c"}) > 0
        and sum(partition_link_statuses[link].get("dropped_requests", 0) for link in {"b-to-a", "c-to-a"}) > 0,
        {link: partition_link_statuses[link].get("dropped_requests") for link in CUT_LINKS},
    )
    isolated_response = isolated_write.get("response") or {}
    isolated_error = isolated_response.get("body", {}).get("error", {})
    check(
        "isolated leader returns structured ambiguous unavailable result",
        isolated_response.get("status") == 503 and isolated_error.get("code") == "unavailable",
        isolated_response,
    )
    check(
        "minority proposal targets expected revision 2",
        isolated_write.get("request", {}).get("expected_revision") == 2
        and isolated_write.get("request", {}).get("configuration", {}).get("routing_policy") == "least-in-flight"
        and valid_write_authorization(isolated_write, "isolated-a-proposal"),
        {
            "expected_revision": isolated_write.get("request", {}).get("expected_revision"),
            "policy": isolated_write.get("request", {}).get("configuration", {}).get("routing_policy"),
        },
    )

    election_statuses = status_bodies(majority_election)
    check(
        "B and C first commit the higher-term no-op at index 3",
        majority_election.get("leader_id") in {"node-b", "node-c"}
        and all(status.get("term", 0) > baseline_term for status in election_statuses.values())
        and all(status.get("commit_index") == 3 for status in election_statuses.values())
        and all((status.get("committed_configuration") or {}).get("revision") == 2 for status in election_statuses.values()),
        {node: {"role": value.get("role"), "term": value.get("term"), "commit": value.get("commit_index")} for node, value in election_statuses.items()},
    )
    majority_statuses = status_bodies(majority_cluster)
    majority_terms = {status.get("term") for status in majority_statuses.values()}
    majority_term = next(iter(majority_terms)) if len(majority_terms) == 1 else None
    check(
        "B and C elect one higher-term leader",
        set(majority_statuses) == {"node-b", "node-c"}
        and len([status for status in majority_statuses.values() if status.get("role") == "leader"]) == 1
        and len(majority_terms) == 1
        and isinstance(majority_term, int)
        and majority_term > baseline_term,
        {node: {"role": value.get("role"), "term": value.get("term")} for node, value in majority_statuses.items()},
    )
    majority_response = majority_write.get("response") or {}
    check(
        "connected majority commits a different revision 4 configuration",
        majority_response.get("status") == 200
        and majority_response.get("body", {}).get("revision") == 4
        and majority_response.get("body", {}).get("configuration", {}).get("routing_policy") == "weighted-round-robin"
        and valid_write_authorization(majority_write, "majority-config-v1"),
        majority_response,
    )
    check(
        "B and C apply majority revision 4",
        all(
            status.get("commit_index") == 4
            and status.get("last_applied") == 4
            and (status.get("committed_configuration") or {}).get("revision") == 4
            and committed_policy(status) == "weighted-round-robin"
            for status in majority_statuses.values()
        ),
        {node: {"commit": value.get("commit_index"), "policy": committed_policy(value)} for node, value in majority_statuses.items()},
    )

    partition_statuses = status_bodies(partition_cluster)
    leaders_by_term: dict[int, list[str]] = {}
    for node_id, status in partition_statuses.items():
        if status.get("role") == "leader":
            leaders_by_term.setdefault(status.get("term"), []).append(node_id)
    check(
        "old A still reports leader only in its older term",
        partition_statuses["node-a"].get("role") == "leader"
        and partition_statuses["node-a"].get("term") == baseline_term
        and len(leaders_by_term) == 2
        and all(len(leaders) == 1 for leaders in leaders_by_term.values()),
        leaders_by_term,
    )
    check(
        "A's applied state stays at baseline during isolation",
        partition_statuses["node-a"].get("commit_index") == 2
        and partition_statuses["node-a"].get("last_applied") == 2
        and (partition_statuses["node-a"].get("committed_configuration") or {}).get("revision") == 2
        and committed_policy(partition_statuses["node-a"]) == "round-robin",
        partition_statuses["node-a"],
    )
    check(
        "majority progresses while isolated A does not",
        all(partition_statuses[node].get("commit_index") == 4 for node in ["node-b", "node-c"])
        and all(committed_policy(partition_statuses[node]) == "weighted-round-robin" for node in ["node-b", "node-c"]),
        {node: {"commit": value.get("commit_index"), "policy": committed_policy(value)} for node, value in partition_statuses.items()},
    )

    partition_states = durable_states(partition_state)
    a_state = partition_states["node-a"]
    b_state = partition_states["node-b"]
    c_state = partition_states["node-c"]
    check(
        "isolated A durably appends but does not commit its old-term suffix",
        a_state.get("commit_index") == 2
        and len(a_state.get("log", [])) == 3
        and a_state["log"][2].get("index") == 3
        and a_state["log"][2].get("term") == baseline_term
        and command_policy(a_state["log"][2]) == "least-in-flight",
        a_state,
    )
    check(
        "B and C have the same higher-term no-op and revision-4 suffix",
        b_state.get("log") == c_state.get("log")
        and b_state.get("current_term") == c_state.get("current_term")
        and b_state.get("commit_index") == c_state.get("commit_index")
        and b_state.get("commit_index") == 4
        and [entry.get("term") for entry in b_state.get("log", [])]
        == [baseline_term, baseline_term, majority_term, majority_term]
        and command_type(b_state["log"][2]) == "noop"
        and command_policy(b_state["log"][3]) == "weighted-round-robin",
        b_state,
    )
    check(
        "conflicting logs retain one identical committed prefix",
        a_state.get("log", [])[:2] == b_state.get("log", [])[:2] == baseline_logs[0]
        and a_state.get("log", [None, None, None])[2] != b_state.get("log", [None, None, None])[2],
        {"a_suffix": a_state.get("log", [None, None, None])[2], "majority_suffix": b_state.get("log", [None, None, None])[2]},
    )

    check(
        "healing opens outbound links before inbound links",
        healing_transition.get("requested_mode") == "allow"
        and healing_transition.get("ordered_link_ids")
        == ["a-to-b", "a-to-c", "b-to-a", "c-to-a"]
        and all(item.get("observation", {}).get("status") == 200 for item in healing_transition.get("transitions", [])),
        healing_transition.get("ordered_link_ids"),
    )
    healed_statuses = status_bodies(healed_cluster)
    final_terms = {status.get("term") for status in healed_statuses.values()}
    check(
        "old leader A steps down after observing the higher term",
        healed_statuses["node-a"].get("role") == "follower"
        and healed_statuses["node-a"].get("term") > partition_statuses["node-a"].get("term")
        and len(final_terms) == 1,
        {node: {"role": value.get("role"), "term": value.get("term")} for node, value in healed_statuses.items()},
    )
    check(
        "all controls apply the healed majority configuration",
        all(
            status.get("commit_index") == 4
            and status.get("last_applied") == 4
            and (status.get("committed_configuration") or {}).get("revision") == 4
            and committed_policy(status) == "weighted-round-robin"
            for status in healed_statuses.values()
        ),
        {node: {"commit": value.get("commit_index"), "policy": committed_policy(value)} for node, value in healed_statuses.items()},
    )
    final_states = durable_states(healed_state)
    final_values = list(final_states.values())
    final_logs = [value.get("log") for value in final_values]
    final_commits = [value.get("commit_index") for value in final_values]
    final_terms = [value.get("current_term") for value in final_values]
    check(
        "all three durable logs and commit indexes converge",
        all(log == final_logs[0] for log in final_logs[1:])
        and final_commits == [4, 4, 4]
        and len(set(final_terms)) == 1,
        {
            node: {
                "sha256": entry["sha256"],
                "term": entry["state"].get("current_term"),
                "voted_for": entry["state"].get("voted_for"),
                "commit_index": entry["state"].get("commit_index"),
            }
            for node, entry in healed_state["nodes"].items()
        },
    )
    final_log = final_values[0].get("log", [])
    check(
        "healing replaces A's conflicting index 3",
        len(final_log) == 4
        and command_type(final_log[2]) == "noop"
        and final_log[2].get("term") == majority_term > a_state["log"][2].get("term")
        and final_log[2] == b_state["log"][2],
        {"before": a_state["log"][2], "after": final_log[2]},
    )
    check(
        "ambiguous minority proposal is absent after convergence",
        all(command_policy(entry) != "least-in-flight" for entry in final_log)
        and command_policy(final_log[3]) == "weighted-round-robin",
        [command_policy(entry) for entry in final_log],
    )
    check(
        "committed baseline prefix survives repair",
        final_log[:2] == baseline_logs[0],
        final_log[:2],
    )
    healed_link_statuses = link_bodies(healed_links)
    check(
        "all six directed links are healed",
        set(healed_link_statuses) == EXPECTED_LINKS
        and all(status.get("mode") == "allow" for status in healed_link_statuses.values())
        and all(healed_link_statuses[link].get("mode_changes") == 2 for link in CUT_LINKS)
        and all(healed_link_statuses[link].get("mode_changes") == 0 for link in MAJORITY_LINKS)
        and all(status.get("upstream_failures") == 0 for status in healed_link_statuses.values())
        and sum(healed_link_statuses[link].get("forwarded_requests", 0) for link in {"b-to-a", "c-to-a"})
        > sum(partition_link_statuses[link].get("forwarded_requests", 0) for link in {"b-to-a", "c-to-a"}),
        {link: {"mode": status.get("mode"), "changes": status.get("mode_changes")} for link, status in healed_link_statuses.items()},
    )

    event_links = link_events.get("links", {})
    check(
        "link journals have contiguous monotonic sequences",
        set(event_links) == EXPECTED_LINKS
        and all(value.get("sequences_contiguous") is True for value in event_links.values())
        and all(
            bool(value.get("records"))
            and value["records"][0].get("sequence") == 1
            and value["records"][0].get("event") == "started"
            and value["records"][0].get("mode") == "allow"
            and all(
                record.get("schema") == "inferlab.raft-link-event.v0.25"
                and record.get("link_id") == link
                and (
                    record.get("source_id"),
                    record.get("target_id"),
                )
                == EXPECTED_LINK_TOPOLOGY[link][:2]
                for record in value.get("records", [])
            )
            for link, value in event_links.items()
        ),
        {link: {"events": value.get("event_count"), "contiguous": value.get("sequences_contiguous")} for link, value in event_links.items()},
    )
    check(
        "journals record four drop/heal pairs and observed drops",
        all(
            [
                (record.get("mode"), record.get("reason"))
                for record in event_links[link].get("records", [])
                if record.get("event") == "mode_changed"
            ]
            == [
                ("drop", "isolate-old-leader-a"),
                ("allow", "heal-old-leader-cut"),
            ]
            for link in CUT_LINKS
        )
        and all(
            not any(record.get("event") == "mode_changed" for record in event_links[link].get("records", []))
            for link in MAJORITY_LINKS
        )
        and sum(event_links[link].get("event_types", {}).get("request_dropped", 0) for link in CUT_LINKS) > 0
        and all(
            record.get("mode") == "drop"
            and record.get("method") == "POST"
            and record.get("path_and_query") in {"/raft/request-vote", "/raft/append-entries"}
            for link in CUT_LINKS
            for record in event_links[link].get("records", [])
            if record.get("event") == "request_dropped"
        ),
        {link: value.get("event_types") for link, value in event_links.items()},
    )
    participants = continuity.get("processes", {})
    proof_shell_pid = continuity.get("proof_shell_pid")
    expected_process_commands = {
        **{name: "control-plane" for name in EXPECTED_NODES},
        **{name: "raft-link-proxy" for name in EXPECTED_LINKS},
        "gateway": "gateway",
        "cpu-worker": "cpu-worker",
    }
    check(
        "nine partition participants keep their exact owned PIDs",
        continuity.get("schema") == "inferlab.raft-partition-process-continuity.v0.25"
        and isinstance(proof_shell_pid, int)
        and set(participants)
        == EXPECTED_NODES | EXPECTED_LINKS | {"gateway", "cpu-worker"}
        and set(continuity.get("partition_participants", []))
        == EXPECTED_NODES | EXPECTED_LINKS
        and all(
            participants[name].get("same_pid") is True
            and participants[name].get("same_start_token") is True
            and participants[name].get("alive") is True
            and participants[name].get("owned_child") is True
            and participants[name].get("non_zombie") is True
            and participants[name].get("parent_pid") == proof_shell_pid
            and participants[name].get("initial_pid") == participants[name].get("current_pid")
            and isinstance(participants[name].get("initial_start_token"), str)
            and bool(participants[name].get("initial_start_token"))
            and participants[name].get("initial_start_token")
            == participants[name].get("current_start_token")
            and isinstance(participants[name].get("process_state"), str)
            and bool(participants[name].get("process_state"))
            and "Z" not in participants[name].get("process_state")
            and (participants[name].get("command") or "").rsplit("/", 1)[-1]
            == expected_process_commands[name]
            for name in EXPECTED_NODES | EXPECTED_LINKS
        ),
        {name: participants.get(name) for name in sorted(EXPECTED_NODES | EXPECTED_LINKS)},
    )
    check(
        "gateway and real CPU worker are proof-owned and live",
        all(
            participants.get(name, {}).get("same_pid") is True
            and participants.get(name, {}).get("same_start_token") is True
            and participants.get(name, {}).get("alive") is True
            and participants.get(name, {}).get("owned_child") is True
            and participants.get(name, {}).get("non_zombie") is True
            and participants.get(name, {}).get("parent_pid") == proof_shell_pid
            and participants.get(name, {}).get("initial_pid")
            == participants.get(name, {}).get("current_pid")
            and isinstance(participants.get(name, {}).get("initial_start_token"), str)
            and bool(participants.get(name, {}).get("initial_start_token"))
            and participants.get(name, {}).get("initial_start_token")
            == participants.get(name, {}).get("current_start_token")
            and isinstance(participants.get(name, {}).get("process_state"), str)
            and bool(participants.get(name, {}).get("process_state"))
            and "Z" not in participants.get(name, {}).get("process_state")
            and (participants.get(name, {}).get("command") or "").rsplit("/", 1)[-1]
            == expected_process_commands[name]
            for name in ["gateway", "cpu-worker"]
        ),
        {name: participants.get(name) for name in ["gateway", "cpu-worker"]},
    )

    figure_assertions = figure_eight.get("assertions", {})
    check(
        "production Figure-8 replay reports every invariant true",
        figure_eight.get("schema") == "inferlab.raft-figure-eight.v0.25"
        and figure_eight.get("scenario") == "raft-paper-figure-8"
        and figure_eight.get("cluster_size") == 5
        and figure_eight.get("majority") == 3
        and figure_eight.get("passed") is True
        and set(figure_assertions) == EXPECTED_FIGURE_ASSERTIONS
        and all(value is True for value in figure_assertions.values()),
        figure_assertions,
    )
    expected_stages = [
        {
            "label": "a",
            "leader_id": "S1",
            "leader_term": 2,
            "logs": [
                {"server_id": "S1", "entry_terms": [1, 2]},
                {"server_id": "S2", "entry_terms": [1, 2]},
                {"server_id": "S3", "entry_terms": [1]},
                {"server_id": "S4", "entry_terms": [1]},
                {"server_id": "S5", "entry_terms": [1]},
            ],
        },
        {
            "label": "b",
            "leader_id": "S5",
            "leader_term": 3,
            "logs": [
                {"server_id": "S1", "entry_terms": [1, 2]},
                {"server_id": "S2", "entry_terms": [1, 2]},
                {"server_id": "S3", "entry_terms": [1]},
                {"server_id": "S4", "entry_terms": [1]},
                {"server_id": "S5", "entry_terms": [1, 3]},
            ],
        },
        {
            "label": "c",
            "leader_id": "S1",
            "leader_term": 4,
            "logs": [
                {"server_id": "S1", "entry_terms": [1, 2]},
                {"server_id": "S2", "entry_terms": [1, 2]},
                {"server_id": "S3", "entry_terms": [1, 2]},
                {"server_id": "S4", "entry_terms": [1]},
                {"server_id": "S5", "entry_terms": [1, 3]},
            ],
        },
    ]
    check(
        "Figure-8 report retains the exact paper stages a through c",
        figure_eight.get("stages") == expected_stages,
        figure_eight.get("stages"),
    )
    old_term = figure_eight.get("old_term_majority", {})
    check(
        "old-term majority is not directly committable",
        old_term
        == {
            "current_term_rule_candidate": None,
            "entry_term": 2,
            "index": 2,
            "leader_term": 4,
            "majority_only_candidate": 2,
            "replica_count": 3,
        },
        old_term,
    )
    unsafe = figure_eight.get("unsafe_branch", {})
    expected_unsafe = {
        "label": "d",
        "candidate_id": "S5",
        "candidate_term": 5,
        "eligible_voters": ["S2", "S3", "S4", "S5"],
        "vote_count": 4,
        "majority_reached": True,
        "overwritten_index": 2,
        "overwritten_entry_term": 2,
        "old_entry_replicas_after_overwrite": 1,
        "old_entry_survives_on_majority": False,
        "logs_after_overwrite": [
            {"server_id": "S1", "entry_terms": [1, 2]},
            {"server_id": "S2", "entry_terms": [1, 3]},
            {"server_id": "S3", "entry_terms": [1, 3]},
            {"server_id": "S4", "entry_terms": [1, 3]},
            {"server_id": "S5", "entry_terms": [1, 3]},
        ],
    }
    check(
        "Figure-8 unsafe branch can overwrite the alleged commit",
        unsafe == expected_unsafe,
        unsafe,
    )
    safe = figure_eight.get("safe_branch", {})
    expected_safe = {
        "label": "e",
        "leader_id": "S1",
        "leader_term": 4,
        "current_term_entry_index": 3,
        "current_term_entry_term": 4,
        "current_term_entry_replicas": 3,
        "current_term_rule_candidate_before_majority": None,
        "current_term_rule_candidate": 3,
        "prior_entry_index": 2,
        "prior_entry_committed_indirectly": True,
        "challenger_id": "S5",
        "challenger_term": 5,
        "challenger_eligible_voters": ["S4", "S5"],
        "challenger_vote_count": 2,
        "challenger_reaches_majority": False,
        "logs_after_current_term_replication": [
            {"server_id": "S1", "entry_terms": [1, 2, 4]},
            {"server_id": "S2", "entry_terms": [1, 2, 4]},
            {"server_id": "S3", "entry_terms": [1, 2, 4]},
            {"server_id": "S4", "entry_terms": [1]},
            {"server_id": "S5", "entry_terms": [1, 3]},
        ],
    }
    check(
        "current-term entry makes Figure-8 safe",
        safe == expected_safe,
        safe,
    )
    check(
        "exact Figure-8 unit regression passes",
        figure_test.get("status") == 0
        and figure_test.get("command")
        == [
            "cargo",
            "test",
            "-p",
            "control-plane",
            "--lib",
            "figure_eight::tests::figure_eight_requires_a_current_term_entry_before_counting_replicas_is_safe",
            "--",
            "--exact",
        ]
        and "figure_eight_requires_a_current_term_entry_before_counting_replicas_is_safe ... ok"
        in figure_test.get("stdout", "")
        and "test result: ok" in figure_test.get("stdout", ""),
        {"command": figure_test.get("command"), "status": figure_test.get("status")},
    )

    gateway_body = gateway.get("status", {}).get("body", {})
    check(
        "gateway installs healed revision 4",
        gateway.get("expected_revision") == 4
        and gateway.get("expected_policy") == "weighted-round-robin"
        and gateway_body.get("routing_snapshot", {}).get("control_revision") == 4
        and gateway_body.get("routing_policy") == "weighted-round-robin",
        {"revision": gateway_body.get("routing_snapshot", {}).get("control_revision"), "policy": gateway_body.get("routing_policy")},
    )
    request_observation = (request.get("requests") or [{}])[0]
    check(
        "real CPU JSON inference succeeds from healed revision",
        request.get("requested") == 1
        and request.get("succeeded") == 1
        and request_observation.get("status") == 200
        and request_observation.get("worker") == "cpu-raft-partition"
        and request_observation.get("config_revision") == 4
        and request_observation.get("control_cluster_id") == "inferlab-primary"
        and bool(request_observation.get("content")),
        request_observation,
    )
    check(
        "real CPU SSE inference reaches DONE from healed revision",
        stream.get("status") == 200
        and stream.get("worker") == "cpu-raft-partition"
        and stream.get("config_revision") == 4
        and stream.get("control_cluster_id") == "inferlab-primary"
        and stream.get("done_received") is True
        and bool(stream.get("pieces")),
        {key: stream.get(key) for key in ["status", "worker", "config_revision", "done_received", "duration_ms"]},
    )
    check(
        "retained evidence is sanitized",
        sanitizer.get("schema") == "inferlab.evidence-sanitizer.v0.25"
        and sanitizer.get("private_material_markers") == 0
        and sanitizer.get("remaining_host_paths") == 0,
        sanitizer,
    )
    check(
        "known proof private seeds are absent",
        private_scan.get("schema") == "inferlab.private-material-scan.v0.25"
        and private_scan.get("matches") == 0
        and private_scan.get("known_ed25519_seed_count", 0) >= 6,
        private_scan,
    )

    passed = sum(assertion["passed"] for assertion in assertions)
    result = {
        "schema": "inferlab.raft-partition-assertions.v0.25",
        "passed": passed,
        "total": len(assertions),
        "all_passed": passed == len(assertions),
        "assertions": assertions,
    }
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    if not result["all_passed"]:
        failed = [assertion["name"] for assertion in assertions if not assertion["passed"]]
        raise SystemExit("failed v0.25 assertions: " + "; ".join(failed))
    print(f"v0.25 proof: {passed}/{len(assertions)} assertions passed")


if __name__ == "__main__":
    main()
