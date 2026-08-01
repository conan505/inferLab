use std::{env, fs, io, path::PathBuf};

use cpu_worker::{
    DecoderMode, GenerationMetrics, Model, PagedCacheConfig, PagedCacheStats, Session, StepOutcome,
};
use serde::Serialize;

#[derive(Serialize)]
struct ProbeOutput {
    implementation: &'static str,
    capacity: CapacityObservation,
    fragmentation: Vec<FragmentationObservation>,
    sharing: SharingObservation,
    eviction: EvictionObservation,
}

#[derive(Serialize)]
struct CapacityObservation {
    page_tokens: u32,
    page_count: u32,
    total_token_slots: u64,
    prompt_tokens_per_session: usize,
    paged_concurrent_sessions: usize,
    contiguous_max_context_reservation_sessions: u64,
    capacity_gain: f64,
    exhausted_error: String,
    at_capacity: PagedCacheStats,
    after_half_released: PagedCacheStats,
    after_backfill: PagedCacheStats,
    after_all_released: PagedCacheStats,
}

#[derive(Serialize)]
struct FragmentationObservation {
    page_tokens: u32,
    page_count: u32,
    sequence_token_lengths: Vec<usize>,
    logical_token_slots: u64,
    allocated_token_slots: u64,
    internal_fragmentation_token_slots: u64,
    internal_fragmentation_percent: f64,
    stats: PagedCacheStats,
}

#[derive(Serialize)]
struct SharingObservation {
    prompt: &'static str,
    prompt_tokens: usize,
    cold_metrics: GenerationMetrics,
    after_cold_release: PagedCacheStats,
    two_warm_sessions_before_decode: PagedCacheStats,
    warm_a_metrics_after_fork: GenerationMetrics,
    warm_b_metrics_after_fork: GenerationMetrics,
    after_copy_on_write: PagedCacheStats,
    longer_prompt_metrics: GenerationMetrics,
    after_warm_release: PagedCacheStats,
}

#[derive(Serialize)]
struct EvictionObservation {
    page_count: u32,
    prefix_capacity: u32,
    inserted_prompts: Vec<&'static str>,
    after_three_prompts: PagedCacheStats,
    evicted_prefix_was_a_miss: bool,
    after_reloading_oldest: PagedCacheStats,
}

fn main() -> io::Result<()> {
    let arguments = Arguments::parse(env::args().skip(1))?;
    let output = ProbeOutput {
        implementation: "inferlab-paged-kv-v1",
        capacity: capacity_observation(&arguments.model)?,
        fragmentation: fragmentation_observations(&arguments.model)?,
        sharing: sharing_observation(&arguments.model)?,
        eviction: eviction_observation(&arguments.model)?,
    };
    fs::write(
        arguments.output,
        serde_json::to_string_pretty(&output).map_err(io::Error::other)? + "\n",
    )
}

fn capacity_observation(model_path: &PathBuf) -> io::Result<CapacityObservation> {
    let config = PagedCacheConfig {
        page_tokens: 4,
        page_count: 16,
        prefix_capacity: 0,
    };
    let model = configured_model(model_path, config)?;
    let prompt = prompt_with_tokens(8);
    let mut sessions = Vec::new();
    for _ in 0..8 {
        let mut session = model
            .session_with_mode(&prompt, 1, DecoderMode::PagedKvCache)
            .map_err(io::Error::other)?;
        one_step(&mut session)?;
        sessions.push(session);
    }
    let at_capacity = model.paged_cache_stats().map_err(io::Error::other)?;
    let mut exhausted = model
        .session_with_mode(&prompt, 1, DecoderMode::PagedKvCache)
        .map_err(io::Error::other)?;
    let exhausted_error = match exhausted.next_token() {
        Ok(_) => return Err(io::Error::other("capacity probe unexpectedly succeeded")),
        Err(error) => error,
    };
    drop(exhausted);

    sessions.truncate(4);
    let after_half_released = model.paged_cache_stats().map_err(io::Error::other)?;
    for _ in 0..4 {
        let mut session = model
            .session_with_mode(&prompt, 1, DecoderMode::PagedKvCache)
            .map_err(io::Error::other)?;
        one_step(&mut session)?;
        sessions.push(session);
    }
    let after_backfill = model.paged_cache_stats().map_err(io::Error::other)?;
    drop(sessions);
    let after_all_released = model.paged_cache_stats().map_err(io::Error::other)?;
    let total_token_slots = u64::from(config.page_tokens) * u64::from(config.page_count);
    let contiguous_sessions = total_token_slots / u64::from(model.info().context_length);
    Ok(CapacityObservation {
        page_tokens: config.page_tokens,
        page_count: config.page_count,
        total_token_slots,
        prompt_tokens_per_session: 8,
        paged_concurrent_sessions: 8,
        contiguous_max_context_reservation_sessions: contiguous_sessions,
        capacity_gain: 8.0 / contiguous_sessions as f64,
        exhausted_error,
        at_capacity,
        after_half_released,
        after_backfill,
        after_all_released,
    })
}

fn fragmentation_observations(model_path: &PathBuf) -> io::Result<Vec<FragmentationObservation>> {
    let lengths = vec![2, 3, 5, 8, 9, 13];
    let logical_slots = lengths.iter().sum::<usize>() as u64;
    let mut observations = Vec::new();
    for page_tokens in [1_u32, 2, 4, 8] {
        let config = PagedCacheConfig {
            page_tokens,
            page_count: 64 / page_tokens,
            prefix_capacity: 0,
        };
        let model = configured_model(model_path, config)?;
        let mut sessions = Vec::new();
        for length in &lengths {
            let mut session = model
                .session_with_mode(&prompt_with_tokens(*length), 1, DecoderMode::PagedKvCache)
                .map_err(io::Error::other)?;
            one_step(&mut session)?;
            sessions.push(session);
        }
        let stats = model.paged_cache_stats().map_err(io::Error::other)?;
        let fragmented_slots = stats.allocated_token_slots - stats.used_token_slots;
        let fragmentation_percent = if stats.allocated_token_slots == 0 {
            0.0
        } else {
            fragmented_slots as f64 / stats.allocated_token_slots as f64 * 100.0
        };
        observations.push(FragmentationObservation {
            page_tokens,
            page_count: config.page_count,
            sequence_token_lengths: lengths.clone(),
            logical_token_slots: logical_slots,
            allocated_token_slots: stats.allocated_token_slots,
            internal_fragmentation_token_slots: fragmented_slots,
            internal_fragmentation_percent: fragmentation_percent,
            stats,
        });
        drop(sessions);
    }
    Ok(observations)
}

fn sharing_observation(model_path: &PathBuf) -> io::Result<SharingObservation> {
    let model = configured_model(
        model_path,
        PagedCacheConfig {
            page_tokens: 4,
            page_count: 8,
            prefix_capacity: 4,
        },
    )?;
    let prompt = "hello systems";
    let prompt_tokens = model.tokenize(prompt).map_err(io::Error::other)?.len();
    let mut cold = model
        .session_with_mode(prompt, 1, DecoderMode::PagedKvCache)
        .map_err(io::Error::other)?;
    one_step(&mut cold)?;
    let cold_metrics = cold.metrics();
    drop(cold);
    let after_cold_release = model.paged_cache_stats().map_err(io::Error::other)?;

    let mut warm_a = model
        .session_with_mode(prompt, 2, DecoderMode::PagedKvCache)
        .map_err(io::Error::other)?;
    let mut warm_b = model
        .session_with_mode(prompt, 2, DecoderMode::PagedKvCache)
        .map_err(io::Error::other)?;
    let two_warm_sessions_before_decode = model.paged_cache_stats().map_err(io::Error::other)?;
    one_step(&mut warm_a)?;
    one_step(&mut warm_b)?;
    one_step(&mut warm_a)?;
    one_step(&mut warm_b)?;
    let warm_a_metrics_after_fork = warm_a.metrics();
    let warm_b_metrics_after_fork = warm_b.metrics();
    let after_copy_on_write = model.paged_cache_stats().map_err(io::Error::other)?;
    drop(warm_a);
    drop(warm_b);

    let mut longer = model
        .session_with_mode("hello systems teach", 1, DecoderMode::PagedKvCache)
        .map_err(io::Error::other)?;
    one_step(&mut longer)?;
    let longer_prompt_metrics = longer.metrics();
    drop(longer);
    let after_warm_release = model.paged_cache_stats().map_err(io::Error::other)?;
    Ok(SharingObservation {
        prompt,
        prompt_tokens,
        cold_metrics,
        after_cold_release,
        two_warm_sessions_before_decode,
        warm_a_metrics_after_fork,
        warm_b_metrics_after_fork,
        after_copy_on_write,
        longer_prompt_metrics,
        after_warm_release,
    })
}

fn eviction_observation(model_path: &PathBuf) -> io::Result<EvictionObservation> {
    let config = PagedCacheConfig {
        page_tokens: 4,
        page_count: 2,
        prefix_capacity: 2,
    };
    let model = configured_model(model_path, config)?;
    let prompts = vec!["hello", "systems", "teach"];
    for prompt in &prompts {
        let mut session = model
            .session_with_mode(prompt, 1, DecoderMode::PagedKvCache)
            .map_err(io::Error::other)?;
        one_step(&mut session)?;
    }
    let after_three_prompts = model.paged_cache_stats().map_err(io::Error::other)?;
    let mut oldest = model
        .session_with_mode(prompts[0], 1, DecoderMode::PagedKvCache)
        .map_err(io::Error::other)?;
    one_step(&mut oldest)?;
    let evicted_prefix_was_a_miss = !oldest.metrics().prefix_cache_hit;
    drop(oldest);
    let after_reloading_oldest = model.paged_cache_stats().map_err(io::Error::other)?;
    Ok(EvictionObservation {
        page_count: config.page_count,
        prefix_capacity: config.prefix_capacity,
        inserted_prompts: prompts,
        after_three_prompts,
        evicted_prefix_was_a_miss,
        after_reloading_oldest,
    })
}

fn configured_model(model_path: &PathBuf, config: PagedCacheConfig) -> io::Result<Model> {
    let mut model = Model::load(model_path).map_err(io::Error::other)?;
    model
        .configure_paged_cache(config)
        .map_err(io::Error::other)?;
    Ok(model)
}

fn one_step(session: &mut Session) -> io::Result<u32> {
    match session.next_token().map_err(io::Error::other)? {
        StepOutcome::Token(step) | StepOutcome::EndOfSequence(step) => Ok(step.token_id),
        StepOutcome::Length => Err(io::Error::other("session reached length before a token")),
    }
}

fn prompt_with_tokens(tokens: usize) -> String {
    assert!(tokens >= 1);
    vec!["hello"; tokens - 1].join(" ")
}

struct Arguments {
    model: PathBuf,
    output: PathBuf,
}

impl Arguments {
    fn parse(mut arguments: impl Iterator<Item = String>) -> io::Result<Self> {
        let mut model = PathBuf::from("models/tiny-inferlab-v1.bin");
        let mut output = None;
        while let Some(argument) = arguments.next() {
            let value = arguments.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("missing value after {argument}"),
                )
            })?;
            match argument.as_str() {
                "--model" => model = PathBuf::from(value),
                "--output" => output = Some(PathBuf::from(value)),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown argument {argument}"),
                    ));
                }
            }
        }
        Ok(Self {
            model,
            output: output.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "--output is required")
            })?,
        })
    }
}
