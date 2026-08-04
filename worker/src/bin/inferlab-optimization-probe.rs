use std::{collections::BTreeMap, env, fs, io, path::PathBuf};

use cpu_worker::{
    DecoderMode, DecodingConfig, Generation, Model, PagedCacheConfig, QuantizationMode,
    QuantizationStats, SamplingConfig, speculative_sample_logits,
};
use serde::Serialize;

#[derive(Serialize)]
struct ProbeOutput {
    implementation: &'static str,
    benchmark_repetitions: usize,
    sampling_samples: usize,
    quantization: Vec<QuantizationObservation>,
    greedy_speculation: GreedySpeculationObservation,
    sampled_speculation: Vec<SampledSpeculationObservation>,
    draft_quality: Vec<DraftQualityObservation>,
}

#[derive(Serialize)]
struct QuantizationObservation {
    mode: QuantizationMode,
    memory: QuantizationStats,
    prompts: usize,
    steps: usize,
    greedy_token_mismatches: usize,
    maximum_absolute_logit_error: f64,
    greedy_path_perplexity: f64,
    median_generation_us: f64,
    p95_generation_us: f64,
    tokens_per_second_at_median: f64,
}

#[derive(Serialize)]
struct GreedySpeculationObservation {
    baseline_median_us: f64,
    baseline_p95_us: f64,
    baseline_target_forward_calls: u64,
    profiles: Vec<GreedySpeculationProfile>,
}

#[derive(Serialize)]
struct GreedySpeculationProfile {
    draft_quantization: QuantizationMode,
    draft_tokens_per_cycle: u32,
    output_matches_target: bool,
    median_generation_us: f64,
    p95_generation_us: f64,
    target_forward_calls: u64,
    draft_forward_calls: u64,
    proposed_tokens: u64,
    accepted_tokens: u64,
    acceptance_rate_percent: f64,
    target_call_reduction_percent: f64,
    wall_time_speedup: f64,
}

#[derive(Serialize)]
struct SampledSpeculationObservation {
    draft_quantization: QuantizationMode,
    temperature: f32,
    samples: usize,
    expected_probability: Vec<f64>,
    target_counts: Vec<u64>,
    speculative_counts: Vec<u64>,
    target_probability: Vec<f64>,
    speculative_probability: Vec<f64>,
    target_maximum_probability_error: f64,
    speculative_maximum_probability_error: f64,
    target_vs_speculative_maximum_error: f64,
    proposed_tokens: u64,
    accepted_tokens: u64,
    rejected_tokens: u64,
    acceptance_rate_percent: f64,
    replay_checks: usize,
    replay_matches: usize,
}

#[derive(Serialize)]
struct DraftQualityObservation {
    name: &'static str,
    target_logits: Vec<f32>,
    draft_logits: Vec<f32>,
    samples: usize,
    output_probability: Vec<f64>,
    maximum_target_probability_error: f64,
    accepted: usize,
    rejected: usize,
    acceptance_rate_percent: f64,
}

fn main() -> io::Result<()> {
    let arguments = Arguments::parse(env::args().skip(1))?;
    let quantization = quantization_observations(&arguments)?;
    let greedy_speculation = greedy_speculation_observation(&arguments)?;
    let sampled_speculation = [QuantizationMode::Int8, QuantizationMode::Int4]
        .into_iter()
        .map(|mode| sampled_speculation_observation(&arguments, mode))
        .collect::<io::Result<Vec<_>>>()?;
    let draft_quality = draft_quality_observations(arguments.samples)?;
    let output = ProbeOutput {
        implementation: "inferlab-optimization-v1",
        benchmark_repetitions: arguments.repetitions,
        sampling_samples: arguments.samples,
        quantization,
        greedy_speculation,
        sampled_speculation,
        draft_quality,
    };
    fs::write(
        arguments.output,
        serde_json::to_string_pretty(&output).map_err(io::Error::other)? + "\n",
    )
}

fn draft_quality_observations(samples: usize) -> io::Result<Vec<DraftQualityObservation>> {
    let target_logits = vec![0.0_f32, 1.0, 2.0];
    let expected = softmax(&target_logits, 1.0);
    [
        ("identical", vec![0.0_f32, 1.0, 2.0]),
        ("softened", vec![0.0_f32, 0.5, 1.0]),
        ("reversed", vec![2.0_f32, 1.0, 0.0]),
    ]
    .into_iter()
    .map(|(name, draft_logits)| {
        let mut counts = vec![0_u64; target_logits.len()];
        let mut accepted = 0;
        for seed in 0..samples as u64 {
            let mut target_state = seed;
            let mut draft_state = seed ^ 0xD1B54A32D192ED03;
            let result = speculative_sample_logits(
                &target_logits,
                &draft_logits,
                &[],
                &SamplingConfig {
                    temperature: 1.0,
                    ..SamplingConfig::default()
                },
                &mut target_state,
                &mut draft_state,
            )
            .map_err(io::Error::other)?;
            counts[result.output_token_id as usize] += 1;
            accepted += usize::from(result.proposal_accepted);
        }
        let output_probability = probabilities(&counts, samples);
        Ok(DraftQualityObservation {
            name,
            target_logits: target_logits.clone(),
            draft_logits,
            samples,
            maximum_target_probability_error: maximum_error(&output_probability, &expected),
            output_probability,
            accepted,
            rejected: samples - accepted,
            acceptance_rate_percent: accepted as f64 / samples as f64 * 100.0,
        })
    })
    .collect()
}

fn quantization_observations(arguments: &Arguments) -> io::Result<Vec<QuantizationObservation>> {
    let prompts = [
        "teach me streaming",
        "why does inference matter",
        "hello systems",
    ];
    let fp32 = Model::load_with_quantization(&arguments.model, QuantizationMode::Fp32)
        .map_err(io::Error::other)?;
    let references = prompts
        .iter()
        .map(|prompt| {
            fp32.generate_with_mode(prompt, 8, DecoderMode::KvCache)
                .map_err(io::Error::other)
        })
        .collect::<io::Result<Vec<_>>>()?;
    [
        QuantizationMode::Fp32,
        QuantizationMode::Int8,
        QuantizationMode::Int4,
    ]
    .into_iter()
    .map(|mode| {
        let model =
            Model::load_with_quantization(&arguments.model, mode).map_err(io::Error::other)?;
        let generations = prompts
            .iter()
            .map(|prompt| {
                model
                    .generate_with_mode(prompt, 8, DecoderMode::KvCache)
                    .map_err(io::Error::other)
            })
            .collect::<io::Result<Vec<_>>>()?;
        let mut maximum_error = 0.0_f64;
        let mut mismatches = 0;
        let mut steps = 0;
        let mut negative_log_likelihood = 0.0;
        for (reference, generation) in references.iter().zip(&generations) {
            for (reference_step, step) in reference.steps.iter().zip(&generation.steps) {
                if reference_step.token_id != step.token_id {
                    mismatches += 1;
                }
                for (left, right) in reference_step.logits.iter().zip(&step.logits) {
                    maximum_error = maximum_error.max(f64::from((left - right).abs()));
                }
                negative_log_likelihood -=
                    selected_log_probability(&step.logits, reference_step.token_id as usize);
                steps += 1;
            }
        }
        let mut durations = Vec::with_capacity(arguments.repetitions * prompts.len());
        for _ in 0..arguments.repetitions {
            for prompt in prompts {
                durations.push(
                    model
                        .generate_with_mode(prompt, 8, DecoderMode::KvCache)
                        .map_err(io::Error::other)?
                        .generation_us,
                );
            }
        }
        durations.sort_by(f64::total_cmp);
        let median = percentile(&durations, 0.50);
        Ok(QuantizationObservation {
            mode,
            memory: model.info().quantization,
            prompts: prompts.len(),
            steps,
            greedy_token_mismatches: mismatches,
            maximum_absolute_logit_error: maximum_error,
            greedy_path_perplexity: (negative_log_likelihood / steps as f64).exp(),
            median_generation_us: median,
            p95_generation_us: percentile(&durations, 0.95),
            tokens_per_second_at_median: 7.0 / (median / 1_000_000.0),
        })
    })
    .collect()
}

fn greedy_speculation_observation(
    arguments: &Arguments,
) -> io::Result<GreedySpeculationObservation> {
    let mut target = Model::load_with_quantization(&arguments.model, QuantizationMode::Fp32)
        .map_err(io::Error::other)?;
    target
        .configure_paged_cache(PagedCacheConfig {
            prefix_capacity: 0,
            ..PagedCacheConfig::default()
        })
        .map_err(io::Error::other)?;
    let prompt = "teach me streaming";
    let baseline = target
        .generate_with_mode(prompt, 8, DecoderMode::PagedKvCache)
        .map_err(io::Error::other)?;
    let mut baseline_durations = Vec::with_capacity(arguments.repetitions);
    for _ in 0..arguments.repetitions {
        baseline_durations.push(
            target
                .generate_with_mode(prompt, 8, DecoderMode::PagedKvCache)
                .map_err(io::Error::other)?
                .generation_us,
        );
    }
    baseline_durations.sort_by(f64::total_cmp);
    let baseline_median = percentile(&baseline_durations, 0.50);
    let mut profiles = Vec::new();
    for draft_mode in [QuantizationMode::Int8, QuantizationMode::Int4] {
        let draft = Model::load_with_quantization(&arguments.model, draft_mode)
            .map_err(io::Error::other)?;
        for draft_tokens in [1_u32, 2, 3] {
            let first =
                speculative_generation(&target, draft.clone(), prompt, 8, draft_tokens, 0, 0.0)?;
            let mut durations = Vec::with_capacity(arguments.repetitions);
            for _ in 0..arguments.repetitions {
                durations.push(
                    speculative_generation(
                        &target,
                        draft.clone(),
                        prompt,
                        8,
                        draft_tokens,
                        0,
                        0.0,
                    )?
                    .generation_us,
                );
            }
            durations.sort_by(f64::total_cmp);
            let metrics = &first.metrics.speculation;
            let median = percentile(&durations, 0.50);
            profiles.push(GreedySpeculationProfile {
                draft_quantization: draft_mode,
                draft_tokens_per_cycle: draft_tokens,
                output_matches_target: token_ids(&first) == token_ids(&baseline),
                median_generation_us: median,
                p95_generation_us: percentile(&durations, 0.95),
                target_forward_calls: metrics.target_forward_calls,
                draft_forward_calls: metrics.draft_forward_calls,
                proposed_tokens: metrics.proposed_tokens,
                accepted_tokens: metrics.accepted_tokens,
                acceptance_rate_percent: metrics.acceptance_rate_percent,
                target_call_reduction_percent: percent_reduction(
                    baseline.metrics.speculation.target_forward_calls,
                    metrics.target_forward_calls,
                ),
                wall_time_speedup: baseline_median / median,
            });
        }
    }
    Ok(GreedySpeculationObservation {
        baseline_median_us: baseline_median,
        baseline_p95_us: percentile(&baseline_durations, 0.95),
        baseline_target_forward_calls: baseline.metrics.speculation.target_forward_calls,
        profiles,
    })
}

fn sampled_speculation_observation(
    arguments: &Arguments,
    draft_mode: QuantizationMode,
) -> io::Result<SampledSpeculationObservation> {
    let target = Model::load_with_quantization(&arguments.model, QuantizationMode::Fp32)
        .map_err(io::Error::other)?;
    let draft =
        Model::load_with_quantization(&arguments.model, draft_mode).map_err(io::Error::other)?;
    let temperature = 2.0;
    let prompt = "hello";
    let first = target_generation(&target, prompt, 2, 0, temperature)?;
    let expected = softmax(&first.steps[0].logits, temperature);
    let vocabulary = expected.len();
    let mut target_counts = vec![0_u64; vocabulary];
    let mut speculative_counts = vec![0_u64; vocabulary];
    let mut proposed = 0;
    let mut accepted = 0;
    let mut rejected = 0;
    let replay_seeds = [0_u64, 1, 42, 9_999];
    let mut retained = BTreeMap::<u64, Vec<u32>>::new();
    for seed in 0..arguments.samples as u64 {
        let target_output = target_generation(&target, prompt, 2, seed, temperature)?;
        target_counts[target_output.steps[0].token_id as usize] += 1;
        let speculative =
            speculative_generation(&target, draft.clone(), prompt, 2, 1, seed, temperature)?;
        speculative_counts[speculative.steps[0].token_id as usize] += 1;
        proposed += speculative.metrics.speculation.proposed_tokens;
        accepted += speculative.metrics.speculation.accepted_tokens;
        rejected += speculative.metrics.speculation.rejected_tokens;
        if replay_seeds.contains(&seed) {
            retained.insert(seed, token_ids(&speculative));
        }
    }
    let mut replay_matches = 0;
    for seed in replay_seeds {
        if seed < arguments.samples as u64
            && retained.get(&seed)
                == Some(&token_ids(&speculative_generation(
                    &target,
                    draft.clone(),
                    prompt,
                    2,
                    1,
                    seed,
                    temperature,
                )?))
        {
            replay_matches += 1;
        }
    }
    let target_probability = probabilities(&target_counts, arguments.samples);
    let speculative_probability = probabilities(&speculative_counts, arguments.samples);
    Ok(SampledSpeculationObservation {
        draft_quantization: draft_mode,
        temperature,
        samples: arguments.samples,
        target_maximum_probability_error: maximum_error(&target_probability, &expected),
        speculative_maximum_probability_error: maximum_error(&speculative_probability, &expected),
        target_vs_speculative_maximum_error: maximum_error(
            &target_probability,
            &speculative_probability,
        ),
        expected_probability: expected,
        target_counts,
        speculative_counts,
        target_probability,
        speculative_probability,
        proposed_tokens: proposed,
        accepted_tokens: accepted,
        rejected_tokens: rejected,
        acceptance_rate_percent: if proposed == 0 {
            0.0
        } else {
            accepted as f64 / proposed as f64 * 100.0
        },
        replay_checks: replay_seeds
            .into_iter()
            .filter(|seed| *seed < arguments.samples as u64)
            .count(),
        replay_matches,
    })
}

fn target_generation(
    target: &Model,
    prompt: &str,
    max_tokens: u32,
    seed: u64,
    temperature: f32,
) -> io::Result<Generation> {
    target
        .generate_with_decoding(
            prompt,
            max_tokens,
            DecoderMode::Recompute,
            DecodingConfig {
                sampling: SamplingConfig {
                    temperature,
                    seed,
                    ..SamplingConfig::default()
                },
                ..DecodingConfig::default()
            },
        )
        .map_err(io::Error::other)
}

fn speculative_generation(
    target: &Model,
    draft: Model,
    prompt: &str,
    max_tokens: u32,
    draft_tokens: u32,
    seed: u64,
    temperature: f32,
) -> io::Result<Generation> {
    target
        .generate_with_speculation(
            prompt,
            max_tokens,
            DecoderMode::PagedKvCache,
            DecodingConfig {
                sampling: SamplingConfig {
                    temperature,
                    seed,
                    ..SamplingConfig::default()
                },
                ..DecodingConfig::default()
            },
            Some(draft),
            draft_tokens,
        )
        .map_err(io::Error::other)
}

fn token_ids(generation: &Generation) -> Vec<u32> {
    generation.steps.iter().map(|step| step.token_id).collect()
}

fn selected_log_probability(logits: &[f32], selected: usize) -> f64 {
    let maximum = logits
        .iter()
        .copied()
        .map(f64::from)
        .fold(f64::NEG_INFINITY, f64::max);
    let denominator = logits
        .iter()
        .map(|logit| (f64::from(*logit) - maximum).exp())
        .sum::<f64>();
    f64::from(logits[selected]) - maximum - denominator.ln()
}

fn softmax(logits: &[f32], temperature: f32) -> Vec<f64> {
    let scaled = logits
        .iter()
        .map(|logit| f64::from(*logit / temperature))
        .collect::<Vec<_>>();
    let maximum = scaled.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let mut probabilities = scaled
        .iter()
        .map(|logit| (logit - maximum).exp())
        .collect::<Vec<_>>();
    let denominator = probabilities.iter().sum::<f64>();
    for probability in &mut probabilities {
        *probability /= denominator;
    }
    probabilities
}

fn probabilities(counts: &[u64], samples: usize) -> Vec<f64> {
    counts
        .iter()
        .map(|count| *count as f64 / samples as f64)
        .collect()
}

fn maximum_error(left: &[f64], right: &[f64]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(left, right)| (left - right).abs())
        .fold(0.0, f64::max)
}

fn percent_reduction(baseline: u64, current: u64) -> f64 {
    if baseline == 0 {
        0.0
    } else {
        (baseline - current) as f64 / baseline as f64 * 100.0
    }
}

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    let index = ((sorted.len() - 1) as f64 * percentile).ceil() as usize;
    sorted[index]
}

struct Arguments {
    model: PathBuf,
    repetitions: usize,
    samples: usize,
    output: PathBuf,
}

impl Arguments {
    fn parse(mut arguments: impl Iterator<Item = String>) -> io::Result<Self> {
        let mut model = PathBuf::from("models/tiny-inferlab-v2.bin");
        let mut repetitions = 101;
        let mut samples = 10_000;
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
                "--repetitions" => repetitions = parse(&argument, &value)?,
                "--samples" => samples = parse(&argument, &value)?,
                "--output" => output = Some(PathBuf::from(value)),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown argument {argument}"),
                    ));
                }
            }
        }
        if repetitions == 0 || samples == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "repetitions and samples must be positive",
            ));
        }
        Ok(Self {
            model,
            repetitions,
            samples,
            output: output.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "--output is required")
            })?,
        })
    }
}

fn parse<T>(name: &str, value: &str) -> io::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value.parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} has an invalid value: {error}"),
        )
    })
}
