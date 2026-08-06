use std::{
    env, io,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gateway::{
    ControlPlaneStatus, RoutingSnapshot, SharedControlPlaneStatus, SharedRoutingSnapshot,
    admission::AdmissionConfig,
    app_with_runtime_config,
    circuit_breaker::CircuitBreakerConfig,
    resilience::{ResilienceConfig, default_jitter_seed},
    routing::{RoutingConfig, RoutingPolicy, WorkerPool, WorkerRegistration},
    routing_lease::{RoutingLeaseExpiryAction, RoutingLeaseGuard, SharedRoutingLease},
    routing_snapshot_store::{
        CommittedRoutingConfiguration, DEFAULT_CONTROL_CLUSTER_ID, PersistedRoutingSnapshot,
        RoutingSnapshotStore, SnapshotFreshness, SnapshotFreshnessPolicy, validate_committed,
        validate_control_cluster_id, validate_expected_control_cluster,
        validate_snapshot_freshness,
    },
};
use reqwest::Client;
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
    let expected_control_cluster_id = if control_plane_urls.is_empty() {
        None
    } else {
        let cluster_id = env::var("INFERLAB_CONTROL_CLUSTER_ID")
            .unwrap_or_else(|_| DEFAULT_CONTROL_CLUSTER_ID.to_owned());
        validate_control_cluster_id(&cluster_id)?;
        Some(cluster_id)
    };
    let snapshot_store = env::var_os("INFERLAB_ROUTING_SNAPSHOT_PATH")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .map(RoutingSnapshotStore::new);
    let snapshot_maximum_age_ms = parse_optional_env("INFERLAB_ROUTING_SNAPSHOT_MAX_AGE_MS")?;
    if snapshot_maximum_age_ms == Some(0_u64) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_ROUTING_SNAPSHOT_MAX_AGE_MS must be positive when configured",
        ));
    }
    let snapshot_freshness_policy = SnapshotFreshnessPolicy {
        maximum_age_ms: snapshot_maximum_age_ms,
        maximum_future_skew_ms: parse_env(
            "INFERLAB_ROUTING_SNAPSHOT_MAX_FUTURE_SKEW_MS",
            1_000_u64,
        )?,
    };
    let routing_lease_ms = parse_optional_env("INFERLAB_ROUTING_LEASE_MS")?;
    if routing_lease_ms == Some(0_u64) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_ROUTING_LEASE_MS must be positive when configured",
        ));
    }
    let routing_lease_expiry_action = env::var("INFERLAB_ROUTING_LEASE_EXPIRY_ACTION")
        .unwrap_or_else(|_| "reject-new".to_owned())
        .parse::<RoutingLeaseExpiryAction>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let bootstrap_wait =
        Duration::from_millis(parse_env("INFERLAB_CONTROL_BOOTSTRAP_WAIT_MS", 3_000_u64)?);
    let control_client = Client::new();
    let mut initial_control = if control_plane_urls.is_empty() {
        None
    } else {
        Some(
            bootstrap_control_configuration(
                &control_client,
                &control_plane_urls,
                bootstrap_wait,
                snapshot_store.as_ref(),
                snapshot_freshness_policy,
                expected_control_cluster_id
                    .as_deref()
                    .expect("control URLs have an expected cluster identity"),
            )
            .await?,
        )
    };
    let (workers, policy) = match &initial_control {
        Some(initial) => committed_pool_input(&initial.committed)?,
        None => (parse_workers(&fallback_workers)?, fallback_policy),
    };
    let pool = Arc::new(build_pool(workers, policy, pool_template)?);
    if let Some(initial) = initial_control.as_mut()
        && initial.bootstrap_source == BootstrapSource::Live
        && let Some(store) = snapshot_store.as_ref()
    {
        let persisted = store.save(&initial.committed).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot durably save live routing revision {} to {}: {error}",
                    initial.committed.revision,
                    store.path().display()
                ),
            )
        })?;
        initial.persisted_at_ms = Some(persisted.saved_at_ms);
        initial.persisted_expires_at_ms =
            snapshot_freshness_policy.expires_at_ms(persisted.saved_at_ms);
    }
    let worker_count = pool.snapshots().len();
    let routing_snapshot = match &initial_control {
        Some(initial) => RoutingSnapshot::committed_in_cluster(
            Arc::clone(&pool),
            &initial.committed.cluster_id,
            initial.committed.revision,
            initial.committed.term,
        ),
        None => RoutingSnapshot::static_workers(Arc::clone(&pool)),
    };
    let shared_routing: SharedRoutingSnapshot = Arc::new(RwLock::new(routing_snapshot));
    let control_status = initial_control.as_ref().map(|initial| {
        Arc::new(RwLock::new(ControlPlaneStatus {
            enabled: true,
            bootstrap_source: Some(initial.bootstrap_source.as_str().to_owned()),
            source_url: initial.source_url.clone(),
            expected_cluster_id: expected_control_cluster_id.clone(),
            last_rejected_cluster_id: None,
            cluster_mismatch_rejections: 0,
            revision: Some(initial.committed.revision),
            term: Some(initial.committed.term),
            last_refresh_ms: (initial.bootstrap_source == BootstrapSource::Live).then(now_ms),
            last_error: None,
            snapshot_path: snapshot_store
                .as_ref()
                .map(|store| store.path().display().to_string()),
            snapshot_max_age_ms: snapshot_store
                .as_ref()
                .and(snapshot_freshness_policy.maximum_age_ms),
            snapshot_max_future_skew_ms: snapshot_store
                .as_ref()
                .map(|_| snapshot_freshness_policy.maximum_future_skew_ms),
            bootstrap_snapshot_age_ms: initial.bootstrap_snapshot_age_ms,
            persisted_revision: initial.persisted_at_ms.map(|_| initial.committed.revision),
            persisted_at_ms: initial.persisted_at_ms,
            persisted_expires_at_ms: initial.persisted_expires_at_ms,
        }))
    });
    if routing_lease_ms.is_some() && initial_control.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_ROUTING_LEASE_MS requires INFERLAB_CONTROL_PLANE_URLS",
        ));
    }
    let routing_lease: Option<SharedRoutingLease> =
        if let (Some(initial), Some(lease_ms)) = (initial_control.as_ref(), routing_lease_ms) {
            let duration = Duration::from_millis(lease_ms);
            let guard = match initial.bootstrap_source {
                BootstrapSource::Live => {
                    RoutingLeaseGuard::from_live(duration, routing_lease_expiry_action, now_ms())
                }
                BootstrapSource::Disk => {
                    let persisted_at_ms = initial.persisted_at_ms.ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "disk bootstrap is missing its persisted routing timestamp",
                        )
                    })?;
                    let observed_age_ms = initial.bootstrap_snapshot_age_ms.ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "disk bootstrap is missing its observed snapshot age",
                        )
                    })?;
                    RoutingLeaseGuard::from_disk(
                        duration,
                        routing_lease_expiry_action,
                        persisted_at_ms,
                        Duration::from_millis(observed_age_ms),
                    )
                }
            };
            Some(Arc::new(guard))
        } else {
            None
        };
    let app = app_with_runtime_config(
        Arc::clone(&shared_routing),
        control_status.clone(),
        routing_lease.clone(),
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
    if let (Some(status), Some(initial)) = (control_status, initial_control.as_ref()) {
        let poll_interval = Duration::from_millis(parse_env("INFERLAB_CONTROL_POLL_MS", 100_u64)?);
        tokio::spawn(watch_control_plane(
            control_client,
            control_plane_urls,
            shared_routing,
            status,
            ControlPlaneWatcherConfig {
                template: pool_template,
                poll_interval,
                snapshot_store: snapshot_store.clone(),
                snapshot_freshness_policy,
                applied_configuration: initial.committed.clone(),
                routing_lease: routing_lease.clone(),
                expected_cluster_id: expected_control_cluster_id
                    .clone()
                    .expect("control watcher has an expected cluster identity"),
            },
        ));
    }
    let listener = TcpListener::bind(&bind).await?;
    info!(
        %bind,
        workers = worker_count,
        %policy,
        control_plane_enabled = initial_control.is_some(),
        control_plane_revision = initial_control
            .as_ref()
            .map(|initial| initial.committed.revision),
        control_plane_bootstrap_source = initial_control
            .as_ref()
            .map(|initial| initial.bootstrap_source.as_str()),
        control_plane_expected_cluster_id = expected_control_cluster_id,
        routing_snapshot_path = snapshot_store
            .as_ref()
            .map(|store| store.path().display().to_string()),
        routing_snapshot_max_age_ms = snapshot_freshness_policy.maximum_age_ms,
        routing_snapshot_max_future_skew_ms = snapshot_freshness_policy.maximum_future_skew_ms,
        routing_lease_ms,
        routing_lease_expiry_action = %routing_lease_expiry_action,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BootstrapSource {
    Live,
    Disk,
}

impl BootstrapSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live-control-plane",
            Self::Disk => "disk-snapshot",
        }
    }
}

#[derive(Clone, Debug)]
struct InitialControlConfiguration {
    source_url: Option<String>,
    committed: CommittedRoutingConfiguration,
    bootstrap_source: BootstrapSource,
    bootstrap_snapshot_age_ms: Option<u64>,
    persisted_at_ms: Option<u64>,
    persisted_expires_at_ms: Option<u64>,
}

struct ControlPlaneWatcherConfig {
    template: PoolTemplate,
    poll_interval: Duration,
    snapshot_store: Option<RoutingSnapshotStore>,
    snapshot_freshness_policy: SnapshotFreshnessPolicy,
    applied_configuration: CommittedRoutingConfiguration,
    routing_lease: Option<SharedRoutingLease>,
    expected_cluster_id: String,
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
    committed: &CommittedRoutingConfiguration,
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
    expected_cluster_id: &str,
) -> io::Result<(String, CommittedRoutingConfiguration)> {
    let deadline = Instant::now() + maximum_wait;
    let mut last_mismatch = None;
    loop {
        let fetched = fetch_control_configuration(client, urls, expected_cluster_id).await;
        if let Some(mismatch) = fetched.mismatches.last() {
            last_mismatch = Some(mismatch.clone());
        }
        if let Some(configuration) = fetched.configuration {
            return Ok(configuration);
        }
        if Instant::now() >= deadline {
            let mismatch = last_mismatch
                .map(|observed| {
                    format!(
                        "; rejected cluster '{}' from {} while expecting '{expected_cluster_id}'",
                        observed.cluster_id, observed.source_url
                    )
                })
                .unwrap_or_default();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "no committed configuration for control cluster '{expected_cluster_id}' became available within {} ms{mismatch}",
                    maximum_wait.as_millis(),
                ),
            ));
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn fetch_control_configuration(
    client: &Client,
    urls: &[String],
    expected_cluster_id: &str,
) -> ControlFetchResult {
    let mut fetched = ControlFetchResult::default();
    for url in urls {
        let response = client
            .get(format!("{url}/v1/control/config"))
            .timeout(Duration::from_millis(250))
            .send()
            .await;
        if let Ok(response) = response
            && response.status().is_success()
            && let Ok(configuration) = response.json::<CommittedRoutingConfiguration>().await
        {
            if configuration.cluster_id == expected_cluster_id {
                fetched.configuration = Some((url.clone(), configuration));
                return fetched;
            }
            fetched.mismatches.push(ClusterMismatch {
                source_url: url.clone(),
                cluster_id: configuration.cluster_id,
            });
        }
    }
    fetched
}

#[derive(Clone, Debug)]
struct ClusterMismatch {
    source_url: String,
    cluster_id: String,
}

#[derive(Debug, Default)]
struct ControlFetchResult {
    configuration: Option<(String, CommittedRoutingConfiguration)>,
    mismatches: Vec<ClusterMismatch>,
}

async fn bootstrap_control_configuration(
    client: &Client,
    urls: &[String],
    maximum_wait: Duration,
    snapshot_store: Option<&RoutingSnapshotStore>,
    freshness_policy: SnapshotFreshnessPolicy,
    expected_cluster_id: &str,
) -> io::Result<InitialControlConfiguration> {
    let mut snapshot_error = None;
    let persisted = snapshot_store.and_then(|store| match store.load() {
        Ok(snapshot) => Some(snapshot),
        Err(error) => {
            snapshot_error = Some(format!("{}: {error}", store.path().display()));
            None
        }
    });
    let live =
        wait_for_control_configuration(client, urls, maximum_wait, expected_cluster_id).await;
    match live {
        Ok((source_url, committed)) => {
            if let Err(error) = validate_committed(&committed) {
                return bootstrap_from_disk(
                    persisted,
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("live control plane returned invalid configuration: {error}"),
                    ),
                    snapshot_store,
                    snapshot_error,
                    freshness_policy,
                    expected_cluster_id,
                );
            }
            if let Some(snapshot) = persisted {
                if snapshot.committed.cluster_id != expected_cluster_id {
                    warn!(
                        disk_cluster_id = %snapshot.committed.cluster_id,
                        %expected_cluster_id,
                        "gateway ignored a durable snapshot from a different control cluster because expected live control is available"
                    );
                } else if snapshot.committed.revision > committed.revision {
                    let freshness = snapshot_freshness(&snapshot, freshness_policy).map_err(
                        |error| {
                            io::Error::new(
                                error.kind(),
                                format!(
                                    "live control revision {} is older than durable revision {}, but the durable snapshot is not eligible for fallback: {error}",
                                    committed.revision, snapshot.committed.revision
                                ),
                            )
                        },
                    )?;
                    warn!(
                        live_revision = committed.revision,
                        disk_revision = snapshot.committed.revision,
                        "gateway refused to roll back below its durable routing revision"
                    );
                    return Ok(initial_from_disk(snapshot, freshness));
                }
                if snapshot.committed.cluster_id == expected_cluster_id
                    && snapshot.committed.revision == committed.revision
                    && snapshot.committed != committed
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "live control plane and durable snapshot disagree at routing revision {}",
                            committed.revision
                        ),
                    ));
                }
            }
            Ok(InitialControlConfiguration {
                source_url: Some(source_url),
                committed,
                bootstrap_source: BootstrapSource::Live,
                bootstrap_snapshot_age_ms: None,
                persisted_at_ms: None,
                persisted_expires_at_ms: None,
            })
        }
        Err(live_error) => bootstrap_from_disk(
            persisted,
            live_error,
            snapshot_store,
            snapshot_error,
            freshness_policy,
            expected_cluster_id,
        ),
    }
}

fn bootstrap_from_disk(
    persisted: Option<PersistedRoutingSnapshot>,
    live_error: io::Error,
    snapshot_store: Option<&RoutingSnapshotStore>,
    snapshot_error: Option<String>,
    freshness_policy: SnapshotFreshnessPolicy,
    expected_cluster_id: &str,
) -> io::Result<InitialControlConfiguration> {
    match persisted {
        Some(snapshot) => {
            validate_expected_control_cluster(&snapshot.committed, expected_cluster_id).map_err(
                |error| {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "control-plane bootstrap failed ({live_error}); durable routing snapshot is not eligible for fallback: {error}"
                        ),
                    )
                },
            )?;
            let freshness = snapshot_freshness(&snapshot, freshness_policy).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "control-plane bootstrap failed ({live_error}); durable routing snapshot is not eligible for fallback: {error}"
                    ),
                )
            })?;
            warn!(
                revision = snapshot.committed.revision,
                term = snapshot.committed.term,
                observed_age_ms = freshness.observed_age_ms,
                snapshot_path = snapshot_store.map(|store| store.path().display().to_string()),
                %live_error,
                "gateway bootstrapped from the last durable routing snapshot"
            );
            Ok(initial_from_disk(snapshot, freshness))
        }
        None => Err(io::Error::new(
            live_error.kind(),
            format!(
                "control-plane bootstrap failed ({live_error}); no valid durable routing snapshot is available{}",
                snapshot_error
                    .map(|error| format!(" ({error})"))
                    .unwrap_or_default()
            ),
        )),
    }
}

fn snapshot_freshness(
    snapshot: &PersistedRoutingSnapshot,
    policy: SnapshotFreshnessPolicy,
) -> io::Result<SnapshotFreshness> {
    validate_snapshot_freshness(snapshot.saved_at_ms, now_ms(), policy)
}

fn initial_from_disk(
    snapshot: PersistedRoutingSnapshot,
    freshness: SnapshotFreshness,
) -> InitialControlConfiguration {
    let saved_at_ms = snapshot.saved_at_ms;
    InitialControlConfiguration {
        source_url: None,
        committed: snapshot.committed,
        bootstrap_source: BootstrapSource::Disk,
        bootstrap_snapshot_age_ms: Some(freshness.observed_age_ms),
        persisted_at_ms: Some(saved_at_ms),
        persisted_expires_at_ms: freshness.expires_at_ms,
    }
}

async fn watch_control_plane(
    client: Client,
    urls: Vec<String>,
    routing: SharedRoutingSnapshot,
    status: SharedControlPlaneStatus,
    config: ControlPlaneWatcherConfig,
) {
    let ControlPlaneWatcherConfig {
        template,
        poll_interval,
        snapshot_store,
        snapshot_freshness_policy,
        mut applied_configuration,
        routing_lease,
        expected_cluster_id,
    } = config;
    loop {
        sleep(poll_interval).await;
        let fetched = fetch_control_configuration(&client, &urls, &expected_cluster_id).await;
        if !fetched.mismatches.is_empty() {
            let last = fetched
                .mismatches
                .last()
                .expect("non-empty mismatch observations");
            warn!(
                expected_cluster_id = %expected_cluster_id,
                observed_cluster_id = %last.cluster_id,
                source_url = %last.source_url,
                rejected = fetched.mismatches.len(),
                "gateway rejected control responses from a different cluster"
            );
            let mut current = status
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            current.last_rejected_cluster_id = Some(last.cluster_id.clone());
            current.cluster_mismatch_rejections = current
                .cluster_mismatch_rejections
                .saturating_add(u64::try_from(fetched.mismatches.len()).unwrap_or(u64::MAX));
        }
        let Some((source_url, committed)) = fetched.configuration else {
            let mut current = status
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            current.last_error = if let Some(last) = fetched.mismatches.last() {
                Some(format!(
                    "control cluster identity mismatch: expected '{expected_cluster_id}', observed '{}' from {}",
                    last.cluster_id, last.source_url
                ))
            } else {
                Some("no control-plane node returned a committed configuration".to_owned())
            };
            continue;
        };
        if let Err(error) = validate_committed(&committed) {
            warn!(
                revision = committed.revision,
                %source_url,
                %error,
                "gateway rejected invalid committed control-plane configuration"
            );
            let mut current = status
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            current.source_url = Some(source_url);
            current.last_refresh_ms = Some(now_ms());
            current.last_error = Some(error.to_string());
            continue;
        }
        let current_revision = routing
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .control_revision
            .unwrap_or(0);
        if committed.revision < current_revision {
            warn!(
                observed_revision = committed.revision,
                current_revision,
                %source_url,
                "gateway ignored a control-plane revision older than its routing snapshot"
            );
            let mut current = status
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            current.source_url = Some(source_url);
            current.last_refresh_ms = Some(now_ms());
            current.last_error = Some(format!(
                "ignored stale control-plane revision {}; current routing revision is {current_revision}",
                committed.revision
            ));
            continue;
        }
        if committed.revision == current_revision && committed != applied_configuration {
            warn!(
                revision = committed.revision,
                %source_url,
                "gateway rejected divergent control-plane content at the applied revision"
            );
            let mut current = status
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            current.source_url = Some(source_url);
            current.last_refresh_ms = Some(now_ms());
            current.last_error = Some(format!(
                "control-plane content diverges at applied routing revision {current_revision}"
            ));
            continue;
        }
        if committed.revision > current_revision {
            let rebuilt = committed_pool_input(&committed)
                .and_then(|(registrations, policy)| build_pool(registrations, policy, template));
            match rebuilt {
                Ok(pool) => {
                    let persisted = if let Some(store) = snapshot_store.as_ref() {
                        match store.save(&committed) {
                            Ok(persisted) => Some(persisted),
                            Err(error) => {
                                warn!(
                                    revision = committed.revision,
                                    snapshot_path = %store.path().display(),
                                    %error,
                                    "gateway refused to apply a revision it could not persist"
                                );
                                status
                                    .write()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                                    .last_error = Some(format!(
                                    "cannot persist routing revision {}: {error}",
                                    committed.revision
                                ));
                                continue;
                            }
                        }
                    } else {
                        None
                    };
                    let policy = pool.policy();
                    *routing
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                        RoutingSnapshot::committed_in_cluster(
                            Arc::new(pool),
                            &committed.cluster_id,
                            committed.revision,
                            committed.term,
                        );
                    applied_configuration = committed.clone();
                    info!(
                        revision = committed.revision,
                        term = committed.term,
                        %policy,
                        %source_url,
                        "gateway applied committed control-plane configuration"
                    );
                    if let Some(persisted) = persisted {
                        let mut current = status
                            .write()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        current.persisted_revision = Some(persisted.committed.revision);
                        current.persisted_at_ms = Some(persisted.saved_at_ms);
                        current.persisted_expires_at_ms =
                            snapshot_freshness_policy.expires_at_ms(persisted.saved_at_ms);
                    }
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
        let verified_at_ms = now_ms();
        if let Some(lease) = routing_lease.as_ref() {
            lease.renew(verified_at_ms);
        }
        let mut current = status
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        current.source_url = Some(source_url);
        current.revision = Some(committed.revision);
        current.term = Some(committed.term);
        current.last_refresh_ms = Some(verified_at_ms);
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

fn parse_optional_env<T>(name: &str) -> io::Result<Option<T>>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match env::var(name) {
        Ok(value) => value.parse::<T>().map(Some).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} has an invalid value: {error}"),
            )
        }),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} cannot be read: {error}"),
        )),
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
    use std::time::Duration;

    use axum::{Json, Router, routing::get};
    use gateway::routing_snapshot_store::{
        CommittedRoutingConfiguration, StoredRoutingConfiguration, StoredWorkerConfiguration,
    };
    use tokio::net::TcpListener;

    use super::{fetch_control_configuration, parse_workers, wait_for_control_configuration};

    fn committed(cluster_id: &str) -> CommittedRoutingConfiguration {
        CommittedRoutingConfiguration {
            cluster_id: cluster_id.to_owned(),
            revision: 2,
            term: 1,
            configuration: StoredRoutingConfiguration {
                routing_policy: "round-robin".to_owned(),
                workers: vec![StoredWorkerConfiguration {
                    id: "worker-a".to_owned(),
                    base_url: "http://127.0.0.1:9001".to_owned(),
                    weight: 1,
                }],
            },
        }
    }

    async fn spawn_control(configuration: CommittedRoutingConfiguration) -> String {
        let app = Router::new().route(
            "/v1/control/config",
            get(move || {
                let configuration = configuration.clone();
                async move { Json(configuration) }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind control fixture");
        let address = listener.local_addr().expect("control fixture address");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("control fixture");
        });
        format!("http://{address}")
    }

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

    #[tokio::test]
    async fn fetch_skips_a_foreign_cluster_and_uses_the_expected_cluster() {
        let foreign = spawn_control(committed("inferlab-foreign")).await;
        let primary = spawn_control(committed("inferlab-primary")).await;

        let fetched = fetch_control_configuration(
            &reqwest::Client::new(),
            &[foreign.clone(), primary.clone()],
            "inferlab-primary",
        )
        .await;

        let (source, configuration) = fetched.configuration.expect("expected cluster result");
        assert_eq!(source, primary);
        assert_eq!(configuration.cluster_id, "inferlab-primary");
        assert_eq!(fetched.mismatches.len(), 1);
        assert_eq!(fetched.mismatches[0].source_url, foreign);
        assert_eq!(fetched.mismatches[0].cluster_id, "inferlab-foreign");
    }

    #[tokio::test]
    async fn bootstrap_wait_reports_the_observed_foreign_cluster() {
        let foreign = spawn_control(committed("inferlab-foreign")).await;

        let error = wait_for_control_configuration(
            &reqwest::Client::new(),
            &[foreign],
            Duration::from_millis(20),
            "inferlab-primary",
        )
        .await
        .expect_err("foreign cluster must not bootstrap");

        assert!(error.to_string().contains("inferlab-primary"));
        assert!(error.to_string().contains("inferlab-foreign"));
    }
}
