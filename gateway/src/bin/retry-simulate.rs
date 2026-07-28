use std::{collections::BTreeMap, process::ExitCode, time::Duration};

use gateway::resilience::{FullJitter, exponential_cap};
use serde_json::{Value, json};

const CLIENTS: u64 = 1_000;
const RETRIES: usize = 3;
const BASE_DELAY_MS: u64 = 100;
const MAX_DELAY_MS: u64 = 800;
const BUCKET_MS: u64 = 25;
const SEED: u64 = 42;

fn main() -> ExitCode {
    let report = simulate();
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("simulation report is serializable")
    );
    ExitCode::SUCCESS
}

fn simulate() -> Value {
    let jitter = FullJitter::with_seed(SEED);
    let mut synchronized = BTreeMap::new();
    let mut full_jitter = BTreeMap::new();

    for _ in 0..CLIENTS {
        let mut synchronized_time = 0;
        let mut jittered_time = 0;
        for retry_index in 0..RETRIES {
            let cap = exponential_cap(
                Duration::from_millis(BASE_DELAY_MS),
                Duration::from_millis(MAX_DELAY_MS),
                retry_index,
            );
            let cap_ms = u64::try_from(cap.as_millis()).expect("small simulation duration");
            synchronized_time += cap_ms;
            jittered_time +=
                u64::try_from(jitter.delay(cap).as_millis()).expect("small simulation duration");
            *synchronized
                .entry(bucket(synchronized_time))
                .or_insert(0_u64) += 1;
            *full_jitter.entry(bucket(jittered_time)).or_insert(0_u64) += 1;
        }
    }

    let synchronized_peak = synchronized.values().copied().max().unwrap_or(0);
    let jitter_peak = full_jitter.values().copied().max().unwrap_or(0);
    json!({
        "schema": "inferlab.retry-jitter-simulation.v0.0.7",
        "config": {
            "clients": CLIENTS,
            "retries_per_client": RETRIES,
            "base_delay_ms": BASE_DELAY_MS,
            "max_delay_ms": MAX_DELAY_MS,
            "bucket_ms": BUCKET_MS,
            "seed": SEED,
        },
        "synchronized_backoff": {
            "peak_retries_in_one_bucket": synchronized_peak,
            "occupied_buckets": synchronized.len(),
            "timeline": timeline(&synchronized),
        },
        "full_jitter": {
            "peak_retries_in_one_bucket": jitter_peak,
            "occupied_buckets": full_jitter.len(),
            "timeline": timeline(&full_jitter),
        },
        "peak_reduction_percent": if synchronized_peak == 0 {
            0.0
        } else {
            (1.0 - jitter_peak as f64 / synchronized_peak as f64) * 100.0
        },
    })
}

fn bucket(milliseconds: u64) -> u64 {
    milliseconds / BUCKET_MS * BUCKET_MS
}

fn timeline(buckets: &BTreeMap<u64, u64>) -> Vec<Value> {
    buckets
        .iter()
        .map(|(time_ms, retries)| json!({"time_ms": time_ms, "retries": retries}))
        .collect()
}
