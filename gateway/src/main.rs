use std::{env, io, sync::Arc, time::Duration};

use gateway::{
    admission::AdmissionConfig,
    app_with_config,
    resilience::{ResilienceConfig, default_jitter_seed},
    routing::{RoutingConfig, RoutingPolicy, WorkerPool, WorkerRegistration},
};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> io::Result<()> {
    init_tracing();

    let bind = env::var("INFERLAB_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
    let workers = env::var("INFERLAB_WORKERS").unwrap_or_else(|_| {
        [
            "worker-a=http://127.0.0.1:9001",
            "worker-b=http://127.0.0.1:9002",
            "worker-c=http://127.0.0.1:9003",
        ]
        .join(",")
    });
    let policy = env::var("INFERLAB_ROUTING_POLICY")
        .unwrap_or_else(|_| "round-robin".to_owned())
        .parse::<RoutingPolicy>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let ewma_alpha = parse_env("INFERLAB_EWMA_ALPHA", 0.25_f64)?;
    let ewma_probe_interval = parse_env("INFERLAB_EWMA_PROBE_INTERVAL", 10_usize)?;
    let consistent_hash_virtual_nodes = parse_env("INFERLAB_CONSISTENT_HASH_VNODES", 128_usize)?;
    let worker_concurrency_limit = parse_env("INFERLAB_WORKER_CONCURRENCY", 8_usize)?;
    let admission_queue_capacity = parse_env("INFERLAB_ADMISSION_QUEUE_CAPACITY", 64_usize)?;
    let request_deadline_ms = parse_env("INFERLAB_REQUEST_DEADLINE_MS", 30_000_u64)?;
    let attempt_timeout_ms = parse_env("INFERLAB_ATTEMPT_TIMEOUT_MS", 5_000_u64)?;
    let max_retries = parse_env("INFERLAB_MAX_RETRIES", 2_usize)?;
    let retry_budget_percent = parse_env("INFERLAB_RETRY_BUDGET_PERCENT", 10_u64)?;
    let retry_base_delay_ms = parse_env("INFERLAB_RETRY_BASE_DELAY_MS", 25_u64)?;
    let retry_max_delay_ms = parse_env("INFERLAB_RETRY_MAX_DELAY_MS", 500_u64)?;
    let jitter_seed = parse_env("INFERLAB_JITTER_SEED", default_jitter_seed())?;
    let pool = Arc::new(
        WorkerPool::from_config(
            parse_workers(&workers)?,
            RoutingConfig {
                policy,
                ewma_alpha,
                ewma_probe_interval,
                consistent_hash_virtual_nodes,
                worker_concurrency_limit,
            },
        )
        .map_err(io::Error::other)?,
    );
    let worker_count = pool.snapshots().len();
    let app = app_with_config(
        pool,
        AdmissionConfig {
            queue_capacity: admission_queue_capacity,
        },
        ResilienceConfig {
            request_deadline: Duration::from_millis(request_deadline_ms),
            attempt_timeout: Duration::from_millis(attempt_timeout_ms),
            max_retries,
            retry_budget_percent,
            retry_base_delay: Duration::from_millis(retry_base_delay_ms),
            retry_max_delay: Duration::from_millis(retry_max_delay_ms),
            jitter_seed,
        },
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let listener = TcpListener::bind(&bind).await?;
    info!(
        %bind,
        workers = worker_count,
        %policy,
        ewma_alpha,
        ewma_probe_interval,
        consistent_hash_virtual_nodes,
        worker_concurrency_limit,
        admission_queue_capacity,
        request_deadline_ms,
        attempt_timeout_ms,
        max_retries,
        retry_budget_percent,
        retry_base_delay_ms,
        retry_max_delay_ms,
        "gateway listening"
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

fn parse_workers(raw: &str) -> io::Result<Vec<WorkerRegistration>> {
    raw.split(',')
        .map(|entry| {
            let (identity, url) = entry.split_once('=').ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid worker '{entry}'; expected id[:weight]=url"),
                )
            })?;
            let (id, weight) = match identity.rsplit_once(':') {
                Some((id, raw_weight)) => {
                    let weight = raw_weight.parse::<u32>().map_err(|error| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("invalid weight in worker '{entry}': {error}"),
                        )
                    })?;
                    if weight == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("worker '{entry}' must have a positive weight"),
                        ));
                    }
                    (id, weight)
                }
                None => (identity, 1),
            };
            Ok(WorkerRegistration::new(id.trim(), url.trim(), weight))
        })
        .collect()
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::parse_workers;

    #[test]
    fn parses_worker_configuration() {
        let workers = parse_workers("a:3=http://a:1,b=http://b:2").expect("valid workers");
        assert_eq!(
            workers[0],
            gateway::routing::WorkerRegistration::new("a", "http://a:1", 3)
        );
        assert_eq!(
            workers[1],
            gateway::routing::WorkerRegistration::new("b", "http://b:2", 1)
        );
    }

    #[test]
    fn rejects_worker_without_separator() {
        assert!(parse_workers("http://a:1").is_err());
    }

    #[test]
    fn rejects_zero_or_invalid_weights() {
        assert!(parse_workers("a:0=http://a").is_err());
        assert!(parse_workers("a:heavy=http://a").is_err());
    }
}
