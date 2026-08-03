use std::{env, io, time::Duration};

use cpu_worker::{DecoderMode, Model, PagedCacheConfig, WorkerConfig, try_app};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> io::Result<()> {
    init_tracing();
    let bind = env::var("INFERLAB_CPU_BIND").unwrap_or_else(|_| "127.0.0.1:9101".to_owned());
    let worker_id =
        env::var("INFERLAB_CPU_WORKER_ID").unwrap_or_else(|_| "cpu-worker-a".to_owned());
    let model_path = env::var("INFERLAB_MODEL_PATH")
        .unwrap_or_else(|_| "models/tiny-inferlab-v2.bin".to_owned());
    let batch_tick_ms = match env::var("INFERLAB_CPU_BATCH_TICK_MS") {
        Ok(value) => value.parse::<u64>().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("INFERLAB_CPU_BATCH_TICK_MS has an invalid value: {error}"),
            )
        })?,
        Err(_) => parse_env("INFERLAB_CPU_TOKEN_DELAY_MS", 0_u64)?,
    };
    let decoder_mode = env::var("INFERLAB_CPU_DECODER_MODE")
        .unwrap_or_else(|_| "paged-kv-cache".to_owned())
        .parse::<DecoderMode>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let max_batch_size = parse_env("INFERLAB_CPU_MAX_BATCH_SIZE", 4_usize)?;
    let scheduler_queue_capacity = parse_env("INFERLAB_CPU_SCHEDULER_QUEUE_CAPACITY", 64_usize)?;
    let page_tokens = parse_env("INFERLAB_CPU_KV_PAGE_TOKENS", 4_u32)?;
    let page_count = parse_env("INFERLAB_CPU_KV_PAGE_COUNT", 64_u32)?;
    let prefix_capacity = parse_env("INFERLAB_CPU_PREFIX_CACHE_CAPACITY", 32_u32)?;
    let model = Model::load(&model_path).map_err(io::Error::other)?;
    let model_info = model.info().clone();
    let app = try_app(
        model,
        WorkerConfig {
            id: worker_id.clone(),
            batch_tick_delay: Duration::from_millis(batch_tick_ms),
            decoder_mode,
            max_batch_size,
            scheduler_queue_capacity,
            paged_cache: PagedCacheConfig {
                page_tokens,
                page_count,
                prefix_capacity,
            },
        },
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let listener = TcpListener::bind(&bind).await?;
    info!(
        %bind,
        %worker_id,
        %model_path,
        batch_tick_ms,
        ?decoder_mode,
        max_batch_size,
        scheduler_queue_capacity,
        page_tokens,
        page_count,
        prefix_capacity,
        vocabulary = model_info.vocabulary,
        context_length = model_info.context_length,
        dimension = model_info.dimension,
        heads = model_info.heads,
        "CPU worker listening"
    );
    axum::serve(listener, app).await
}

fn parse_env<T>(name: &str, default: T) -> io::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match env::var(name) {
        Ok(value) => value.parse::<T>().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} has an invalid value: {error}"),
            )
        }),
        Err(_) => Ok(default),
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
