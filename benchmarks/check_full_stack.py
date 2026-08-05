#!/usr/bin/env python3
"""Check retained v0.13 real-worker full-stack integration claims."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


def load(directory: Path, name: str) -> dict:
    return json.loads((directory / name).read_text())


def assertion(name: str, passed: bool, observed) -> dict:
    return {"name": name, "passed": bool(passed), "observed": observed}


def revisions(request_set: dict) -> list[int | None]:
    return [request.get("config_revision") for request in request_set["requests"]]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    evidence = args.evidence_dir

    initial_election = load(evidence, "initial-election.json")
    initial_config = load(evidence, "config-initial.json")["committed"]
    initial_gateway = load(evidence, "gateway-initial.json")
    health = load(evidence, "worker-health.json")
    affinity = load(evidence, "affinity.json")
    worker_fault = load(evidence, "worker-fault.json")
    failover = load(evidence, "failover.json")
    live_config = load(evidence, "config-live.json")["committed"]
    live_gateway = load(evidence, "gateway-live.json")
    post_reconfigure = load(evidence, "post-reconfigure.json")
    control_leader = load(evidence, "control-leader.json")
    control_fault = load(evidence, "control-fault.json")
    election_continuity = load(evidence, "election-continuity.json")
    re_election = load(evidence, "re-election.json")
    weighted_config = load(evidence, "config-weighted.json")["committed"]
    weighted_gateway = load(evidence, "gateway-weighted.json")
    weighted = load(evidence, "weighted.json")
    streamed = load(evidence, "stream.json")
    environment = load(evidence, "environment.json")

    initial_revision = initial_config["revision"]
    live_revision = live_config["revision"]
    weighted_revision = weighted_config["revision"]
    initial_workers = {
        worker["id"] for worker in initial_config["configuration"]["workers"]
    }
    live_workers = {
        worker["id"] for worker in live_config["configuration"]["workers"]
    }
    dead_worker = worker_fault["target"]
    failover_request = failover["requests"][0]
    heavy_worker = weighted_config["configuration"]["workers"][0]["id"]
    light_worker = weighted_config["configuration"]["workers"][1]["id"]
    all_request_sets = [
        affinity,
        failover,
        post_reconfigure,
        election_continuity,
        weighted,
    ]
    all_requests = [
        request for request_set in all_request_sets for request in request_set["requests"]
    ]
    successful_requests = sum(request.get("status") == 200 for request in all_requests)
    total_requests = len(all_requests)

    worker_health = health["workers"]
    health_summary = {
        worker_id: {
            "status": observation["status"],
            "attention": observation.get("body", {})
            .get("model", {})
            .get("attention"),
            "draft": observation.get("body", {})
            .get("speculative_draft", {})
            .get("quantization", {})
            .get("mode"),
            "decoder_mode": observation.get("body", {}).get("decoder_mode"),
        }
        for worker_id, observation in worker_health.items()
    }

    assertions = [
        assertion(
            "three-node Raft cluster elects exactly one initial leader",
            initial_election["leader_id"] in {"node-a", "node-b", "node-c"}
            and sum(
                status.get("status") == 200
                and status.get("body", {}).get("role") == "leader"
                for status in initial_election["statuses"]
            )
            == 1,
            {
                "leader": initial_election["leader_id"],
                "term": initial_election["term"],
            },
        ),
        assertion(
            "initial committed configuration contains three real CPU workers",
            initial_config["configuration"]["routing_policy"] == "consistent-hash"
            and initial_workers == {"cpu-real-a", "cpu-real-b", "cpu-real-c"},
            initial_config,
        ),
        assertion(
            "all real workers expose online-tiled FP32 attention, paged cache, and INT8 drafts",
            set(worker_health) == initial_workers
            and all(
                summary["status"] == 200
                and summary["attention"]["algorithm"] == "online-tiled"
                and summary["attention"]["precision"] == "fp32"
                and summary["attention"]["tile_tokens"] == 32
                and summary["draft"] == "int8"
                and summary["decoder_mode"] == "paged-kv-cache"
                for summary in health_summary.values()
            ),
            health_summary,
        ),
        assertion(
            "gateway atomically applies the initial revision and term",
            initial_gateway["status"]["body"]["routing_snapshot"][
                "control_revision"
            ]
            == initial_revision
            and initial_gateway["status"]["body"]["routing_snapshot"][
                "control_term"
            ]
            == initial_config["term"],
            initial_gateway["status"]["body"]["routing_snapshot"],
        ),
        assertion(
            "consistent-hash affinity sends a repeated prompt to one worker",
            affinity["succeeded"] == 2
            and affinity["same_worker"]
            and affinity["same_content"],
            {
                "workers": [item["worker"] for item in affinity["requests"]],
                "content": affinity["requests"][0].get("content"),
            },
        ),
        assertion(
            "the second affinity request reuses a real paged prefix",
            affinity["first_prefix_cache_hit"] is False
            and affinity["second_prefix_cache_hit"] is True,
            {
                "first": affinity["first_prefix_cache_hit"],
                "second": affinity["second_prefix_cache_hit"],
            },
        ),
        assertion(
            "initial request headers fence both requests to the initial revision",
            revisions(affinity) == [initial_revision, initial_revision]
            and all(
                item["config_term"] == initial_config["term"]
                for item in affinity["requests"]
            ),
            {
                "revisions": revisions(affinity),
                "terms": [item["config_term"] for item in affinity["requests"]],
            },
        ),
        assertion(
            "worker fault targets the affinity owner and records an owned child process",
            dead_worker == affinity["requests"][0]["worker"]
            and worker_fault["event"] == "worker_killed"
            and worker_fault["scope"] == "owned-child-process"
            and worker_fault["pid"] > 0,
            worker_fault,
        ),
        assertion(
            "pre-header connection failure retries once to a live real worker",
            failover["succeeded"] == 1
            and failover_request["attempts"] == 2
            and failover_request["worker"] in live_workers
            and failover_request["worker"] != dead_worker,
            failover_request,
        ),
        assertion(
            "retry preserves exact completion and the request-start configuration revision",
            failover_request["content"] == affinity["requests"][0]["content"]
            and failover_request["config_revision"] == initial_revision
            and failover_request["config_term"] == initial_config["term"],
            {
                "content": failover_request.get("content"),
                "revision": failover_request.get("config_revision"),
                "term": failover_request.get("config_term"),
            },
        ),
        assertion(
            "next committed revision removes only the failed worker",
            live_revision > initial_revision
            and len(live_workers) == 2
            and dead_worker not in live_workers
            and live_workers == initial_workers - {dead_worker},
            live_config,
        ),
        assertion(
            "gateway applies the live-worker revision without rollback",
            live_gateway["status"]["body"]["routing_snapshot"][
                "control_revision"
            ]
            == live_revision
            and {
                worker["id"] for worker in live_gateway["status"]["body"]["workers"]
            }
            == live_workers,
            live_gateway["status"]["body"],
        ),
        assertion(
            "post-reconfiguration requests need one attempt and never select the failed worker",
            post_reconfigure["succeeded"] == post_reconfigure["requested"] == 4
            and all(item["attempts"] == 1 for item in post_reconfigure["requests"])
            and all(item["worker"] in live_workers for item in post_reconfigure["requests"])
            and revisions(post_reconfigure) == [live_revision] * 4,
            {
                "workers": [item["worker"] for item in post_reconfigure["requests"]],
                "attempts": [item["attempts"] for item in post_reconfigure["requests"]],
                "revisions": revisions(post_reconfigure),
            },
        ),
        assertion(
            "control-plane fault kills the observed leader child process",
            control_fault["target"] == control_leader["leader_id"]
            and control_fault["event"] == "leader_killed"
            and control_fault["scope"] == "owned-child-process",
            control_fault,
        ),
        assertion(
            "all real-model requests succeed while the control plane elects a leader",
            election_continuity["succeeded"]
            == election_continuity["requested"]
            == 6
            and revisions(election_continuity) == [live_revision] * 6,
            {
                "succeeded": election_continuity["succeeded"],
                "revisions": revisions(election_continuity),
            },
        ),
        assertion(
            "remaining Raft majority elects a new leader in a bounded interval",
            re_election["leader_id"] != control_leader["leader_id"]
            and re_election["term"] > control_leader["term"]
            and 0 < re_election["latency_ms"] < 1_500,
            {
                "old": control_leader["leader_id"],
                "new": re_election["leader_id"],
                "latency_ms": re_election["latency_ms"],
                "term": re_election["term"],
            },
        ),
        assertion(
            "new leader commits a strictly newer weighted revision",
            weighted_revision > live_revision
            and weighted_config["term"] == re_election["term"]
            and weighted_config["configuration"]["routing_policy"]
            == "weighted-round-robin",
            weighted_config,
        ),
        assertion(
            "gateway applies the weighted revision and exposes it atomically",
            weighted_gateway["status"]["body"]["routing_snapshot"][
                "control_revision"
            ]
            == weighted_revision
            and weighted_gateway["status"]["body"]["routing_snapshot"][
                "control_term"
            ]
            == weighted_config["term"],
            weighted_gateway["status"]["body"]["routing_snapshot"],
        ),
        assertion(
            "three-to-one real-worker weights produce an exact six-to-two distribution",
            weighted["succeeded"] == weighted["requested"] == 8
            and weighted["worker_counts"].get(heavy_worker) == 6
            and weighted["worker_counts"].get(light_worker) == 2
            and revisions(weighted) == [weighted_revision] * 8,
            {
                "heavy": heavy_worker,
                "light": light_worker,
                "counts": weighted["worker_counts"],
                "revisions": revisions(weighted),
            },
        ),
        assertion(
            "final SSE stream reconstructs the real completion and terminates",
            streamed["status"] == 200
            and streamed["done_received"]
            and streamed["finish_reason"] == "stop"
            and streamed["content"] == affinity["requests"][0]["content"]
            and streamed["config_revision"] == weighted_revision,
            streamed,
        ),
        assertion(
            "final stream exercises speculative decoding through the online-attention worker",
            streamed["generation"]["speculation"]["enabled"]
            and streamed["generation"]["speculation"]["draft_quantization"] == "int8"
            and streamed["generation"]["speculation"]["target_forward_calls"] == 2
            and streamed["generation"]["speculation"]["accepted_tokens"] == 6,
            streamed["generation"]["speculation"],
        ),
        assertion(
            "all twenty-one non-stream requests plus final SSE complete successfully",
            total_requests == 21
            and successful_requests == 21
            and streamed["status"] == 200,
            {
                "non_stream_successes": successful_requests,
                "non_stream_total": total_requests,
                "stream_status": streamed["status"],
            },
        ),
        assertion(
            "environment records the CPU evidence boundary without a CUDA claim",
            environment["cuda"]["toolchain_available"] is False
            and environment["cuda"]["pytorch_available"] is False
            and "host CPU" in environment["benchmark_note"],
            environment["cuda"],
        ),
    ]

    passed_count = sum(item["passed"] for item in assertions)
    result = {
        "passed": passed_count == len(assertions),
        "assertions_passed": passed_count,
        "assertions_total": len(assertions),
        "initial_revision": initial_revision,
        "live_revision": live_revision,
        "weighted_revision": weighted_revision,
        "failed_worker": dead_worker,
        "failover_attempts": failover_request.get("attempts"),
        "election_request_successes": election_continuity["succeeded"],
        "re_election_latency_ms": re_election["latency_ms"],
        "weighted_distribution": weighted["worker_counts"],
        "non_stream_requests_succeeded": successful_requests,
        "stream_succeeded": streamed["status"] == 200 and streamed["done_received"],
        "assertions": assertions,
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    for item in assertions:
        print(f"{'PASS' if item['passed'] else 'FAIL'} {item['name']}")
    print(f"{passed_count}/{len(assertions)} assertions passed")
    if not result["passed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
