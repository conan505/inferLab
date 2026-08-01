use std::{env, fs, io, path::PathBuf};

use cpu_worker::{DecoderMode, Generation, Model, PagedCacheConfig, PagedCacheStats};
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
    let mut model = Model::load(&arguments.model).map_err(io::Error::other)?;
    model
        .configure_paged_cache(arguments.paged_cache)
        .map_err(io::Error::other)?;
    let mut generations = Vec::with_capacity(arguments.repetitions);
    for _ in 0..arguments.repetitions {
        generations.push(
            model
                .generate_with_mode(&arguments.prompt, arguments.max_tokens, arguments.mode)
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
}

impl Arguments {
    fn parse(mut arguments: impl Iterator<Item = String>) -> io::Result<Self> {
        let mut parsed = Self {
            model: PathBuf::from("models/tiny-inferlab-v1.bin"),
            prompt: "teach me streaming".to_owned(),
            max_tokens: 8,
            repetitions: 1,
            output: None,
            mode: DecoderMode::PagedKvCache,
            paged_cache: PagedCacheConfig::default(),
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
