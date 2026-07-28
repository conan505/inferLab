use std::{env, io, time::Duration};

use fake_worker::{Config, app};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> io::Result<()> {
    init_tracing();

    let bind = env::var("FAKE_WORKER_BIND").unwrap_or_else(|_| "127.0.0.1:9001".to_owned());
    let config = Config {
        id: env::var("FAKE_WORKER_ID").unwrap_or_else(|_| "worker-a".to_owned()),
        initial_delay: duration_from_env("FAKE_WORKER_INITIAL_DELAY_MS", 25)?,
        token_delay: duration_from_env("FAKE_WORKER_TOKEN_DELAY_MS", 40)?,
        fail_every: optional_positive_u64("FAKE_WORKER_FAIL_EVERY")?,
    };

    let listener = TcpListener::bind(&bind).await?;
    info!(worker_id = %config.id, %bind, "fake worker listening");
    axum::serve(listener, app(config)).await
}

fn duration_from_env(name: &str, default_ms: u64) -> io::Result<Duration> {
    let milliseconds = match env::var(name) {
        Ok(value) => value.parse::<u64>().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} must be an unsigned integer: {error}"),
            )
        })?,
        Err(_) => default_ms,
    };
    Ok(Duration::from_millis(milliseconds))
}

fn optional_positive_u64(name: &str) -> io::Result<Option<u64>> {
    let Ok(value) = env::var(name) else {
        return Ok(None);
    };
    let parsed = value.parse::<u64>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be an unsigned integer: {error}"),
        )
    })?;
    Ok((parsed > 0).then_some(parsed))
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
