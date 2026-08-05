use std::{
    env, io,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gateway::{
    ControlPlaneStatus, RoutingSnapshot, SharedControlPlaneStatus, SharedRoutingSnapshot,
    admission::AdmissionConfig,
    app_with_dynamic_config,
    circuit_breaker::CircuitBreakerConfig,
    resilience::{ResilienceConfig, default_jitter_seed},
    routing::{RoutingConfig, RoutingPolicy, WorkerPool, WorkerRegistration},
};
use reqwest::Client;
use serde::Deserialize;
use tokio::{
    net::TcpListener,
    time::{Instant, sleep},
};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> io::Result<()> {
    init_tracing();

    let bind = env::var("INFERLAB_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
    let fallback_workers = env::var("INFERLAB_WORKERS").unwrap_or_else(|_| {
        [
            "worker-a=http://127.0.0.1:9001",
            "worker-b=http://127.0.0.1:9002",
            "worker-c=http://127.0.0.1:9003",
        ]
        .join(",")
    });
    let fallback_policy = env::var("INFERLAB_ROUTING_POLICY")
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
    let circuit_window_size = parse_env("INFERLAB_CIRCUIT_WINDOW_SIZE", 10_usize)?;
    let circuit_minimum_requests = parse_env("INFERLAB_CIRCUIT_MIN_REQUESTS", 5_usize)?;
    let circuit_failure_rate_percent = parse_env("INFERLAB_CIRCUIT_FAILURE_RATE_PERCENT", 50_u64)?;
    let circuit_open_duration_ms = parse_env("INFERLAB_CIRCUIT_OPEN_MS", 5_000_u64)?;
    let pool_template = PoolTemplate {
        ewma_alpha,
        ewma_probe_interval,
        consistent_hash_virtual_nodes,
        worker_concurrency_limit,
        circuit_breaker: CircuitBreakerConfig {
            window_size: circuit_window_size,
            minimum_requests: circuit_minimum_requests,
            failure_rate_percent: circuit_failure_rate_percent,
            open_duration: Duration::from_millis(circuit_open_duration_ms),
        },
    };
    let control_plane_urls = parse_control_plane_urls();
    let control_client = Client::new();
    let initial_control = if control_plane_urls.is_empty() {
        None
    } else {
        Some(
            wait_for_control_configuration(
                &control_client,
                &control_plane_urls,
                Duration::from_secs(3),
            )
            .await?,
        )
    };
    let (workers, policy) = match &initial_control {
        Some((_, committed)) => committed_pool_input(committed)?,
        None => (parse_workers(&fallback_workers)?, fallback_policy),
    };
    let pool = Arc::new(build_pool(workers, policy, pool_template)?);
    let worker_count = pool.snapshots().len();
    let routing_snapshot = match &initial_control {
        Some((_, committed)) => {
            RoutingSnapshot::committed(Arc::clone(&pool), committed.revision, committed.term)
        }
        None => RoutingSnapshot::static_workers(Arc::clone(&pool)),
    };
    let shared_routing: SharedRoutingSnapshot = Arc::new(RwLock::new(routing_snapshot));
    let control_status = initial_control.as_ref().map(|(source_url, committed)| {
        Arc::new(RwLock::new(ControlPlaneStatus {
            enabled: true,
            source_url: Some(source_url.clone()),
            revision: Some(committed.revision),
            term: Some(committed.term),
            last_refresh_ms: Some(now_ms()),
            last_error: None,
        }))
    });
    let app = app_with_dynamic_config(
        Arc::clone(&shared_routing),
        control_status.clone(),
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
    if let Some(status) = control_status {
        let poll_interval = Duration::from_millis(parse_env("INFERLAB_CONTROL_POLL_MS", 100_u64)?);
        tokio::spawn(watch_control_plane(
            control_client,
            control_plane_urls,
            shared_routing,
            status,
            pool_template,
            poll_interval,
        ));
    }
    let listener = TcpListener::bind(&bind).await?;
    info!(
        %bind,
        workers = worker_count,
        %policy,
        control_plane_enabled = initial_control.is_some(),
        control_plane_revision = initial_control.as_ref().map(|(_, config)| config.revision),
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
        circuit_window_size,
        circuit_minimum_requests,
        circuit_failure_rate_percent,
        circuit_open_duration_ms,
        "gateway listening"
    );
    axum::serve(listener, app).await
}

#[derive(Clone, Copy)]
struct PoolTemplate {
    ewma_alpha: f64,
    ewma_probe_interval: usize,
    consistent_hash_virtual_nodes: usize,
    worker_concurrency_limit: usize,
    circuit_breaker: CircuitBreakerConfig,
}

#[derive(Clone, Debug, Deserialize)]
struct CommittedControlConfiguration {
    revision: u64,
    term: u64,
    configuration: ControlRoutingConfiguration,
}

#[derive(Clone, Debug, Deserialize)]
struct ControlRoutingConfiguration {
    routing_policy: String,
    workers: Vec<ControlWorkerConfiguration>,
}

#[derive(Clone, Debug, Deserialize)]
struct ControlWorkerConfiguration {
    id: String,
    base_url: String,
    weight: u32,
}

fn build_pool(
    workers: Vec<WorkerRegistration>,
    policy: RoutingPolicy,
    template: PoolTemplate,
) -> io::Result<WorkerPool> {
    WorkerPool::from_config_with_circuit_breaker(
        workers,
        RoutingConfig {
            policy,
            ewma_alpha: template.ewma_alpha,
            ewma_probe_interval: template.ewma_probe_interval,
            consistent_hash_virtual_nodes: template.consistent_hash_virtual_nodes,
            worker_concurrency_limit: template.worker_concurrency_limit,
        },
        template.circuit_breaker,
    )
    .map_err(io::Error::other)
}

fn committed_pool_input(
    committed: &CommittedControlConfiguration,
) -> io::Result<(Vec<WorkerRegistration>, RoutingPolicy)> {
    let policy = committed
        .configuration
        .routing_policy
        .parse::<RoutingPolicy>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let workers = committed
        .configuration
        .workers
        .iter()
        .map(|worker| WorkerRegistration::new(&worker.id, &worker.base_url, worker.weight))
        .collect();
    Ok((workers, policy))
}

fn parse_control_plane_urls() -> Vec<String> {
    env::var("INFERLAB_CONTROL_PLANE_URLS")
        .ok()
        .map(|raw| {
            raw.split(',')
                .map(|url| url.trim().trim_end_matches('/').to_owned())
                .filter(|url| !url.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

async fn wait_for_control_configuration(
    client: &Client,
    urls: &[String],
    maximum_wait: Duration,
) -> io::Result<(String, CommittedControlConfiguration)> {
    let deadline = Instant::now() + maximum_wait;
    loop {
        if let Some(configuration) = fetch_control_configuration(client, urls).await {
            return Ok(configuration);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no committed control-plane configuration became available within 3 seconds",
            ));
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn fetch_control_configuration(
    client: &Client,
    urls: &[String],
) -> Option<(String, CommittedControlConfiguration)> {
    for url in urls {
        let response = client
            .get(format!("{url}/v1/control/config"))
            .timeout(Duration::from_millis(250))
            .send()
            .await;
        if let Ok(response) = response
            && response.status().is_success()
            && let Ok(configuration) = response.json::<CommittedControlConfiguration>().await
        {
            return Some((url.clone(), configuration));
        }
    }
    None
}

async fn watch_control_plane(
    client: Client,
    urls: Vec<String>,
    routing: SharedRoutingSnapshot,
    status: SharedControlPlaneStatus,
    template: PoolTemplate,
    poll_interval: Duration,
) {
    loop {
        sleep(poll_interval).await;
        let Some((source_url, committed)) = fetch_control_configuration(&client, &urls).await
        else {
            let mut current = status
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            current.last_error =
                Some("no control-plane node returned a committed configuration".to_owned());
            continue;
        };
        let current_revision = routing
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .control_revision
            .unwrap_or(0);
        if committed.revision > current_revision {
            let rebuilt = committed_pool_input(&committed)
                .and_then(|(registrations, policy)| build_pool(registrations, policy, template));
            match rebuilt {
                Ok(pool) => {
                    let policy = pool.policy();
                    *routing
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                        RoutingSnapshot::committed(
                            Arc::new(pool),
                            committed.revision,
                            committed.term,
                        );
                    info!(
                        revision = committed.revision,
                        term = committed.term,
                        %policy,
                        %source_url,
                        "gateway applied committed control-plane configuration"
                    );
                }
                Err(error) => {
                    warn!(
                        revision = committed.revision,
                        %error,
                        "gateway rejected invalid committed control-plane configuration"
                    );
                    status
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .last_error = Some(error.to_string());
                    continue;
                }
            }
        }
        let mut current = status
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        current.source_url = Some(source_url);
        current.revision = Some(committed.revision);
        current.term = Some(committed.term);
        current.last_refresh_ms = Some(now_ms());
        current.last_error = None;
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
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
