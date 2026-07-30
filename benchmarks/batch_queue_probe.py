#!/usr/bin/env python3
"""Exercise InferLab's durable batch queue on both sides of a service restart."""

import argparse
import json
import sqlite3
import urllib.error
import urllib.request


def request_json(base_url: str, method: str, path: str, payload=None) -> dict:
    encoded = None
    headers = {}
    if payload is not None:
        encoded = json.dumps(payload).encode("utf-8")
        headers["content-type"] = "application/json"
    request = urllib.request.Request(
        f"{base_url}{path}",
        data=encoded,
        headers=headers,
        method=method,
    )
    try:
        with urllib.request.urlopen(request, timeout=2) as response:
            body = response.read()
            return {
                "status": response.status,
                "body": json.loads(body) if body else None,
            }
    except urllib.error.HTTPError as error:
        body = error.read()
        return {
            "status": error.code,
            "body": json.loads(body) if body else None,
        }


def apply_effect(database_path: str, idempotency_key: str) -> dict:
    with sqlite3.connect(database_path) as connection:
        connection.execute(
            """
            CREATE TABLE IF NOT EXISTS effects (
                idempotency_key TEXT PRIMARY KEY,
                result TEXT NOT NULL
            )
            """
        )
        cursor = connection.execute(
            """
            INSERT OR IGNORE INTO effects (idempotency_key, result)
            VALUES (?, ?)
            """,
            (idempotency_key, "summary-written-once"),
        )
        connection.commit()
        rows = connection.execute(
            """
            SELECT idempotency_key, result
            FROM effects
            ORDER BY idempotency_key
            """
        ).fetchall()
    return {
        "idempotency_key": idempotency_key,
        "created": cursor.rowcount == 1,
        "effects": [
            {"idempotency_key": row[0], "result": row[1]} for row in rows
        ],
    }


def prepare(base_url: str, effect_ledger: str) -> dict:
    crash_payload = {
        "idempotency_key": "proof-crash-job",
        "payload": {"prompt": "survive a queue service restart"},
        "max_attempts": 3,
    }
    crash_enqueue = request_json(
        base_url, "POST", "/v1/batch/jobs", crash_payload
    )
    duplicate = request_json(
        base_url, "POST", "/v1/batch/jobs", crash_payload
    )
    conflict_payload = {
        **crash_payload,
        "payload": {"prompt": "this must conflict"},
    }
    conflict = request_json(
        base_url, "POST", "/v1/batch/jobs", conflict_payload
    )
    pending_enqueue = request_json(
        base_url,
        "POST",
        "/v1/batch/jobs",
        {
            "idempotency_key": "proof-pending-job",
            "payload": {"prompt": "remain pending across restart"},
            "max_attempts": 3,
        },
    )
    crash_claim = request_json(
        base_url,
        "POST",
        "/v1/batch/claim",
        {
            "consumer_id": "consumer-before-crash",
            "visibility_timeout_ms": 600,
        },
    )
    effect_before_crash = apply_effect(
        effect_ledger, crash_claim["body"]["idempotency_key"]
    )
    status = request_json(base_url, "GET", "/internal/status")
    return {
        "schema": "inferlab.batch-before-crash.v0.5",
        "crash_enqueue": crash_enqueue,
        "duplicate_enqueue": duplicate,
        "conflicting_enqueue": conflict,
        "pending_enqueue": pending_enqueue,
        "crash_claim": crash_claim,
        "effect_before_crash": effect_before_crash,
        "status_before_crash": status,
    }


def recover(base_url: str, before: dict, effect_ledger: str) -> dict:
    crash_job = before["crash_enqueue"]["body"]["job"]
    first_claim = before["crash_claim"]["body"]
    duplicate_after_restart = request_json(
        base_url,
        "POST",
        "/v1/batch/jobs",
        {
            "idempotency_key": "proof-crash-job",
            "payload": {"prompt": "survive a queue service restart"},
            "max_attempts": 3,
        },
    )
    redelivery = request_json(
        base_url,
        "POST",
        "/v1/batch/claim",
        {
            "consumer_id": "consumer-after-crash",
            "visibility_timeout_ms": 5_000,
        },
    )
    duplicate_effect_application = apply_effect(
        effect_ledger, redelivery["body"]["idempotency_key"]
    )
    stale_ack = request_json(
        base_url,
        "POST",
        f"/v1/batch/jobs/{crash_job['id']}/ack",
        {
            "consumer_id": first_claim["consumer_id"],
            "claim_token": first_claim["claim_token"],
        },
    )
    current_claim = redelivery["body"]
    current_ack = request_json(
        base_url,
        "POST",
        f"/v1/batch/jobs/{crash_job['id']}/ack",
        {
            "consumer_id": current_claim["consumer_id"],
            "claim_token": current_claim["claim_token"],
        },
    )

    pending_claim = request_json(
        base_url,
        "POST",
        "/v1/batch/claim",
        {
            "consumer_id": "pending-consumer",
            "visibility_timeout_ms": 5_000,
        },
    )
    pending_ack = request_json(
        base_url,
        "POST",
        f"/v1/batch/jobs/{pending_claim['body']['job_id']}/ack",
        {
            "consumer_id": pending_claim["body"]["consumer_id"],
            "claim_token": pending_claim["body"]["claim_token"],
        },
    )

    poison_enqueue = request_json(
        base_url,
        "POST",
        "/v1/batch/jobs",
        {
            "idempotency_key": "proof-poison-job",
            "payload": {"prompt": "always fail this job"},
            "max_attempts": 2,
        },
    )
    poison_first_claim = request_json(
        base_url,
        "POST",
        "/v1/batch/claim",
        {
            "consumer_id": "poison-consumer-a",
            "visibility_timeout_ms": 5_000,
        },
    )
    poison_first_failure = request_json(
        base_url,
        "POST",
        f"/v1/batch/jobs/{poison_first_claim['body']['job_id']}/fail",
        {
            "consumer_id": poison_first_claim["body"]["consumer_id"],
            "claim_token": poison_first_claim["body"]["claim_token"],
            "error": "deterministic poison failure one",
        },
    )
    poison_second_claim = request_json(
        base_url,
        "POST",
        "/v1/batch/claim",
        {
            "consumer_id": "poison-consumer-b",
            "visibility_timeout_ms": 5_000,
        },
    )
    poison_second_failure = request_json(
        base_url,
        "POST",
        f"/v1/batch/jobs/{poison_second_claim['body']['job_id']}/fail",
        {
            "consumer_id": poison_second_claim["body"]["consumer_id"],
            "claim_token": poison_second_claim["body"]["claim_token"],
            "error": "deterministic poison failure two",
        },
    )
    poison_job_id = poison_enqueue["body"]["job"]["id"]
    return {
        "schema": "inferlab.batch-after-restart.v0.5",
        "duplicate_after_restart": duplicate_after_restart,
        "redelivery": redelivery,
        "duplicate_effect_application": duplicate_effect_application,
        "stale_ack": stale_ack,
        "current_ack": current_ack,
        "pending_claim": pending_claim,
        "pending_ack": pending_ack,
        "poison_enqueue": poison_enqueue,
        "poison_first_claim": poison_first_claim,
        "poison_first_failure": poison_first_failure,
        "poison_second_claim": poison_second_claim,
        "poison_second_failure": poison_second_failure,
        "poison_job": request_json(
            base_url, "GET", f"/v1/batch/jobs/{poison_job_id}"
        ),
        "dead_letters": request_json(
            base_url, "GET", "/v1/batch/dead-letter"
        ),
        "status_after_recovery": request_json(
            base_url, "GET", "/internal/status"
        ),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--phase", choices=["prepare", "recover"], required=True)
    parser.add_argument("--before")
    parser.add_argument("--effect-ledger", required=True)
    args = parser.parse_args()
    if args.phase == "prepare":
        result = prepare(args.base_url, args.effect_ledger)
    else:
        if not args.before:
            parser.error("--before is required for recover")
        with open(args.before, encoding="utf-8") as source:
            result = recover(
                args.base_url, json.load(source), args.effect_ledger
            )
    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
