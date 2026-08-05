use std::{env, fs, hint::black_box, io, path::PathBuf, time::Instant};

use cpu_worker::{
    AttentionAlgorithm, AttentionConfig, AttentionPrecision, AttentionStats, run_attention,
};
use serde::Serialize;

#[derive(Serialize)]
struct ProbeOutput {
    implementation: &'static str,
    benchmark_repetitions: usize,
    benchmark_tile_tokens: u32,
    accumulation_precision: &'static str,
    fixture: FixtureObservation,
    causal_isolation: CausalIsolationObservation,
    large_score_stability: Vec<StabilityObservation>,
    sequence_scaling: Vec<ScalingObservation>,
}

#[derive(Serialize)]
struct FixtureObservation {
    tokens: usize,
    heads: usize,
    head_dimension: usize,
    tile_tokens: u32,
    queries: Vec<f32>,
    keys: Vec<f32>,
    values: Vec<f32>,
    variants: Vec<VariantObservation>,
}

#[derive(Serialize)]
struct VariantObservation {
    algorithm: AttentionAlgorithm,
    precision: AttentionPrecision,
    output: Vec<f32>,
    maximum_absolute_error_to_materialized_fp32: f64,
    stats: AttentionStats,
}

#[derive(Serialize)]
struct CausalIsolationObservation {
    query_position: usize,
    changed_future_positions: Vec<usize>,
    materialized_maximum_output_change: f64,
    online_tiled_maximum_output_change: f64,
}

#[derive(Serialize)]
struct StabilityObservation {
    precision: AttentionPrecision,
    all_outputs_finite: bool,
    maximum_absolute_algorithm_difference: f64,
}

#[derive(Serialize)]
struct ScalingObservation {
    tokens: usize,
    heads: usize,
    head_dimension: usize,
    profiles: Vec<ScalingProfile>,
}

#[derive(Serialize)]
struct ScalingProfile {
    algorithm: AttentionAlgorithm,
    median_us: f64,
    p95_us: f64,
    maximum_absolute_error_to_materialized: f64,
    checksum: f64,
    stats: AttentionStats,
}

#[derive(Clone)]
struct Arguments {
    repetitions: usize,
    tile_tokens: u32,
    output: PathBuf,
}

impl Arguments {
    fn parse(mut values: impl Iterator<Item = String>) -> io::Result<Self> {
        let mut repetitions = 31_usize;
        let mut tile_tokens = 32_u32;
        let mut output = None;
        while let Some(argument) = values.next() {
            match argument.as_str() {
                "--repetitions" => {
                    repetitions = required(&mut values, "--repetitions")?
                        .parse()
                        .map_err(|_| invalid("--repetitions must be a positive integer"))?;
                    if repetitions == 0 {
                        return Err(invalid("--repetitions must be positive"));
                    }
                }
                "--output" => output = Some(PathBuf::from(required(&mut values, "--output")?)),
                "--tile-tokens" => {
                    tile_tokens = required(&mut values, "--tile-tokens")?
                        .parse()
                        .map_err(|_| invalid("--tile-tokens must be an integer"))?;
                    if tile_tokens == 0 || tile_tokens > 4096 {
                        return Err(invalid("--tile-tokens must be between 1 and 4096"));
                    }
                }
                _ => return Err(invalid(&format!("unknown argument '{argument}'"))),
            }
        }
        Ok(Self {
            repetitions,
            tile_tokens,
            output: output.ok_or_else(|| invalid("--output is required"))?,
        })
    }
}

fn main() -> io::Result<()> {
    let arguments = Arguments::parse(env::args().skip(1))?;
    let fixture = fixture_observation()?;
    let output = ProbeOutput {
        implementation: "inferlab-attention-v1",
        benchmark_repetitions: arguments.repetitions,
        benchmark_tile_tokens: arguments.tile_tokens,
        accumulation_precision: "fp32",
        causal_isolation: causal_isolation_observation()?,
        large_score_stability: large_score_stability()?,
        sequence_scaling: [32_usize, 64, 128, 256]
            .into_iter()
            .map(|tokens| scaling_observation(tokens, arguments.repetitions, arguments.tile_tokens))
            .collect::<io::Result<Vec<_>>>()?,
        fixture,
    };
    fs::write(
        arguments.output,
        serde_json::to_string_pretty(&output).map_err(io::Error::other)? + "\n",
    )
}

fn fixture_observation() -> io::Result<FixtureObservation> {
    let tokens = 11;
    let heads = 3;
    let head_dimension = 8;
    let tile_tokens = 4;
    let width = heads * head_dimension;
    let queries = deterministic_values(tokens * width, 0.131, 0.8, 0.07);
    let keys = deterministic_values(tokens * width, 0.173, 0.7, -0.11);
    let values = deterministic_values(tokens * width, 0.097, 0.6, 0.19);
    let baseline = run(
        &queries,
        &keys,
        &values,
        tokens,
        heads,
        head_dimension,
        AttentionAlgorithm::Materialized,
        AttentionPrecision::Fp32,
        tile_tokens,
    )?;
    let mut variants = Vec::new();
    for precision in [
        AttentionPrecision::Fp32,
        AttentionPrecision::Fp16,
        AttentionPrecision::Bf16,
    ] {
        for algorithm in [
            AttentionAlgorithm::Materialized,
            AttentionAlgorithm::OnlineTiled,
        ] {
            let result = run(
                &queries,
                &keys,
                &values,
                tokens,
                heads,
                head_dimension,
                algorithm,
                precision,
                tile_tokens,
            )?;
            variants.push(VariantObservation {
                algorithm,
                precision,
                maximum_absolute_error_to_materialized_fp32: maximum_error(
                    &result.output,
                    &baseline.output,
                ),
                output: result.output,
                stats: result.stats,
            });
        }
    }
    Ok(FixtureObservation {
        tokens,
        heads,
        head_dimension,
        tile_tokens,
        queries,
        keys,
        values,
        variants,
    })
}

fn causal_isolation_observation() -> io::Result<CausalIsolationObservation> {
    let heads = 2;
    let head_dimension = 4;
    let tokens = 4;
    let width = heads * head_dimension;
    let queries = deterministic_values(width, 0.31, 0.8, 0.0);
    let keys = deterministic_values(tokens * width, 0.27, 0.7, 0.2);
    let values = deterministic_values(tokens * width, 0.23, 0.6, -0.3);
    let mut changed_keys = keys.clone();
    let mut changed_values = values.clone();
    for index in width..tokens * width {
        changed_keys[index] = changed_keys[index] * -17.0 + 23.0;
        changed_values[index] = changed_values[index] * 19.0 - 29.0;
    }
    let mut changes = Vec::new();
    for algorithm in [
        AttentionAlgorithm::Materialized,
        AttentionAlgorithm::OnlineTiled,
    ] {
        let config = AttentionConfig {
            algorithm,
            precision: AttentionPrecision::Fp32,
            tile_tokens: 2,
            causal: true,
        };
        let original = run_attention(
            &queries,
            &keys,
            &values,
            1,
            tokens,
            heads,
            head_dimension,
            0,
            config,
        )
        .map_err(io::Error::other)?;
        let changed = run_attention(
            &queries,
            &changed_keys,
            &changed_values,
            1,
            tokens,
            heads,
            head_dimension,
            0,
            config,
        )
        .map_err(io::Error::other)?;
        changes.push(maximum_error(&original.output, &changed.output));
    }
    Ok(CausalIsolationObservation {
        query_position: 0,
        changed_future_positions: vec![1, 2, 3],
        materialized_maximum_output_change: changes[0],
        online_tiled_maximum_output_change: changes[1],
    })
}

fn large_score_stability() -> io::Result<Vec<StabilityObservation>> {
    let tokens = 9;
    let heads = 2;
    let head_dimension = 8;
    let count = tokens * heads * head_dimension;
    let queries = (0..count)
        .map(|index| if index % 3 == 0 { 220.0 } else { -180.0 })
        .collect::<Vec<_>>();
    let keys = (0..count)
        .map(|index| if index % 5 < 2 { 190.0 } else { -210.0 })
        .collect::<Vec<_>>();
    let values = deterministic_values(count, 0.29, 0.9, 0.1);
    let mut observations = Vec::new();
    for precision in [
        AttentionPrecision::Fp32,
        AttentionPrecision::Fp16,
        AttentionPrecision::Bf16,
    ] {
        let materialized = run(
            &queries,
            &keys,
            &values,
            tokens,
            heads,
            head_dimension,
            AttentionAlgorithm::Materialized,
            precision,
            3,
        )?;
        let online = run(
            &queries,
            &keys,
            &values,
            tokens,
            heads,
            head_dimension,
            AttentionAlgorithm::OnlineTiled,
            precision,
            3,
        )?;
        observations.push(StabilityObservation {
            precision,
            all_outputs_finite: materialized
                .output
                .iter()
                .chain(&online.output)
                .all(|value| value.is_finite()),
            maximum_absolute_algorithm_difference: maximum_error(
                &materialized.output,
                &online.output,
            ),
        });
    }
    Ok(observations)
}

fn scaling_observation(
    tokens: usize,
    repetitions: usize,
    tile_tokens: u32,
) -> io::Result<ScalingObservation> {
    let heads = 4;
    let head_dimension = 32;
    let count = tokens * heads * head_dimension;
    let queries = deterministic_values(count, 0.0131, 0.8, 0.07);
    let keys = deterministic_values(count, 0.0173, 0.7, -0.11);
    let values = deterministic_values(count, 0.0097, 0.6, 0.19);
    let reference = run(
        &queries,
        &keys,
        &values,
        tokens,
        heads,
        head_dimension,
        AttentionAlgorithm::Materialized,
        AttentionPrecision::Fp32,
        tile_tokens,
    )?;
    let mut profiles = Vec::new();
    for algorithm in [
        AttentionAlgorithm::Materialized,
        AttentionAlgorithm::OnlineTiled,
    ] {
        let config = AttentionConfig {
            algorithm,
            precision: AttentionPrecision::Fp32,
            tile_tokens,
            causal: true,
        };
        for _ in 0..3 {
            black_box(
                run_attention(
                    &queries,
                    &keys,
                    &values,
                    tokens,
                    tokens,
                    heads,
                    head_dimension,
                    0,
                    config,
                )
                .map_err(io::Error::other)?,
            );
        }
        let mut durations = Vec::with_capacity(repetitions);
        let mut last = None;
        for _ in 0..repetitions {
            let started = Instant::now();
            let result = run_attention(
                &queries,
                &keys,
                &values,
                tokens,
                tokens,
                heads,
                head_dimension,
                0,
                config,
            )
            .map_err(io::Error::other)?;
            durations.push(started.elapsed().as_secs_f64() * 1_000_000.0);
            black_box(&result.output);
            last = Some(result);
        }
        durations.sort_by(f64::total_cmp);
        let result = last.expect("positive repetition count");
        profiles.push(ScalingProfile {
            algorithm,
            median_us: percentile(&durations, 0.50),
            p95_us: percentile(&durations, 0.95),
            maximum_absolute_error_to_materialized: maximum_error(
                &result.output,
                &reference.output,
            ),
            checksum: result.output.iter().map(|value| f64::from(*value)).sum(),
            stats: result.stats,
        });
    }
    Ok(ScalingObservation {
        tokens,
        heads,
        head_dimension,
        profiles,
    })
}

#[allow(clippy::too_many_arguments)]
fn run(
    queries: &[f32],
    keys: &[f32],
    values: &[f32],
    tokens: usize,
    heads: usize,
    head_dimension: usize,
    algorithm: AttentionAlgorithm,
    precision: AttentionPrecision,
    tile_tokens: u32,
) -> io::Result<cpu_worker::AttentionRun> {
    run_attention(
        queries,
        keys,
        values,
        tokens,
        tokens,
        heads,
        head_dimension,
        0,
        AttentionConfig {
            algorithm,
            precision,
            tile_tokens,
            causal: true,
        },
    )
    .map_err(io::Error::other)
}

fn deterministic_values(count: usize, frequency: f32, amplitude: f32, phase: f32) -> Vec<f32> {
    (0..count)
        .map(|index| {
            let position = index as f32 + 1.0;
            amplitude * (position * frequency + phase).sin()
                + 0.17 * (position * frequency * 0.37 - phase).cos()
        })
        .collect()
}

fn maximum_error(left: &[f32], right: &[f32]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(left, right)| f64::from((left - right).abs()))
        .fold(0.0, f64::max)
}

fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    let index = ((sorted.len() - 1) as f64 * quantile).round() as usize;
    sorted[index]
}

fn required(values: &mut impl Iterator<Item = String>, flag: &str) -> io::Result<String> {
    values
        .next()
        .ok_or_else(|| invalid(&format!("{flag} requires a value")))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
