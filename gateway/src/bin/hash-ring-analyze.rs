use std::{collections::BTreeMap, env, process::ExitCode};

use gateway::routing::ConsistentHashRing;
use serde_json::{Value, json};

const DEFAULT_KEYS: usize = 20_000;
const VIRTUAL_NODE_COUNTS: [usize; 3] = [1, 16, 128];

fn main() -> ExitCode {
    match parse_key_count().and_then(analyze) {
        Ok(report) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).expect("JSON report is serializable")
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("hash-ring-analyze: {error}");
            ExitCode::FAILURE
        }
    }
}

fn parse_key_count() -> Result<usize, String> {
    let mut arguments = env::args().skip(1);
    let mut key_count = DEFAULT_KEYS;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--keys" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--keys requires a positive integer".to_owned())?;
                key_count = value
                    .parse()
                    .map_err(|_| format!("invalid --keys value '{value}'"))?;
            }
            "--help" | "-h" => {
                println!("Usage: hash-ring-analyze [--keys COUNT]");
                return Err("help requested".to_owned());
            }
            _ => return Err(format!("unknown argument '{argument}'")),
        }
    }
    if key_count == 0 {
        return Err("--keys must be greater than zero".to_owned());
    }
    Ok(key_count)
}

fn analyze(key_count: usize) -> Result<Value, String> {
    let original = ["worker-a", "worker-b", "worker-c"];
    let mut distributions = Vec::new();
    for virtual_nodes in VIRTUAL_NODE_COUNTS {
        let ring = ring(&original, virtual_nodes)?;
        let counts = distribution(&ring, &original, key_count);
        distributions.push(json!({
            "virtual_nodes_per_worker": virtual_nodes,
            "total_ring_points": ring.virtual_point_count(),
            "worker_counts": counts,
            "max_relative_deviation_from_equal_share": max_relative_deviation(&counts, key_count),
        }));
    }

    let before_addition = ring(&original, 128)?;
    let with_added_worker = ring(&["worker-a", "worker-b", "worker-c", "worker-d"], 128)?;
    let addition = compare_rings(
        &before_addition,
        &with_added_worker,
        "worker-d",
        key_count,
        ChangeKind::Addition,
    );

    let before_removal = ring(&["worker-a", "worker-b", "worker-c", "worker-d"], 128)?;
    let after_removal = ring(&original, 128)?;
    let removal = compare_rings(
        &before_removal,
        &after_removal,
        "worker-d",
        key_count,
        ChangeKind::Removal,
    );

    let replay = ring(&original, 128)?;
    let deterministic_replay = (0..key_count).all(|number| {
        let key = key(number);
        before_addition.owner(key.as_bytes()) == replay.owner(key.as_bytes())
    });

    Ok(json!({
        "schema": "inferlab.consistent-hash.v0.0.5",
        "key_count": key_count,
        "hash": "FNV-1a-64 plus MurmurHash3 fmix64 avalanche",
        "distribution": distributions,
        "worker_addition": addition,
        "worker_removal": removal,
        "deterministic_replay": deterministic_replay,
    }))
}

fn ring(worker_ids: &[&str], virtual_nodes: usize) -> Result<ConsistentHashRing, String> {
    ConsistentHashRing::new(
        worker_ids
            .iter()
            .map(|worker| (*worker).to_owned())
            .collect(),
        virtual_nodes,
    )
}

fn distribution(
    ring: &ConsistentHashRing,
    worker_ids: &[&str],
    key_count: usize,
) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = worker_ids
        .iter()
        .map(|worker_id| ((*worker_id).to_owned(), 0))
        .collect();
    for number in 0..key_count {
        let key = key(number);
        *counts
            .entry(ring.owner(key.as_bytes()).to_owned())
            .or_default() += 1;
    }
    counts
}

fn max_relative_deviation(counts: &BTreeMap<String, usize>, key_count: usize) -> f64 {
    let expected = key_count as f64 / counts.len() as f64;
    counts
        .values()
        .map(|count| (*count as f64 - expected).abs() / expected)
        .fold(0.0, f64::max)
}

#[derive(Clone, Copy)]
enum ChangeKind {
    Addition,
    Removal,
}

fn compare_rings(
    before: &ConsistentHashRing,
    after: &ConsistentHashRing,
    changed_worker: &str,
    key_count: usize,
    kind: ChangeKind,
) -> Value {
    let mut remapped = 0;
    let mut unexpected_remaps = 0;
    for number in 0..key_count {
        let key = key(number);
        let old_owner = before.owner(key.as_bytes());
        let new_owner = after.owner(key.as_bytes());
        if old_owner != new_owner {
            remapped += 1;
            let expected = match kind {
                ChangeKind::Addition => new_owner == changed_worker,
                ChangeKind::Removal => old_owner == changed_worker,
            };
            if !expected {
                unexpected_remaps += 1;
            }
        }
    }

    json!({
        "changed_worker": changed_worker,
        "remapped_keys": remapped,
        "remapped_fraction": remapped as f64 / key_count as f64,
        "unexpected_remaps": unexpected_remaps,
    })
}

fn key(number: usize) -> String {
    format!("tenant-{}/prompt-prefix-{number}", number % 97)
}
