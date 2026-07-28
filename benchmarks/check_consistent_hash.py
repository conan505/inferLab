#!/usr/bin/env python3
"""Validate the deterministic distribution and remapping claims for v0.0.5."""

import argparse
import json


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--analysis", required=True)
    args = parser.parse_args()

    with open(args.analysis, encoding="utf-8") as source:
        analysis = json.load(source)

    key_count = analysis["key_count"]
    distributions = {
        item["virtual_nodes_per_worker"]: item for item in analysis["distribution"]
    }
    one_vnode = distributions[1]
    full_ring = distributions[128]
    addition = analysis["worker_addition"]
    removal = analysis["worker_removal"]

    checks = {
        "schema_is_v0_0_5": (
            analysis["schema"] == "inferlab.consistent-hash.v0.0.5"
        ),
        "same_inputs_replay_identically": analysis["deterministic_replay"],
        "every_distribution_accounts_for_every_key": all(
            sum(item["worker_counts"].values()) == key_count
            for item in analysis["distribution"]
        ),
        "all_workers_own_keys_with_128_vnodes": all(
            len(full_ring["worker_counts"]) == 3
            and count > 0
            for count in full_ring["worker_counts"].values()
        ),
        "virtual_nodes_improve_balance": (
            full_ring["max_relative_deviation_from_equal_share"]
            < one_vnode["max_relative_deviation_from_equal_share"]
        ),
        "128_vnodes_are_within_20_percent_of_equal_share": (
            full_ring["max_relative_deviation_from_equal_share"] <= 0.20
        ),
        "addition_only_moves_keys_to_the_new_worker": (
            addition["unexpected_remaps"] == 0
        ),
        "removal_only_moves_keys_owned_by_the_removed_worker": (
            removal["unexpected_remaps"] == 0
        ),
        "addition_moves_a_minority_near_one_quarter": (
            0.15 <= addition["remapped_fraction"] <= 0.35
        ),
        "removal_moves_a_minority_near_one_quarter": (
            0.15 <= removal["remapped_fraction"] <= 0.35
        ),
    }
    report = {
        "schema": "inferlab.consistent-hash-check.v0.0.5",
        "key_count": key_count,
        "one_vnode_max_relative_deviation": one_vnode[
            "max_relative_deviation_from_equal_share"
        ],
        "128_vnodes_max_relative_deviation": full_ring[
            "max_relative_deviation_from_equal_share"
        ],
        "addition_remapped_fraction": addition["remapped_fraction"],
        "removal_remapped_fraction": removal["remapped_fraction"],
        "checks": checks,
    }
    print(json.dumps(report, indent=2, sort_keys=True))

    if not all(checks.values()):
        raise SystemExit("consistent-hash analysis did not satisfy every check")


if __name__ == "__main__":
    main()
