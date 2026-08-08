use std::{env, io};

use std::sync::Arc;

use batch_queue::{QueueMetrics, QueueStore, app};
use observability::{
    HttpMetrics, MetricsRegistry, MetricsServerConfig, Service, init_tracing, serve_metrics,
};
use tokio::net::TcpListener;
use tracing::info;

#[tokio::main]
async fn main() -> io::Result<()> {
    init_tracing(Service::BatchQueue)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let metrics_config = MetricsServerConfig::from_env()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;

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
    match metrics_config {
        None => axum::serve(listener, app(store)).await,
        Some(metrics_config) => {
            let mut registry = MetricsRegistry::new();
            let http = HttpMetrics::register(&mut registry, Service::BatchQueue)
                .map_err(io::Error::other)?;
            QueueMetrics::register(&mut registry, Arc::clone(&store)).map_err(io::Error::other)?;
            let registry = Arc::new(registry);
            let application = http.instrument(app(store));
            let ((), ()) = tokio::try_join!(
                async { axum::serve(listener, application).await },
                serve_metrics(metrics_config, registry),
            )?;
            Ok(())
        }
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
