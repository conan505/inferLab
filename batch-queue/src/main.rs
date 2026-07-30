use std::{env, io};

use batch_queue::{QueueStore, app};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let bind = env::var("INFERLAB_BATCH_BIND").unwrap_or_else(|_| "127.0.0.1:8081".to_owned());
    let wal_path =
        env::var("INFERLAB_BATCH_WAL").unwrap_or_else(|_| "./data/inferlab-batch.wal".to_owned());
    let store = QueueStore::open(&wal_path).map_err(io::Error::other)?;
    let snapshot = store.snapshot(now_ms()).map_err(io::Error::other)?;
    let listener = TcpListener::bind(&bind).await?;
    info!(
        bind,
        wal_path,
        jobs = snapshot.jobs_total,
        wal_events = snapshot.wal_events,
        "InferLab durable batch queue listening"
    );
    axum::serve(listener, app(store)).await
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
