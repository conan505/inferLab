use std::{env, fs, io, path::PathBuf};

use cpu_worker::{
    AttentionAlgorithm, AttentionConfig, AttentionPrecision, DecoderMode, DecodingConfig,
    Generation, Model, PagedCacheConfig, PagedCacheStats, QuantizationMode, ResponseFormat,
    inference_summary_response_format,
};
use serde::Serialize;

#[derive(Serialize)]
struct CliOutput {
    implementation: &'static str,
    repetitions: usize,
    median_generation_us: f64,
    p95_generation_us: f64,
    paged_cache: PagedCacheStats,
    generation: Generation,
}

fn main() -> io::Result<()> {
    let arguments = Arguments::parse(env::args().skip(1))?;
    let attention = AttentionConfig {
        algorithm: arguments.attention_algorithm,
        precision: arguments.attention_precision,
        tile_tokens: arguments.attention_tile_tokens,
        causal: true,
    };
    let mut model = Model::load_with_options(&arguments.model, arguments.quantization, attention)
        .map_err(io::Error::other)?;
    model
        .configure_paged_cache(arguments.paged_cache)
        .map_err(io::Error::other)?;
    let draft_model = if arguments.speculative_tokens > 0 {
        Some(
            Model::load_with_options(&arguments.model, arguments.draft_quantization, attention)
                .map_err(io::Error::other)?,
        )
    } else {
        None
    };
    let mut generations = Vec::with_capacity(arguments.repetitions);
    for _ in 0..arguments.repetitions {
        generations.push(
            model
                .generate_with_speculation(
                    &arguments.prompt,
                    arguments.max_tokens,
                    arguments.mode,
                    arguments.decoding.clone(),
                    draft_model.clone(),
                    arguments.speculative_tokens,
                )
                .map_err(io::Error::other)?,
        );
    }
    let mut durations = generations
        .iter()
        .map(|generation| generation.generation_us)
        .collect::<Vec<_>>();
    durations.sort_by(f64::total_cmp);
    let median_generation_us = percentile(&durations, 0.50);
    let p95_generation_us = percentile(&durations, 0.95);
    let generation = generations
        .into_iter()
        .next()
        .expect("repetitions is positive");
    let paged_cache = model.paged_cache_stats().map_err(io::Error::other)?;
    let output = serde_json::to_string_pretty(&CliOutput {
        implementation: "inferlab-cpp",
        repetitions: arguments.repetitions,
        median_generation_us,
        p95_generation_us,
        paged_cache,
        generation,
    })
    .map_err(io::Error::other)?;
    if let Some(path) = arguments.output {
        fs::write(path, format!("{output}\n"))?;
    } else {
        println!("{output}");
    }
    Ok(())
}

struct Arguments {
    model: PathBuf,
    prompt: String,
    max_tokens: u32,
    repetitions: usize,
    output: Option<PathBuf>,
    mode: DecoderMode,
    paged_cache: PagedCacheConfig,
    decoding: DecodingConfig,
    quantization: QuantizationMode,
    draft_quantization: QuantizationMode,
    speculative_tokens: u32,
    attention_algorithm: AttentionAlgorithm,
    attention_precision: AttentionPrecision,
    attention_tile_tokens: u32,
}

impl Arguments {
    fn parse(mut arguments: impl Iterator<Item = String>) -> io::Result<Self> {
        let mut parsed = Self {
            model: PathBuf::from("models/tiny-inferlab-v2.bin"),
            prompt: "teach me streaming".to_owned(),
            max_tokens: 8,
            repetitions: 1,
            output: None,
            mode: DecoderMode::PagedKvCache,
            paged_cache: PagedCacheConfig::default(),
            decoding: DecodingConfig::default(),
            quantization: QuantizationMode::Fp32,
            draft_quantization: QuantizationMode::Int8,
            speculative_tokens: 0,
            attention_algorithm: AttentionAlgorithm::Materialized,
            attention_precision: AttentionPrecision::Fp32,
            attention_tile_tokens: 16,
        };
        while let Some(argument) = arguments.next() {
            let value = arguments.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("missing value after {argument}"),
                )
            })?;
            match argument.as_str() {
                "--model" => parsed.model = PathBuf::from(value),
                "--quantization" => parsed.quantization = parse(&argument, &value)?,
                "--draft-quantization" => parsed.draft_quantization = parse(&argument, &value)?,
                "--speculative-tokens" => parsed.speculative_tokens = parse(&argument, &value)?,
                "--attention-kernel" => parsed.attention_algorithm = parse(&argument, &value)?,
                "--attention-precision" => parsed.attention_precision = parse(&argument, &value)?,
                "--attention-tile-tokens" => {
                    parsed.attention_tile_tokens = parse(&argument, &value)?
                }
                "--prompt" => parsed.prompt = value,
                "--max-tokens" => parsed.max_tokens = parse(&argument, &value)?,
                "--repetitions" => parsed.repetitions = parse(&argument, &value)?,
                "--output" => parsed.output = Some(PathBuf::from(value)),
                "--mode" => parsed.mode = parse(&argument, &value)?,
                "--page-tokens" => parsed.paged_cache.page_tokens = parse(&argument, &value)?,
                "--page-count" => parsed.paged_cache.page_count = parse(&argument, &value)?,
                "--prefix-capacity" => {
                    parsed.paged_cache.prefix_capacity = parse(&argument, &value)?
                }
                "--temperature" => parsed.decoding.sampling.temperature = parse(&argument, &value)?,
                "--top-k" => parsed.decoding.sampling.top_k = parse(&argument, &value)?,
                "--top-p" => parsed.decoding.sampling.top_p = parse(&argument, &value)?,
                "--repetition-penalty" => {
                    parsed.decoding.sampling.repetition_penalty = parse(&argument, &value)?
                }
                "--seed" => parsed.decoding.sampling.seed = parse(&argument, &value)?,
                "--ban-token" => parsed
                    .decoding
                    .sampling
                    .banned_token_ids
                    .push(parse(&argument, &value)?),
                "--response-format" => {
                    parsed.decoding.response_format = match value.as_str() {
                        "text" => ResponseFormat::Text,
                        "json-schema" => inference_summary_response_format(),
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "--response-format must be text or json-schema",
                            ));
                        }
                    }
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown argument {argument}"),
                    ));
                }
            }
        }
        if parsed.repetitions == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--repetitions must be positive",
            ));
        }
        Ok(parsed)
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

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    let index = ((sorted.len() - 1) as f64 * percentile).ceil() as usize;
    sorted[index]
}
