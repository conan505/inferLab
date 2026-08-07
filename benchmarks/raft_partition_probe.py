#!/usr/bin/env python3
"""Drive and sanitize the controlled InferLab v0.25 Raft partition proof.

The probe intentionally models directed *Raft HTTP RPC* delivery, not packets.
It uses no ambient HTTP proxy and records observations rather than inferring
consensus state from client status codes.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import time
import urllib.error
import urllib.request
from collections import Counter
from pathlib import Path
from typing import Any, Callable


def wall_ms() -> float:
    return round(time.time() * 1000.0, 3)


def monotonic_ms() -> float:
    return time.perf_counter() * 1000.0


def parse_mapping(raw: str) -> dict[str, str]:
    values: dict[str, str] = {}
    for component in raw.split(","):
        if not component.strip():
            continue
        key, separator, value = component.partition("=")
        if not separator or not key.strip() or not value.strip():
            raise SystemExit("mapping values must use non-empty name=value entries")
        if key.strip() in values:
            raise SystemExit(f"duplicate mapping name: {key.strip()}")
        values[key.strip()] = value.strip()
    if not values:
        raise SystemExit("at least one mapping entry is required")
    return values


def parse_json_bytes(raw: bytes) -> Any:
    if not raw:
        return None
    try:
        return json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        return raw.decode("utf-8", errors="replace")


def public_error(error: BaseException) -> str:
    reason = getattr(error, "reason", error)
    return type(reason if isinstance(reason, BaseException) else error).__name__


def request_json(
    url: str,
    *,
    method: str = "GET",
    body: Any = None,
    timeout: float = 0.5,
) -> dict[str, Any]:
    started = monotonic_ms()
    data = None if body is None else json.dumps(body).encode("utf-8")
    request = urllib.request.Request(
        url,
        data=data,
        method=method,
        headers={"content-type": "application/json"} if data is not None else {},
    )
    # A loopback proof must not accidentally traverse environment proxies.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    try:
        response = opener.open(request, timeout=timeout)
    except urllib.error.HTTPError as error:
        response = error
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        return {
            "status": None,
            "duration_ms": round(monotonic_ms() - started, 3),
            "transport_error": public_error(error),
        }
    with response:
        return {
            "status": response.status,
            "duration_ms": round(monotonic_ms() - started, 3),
            "body": parse_json_bytes(response.read()),
        }


def wait_loop(
    timeout: float,
    sample: Callable[[], Any],
    matches: Callable[[Any], bool],
    description: str,
) -> tuple[int, Any, float]:
    started = monotonic_ms()
    deadline = time.monotonic() + timeout
    samples = 0
    latest: Any = None
    while True:
        samples += 1
        latest = sample()
        if matches(latest):
            return samples, latest, monotonic_ms() - started
        if time.monotonic() >= deadline:
            raise SystemExit(
                json.dumps(
                    {"error": f"timed out waiting for {description}", "latest": latest},
                    indent=2,
                    sort_keys=True,
                )
            )
        time.sleep(0.025)


def cluster_statuses(nodes: dict[str, str]) -> dict[str, Any]:
    return {
        node_id: {
            "base_url": url.rstrip("/"),
            "observation": request_json(f"{url.rstrip('/')}/v1/control/status"),
        }
        for node_id, url in nodes.items()
    }


def cluster_matches(args: argparse.Namespace, statuses: dict[str, Any]) -> bool:
    bodies: dict[str, dict[str, Any]] = {}
    for node_id, entry in statuses.items():
        observation = entry["observation"]
        if observation.get("status") != 200 or not isinstance(observation.get("body"), dict):
            return False
        body = observation["body"]
        if body.get("node_id") != node_id:
            return False
        bodies[node_id] = body
    leaders = [node_id for node_id, body in bodies.items() if body.get("role") == "leader"]
    if len(leaders) != 1:
        return False
    if args.expected_leader and leaders != [args.expected_leader]:
        return False
    if args.required_follower and bodies.get(args.required_follower, {}).get("role") != "follower":
        return False
    for body in bodies.values():
        if args.minimum_term is not None and body.get("term", -1) < args.minimum_term:
            return False
        if args.commit_index is not None and body.get("commit_index") != args.commit_index:
            return False
        if args.revision is not None:
            committed = body.get("committed_configuration") or {}
            if committed.get("revision") != args.revision:
                return False
            if args.policy and (committed.get("configuration") or {}).get("routing_policy") != args.policy:
                return False
    return True


def capture_cluster(args: argparse.Namespace) -> None:
    nodes = parse_mapping(args.nodes)
    statuses = cluster_statuses(nodes)
    print(
        json.dumps(
            {
                "schema": "inferlab.raft-partition-cluster.v0.25",
                "observed_at_ms": wall_ms(),
                "statuses": statuses,
            },
            indent=2,
            sort_keys=True,
        )
    )
    if any(entry["observation"].get("status") != 200 for entry in statuses.values()):
        raise SystemExit(1)


def wait_cluster(args: argparse.Namespace) -> None:
    nodes = parse_mapping(args.nodes)
    samples, statuses, duration = wait_loop(
        args.timeout,
        lambda: cluster_statuses(nodes),
        lambda observed: cluster_matches(args, observed),
        f"cluster condition across {','.join(nodes)}",
    )
    leaders = [
        node_id
        for node_id, entry in statuses.items()
        if entry["observation"]["body"]["role"] == "leader"
    ]
    print(
        json.dumps(
            {
                "schema": "inferlab.raft-partition-cluster-wait.v0.25",
                "observed_at_ms": wall_ms(),
                "duration_ms": round(duration, 3),
                "samples": samples,
                "leader_id": leaders[0],
                "expected": {
                    "expected_leader": args.expected_leader,
                    "required_follower": args.required_follower,
                    "minimum_term": args.minimum_term,
                    "commit_index": args.commit_index,
                    "revision": args.revision,
                    "policy": args.policy,
                },
                "statuses": statuses,
            },
            indent=2,
            sort_keys=True,
        )
    )


def link_statuses(links: dict[str, str]) -> dict[str, Any]:
    return {
        link_id: {
            "base_url": url.rstrip("/"),
            "observation": request_json(f"{url.rstrip('/')}/v1/link/status"),
        }
        for link_id, url in links.items()
    }


def capture_links(args: argparse.Namespace) -> None:
    links = parse_mapping(args.links)
    statuses = link_statuses(links)
    print(
        json.dumps(
            {
                "schema": "inferlab.raft-partition-links.v0.25",
                "observed_at_ms": wall_ms(),
                "statuses": statuses,
            },
            indent=2,
            sort_keys=True,
        )
    )
    if any(entry["observation"].get("status") != 200 for entry in statuses.values()):
        raise SystemExit(1)


def set_links(args: argparse.Namespace) -> None:
    links = parse_mapping(args.links)
    before = link_statuses(links)
    transitions = []
    for link_id, url in links.items():
        observation = request_json(
            f"{url.rstrip('/')}/v1/link/mode",
            method="PUT",
            body={"mode": args.mode, "reason": args.reason},
        )
        transitions.append({"link_id": link_id, "observation": observation})
        body = observation.get("body") or {}
        if observation.get("status") != 200 or body.get("mode") != args.mode:
            print(
                json.dumps(
                    {"error": "link transition failed", "transition": transitions[-1]},
                    indent=2,
                    sort_keys=True,
                )
            )
            raise SystemExit(1)
    after = link_statuses(links)
    print(
        json.dumps(
            {
                "schema": "inferlab.raft-link-transition.v0.25",
                "observed_at_ms": wall_ms(),
                "requested_mode": args.mode,
                "reason": args.reason,
                "ordered_link_ids": list(links),
                "before": before,
                "transitions": transitions,
                "after": after,
            },
            indent=2,
            sort_keys=True,
        )
    )


def submit_write(args: argparse.Namespace) -> None:
    body = json.loads(args.body.read_text(encoding="utf-8"))
    started_at = wall_ms()
    response = request_json(
        f"{args.url.rstrip('/')}/v1/control/config",
        method="PUT",
        body=body,
        timeout=args.timeout,
    )
    print(
        json.dumps(
            {
                "schema": "inferlab.raft-partition-write.v0.25",
                "started_at_ms": started_at,
                "observed_at_ms": wall_ms(),
                "request": body,
                "response": response,
            },
            indent=2,
            sort_keys=True,
        )
    )
    if response.get("status") != args.status:
        raise SystemExit(1)


def read_state(data_root: Path, node_ids: list[str]) -> dict[str, Any]:
    states: dict[str, Any] = {}
    for node_id in node_ids:
        raw = (data_root / node_id / "state.json").read_bytes()
        states[node_id] = {
            "sha256": hashlib.sha256(raw).hexdigest(),
            "state": json.loads(raw),
        }
    return states


def capture_state(args: argparse.Namespace) -> None:
    node_ids = [value.strip() for value in args.node_ids.split(",") if value.strip()]
    states = read_state(args.data_root, node_ids)
    print(
        json.dumps(
            {
                "schema": "inferlab.raft-partition-durable-state.v0.25",
                "observed_at_ms": wall_ms(),
                "nodes": states,
            },
            indent=2,
            sort_keys=True,
        )
    )


def state_converged(
    states: dict[str, Any], expected_commit: int, expected_policy: str
) -> bool:
    values = [entry["state"] for entry in states.values()]
    if not values or any(value.get("commit_index") != expected_commit for value in values):
        return False
    logs = [value.get("log") for value in values]
    if any(log != logs[0] for log in logs[1:]):
        return False
    if not logs[0] or logs[0][-1].get("index") != expected_commit:
        return False
    command = logs[0][expected_commit - 1].get("command") or {}
    configuration = command.get("configuration") or {}
    return configuration.get("routing_policy") == expected_policy


def wait_state(args: argparse.Namespace) -> None:
    node_ids = [value.strip() for value in args.node_ids.split(",") if value.strip()]
    samples, states, duration = wait_loop(
        args.timeout,
        lambda: read_state(args.data_root, node_ids),
        lambda observed: state_converged(observed, args.commit_index, args.policy),
        f"durable convergence at index {args.commit_index}",
    )
    print(
        json.dumps(
            {
                "schema": "inferlab.raft-partition-durable-convergence.v0.25",
                "observed_at_ms": wall_ms(),
                "duration_ms": round(duration, 3),
                "samples": samples,
                "expected_commit_index": args.commit_index,
                "expected_policy": args.policy,
                "nodes": states,
            },
            indent=2,
            sort_keys=True,
        )
    )


def capture_events(args: argparse.Namespace) -> None:
    paths = parse_mapping(args.events)
    links: dict[str, Any] = {}
    for link_id, raw_path in paths.items():
        records = [
            json.loads(line)
            for line in Path(raw_path).read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
        sequences = [record.get("sequence") for record in records]
        links[link_id] = {
            "event_count": len(records),
            "event_types": dict(sorted(Counter(record.get("event") for record in records).items())),
            "sequences_contiguous": sequences == list(range(1, len(records) + 1)),
            "records": records,
        }
    print(
        json.dumps(
            {
                "schema": "inferlab.raft-link-events-capture.v0.25",
                "observed_at_ms": wall_ms(),
                "links": links,
            },
            indent=2,
            sort_keys=True,
        )
    )


def command_evidence(args: argparse.Namespace) -> None:
    command = json.loads(args.command_json)
    stdout = args.stdout_file.read_text(encoding="utf-8", errors="replace")
    stderr = args.stderr_file.read_text(encoding="utf-8", errors="replace")
    print(
        json.dumps(
            {
                "schema": "inferlab.command-evidence.v0.25",
                "command": command,
                "status": args.status,
                "stdout": stdout,
                "stderr": stderr,
            },
            indent=2,
            sort_keys=True,
        )
    )
    if args.status != 0:
        raise SystemExit(1)


def sanitize(args: argparse.Namespace) -> None:
    evidence = args.evidence_dir.resolve()
    exact_paths = {
        args.proof_root,
        os.path.normpath(args.proof_root),
        str(Path(args.proof_root).resolve()),
        args.project_root,
        os.path.normpath(args.project_root),
        str(Path(args.project_root).resolve()),
    }
    exact_paths = {value for value in exact_paths if value and value != os.path.sep}
    sensitive_keys = {"data_directory", "event_path", "state_path", "snapshot_path"}
    host_path = re.compile(r"(?:/Users|/home|/private/var|/tmp)/[^\s\"'<>]+")
    forbidden = ("-----BEGIN", "PRIVATE KEY", "CERTIFICATE-----")
    replacements = 0

    def redact(value: Any, key: str | None = None) -> Any:
        nonlocal replacements
        if isinstance(value, dict):
            return {item_key: redact(item_value, item_key) for item_key, item_value in value.items()}
        if isinstance(value, list):
            return [redact(item) for item in value]
        if not isinstance(value, str):
            return value
        if key in sensitive_keys:
            replacements += 1
            return "<redacted-sensitive-path>"
        result = value
        for path in sorted(exact_paths, key=len, reverse=True):
            if path in result:
                replacements += result.count(path)
                result = result.replace(path, "<redacted-host-path>")
        result, count = host_path.subn("<redacted-host-path>", result)
        replacements += count
        return result

    files = sorted(evidence.glob("*.json"))
    for path in files:
        value = json.loads(path.read_text(encoding="utf-8"))
        path.write_text(json.dumps(redact(value), indent=2, sort_keys=True) + "\n", encoding="utf-8")
    retained = "\n".join(path.read_text(encoding="utf-8") for path in files)
    leaks = [marker for marker in forbidden if marker in retained]
    remaining_paths = [path for path in exact_paths if path in retained]
    if leaks or remaining_paths or host_path.search(retained):
        raise SystemExit("evidence sanitizer left private material or host paths")
    print(
        json.dumps(
            {
                "schema": "inferlab.evidence-sanitizer.v0.25",
                "files_sanitized": [path.name for path in files],
                "replacement_count": replacements,
                "private_material_markers": 0,
                "remaining_host_paths": 0,
            },
            indent=2,
            sort_keys=True,
        )
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)

    capture = commands.add_parser("capture-cluster")
    capture.add_argument("--nodes", required=True)
    capture.set_defaults(handler=capture_cluster)

    wait = commands.add_parser("wait-cluster")
    wait.add_argument("--nodes", required=True)
    wait.add_argument("--timeout", type=float, default=10.0)
    wait.add_argument("--expected-leader")
    wait.add_argument("--required-follower")
    wait.add_argument("--minimum-term", type=int)
    wait.add_argument("--commit-index", type=int)
    wait.add_argument("--revision", type=int)
    wait.add_argument("--policy")
    wait.set_defaults(handler=wait_cluster)

    links = commands.add_parser("capture-links")
    links.add_argument("--links", required=True)
    links.set_defaults(handler=capture_links)

    transition = commands.add_parser("set-links")
    transition.add_argument("--links", required=True)
    transition.add_argument("--mode", choices=["allow", "drop"], required=True)
    transition.add_argument("--reason", required=True)
    transition.set_defaults(handler=set_links)

    write = commands.add_parser("submit-write")
    write.add_argument("--url", required=True)
    write.add_argument("--body", type=Path, required=True)
    write.add_argument("--status", type=int, required=True)
    write.add_argument("--timeout", type=float, default=3.0)
    write.set_defaults(handler=submit_write)

    state = commands.add_parser("capture-state")
    state.add_argument("--data-root", type=Path, required=True)
    state.add_argument("--node-ids", default="node-a,node-b,node-c")
    state.set_defaults(handler=capture_state)

    state_wait = commands.add_parser("wait-state")
    state_wait.add_argument("--data-root", type=Path, required=True)
    state_wait.add_argument("--node-ids", default="node-a,node-b,node-c")
    state_wait.add_argument("--commit-index", type=int, required=True)
    state_wait.add_argument("--policy", required=True)
    state_wait.add_argument("--timeout", type=float, default=10.0)
    state_wait.set_defaults(handler=wait_state)

    events = commands.add_parser("capture-events")
    events.add_argument("--events", required=True)
    events.set_defaults(handler=capture_events)

    command = commands.add_parser("command-evidence")
    command.add_argument("--command-json", required=True)
    command.add_argument("--status", type=int, required=True)
    command.add_argument("--stdout-file", type=Path, required=True)
    command.add_argument("--stderr-file", type=Path, required=True)
    command.set_defaults(handler=command_evidence)

    sanitizer = commands.add_parser("sanitize-evidence")
    sanitizer.add_argument("--evidence-dir", type=Path, required=True)
    sanitizer.add_argument("--proof-root", required=True)
    sanitizer.add_argument("--project-root", required=True)
    sanitizer.set_defaults(handler=sanitize)
    return parser


def main() -> None:
    args = build_parser().parse_args()
    args.handler(args)


if __name__ == "__main__":
    main()
