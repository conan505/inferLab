use std::{env, io, time::Duration};

use cpu_worker::{Model, WorkerConfig, app};
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
        .unwrap_or_else(|_| "models/tiny-inferlab-v1.bin".to_owned());
    let token_delay_ms = parse_env("INFERLAB_CPU_TOKEN_DELAY_MS", 0_u64)?;
    let model = Model::load(&model_path).map_err(io::Error::other)?;
    let model_info = model.info().clone();
    let app = app(
        model,
        WorkerConfig {
            id: worker_id.clone(),
            token_delay: Duration::from_millis(token_delay_ms),
        },
    );
    let listener = TcpListener::bind(&bind).await?;
    info!(
        %bind,
        %worker_id,
        %model_path,
        token_delay_ms,
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
