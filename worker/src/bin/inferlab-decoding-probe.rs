use std::{collections::BTreeMap, env, fs, io, path::PathBuf};

use cpu_worker::{
    DecoderMode, DecodingConfig, GenerationMetrics, LogitSelection, Model, PagedCacheConfig,
    PagedCacheStats, SamplingConfig, inference_summary_response_format, sample_logits,
};
use serde::Serialize;

#[derive(Serialize)]
struct ProbeOutput {
    implementation: &'static str,
    processor_cases: Vec<ProcessorCase>,
    temperature_distributions: Vec<DistributionObservation>,
    structured: StructuredObservation,
}

#[derive(Serialize)]
struct ProcessorCase {
    name: &'static str,
    logits: Vec<f32>,
    history: Vec<u32>,
    sampling: SamplingConfig,
    allowed_token_ids: Option<Vec<u32>>,
    selection: LogitSelection,
}

#[derive(Serialize)]
struct DistributionObservation {
    temperature: f32,
    logits: Vec<f32>,
    samples: usize,
    expected_probability: Vec<f64>,
    observed_counts: Vec<u64>,
    observed_probability: Vec<f64>,
    maximum_absolute_probability_error: f64,
    replay_sequence_matches: bool,
}

#[derive(Serialize)]
struct StructuredObservation {
    samples: usize,
    parser_valid: usize,
    schema_valid: usize,
    stop_finished: usize,
    replay_checks: usize,
    replay_matches: usize,
    distinct_outputs: usize,
    combination_counts: BTreeMap<String, u64>,
    answer_counts: BTreeMap<String, u64>,
    confidence_counts: BTreeMap<String, u64>,
    examples: Vec<String>,
    first_metrics: GenerationMetrics,
    final_cache: PagedCacheStats,
}

fn main() -> io::Result<()> {
    let arguments = Arguments::parse(env::args().skip(1))?;
    let output = ProbeOutput {
        implementation: "inferlab-decoding-v1",
        processor_cases: processor_cases()?,
        temperature_distributions: [0.5_f32, 1.0, 2.0]
            .into_iter()
            .map(|temperature| distribution_observation(temperature, arguments.samples))
            .collect::<io::Result<Vec<_>>>()?,
        structured: structured_observation(&arguments.model, arguments.samples)?,
    };
    fs::write(
        arguments.output,
        serde_json::to_string_pretty(&output).map_err(io::Error::other)? + "\n",
    )
}

fn processor_cases() -> io::Result<Vec<ProcessorCase>> {
    let cases = [
        (
            "greedy",
            vec![1.0, 4.0, 3.0, 2.0],
            vec![],
            SamplingConfig::default(),
            None,
        ),
        (
            "token-ban",
            vec![1.0, 4.0, 3.0, 2.0],
            vec![],
            SamplingConfig {
                banned_token_ids: vec![1],
                ..SamplingConfig::default()
            },
            None,
        ),
        (
            "repetition-penalty",
            vec![1.0, 4.0, 3.0, 2.0],
            vec![1],
            SamplingConfig {
                repetition_penalty: 2.0,
                ..SamplingConfig::default()
            },
            None,
        ),
        (
            "top-k",
            vec![1.0, 4.0, 3.0, 2.0],
            vec![],
            SamplingConfig {
                temperature: 1.0,
                top_k: 2,
                seed: 17,
                ..SamplingConfig::default()
            },
            None,
        ),
        (
            "top-p",
            vec![1.0, 4.0, 3.0, 2.0],
            vec![],
            SamplingConfig {
                temperature: 1.0,
                top_p: 0.6,
                seed: 17,
                ..SamplingConfig::default()
            },
            None,
        ),
        (
            "grammar-mask",
            vec![1.0, 4.0, 3.0, 2.0],
            vec![],
            SamplingConfig::default(),
            Some(vec![0, 3]),
        ),
    ];
    cases
        .into_iter()
        .map(|(name, logits, history, sampling, allowed_token_ids)| {
            let mut random_state = sampling.seed;
            let selection = sample_logits(
                &logits,
                &history,
                &sampling,
                allowed_token_ids.as_deref(),
                &mut random_state,
            )
            .map_err(io::Error::other)?;
            Ok(ProcessorCase {
                name,
                logits,
                history,
                sampling,
                allowed_token_ids,
                selection,
            })
        })
        .collect()
}

fn distribution_observation(
    temperature: f32,
    samples: usize,
) -> io::Result<DistributionObservation> {
    let logits = vec![0.0_f32, 1.0, 2.0];
    let sampling = SamplingConfig {
        temperature,
        ..SamplingConfig::default()
    };
    let mut counts = vec![0_u64; logits.len()];
    for seed in 0..samples as u64 {
        let mut random_state = seed;
        let selected = sample_logits(&logits, &[], &sampling, None, &mut random_state)
            .map_err(io::Error::other)?;
        counts[selected.token_id as usize] += 1;
    }
    let expected = softmax(&logits, temperature);
    let observed = counts
        .iter()
        .map(|count| *count as f64 / samples as f64)
        .collect::<Vec<_>>();
    let maximum_error = expected
        .iter()
        .zip(&observed)
        .map(|(left, right)| (left - right).abs())
        .fold(0.0_f64, f64::max);
    let replay = |seed| -> io::Result<Vec<u32>> {
        let mut random_state = seed;
        (0..128)
            .map(|_| {
                sample_logits(&logits, &[], &sampling, None, &mut random_state)
                    .map(|selection| selection.token_id)
                    .map_err(io::Error::other)
            })
            .collect()
    };
    let replay_sequence_matches = replay(9_191)? == replay(9_191)?;
    Ok(DistributionObservation {
        temperature,
        logits,
        samples,
        expected_probability: expected,
        observed_counts: counts,
        observed_probability: observed,
        maximum_absolute_probability_error: maximum_error,
        replay_sequence_matches,
    })
}

fn structured_observation(
    model_path: &PathBuf,
    samples: usize,
) -> io::Result<StructuredObservation> {
    let mut model = Model::load(model_path).map_err(io::Error::other)?;
    model
        .configure_paged_cache(PagedCacheConfig {
            page_tokens: 4,
            page_count: 64,
            prefix_capacity: 4,
        })
        .map_err(io::Error::other)?;
    let mut parser_valid = 0;
    let mut schema_valid = 0;
    let mut stop_finished = 0;
    let mut combination_counts = BTreeMap::<String, u64>::new();
    let mut answer_counts = BTreeMap::<String, u64>::new();
    let mut confidence_counts = BTreeMap::<String, u64>::new();
    let mut examples = Vec::new();
    let mut first_metrics = None;
    let mut retained = BTreeMap::<u64, String>::new();
    let replay_seeds = [0_u64, 1, 42, 9_999];

    for seed in 0..samples as u64 {
        let generation = structured_generation(&model, seed)?;
        if first_metrics.is_none() {
            first_metrics = Some(generation.metrics.clone());
        }
        if replay_seeds.contains(&seed) {
            retained.insert(seed, generation.text.clone());
        }
        if generation.finish_reason == "stop" {
            stop_finished += 1;
        }
        if examples.len() < 12 && !examples.contains(&generation.text) {
            examples.push(generation.text.clone());
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&generation.text) else {
            continue;
        };
        parser_valid += 1;
        let Some(object) = value.as_object() else {
            continue;
        };
        let answer = object.get("answer").and_then(serde_json::Value::as_str);
        let confidence = object.get("confidence").and_then(serde_json::Value::as_str);
        if object.len() != 2
            || !matches!(answer, Some("InferLab" | "systems" | "tokens"))
            || !matches!(confidence, Some("high" | "medium" | "low"))
        {
            continue;
        }
        schema_valid += 1;
        let answer = answer.expect("validated answer").to_owned();
        let confidence = confidence.expect("validated confidence").to_owned();
        *answer_counts.entry(answer.clone()).or_default() += 1;
        *confidence_counts.entry(confidence.clone()).or_default() += 1;
        *combination_counts
            .entry(format!("{answer}/{confidence}"))
            .or_default() += 1;
    }

    let mut replay_matches = 0;
    for seed in replay_seeds {
        if seed < samples as u64
            && retained.get(&seed) == Some(&structured_generation(&model, seed)?.text)
        {
            replay_matches += 1;
        }
    }
    let final_cache = model.paged_cache_stats().map_err(io::Error::other)?;
    Ok(StructuredObservation {
        samples,
        parser_valid,
        schema_valid,
        stop_finished,
        replay_checks: replay_seeds
            .into_iter()
            .filter(|seed| *seed < samples as u64)
            .count(),
        replay_matches,
        distinct_outputs: combination_counts.len(),
        combination_counts,
        answer_counts,
        confidence_counts,
        examples,
        first_metrics: first_metrics.ok_or_else(|| io::Error::other("samples must be positive"))?,
        final_cache,
    })
}

fn structured_generation(model: &Model, seed: u64) -> io::Result<cpu_worker::Generation> {
    model
        .generate_with_decoding(
            "teach me streaming",
            6,
            DecoderMode::PagedKvCache,
            DecodingConfig {
                sampling: SamplingConfig {
                    temperature: 1.0,
                    seed,
                    ..SamplingConfig::default()
                },
                response_format: inference_summary_response_format(),
            },
        )
        .map_err(io::Error::other)
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

struct Arguments {
    model: PathBuf,
    samples: usize,
    output: PathBuf,
}

impl Arguments {
    fn parse(mut arguments: impl Iterator<Item = String>) -> io::Result<Self> {
        let mut model = PathBuf::from("models/tiny-inferlab-v2.bin");
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
        if samples == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--samples must be positive",
            ));
        }
        Ok(Self {
            model,
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
