#!/usr/bin/env python3
"""Validate the retained InferLab v0.5 durable-queue evidence."""

import argparse
import json


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--before", required=True)
    parser.add_argument("--after", required=True)
    parser.add_argument("--wal", required=True)
    args = parser.parse_args()

    with open(args.before, encoding="utf-8") as source:
        before = json.load(source)
    with open(args.after, encoding="utf-8") as source:
        after = json.load(source)
    with open(args.wal, encoding="utf-8") as source:
        events = [json.loads(line) for line in source if line.strip()]

    crash_job = before["crash_enqueue"]["body"]["job"]
    pending_job = before["pending_enqueue"]["body"]["job"]
    first_claim = before["crash_claim"]["body"]
    redelivery = after["redelivery"]["body"]
    poison_job = after["poison_job"]["body"]
    dead_letters = after["dead_letters"]["body"]
    final_status = after["status_after_recovery"]["body"]
    event_types = [event["type"] for event in events]
    expected_event_types = [
        "enqueued",
        "enqueued",
        "claimed",
        "released",
        "claimed",
        "acknowledged",
        "claimed",
        "acknowledged",
        "enqueued",
        "claimed",
        "released",
        "claimed",
        "dead_lettered",
    ]

    checks = {
        "schemas_are_v0_5": (
            before["schema"] == "inferlab.batch-before-crash.v0.5"
            and after["schema"] == "inferlab.batch-after-restart.v0.5"
        ),
        "enqueue_was_created": (
            before["crash_enqueue"]["status"] == 201
            and before["crash_enqueue"]["body"]["created"] is True
        ),
        "duplicate_before_crash_reused_job": (
            before["duplicate_enqueue"]["status"] == 200
            and before["duplicate_enqueue"]["body"]["created"] is False
            and before["duplicate_enqueue"]["body"]["job"]["id"]
            == crash_job["id"]
        ),
        "idempotency_conflict_was_rejected": (
            before["conflicting_enqueue"]["status"] == 409
            and before["conflicting_enqueue"]["body"]["error"]["code"]
            == "idempotency_conflict"
        ),
        "claimed_state_was_durable_before_crash": (
            first_claim["job_id"] == crash_job["id"]
            and first_claim["attempt"] == 1
            and before["status_before_crash"]["body"]["claimed"] == 1
            and before["status_before_crash"]["body"]["pending"] == 1
        ),
        "idempotency_survived_restart": (
            after["duplicate_after_restart"]["status"] == 200
            and after["duplicate_after_restart"]["body"]["created"] is False
            and after["duplicate_after_restart"]["body"]["job"]["id"]
            == crash_job["id"]
        ),
        "expired_job_redelivered_after_restart": (
            redelivery["job_id"] == crash_job["id"]
            and redelivery["attempt"] == 2
            and redelivery["consumer_id"] == "consumer-after-crash"
            and redelivery["claim_token"] != first_claim["claim_token"]
        ),
        "duplicate_effect_was_suppressed": (
            before["effect_before_crash"]["created"] is True
            and after["duplicate_effect_application"]["created"] is False
            and first_claim["idempotency_key"]
            == redelivery["idempotency_key"]
            == before["effect_before_crash"]["idempotency_key"]
            and len(after["duplicate_effect_application"]["effects"]) == 1
        ),
        "old_claim_was_fenced": (
            after["stale_ack"]["status"] == 409
            and after["stale_ack"]["body"]["error"]["code"]
            == "stale_claim"
        ),
        "new_claim_completed_job": (
            after["current_ack"]["status"] == 200
            and after["current_ack"]["body"]["status"] == "completed"
            and after["current_ack"]["body"]["attempts"] == 2
        ),
        "unclaimed_job_survived_restart": (
            after["pending_claim"]["body"]["job_id"] == pending_job["id"]
            and after["pending_ack"]["body"]["status"] == "completed"
        ),
        "attempts_are_bounded": (
            after["poison_first_claim"]["body"]["attempt"] == 1
            and after["poison_second_claim"]["body"]["attempt"] == 2
            and poison_job["attempts"] == poison_job["max_attempts"] == 2
        ),
        "poison_job_entered_dead_letter_queue": (
            after["poison_first_failure"]["body"]["status"] == "pending"
            and after["poison_second_failure"]["body"]["status"]
            == "dead_letter"
            and poison_job["status"] == "dead_letter"
            and [job["id"] for job in dead_letters] == [poison_job["id"]]
        ),
        "wal_has_exact_transition_history": (
            event_types == expected_event_types
            and events[3]["expired"] is True
            and events[3]["reason"] == "visibility_timeout"
            and events[-1]["expired"] is False
        ),
        "final_accounting_matches_history": (
            final_status["jobs_total"] == 3
            and final_status["pending"] == 0
            and final_status["claimed"] == 0
            and final_status["completed"] == 2
            and final_status["dead_letter"] == 1
            and final_status["claims_total"] == 5
            and final_status["acknowledgments_total"] == 2
            and final_status["redeliveries_total"] == 2
            and final_status["explicit_failures_total"] == 2
            and final_status["dead_lettered_total"] == 1
            and final_status["wal_events"] == len(events) == 13
        ),
    }
    report = {
        "schema": "inferlab.batch-check.v0.5",
        "jobs": {
            "crash_job": crash_job["id"],
            "pending_job": pending_job["id"],
            "poison_job": poison_job["id"],
        },
        "event_types": event_types,
        "final_status": final_status,
        "checks": checks,
        "passed": all(checks.values()),
    }
    print(json.dumps(report, indent=2, sort_keys=True))
    if not report["passed"]:
        failed = [name for name, passed in checks.items() if not passed]
        raise SystemExit(
            "batch queue evidence did not satisfy: " + ", ".join(failed)
        )


if __name__ == "__main__":
    main()
