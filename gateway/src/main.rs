use std::{
    env, io,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gateway::{
    ControlPlaneStatus, HostedGatewayRouters, RoutingSnapshot, ServiceSigningStatus,
    SharedControlPlaneStatus, SharedRoutingSnapshot,
    admission::AdmissionConfig,
    app_with_runtime_config_and_public_authentication_and_observability,
    circuit_breaker::CircuitBreakerConfig,
    control_authentication::{ControlAuthenticator, SigningKeyTransition, same_routing_payload},
    hosted_apps_with_runtime_config_and_observability,
    public_authentication::{OperatorApiAuthenticator, PublicApiAuthenticator},
    public_edge::{
        DEFAULT_MAX_MESSAGES, DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_MAX_PROMPT_BYTES,
        DEFAULT_RATE_BURST, DEFAULT_RATE_REQUESTS_PER_MINUTE, PublicEdgeConfig, PublicEdgeMode,
    },
    resilience::{ResilienceConfig, default_jitter_seed},
    routing::{RoutingConfig, RoutingPolicy, WorkerPool, WorkerRegistration},
    routing_lease::{RoutingLeaseExpiryAction, RoutingLeaseGuard, SharedRoutingLease},
    routing_snapshot_store::{
        CommittedRoutingConfiguration, DEFAULT_CONTROL_CLUSTER_ID, PersistedRoutingSnapshot,
        RoutingSnapshotStore, SnapshotFreshness, SnapshotFreshnessPolicy, validate_committed,
        validate_control_cluster_id, validate_expected_control_cluster,
        validate_snapshot_freshness,
    },
    service_client::{ControlServiceClient, parse_control_service_targets},
};
use observability::{MetricsRegistry, MetricsServerConfig, Service, init_tracing, serve_metrics};
use reqwest::Client;
use service_auth::{
    LEGACY_CREDENTIAL_ID, ServiceSigner, ServiceSignerActivationOutcome, ServiceSigningError,
    ServiceSigningErrorKind, ServiceSigningIdentity, VerifiedServiceSigningBundle,
};
use tokio::{
    net::TcpListener,
    time::{Instant, sleep},
};
use tracing::{info, warn};

#[tokio::main]
async fn main() -> io::Result<()> {
    init_tracing(Service::Gateway).map_err(io::Error::other)?;
    let metrics_server = MetricsServerConfig::from_env().map_err(io::Error::other)?;
    let mut metrics_registry = MetricsRegistry::new();

    let public_edge_mode = public_edge_mode_from_environment()?;
    let bind = match (public_edge_mode, env::var("INFERLAB_BIND")) {
        (_, Ok(bind)) => bind,
        (_, Err(env::VarError::NotPresent)) => "127.0.0.1:8080".to_owned(),
        (PublicEdgeMode::Local, Err(env::VarError::NotUnicode(_))) => "127.0.0.1:8080".to_owned(),
        (PublicEdgeMode::Hosted, Err(env::VarError::NotUnicode(_))) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "INFERLAB_BIND must be valid UTF-8 in hosted mode",
            ));
        }
    };
    let public_api_authentication = public_api_authenticator_from_environment()?;
    let public_api_authentication_status = public_api_authentication.status();
    let hosted_gateway =
        hosted_gateway_configuration(public_edge_mode, &bind, &public_api_authentication)?;
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
    let control_plane_urls = parse_control_plane_urls()?;
    let expected_control_cluster_id = if control_plane_urls.is_empty() {
        None
    } else {
        let cluster_id =
            control_cluster_id_from_env_result(env::var("INFERLAB_CONTROL_CLUSTER_ID"))?;
        validate_control_cluster_id(&cluster_id)?;
        Some(cluster_id)
    };
    let trusted_control_keys = optional_string_env("INFERLAB_CONTROL_TRUSTED_KEYS")?;
    let revoked_control_key_ids = optional_string_env("INFERLAB_CONTROL_REVOKED_KEY_IDS")?;
    let control_authenticator = Arc::new(ControlAuthenticator::from_configuration(
        trusted_control_keys.as_deref(),
        revoked_control_key_ids.as_deref(),
    )?);
    if control_authenticator.required() && control_plane_urls.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_CONTROL_TRUSTED_KEYS requires INFERLAB_CONTROL_PLANE_URLS",
        ));
    }
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
    let GatewayServiceClientBootstrap {
        client: control_client,
        signing_status,
        signing_watcher,
    } = gateway_service_client(
        Client::new(),
        &control_plane_urls,
        expected_control_cluster_id.as_deref(),
    )?;
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
                &control_authenticator,
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
        Some(initial) => RoutingSnapshot::committed_authenticated(
            Arc::clone(&pool),
            &initial.committed.cluster_id,
            initial.verified_signing_key_id.clone(),
            initial.committed.revision,
            initial.committed.term,
        ),
        None => RoutingSnapshot::static_workers(Arc::clone(&pool)),
    };
    let shared_routing: SharedRoutingSnapshot = Arc::new(RwLock::new(routing_snapshot));
    let control_status = initial_control.as_ref().map(|initial| {
        Arc::new(RwLock::new(ControlPlaneStatus {
            enabled: true,
            service_authentication_enabled: control_client.authentication_enabled(),
            service_id: control_client.service_id(),
            service_credential_id: control_client.credential_id(),
            service_signing: signing_status,
            control_service_targets: control_client.configured_targets(),
            bootstrap_source: Some(initial.bootstrap_source.as_str().to_owned()),
            source_url: initial.source_url.clone(),
            expected_cluster_id: expected_control_cluster_id.clone(),
            last_rejected_cluster_id: None,
            cluster_mismatch_rejections: 0,
            authentication_required: control_authenticator.required(),
            trusted_signing_key_ids: control_authenticator.trusted_key_ids(),
            revoked_signing_key_ids: control_authenticator.revoked_key_ids(),
            active_signing_key_id: initial.verified_signing_key_id.clone(),
            last_rejected_signing_key_id: None,
            signature_verifications: u64::from(initial.verified_signing_key_id.is_some()),
            signature_rejections: 0,
            signing_key_downgrade_rejections: 0,
            last_authentication_error: None,
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
    let admission_config = AdmissionConfig {
        queue_capacity: admission_queue_capacity,
    };
    let resilience_config = ResilienceConfig {
        request_deadline: Duration::from_millis(request_deadline_ms),
        attempt_timeout: Duration::from_millis(attempt_timeout_ms),
        max_retries,
        retry_budget_percent,
        retry_base_delay: Duration::from_millis(retry_base_delay_ms),
        retry_max_delay: Duration::from_millis(retry_max_delay_ms),
        jitter_seed,
    };
    let applications = match hosted_gateway.as_ref() {
        Some(hosted) => GatewayApplications::Hosted(
            hosted_apps_with_runtime_config_and_observability(
                Arc::clone(&shared_routing),
                control_status.clone(),
                routing_lease.clone(),
                admission_config,
                resilience_config,
                public_api_authentication,
                hosted.operator_authentication.clone(),
                hosted.public_edge,
                &mut metrics_registry,
            )
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?,
        ),
        None => GatewayApplications::Local(
            app_with_runtime_config_and_public_authentication_and_observability(
                Arc::clone(&shared_routing),
                control_status.clone(),
                routing_lease.clone(),
                admission_config,
                resilience_config,
                public_api_authentication,
                &mut metrics_registry,
            )
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?,
        ),
    };
    let bound_gateway = match (applications, hosted_gateway.as_ref()) {
        (GatewayApplications::Local(app), None) => BoundGateway::Local {
            listener: TcpListener::bind(&bind).await?,
            app,
        },
        (GatewayApplications::Hosted(apps), Some(hosted)) => {
            let public_listener = TcpListener::bind(hosted.public_bind)
                .await
                .map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "bind hosted public listener {}: {error}",
                            hosted.public_bind
                        ),
                    )
                })?;
            let operator_listener =
                TcpListener::bind(hosted.operator_bind)
                    .await
                    .map_err(|error| {
                        io::Error::new(
                            error.kind(),
                            format!(
                                "bind hosted operator listener {}: {error}",
                                hosted.operator_bind
                            ),
                        )
                    })?;
            BoundGateway::Hosted {
                public_listener,
                operator_listener,
                apps,
            }
        }
        _ => {
            return Err(io::Error::other(
                "gateway listener mode did not match its router configuration",
            ));
        }
    };
    if let (Some(status), Some(initial)) = (control_status.clone(), initial_control.as_ref()) {
        let poll_interval = Duration::from_millis(parse_env("INFERLAB_CONTROL_POLL_MS", 100_u64)?);
        tokio::spawn(watch_control_plane(
            control_client.clone(),
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
                authenticator: Arc::clone(&control_authenticator),
            },
        ));
    }
    let signing_watcher_task =
        if let (Some(watcher), Some(status)) = (signing_watcher, control_status.clone()) {
            Some(tokio::spawn(watch_service_signing_bundle(watcher, status)))
        } else {
            None
        };
    info!(
        %bind,
        public_edge_mode = %public_edge_mode,
        operator_bind = ?hosted_gateway.as_ref().map(|hosted| hosted.operator_bind),
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
        control_authentication_required = control_authenticator.required(),
        service_authentication_enabled = control_client.authentication_enabled(),
        public_api_authentication_enabled = public_api_authentication_status.enabled,
        public_api_key_count = public_api_authentication_status.key_count,
        service_id = control_client.service_id(),
        service_credential_id = control_client.credential_id(),
        control_service_targets = ?control_client.configured_targets(),
        trusted_control_signing_key_ids = ?control_authenticator.trusted_key_ids(),
        revoked_control_signing_key_ids = ?control_authenticator.revoked_key_ids(),
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
    let metrics_registry = Arc::new(metrics_registry);
    serve_gateway(
        bound_gateway,
        metrics_server,
        metrics_registry,
        signing_watcher_task,
    )
    .await
}

#[derive(Clone)]
struct HostedGatewayConfiguration {
    public_bind: SocketAddr,
    operator_bind: SocketAddr,
    operator_authentication: OperatorApiAuthenticator,
    public_edge: PublicEdgeConfig,
}

enum GatewayApplications {
    Local(axum::Router),
    Hosted(HostedGatewayRouters),
}

enum BoundGateway {
    Local {
        listener: TcpListener,
        app: axum::Router,
    },
    Hosted {
        public_listener: TcpListener,
        operator_listener: TcpListener,
        apps: HostedGatewayRouters,
    },
}

fn public_edge_mode_from_environment() -> io::Result<PublicEdgeMode> {
    match env::var("INFERLAB_PUBLIC_EDGE_MODE") {
        Ok(mode) => {
            if mode.len() > 16 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "INFERLAB_PUBLIC_EDGE_MODE must be local or hosted",
                ));
            }
            mode.parse()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
        }
        Err(env::VarError::NotPresent) => Ok(PublicEdgeMode::Local),
        Err(env::VarError::NotUnicode(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_PUBLIC_EDGE_MODE must be valid UTF-8",
        )),
    }
}

fn hosted_gateway_configuration(
    mode: PublicEdgeMode,
    bind: &str,
    public_authentication: &PublicApiAuthenticator,
) -> io::Result<Option<HostedGatewayConfiguration>> {
    if mode == PublicEdgeMode::Local {
        for name in [
            "INFERLAB_OPERATOR_BIND",
            "INFERLAB_OPERATOR_API_KEY",
            "INFERLAB_PUBLIC_MAX_MESSAGES",
            "INFERLAB_PUBLIC_MAX_PROMPT_BYTES",
            "INFERLAB_PUBLIC_MAX_OUTPUT_TOKENS",
            "INFERLAB_PUBLIC_RATE_REQUESTS_PER_MINUTE",
            "INFERLAB_PUBLIC_RATE_BURST",
        ] {
            if env::var_os(name).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{name} requires INFERLAB_PUBLIC_EDGE_MODE=hosted"),
                ));
            }
        }
        return Ok(None);
    }
    if !public_authentication.status().enabled {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "hosted public edge requires explicit nonempty INFERLAB_PUBLIC_API_KEYS",
        ));
    }
    let public_bind = parse_hosted_bind("INFERLAB_BIND", bind)?;
    let operator_bind_raw = required_utf8_environment("INFERLAB_OPERATOR_BIND")?;
    let operator_bind = parse_hosted_bind("INFERLAB_OPERATOR_BIND", &operator_bind_raw)?;
    if socket_binds_overlap(public_bind, operator_bind) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_BIND and INFERLAB_OPERATOR_BIND must not overlap",
        ));
    }
    let operator_key = required_utf8_environment("INFERLAB_OPERATOR_API_KEY")?;
    let operator_authentication = OperatorApiAuthenticator::from_configuration(&operator_key)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if public_authentication.overlaps_operator(&operator_authentication) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_OPERATOR_API_KEY must not match any INFERLAB_PUBLIC_API_KEYS entry",
        ));
    }
    let public_edge = PublicEdgeConfig::hosted(
        parse_hosted_env("INFERLAB_PUBLIC_MAX_MESSAGES", DEFAULT_MAX_MESSAGES)?,
        parse_hosted_env("INFERLAB_PUBLIC_MAX_PROMPT_BYTES", DEFAULT_MAX_PROMPT_BYTES)?,
        parse_hosted_env(
            "INFERLAB_PUBLIC_MAX_OUTPUT_TOKENS",
            DEFAULT_MAX_OUTPUT_TOKENS,
        )?,
        parse_hosted_env(
            "INFERLAB_PUBLIC_RATE_REQUESTS_PER_MINUTE",
            DEFAULT_RATE_REQUESTS_PER_MINUTE,
        )?,
        parse_hosted_env("INFERLAB_PUBLIC_RATE_BURST", DEFAULT_RATE_BURST)?,
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    Ok(Some(HostedGatewayConfiguration {
        public_bind,
        operator_bind,
        operator_authentication,
        public_edge,
    }))
}

fn required_utf8_environment(name: &str) -> io::Result<String> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        Ok(_) | Err(env::VarError::NotPresent) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be explicitly configured and nonempty in hosted mode"),
        )),
        Err(env::VarError::NotUnicode(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be valid UTF-8"),
        )),
    }
}

fn parse_hosted_env<T>(name: &str, default: T) -> io::Result<T>
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
        Err(env::VarError::NotPresent) => Ok(default),
        Err(env::VarError::NotUnicode(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be valid UTF-8 in hosted mode"),
        )),
    }
}

fn parse_hosted_bind(name: &str, value: &str) -> io::Result<SocketAddr> {
    value.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be an explicit IP socket address"),
        )
    })
}

fn socket_binds_overlap(left: SocketAddr, right: SocketAddr) -> bool {
    if left.port() == 0 || right.port() == 0 || left.port() != right.port() {
        return false;
    }
    left.ip() == right.ip()
        || left.ip().is_unspecified()
        || right.ip().is_unspecified()
        || matches!(
            (left.ip(), right.ip()),
            (IpAddr::V4(_), IpAddr::V6(ip)) | (IpAddr::V6(ip), IpAddr::V4(_)) if ip.is_unspecified()
        )
}

async fn serve_gateway(
    gateway: BoundGateway,
    metrics_server: Option<MetricsServerConfig>,
    metrics_registry: Arc<MetricsRegistry>,
    signing_watcher: Option<tokio::task::JoinHandle<()>>,
) -> io::Result<()> {
    if let Some(mut signing_watcher) = signing_watcher {
        tokio::select! {
            result = serve_gateway_listeners(gateway, metrics_server, metrics_registry) => {
                signing_watcher.abort();
                let _ = signing_watcher.await;
                result
            }
            result = &mut signing_watcher => service_signing_watcher_finished(result),
        }
    } else {
        serve_gateway_listeners(gateway, metrics_server, metrics_registry).await
    }
}

async fn serve_gateway_listeners(
    gateway: BoundGateway,
    metrics_server: Option<MetricsServerConfig>,
    metrics_registry: Arc<MetricsRegistry>,
) -> io::Result<()> {
    match (gateway, metrics_server) {
        (BoundGateway::Local { listener, app }, Some(metrics)) => {
            tokio::select! {
                result = axum::serve(listener, app) => listener_finished("public", result),
                result = serve_metrics(metrics, metrics_registry) => {
                    listener_finished("metrics", result)
                }
            }
        }
        (BoundGateway::Local { listener, app }, None) => {
            listener_finished("public", axum::serve(listener, app).await)
        }
        (
            BoundGateway::Hosted {
                public_listener,
                operator_listener,
                apps,
            },
            Some(metrics),
        ) => {
            tokio::select! {
                result = axum::serve(public_listener, apps.public) => {
                    listener_finished("public", result)
                },
                result = axum::serve(operator_listener, apps.operator) => {
                    listener_finished("operator", result)
                },
                result = serve_metrics(metrics, metrics_registry) => {
                    listener_finished("metrics", result)
                }
            }
        }
        (
            BoundGateway::Hosted {
                public_listener,
                operator_listener,
                apps,
            },
            None,
        ) => {
            tokio::select! {
                result = axum::serve(public_listener, apps.public) => {
                    listener_finished("public", result)
                },
                result = axum::serve(operator_listener, apps.operator) => {
                    listener_finished("operator", result)
                }
            }
        }
    }
}

fn service_signing_watcher_finished(result: Result<(), tokio::task::JoinError>) -> io::Result<()> {
    match result {
        Ok(()) => Err(io::Error::other(
            "service-signing watcher exited unexpectedly",
        )),
        Err(error) if error.is_cancelled() => Err(io::Error::other(
            "service-signing watcher was cancelled unexpectedly",
        )),
        Err(error) if error.is_panic() => Err(io::Error::other("service-signing watcher panicked")),
        Err(_) => Err(io::Error::other("service-signing watcher failed")),
    }
}

fn listener_finished(name: &str, result: io::Result<()>) -> io::Result<()> {
    match result {
        Ok(()) => Err(io::Error::other(format!(
            "{name} listener exited unexpectedly"
        ))),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("{name} listener failed: {error}"),
        )),
    }
}

fn public_api_authenticator_from_environment() -> io::Result<PublicApiAuthenticator> {
    let configuration = match env::var("INFERLAB_PUBLIC_API_KEYS") {
        Ok(configuration) => Some(configuration),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "INFERLAB_PUBLIC_API_KEYS must be valid UTF-8",
            ));
        }
    };
    PublicApiAuthenticator::from_configuration(configuration.as_deref())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
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
    verified_signing_key_id: Option<String>,
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
    authenticator: Arc<ControlAuthenticator>,
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

fn parse_control_plane_urls() -> io::Result<Vec<String>> {
    Ok(optional_string_env("INFERLAB_CONTROL_PLANE_URLS")?
        .map(|raw| {
            raw.split(',')
                .map(|url| url.trim().trim_end_matches('/').to_owned())
                .filter(|url| !url.is_empty())
                .collect()
        })
        .unwrap_or_default())
}

const DEFAULT_SERVICE_SIGNING_BUNDLE_POLL_MS: u64 = 100;
const MIN_SERVICE_SIGNING_BUNDLE_POLL_MS: u64 = 25;
const MAX_SERVICE_SIGNING_BUNDLE_POLL_MS: u64 = 60_000;

struct GatewayServiceClientBootstrap {
    client: ControlServiceClient,
    signing_status: Option<ServiceSigningStatus>,
    signing_watcher: Option<ServiceSigningBundleWatcher>,
}

struct ServiceSigningBundleWatcher {
    signer: Arc<ServiceSigner>,
    path: PathBuf,
    poll_interval: Duration,
    expected_cluster_id: String,
    expected_service_id: String,
}

fn gateway_service_client(
    http: Client,
    control_urls: &[String],
    expected_cluster_id: Option<&str>,
) -> io::Result<GatewayServiceClientBootstrap> {
    let service_id = optional_string_env("INFERLAB_GATEWAY_SERVICE_ID")?;
    let credential_id = optional_string_env("INFERLAB_GATEWAY_SERVICE_CREDENTIAL_ID")?;
    let private_key = optional_string_env("INFERLAB_GATEWAY_SERVICE_PRIVATE_KEY_B64")?;
    let targets = optional_string_env("INFERLAB_CONTROL_SERVICE_TARGETS")?;
    let bundle_path = optional_path_from_env_result(
        "INFERLAB_GATEWAY_SERVICE_SIGNING_BUNDLE_PATH",
        env::var("INFERLAB_GATEWAY_SERVICE_SIGNING_BUNDLE_PATH"),
    )?;
    let bundle_poll_ms =
        parse_optional_env::<u64>("INFERLAB_GATEWAY_SERVICE_SIGNING_BUNDLE_POLL_MS")?;
    gateway_service_client_from_configuration(
        http,
        control_urls,
        expected_cluster_id,
        GatewayServiceAuthenticationConfiguration {
            service_id,
            credential_id,
            private_key,
            targets,
            bundle_path,
            bundle_poll_ms,
        },
    )
}

#[derive(Default)]
struct GatewayServiceAuthenticationConfiguration {
    service_id: Option<String>,
    credential_id: Option<String>,
    private_key: Option<String>,
    targets: Option<String>,
    bundle_path: Option<PathBuf>,
    bundle_poll_ms: Option<u64>,
}

fn gateway_service_client_from_configuration(
    http: Client,
    control_urls: &[String],
    expected_cluster_id: Option<&str>,
    configuration: GatewayServiceAuthenticationConfiguration,
) -> io::Result<GatewayServiceClientBootstrap> {
    let GatewayServiceAuthenticationConfiguration {
        service_id,
        credential_id,
        private_key,
        targets,
        bundle_path,
        bundle_poll_ms,
    } = configuration;
    if bundle_path.is_none() && bundle_poll_ms.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_GATEWAY_SERVICE_SIGNING_BUNDLE_POLL_MS requires INFERLAB_GATEWAY_SERVICE_SIGNING_BUNDLE_PATH",
        ));
    }
    if bundle_path.is_some() && (credential_id.is_some() || private_key.is_some()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_GATEWAY_SERVICE_SIGNING_BUNDLE_PATH is mutually exclusive with INFERLAB_GATEWAY_SERVICE_CREDENTIAL_ID and INFERLAB_GATEWAY_SERVICE_PRIVATE_KEY_B64",
        ));
    }
    match (service_id, credential_id, private_key, targets, bundle_path) {
        (None, None, None, None, None) => Ok(GatewayServiceClientBootstrap {
            client: ControlServiceClient::disabled(http),
            signing_status: None,
            signing_watcher: None,
        }),
        (Some(service_id), credential_id, Some(private_key), Some(targets), None) => {
            let expected_cluster_id = expected_cluster_id.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "gateway service authentication requires INFERLAB_CONTROL_PLANE_URLS",
                )
            })?;
            let identity = ServiceSigningIdentity::from_base64_seed_with_credential(
                service_id,
                credential_id.unwrap_or_else(|| LEGACY_CREDENTIAL_ID.to_owned()),
                &private_key,
            )
            .map(Arc::new)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let signer = Arc::new(ServiceSigner::from_static(identity));
            let targets = parse_control_service_targets(&targets)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let client = ControlServiceClient::authenticated(
                http,
                Arc::clone(&signer),
                expected_cluster_id,
                targets,
                control_urls,
            )
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            Ok(GatewayServiceClientBootstrap {
                client,
                signing_status: Some(gateway_service_signing_status(&signer)),
                signing_watcher: None,
            })
        }
        (Some(service_id), None, None, Some(targets), Some(bundle_path)) => {
            if bundle_path.as_os_str().is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "INFERLAB_GATEWAY_SERVICE_SIGNING_BUNDLE_PATH must not be empty",
                ));
            }
            let expected_cluster_id = expected_cluster_id.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "gateway service authentication requires INFERLAB_CONTROL_PLANE_URLS",
                )
            })?;
            let poll_ms = bundle_poll_ms.unwrap_or(DEFAULT_SERVICE_SIGNING_BUNDLE_POLL_MS);
            if !(MIN_SERVICE_SIGNING_BUNDLE_POLL_MS..=MAX_SERVICE_SIGNING_BUNDLE_POLL_MS)
                .contains(&poll_ms)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "INFERLAB_GATEWAY_SERVICE_SIGNING_BUNDLE_POLL_MS must be between {MIN_SERVICE_SIGNING_BUNDLE_POLL_MS} and {MAX_SERVICE_SIGNING_BUNDLE_POLL_MS} milliseconds"
                    ),
                ));
            }
            let bundle =
                VerifiedServiceSigningBundle::load(&bundle_path, expected_cluster_id, &service_id)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let signer = Arc::new(ServiceSigner::from_bundle(bundle));
            let targets = parse_control_service_targets(&targets)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let client = ControlServiceClient::authenticated(
                http,
                Arc::clone(&signer),
                expected_cluster_id,
                targets,
                control_urls,
            )
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            Ok(GatewayServiceClientBootstrap {
                client,
                signing_status: Some(gateway_service_signing_status(&signer)),
                signing_watcher: Some(ServiceSigningBundleWatcher {
                    signer,
                    path: bundle_path,
                    poll_interval: Duration::from_millis(poll_ms),
                    expected_cluster_id: expected_cluster_id.to_owned(),
                    expected_service_id: service_id,
                }),
            })
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_GATEWAY_SERVICE_ID and INFERLAB_CONTROL_SERVICE_TARGETS must be configured with exactly one signing source: INFERLAB_GATEWAY_SERVICE_SIGNING_BUNDLE_PATH, or legacy INFERLAB_GATEWAY_SERVICE_PRIVATE_KEY_B64 with optional INFERLAB_GATEWAY_SERVICE_CREDENTIAL_ID",
        )),
    }
}

fn gateway_service_signing_status(signer: &ServiceSigner) -> ServiceSigningStatus {
    let status = signer.status();
    ServiceSigningStatus {
        mode: status.mode.as_str().to_owned(),
        active_credential_id: status.active_credential_id,
        bundle_generation: status.bundle_generation,
        configured_credential_count: status.configured_credential_count,
        successful_activations: status.successful_activations,
        rejected_reloads: status.rejected_reloads,
        last_error_kind: status
            .last_error_kind
            .map(service_signing_error_kind_name)
            .map(str::to_owned),
    }
}

async fn watch_service_signing_bundle(
    watcher: ServiceSigningBundleWatcher,
    status: SharedControlPlaneStatus,
) {
    let mut interval = tokio::time::interval(watcher.poll_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;
    let mut watch_state = ServiceSigningWatchLoopState::default();
    loop {
        interval.tick().await;
        let observation = service_signing_bundle_observation(&watcher.path);
        run_service_signing_watch_iteration(&mut watch_state, observation, |last_reported_error| {
            reload_service_signing_bundle_with_reported_error(
                &watcher,
                &status,
                last_reported_error,
            )
            .map_err(|error| error.kind())
        });
    }
}

#[derive(Default)]
struct ServiceSigningWatchLoopState {
    last_observation: Option<ServiceSigningBundleObservation>,
    retry_unchanged_source: bool,
    last_reported_error: Option<ServiceSigningErrorKind>,
}

fn run_service_signing_watch_iteration(
    state: &mut ServiceSigningWatchLoopState,
    observation: ServiceSigningBundleObservation,
    reload: impl FnOnce(
        Option<ServiceSigningErrorKind>,
    ) -> Result<ServiceSignerActivationOutcome, ServiceSigningErrorKind>,
) -> bool {
    let observation_changed = state.last_observation.as_ref() != Some(&observation);
    if observation_changed {
        state.last_observation = Some(observation);
        state.retry_unchanged_source = false;
        state.last_reported_error = None;
    } else if !state.retry_unchanged_source {
        return false;
    }

    let result = reload(state.last_reported_error);
    match result {
        Ok(_) => {
            state.retry_unchanged_source = false;
            state.last_reported_error = None;
        }
        Err(kind) => {
            state.retry_unchanged_source = kind == ServiceSigningErrorKind::SourceUnavailable;
            state.last_reported_error = Some(kind);
        }
    }
    true
}

#[cfg(test)]
fn reload_service_signing_bundle(
    watcher: &ServiceSigningBundleWatcher,
    status: &SharedControlPlaneStatus,
) -> Result<ServiceSignerActivationOutcome, ServiceSigningError> {
    reload_service_signing_bundle_with_reported_error(watcher, status, None)
}

fn reload_service_signing_bundle_with_reported_error(
    watcher: &ServiceSigningBundleWatcher,
    status: &SharedControlPlaneStatus,
    last_reported_error: Option<ServiceSigningErrorKind>,
) -> Result<ServiceSignerActivationOutcome, ServiceSigningError> {
    let candidate = match VerifiedServiceSigningBundle::load(
        &watcher.path,
        &watcher.expected_cluster_id,
        &watcher.expected_service_id,
    ) {
        Ok(candidate) => candidate,
        Err(error) => {
            if last_reported_error != Some(error.kind()) {
                watcher.signer.record_rejection(error.kind());
                publish_gateway_service_signing_status(status, &watcher.signer);
                warn!(
                    reason = service_signing_error_kind_name(error.kind()),
                    "gateway retained the last-known-good service signer"
                );
            }
            return Err(error);
        }
    };
    // File custody and distribution are operator preconditions. The shared signer still enforces
    // whole-bundle identity binding, strict generation ordering, fork rejection and
    // last-known-good retention atomically.
    let outcome = watcher.signer.activate_bundle(candidate, |_| true);
    match outcome {
        Ok(ServiceSignerActivationOutcome::Activated) => {
            let snapshot = watcher.signer.snapshot();
            publish_gateway_service_signing_status(status, &watcher.signer);
            info!(
                generation = ?snapshot.bundle_generation(),
                credential_id = snapshot.credential_id(),
                "gateway activated a service-signing bundle"
            );
        }
        Ok(ServiceSignerActivationOutcome::Unchanged) => {
            // An exact-current candidate is also a successful source recovery. The shared signer
            // clears its previous error and the operator view must publish that transition.
            publish_gateway_service_signing_status(status, &watcher.signer);
        }
        Err(ref error) => {
            publish_gateway_service_signing_status(status, &watcher.signer);
            warn!(
                reason = service_signing_error_kind_name(error.kind()),
                "gateway retained the last-known-good service signer"
            );
        }
    }
    outcome
}

fn publish_gateway_service_signing_status(
    status: &SharedControlPlaneStatus,
    signer: &ServiceSigner,
) {
    let snapshot = signer.snapshot();
    let mut current = status
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    current.service_credential_id = Some(snapshot.credential_id().to_owned());
    current.service_signing = Some(gateway_service_signing_status(signer));
}

fn service_signing_error_kind_name(kind: ServiceSigningErrorKind) -> &'static str {
    match kind {
        ServiceSigningErrorKind::SourceUnavailable => "source_unavailable",
        ServiceSigningErrorKind::NotRegularFile => "not_regular_file",
        ServiceSigningErrorKind::UnsafePermissions => "unsafe_permissions",
        ServiceSigningErrorKind::BundleTooLarge => "bundle_too_large",
        ServiceSigningErrorKind::InvalidJson => "invalid_json",
        ServiceSigningErrorKind::InvalidSchema => "invalid_schema",
        ServiceSigningErrorKind::InvalidClusterId => "invalid_cluster_id",
        ServiceSigningErrorKind::InvalidServiceId => "invalid_service_id",
        ServiceSigningErrorKind::InvalidGeneration => "invalid_generation",
        ServiceSigningErrorKind::InvalidCredentialSet => "invalid_credential_set",
        ServiceSigningErrorKind::InvalidPrivateKey => "invalid_private_key",
        ServiceSigningErrorKind::UnknownActiveCredential => "unknown_active_credential",
        ServiceSigningErrorKind::StaticSigner => "static_signer",
        ServiceSigningErrorKind::ClusterMismatch => "cluster_mismatch",
        ServiceSigningErrorKind::ServiceMismatch => "service_mismatch",
        ServiceSigningErrorKind::StaleGeneration => "stale_generation",
        ServiceSigningErrorKind::GenerationFork => "generation_fork",
        ServiceSigningErrorKind::CandidateRejected => "candidate_rejected",
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ServiceSigningBundleObservation {
    Present(ServiceSigningBundleFileStamp),
    Unavailable(io::ErrorKind),
}

#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct ServiceSigningBundleFileStamp {
    device: u64,
    inode: u64,
    mode: u32,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[cfg(not(unix))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct ServiceSigningBundleFileStamp {
    size: u64,
    modified: Option<std::time::SystemTime>,
    readonly: bool,
}

#[cfg(unix)]
fn service_signing_bundle_observation(path: &std::path::Path) -> ServiceSigningBundleObservation {
    use std::os::unix::fs::MetadataExt as _;

    match std::fs::metadata(path) {
        Ok(metadata) => ServiceSigningBundleObservation::Present(ServiceSigningBundleFileStamp {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }),
        Err(error) => ServiceSigningBundleObservation::Unavailable(error.kind()),
    }
}

#[cfg(not(unix))]
fn service_signing_bundle_observation(path: &std::path::Path) -> ServiceSigningBundleObservation {
    match std::fs::metadata(path) {
        Ok(metadata) => ServiceSigningBundleObservation::Present(ServiceSigningBundleFileStamp {
            size: metadata.len(),
            modified: metadata.modified().ok(),
            readonly: metadata.permissions().readonly(),
        }),
        Err(error) => ServiceSigningBundleObservation::Unavailable(error.kind()),
    }
}

async fn wait_for_control_configuration(
    client: &ControlServiceClient,
    urls: &[String],
    maximum_wait: Duration,
    expected_cluster_id: &str,
    authenticator: &ControlAuthenticator,
) -> io::Result<VerifiedControlConfiguration> {
    let deadline = Instant::now() + maximum_wait;
    let mut last_mismatch = None;
    let mut last_authentication_failure = None;
    loop {
        let fetched =
            fetch_control_configuration(client, urls, expected_cluster_id, authenticator).await;
        if let Some(mismatch) = fetched.mismatches.last() {
            last_mismatch = Some(mismatch.clone());
        }
        if let Some(failure) = fetched.authentication_failures.last() {
            last_authentication_failure = Some(failure.clone());
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
            let authentication = last_authentication_failure
                .map(|failure| {
                    format!(
                        "; rejected control authentication from {}{}: {}",
                        failure.source_url,
                        failure
                            .key_id
                            .map(|key_id| format!(" using key '{key_id}'"))
                            .unwrap_or_default(),
                        failure.error
                    )
                })
                .unwrap_or_default();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "no authenticated committed configuration for control cluster '{expected_cluster_id}' became available within {} ms{authentication}{mismatch}",
                    maximum_wait.as_millis(),
                ),
            ));
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn fetch_control_configuration(
    client: &ControlServiceClient,
    urls: &[String],
    expected_cluster_id: &str,
    authenticator: &ControlAuthenticator,
) -> ControlFetchResult {
    let mut fetched = ControlFetchResult::default();
    for url in urls {
        let response = client.get_configuration(url).await;
        if let Ok(response) = response
            && response.status().is_success()
            && let Ok(configuration) = response.json::<CommittedRoutingConfiguration>().await
        {
            let signing_key_id = match authenticator.verify(&configuration) {
                Ok(key_id) => key_id,
                Err(error) => {
                    fetched.authentication_failures.push(AuthenticationFailure {
                        source_url: url.clone(),
                        key_id: configuration
                            .authentication
                            .as_ref()
                            .map(|authentication| authentication.key_id.clone()),
                        error: error.to_string(),
                    });
                    continue;
                }
            };
            if configuration.cluster_id == expected_cluster_id {
                fetched.configuration = Some(VerifiedControlConfiguration {
                    source_url: url.clone(),
                    committed: configuration,
                    signing_key_id,
                });
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

#[derive(Clone, Debug)]
struct AuthenticationFailure {
    source_url: String,
    key_id: Option<String>,
    error: String,
}

#[derive(Debug)]
struct VerifiedControlConfiguration {
    source_url: String,
    committed: CommittedRoutingConfiguration,
    signing_key_id: Option<String>,
}

#[derive(Debug, Default)]
struct ControlFetchResult {
    configuration: Option<VerifiedControlConfiguration>,
    mismatches: Vec<ClusterMismatch>,
    authentication_failures: Vec<AuthenticationFailure>,
}

async fn bootstrap_control_configuration(
    client: &ControlServiceClient,
    urls: &[String],
    maximum_wait: Duration,
    snapshot_store: Option<&RoutingSnapshotStore>,
    freshness_policy: SnapshotFreshnessPolicy,
    expected_cluster_id: &str,
    authenticator: &ControlAuthenticator,
) -> io::Result<InitialControlConfiguration> {
    let mut snapshot_error = None;
    let persisted = snapshot_store.and_then(|store| match store.load() {
        Ok(snapshot) => Some(snapshot),
        Err(error) => {
            snapshot_error = Some(format!("{}: {error}", store.path().display()));
            None
        }
    });
    let live = wait_for_control_configuration(
        client,
        urls,
        maximum_wait,
        expected_cluster_id,
        authenticator,
    )
    .await;
    match live {
        Ok(verified) => {
            let VerifiedControlConfiguration {
                source_url,
                committed,
                signing_key_id,
            } = verified;
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
                    authenticator,
                );
            }
            if let Some(snapshot) = persisted.as_ref() {
                let disk_signing_key_id = match authenticator.verify(&snapshot.committed) {
                    Ok(key_id) => Some(key_id),
                    Err(error) => {
                        warn!(
                            snapshot_path = snapshot_store.map(|store| store.path().display().to_string()),
                            %error,
                            "gateway ignored a durable snapshot that failed control authentication because authenticated live control is available"
                        );
                        None
                    }
                };
                if disk_signing_key_id.is_none() && authenticator.required() {
                    // Authenticated live control is authoritative and will repair the file below.
                } else if snapshot.committed.cluster_id != expected_cluster_id {
                    warn!(
                        disk_cluster_id = %snapshot.committed.cluster_id,
                        %expected_cluster_id,
                        "gateway ignored a durable snapshot from a different control cluster because expected live control is available"
                    );
                } else if authenticator.key_transition(
                    disk_signing_key_id
                        .as_ref()
                        .and_then(|key_id| key_id.as_deref()),
                    signing_key_id.as_deref(),
                ) == SigningKeyTransition::Downgrade
                {
                    let freshness = snapshot_freshness(snapshot, freshness_policy).map_err(
                        |error| {
                            io::Error::new(
                                error.kind(),
                                format!(
                                    "live control signing key '{}' is lower priority than durable key '{}', but the durable snapshot is not eligible for fallback: {error}",
                                    signing_key_id.as_deref().unwrap_or("none"),
                                    disk_signing_key_id
                                        .as_ref()
                                        .and_then(|key_id| key_id.as_deref())
                                        .unwrap_or("none")
                                ),
                            )
                        },
                    )?;
                    warn!(
                        live_signing_key_id = signing_key_id.as_deref(),
                        disk_signing_key_id = disk_signing_key_id
                            .as_ref()
                            .and_then(|key_id| key_id.as_deref()),
                        "gateway refused to downgrade below its durable control signing key"
                    );
                    return Ok(initial_from_disk(
                        snapshot.clone(),
                        freshness,
                        disk_signing_key_id.flatten(),
                    ));
                } else if snapshot.committed.revision > committed.revision {
                    let freshness = snapshot_freshness(snapshot, freshness_policy).map_err(
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
                    return Ok(initial_from_disk(
                        snapshot.clone(),
                        freshness,
                        disk_signing_key_id.flatten(),
                    ));
                }
                if snapshot.committed.cluster_id == expected_cluster_id
                    && snapshot.committed.revision == committed.revision
                    && !same_routing_payload(&snapshot.committed, &committed)
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
                verified_signing_key_id: signing_key_id,
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
            authenticator,
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
    authenticator: &ControlAuthenticator,
) -> io::Result<InitialControlConfiguration> {
    match persisted {
        Some(snapshot) => {
            let signing_key_id = authenticator.verify(&snapshot.committed).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "control-plane bootstrap failed ({live_error}); durable routing snapshot is not eligible for fallback: control authentication failed: {error}"
                    ),
                )
            })?;
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
            Ok(initial_from_disk(snapshot, freshness, signing_key_id))
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
    verified_signing_key_id: Option<String>,
) -> InitialControlConfiguration {
    let saved_at_ms = snapshot.saved_at_ms;
    InitialControlConfiguration {
        source_url: None,
        committed: snapshot.committed,
        verified_signing_key_id,
        bootstrap_source: BootstrapSource::Disk,
        bootstrap_snapshot_age_ms: Some(freshness.observed_age_ms),
        persisted_at_ms: Some(saved_at_ms),
        persisted_expires_at_ms: freshness.expires_at_ms,
    }
}

async fn watch_control_plane(
    client: ControlServiceClient,
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
        authenticator,
    } = config;
    loop {
        sleep(poll_interval).await;
        let fetched =
            fetch_control_configuration(&client, &urls, &expected_cluster_id, &authenticator).await;
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
        if !fetched.authentication_failures.is_empty() {
            let last = fetched
                .authentication_failures
                .last()
                .expect("non-empty authentication failures");
            warn!(
                key_id = last.key_id.as_deref(),
                source_url = %last.source_url,
                error = %last.error,
                rejected = fetched.authentication_failures.len(),
                "gateway rejected control responses that failed authentication"
            );
            let mut current = status
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            current.last_rejected_signing_key_id = last.key_id.clone();
            current.signature_rejections = current.signature_rejections.saturating_add(
                u64::try_from(fetched.authentication_failures.len()).unwrap_or(u64::MAX),
            );
            current.last_authentication_error = Some(last.error.clone());
        }
        let Some(verified) = fetched.configuration else {
            let mut current = status
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            current.last_error = if let Some(last) = fetched.authentication_failures.last() {
                Some(format!(
                    "control authentication failed for response from {}{}: {}",
                    last.source_url,
                    last.key_id
                        .as_deref()
                        .map(|key_id| format!(" using key '{key_id}'"))
                        .unwrap_or_default(),
                    last.error
                ))
            } else if let Some(last) = fetched.mismatches.last() {
                Some(format!(
                    "control cluster identity mismatch: expected '{expected_cluster_id}', observed '{}' from {}",
                    last.cluster_id, last.source_url
                ))
            } else {
                Some("no control-plane node returned a committed configuration".to_owned())
            };
            continue;
        };
        let VerifiedControlConfiguration {
            source_url,
            committed,
            signing_key_id,
        } = verified;
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
        let current_snapshot = routing
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let current_revision = current_snapshot.control_revision.unwrap_or(0);
        let key_transition = authenticator.key_transition(
            current_snapshot.control_signing_key_id.as_deref(),
            signing_key_id.as_deref(),
        );
        if key_transition == SigningKeyTransition::Downgrade {
            warn!(
                current_signing_key_id = current_snapshot.control_signing_key_id.as_deref(),
                observed_signing_key_id = signing_key_id.as_deref(),
                %source_url,
                "gateway rejected a control signing-key downgrade"
            );
            let mut current = status
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            current.source_url = Some(source_url);
            current.signing_key_downgrade_rejections =
                current.signing_key_downgrade_rejections.saturating_add(1);
            current.signature_verifications = current.signature_verifications.saturating_add(1);
            current.last_error = Some(format!(
                "ignored control signing-key downgrade from '{}' to '{}'",
                current_snapshot
                    .control_signing_key_id
                    .as_deref()
                    .unwrap_or("none"),
                signing_key_id.as_deref().unwrap_or("none")
            ));
            continue;
        }
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
        if committed.revision == current_revision
            && !same_routing_payload(&committed, &applied_configuration)
        {
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
        let is_new_revision = committed.revision > current_revision;
        let is_signing_key_rotation = key_transition == SigningKeyTransition::Upgrade
            && committed.revision == current_revision
            && signing_key_id != current_snapshot.control_signing_key_id;
        if is_new_revision || is_signing_key_rotation {
            let rebuilt = if is_new_revision {
                committed_pool_input(&committed)
                    .and_then(|(registrations, policy)| build_pool(registrations, policy, template))
                    .map(Arc::new)
            } else {
                Ok(Arc::clone(&current_snapshot.workers))
            };
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
                        RoutingSnapshot::committed_authenticated(
                            pool,
                            &committed.cluster_id,
                            signing_key_id.clone(),
                            committed.revision,
                            committed.term,
                        );
                    applied_configuration = committed.clone();
                    info!(
                        revision = committed.revision,
                        term = committed.term,
                        signing_key_id = signing_key_id.as_deref(),
                        signing_key_rotation = is_signing_key_rotation,
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
        current.active_signing_key_id = signing_key_id;
        if current.active_signing_key_id.is_some() {
            current.signature_verifications = current.signature_verifications.saturating_add(1);
        }
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

fn optional_path_from_env_result(
    name: &str,
    value: Result<String, env::VarError>,
) -> io::Result<Option<PathBuf>> {
    optional_string_from_env_result(name, value).map(|value| value.map(PathBuf::from))
}

fn optional_string_env(name: &str) -> io::Result<Option<String>> {
    optional_string_from_env_result(name, env::var(name))
}

fn control_cluster_id_from_env_result(value: Result<String, env::VarError>) -> io::Result<String> {
    optional_string_from_env_result("INFERLAB_CONTROL_CLUSTER_ID", value)
        .map(|value| value.unwrap_or_else(|| DEFAULT_CONTROL_CLUSTER_ID.to_owned()))
}

fn optional_string_from_env_result(
    name: &str,
    value: Result<String, env::VarError>,
) -> io::Result<Option<String>> {
    match value {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must contain valid Unicode"),
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

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        fs, io,
        path::PathBuf,
        sync::{Arc, RwLock},
        time::Duration,
    };

    use axum::{Json, Router, routing::get};
    use gateway::ControlPlaneStatus;
    use gateway::control_authentication::ControlAuthenticator;
    use gateway::routing_snapshot_store::{
        CommittedRoutingConfiguration, DEFAULT_CONTROL_CLUSTER_ID, StoredRoutingConfiguration,
        StoredWorkerConfiguration,
    };
    use gateway::service_client::ControlServiceClient;
    use service_auth::{
        SERVICE_SIGNING_BUNDLE_SCHEMA, ServiceSignerActivationOutcome, ServiceSigningErrorKind,
    };
    use tokio::net::TcpListener;

    use super::{
        GatewayServiceAuthenticationConfiguration, ServiceSigningWatchLoopState,
        control_cluster_id_from_env_result, fetch_control_configuration,
        gateway_service_client_from_configuration, now_ms, optional_path_from_env_result,
        optional_string_from_env_result, parse_workers, reload_service_signing_bundle,
        run_service_signing_watch_iteration, service_signing_bundle_observation,
        service_signing_watcher_finished, socket_binds_overlap, wait_for_control_configuration,
    };

    const SERVICE_SIGNING_SEED: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";
    const SERVICE_SIGNING_SEED_B: &str = "oRHYnSe9L9fS2eMjpnvZPZ7tg09poPfRXpAMlzsqHkg=";

    fn encoded_signing_bundle(generation: u64, active: &str) -> Vec<u8> {
        format!(
            r#"{{"schema":"{SERVICE_SIGNING_BUNDLE_SCHEMA}","cluster_id":"inferlab-primary","generation":{generation},"service_id":"gateway-primary","active_credential_id":"{active}","credentials":[{{"credential_id":"key-a","private_key_base64":"{SERVICE_SIGNING_SEED}"}},{{"credential_id":"key-b","private_key_base64":"{SERVICE_SIGNING_SEED_B}"}}]}}"#
        )
        .into_bytes()
    }

    fn write_signing_bundle(path: &std::path::Path, bytes: &[u8]) {
        fs::write(path, bytes).expect("write signing bundle");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .expect("secure signing bundle permissions");
        }
    }

    #[cfg(unix)]
    #[test]
    fn watched_gateway_signing_path_rejects_non_unicode_environment_values() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

        let error = optional_path_from_env_result(
            "INFERLAB_GATEWAY_SERVICE_SIGNING_BUNDLE_PATH",
            Err(std::env::VarError::NotUnicode(OsString::from_vec(vec![
                0xff,
            ]))),
        )
        .expect_err("non-Unicode bundle path must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("valid Unicode"));
    }

    #[cfg(unix)]
    #[test]
    fn gateway_service_identity_environment_rejects_non_unicode_values() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

        for name in [
            "INFERLAB_GATEWAY_SERVICE_ID",
            "INFERLAB_GATEWAY_SERVICE_CREDENTIAL_ID",
            "INFERLAB_GATEWAY_SERVICE_PRIVATE_KEY_B64",
            "INFERLAB_CONTROL_SERVICE_TARGETS",
            "INFERLAB_CONTROL_PLANE_URLS",
            "INFERLAB_CONTROL_TRUSTED_KEYS",
            "INFERLAB_CONTROL_REVOKED_KEY_IDS",
        ] {
            let error = optional_string_from_env_result(
                name,
                Err(std::env::VarError::NotUnicode(OsString::from_vec(vec![
                    0xff,
                ]))),
            )
            .expect_err("non-Unicode security configuration must fail closed");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains(name));
            assert!(error.to_string().contains("valid Unicode"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn control_cluster_id_defaults_only_when_absent_and_rejects_non_unicode() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

        assert_eq!(
            control_cluster_id_from_env_result(Err(std::env::VarError::NotPresent))
                .expect("absent cluster ID"),
            DEFAULT_CONTROL_CLUSTER_ID
        );
        assert_eq!(
            control_cluster_id_from_env_result(Ok("inferlab-primary".to_owned()))
                .expect("configured cluster ID"),
            "inferlab-primary"
        );
        let error = control_cluster_id_from_env_result(Err(std::env::VarError::NotUnicode(
            OsString::from_vec(vec![0xff]),
        )))
        .expect_err("non-Unicode cluster ID must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("INFERLAB_CONTROL_CLUSTER_ID"));
        assert!(error.to_string().contains("valid Unicode"));
    }

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
            authentication: None,
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

    #[test]
    fn hosted_bind_collision_check_is_conservative_for_wildcards() {
        let address = |value: &str| value.parse().expect("socket address");
        assert!(socket_binds_overlap(
            address("127.0.0.1:8080"),
            address("127.0.0.1:8080")
        ));
        assert!(socket_binds_overlap(
            address("0.0.0.0:8080"),
            address("127.0.0.1:8080")
        ));
        assert!(!socket_binds_overlap(
            address("127.0.0.1:8080"),
            address("127.0.0.1:8081")
        ));
        assert!(!socket_binds_overlap(
            address("127.0.0.1:0"),
            address("127.0.0.1:0")
        ));
    }

    #[test]
    fn legacy_static_gateway_signing_configuration_remains_compatible() {
        let urls = vec!["http://127.0.0.1:9910".to_owned()];
        let bootstrap = gateway_service_client_from_configuration(
            reqwest::Client::new(),
            &urls,
            Some("inferlab-primary"),
            GatewayServiceAuthenticationConfiguration {
                service_id: Some("gateway-primary".to_owned()),
                private_key: Some(SERVICE_SIGNING_SEED.to_owned()),
                targets: Some("control-a=http://127.0.0.1:9910".to_owned()),
                ..GatewayServiceAuthenticationConfiguration::default()
            },
        )
        .expect("legacy static signer");

        assert!(bootstrap.client.authentication_enabled());
        assert_eq!(
            bootstrap.client.service_id().as_deref(),
            Some("gateway-primary")
        );
        assert_eq!(
            bootstrap.client.credential_id().as_deref(),
            Some(service_auth::LEGACY_CREDENTIAL_ID)
        );
        let status = bootstrap.signing_status.expect("signing status");
        assert_eq!(status.mode, "static");
        assert_eq!(status.bundle_generation, None);
        assert!(bootstrap.signing_watcher.is_none());
    }

    #[test]
    fn watched_gateway_signing_rejects_legacy_credentials_and_orphan_polling() {
        let urls = vec!["http://127.0.0.1:9910".to_owned()];
        let mixed = gateway_service_client_from_configuration(
            reqwest::Client::new(),
            &urls,
            Some("inferlab-primary"),
            GatewayServiceAuthenticationConfiguration {
                service_id: Some("gateway-primary".to_owned()),
                credential_id: Some("key-a".to_owned()),
                private_key: Some(SERVICE_SIGNING_SEED.to_owned()),
                targets: Some("control-a=http://127.0.0.1:9910".to_owned()),
                bundle_path: Some(PathBuf::from("signer.json")),
                bundle_poll_ms: Some(100),
            },
        )
        .err()
        .expect("mixed signing modes must fail");
        assert!(mixed.to_string().contains("mutually exclusive"));

        let orphan = gateway_service_client_from_configuration(
            reqwest::Client::new(),
            &urls,
            Some("inferlab-primary"),
            GatewayServiceAuthenticationConfiguration {
                bundle_poll_ms: Some(100),
                ..GatewayServiceAuthenticationConfiguration::default()
            },
        )
        .err()
        .expect("orphan polling must fail");
        assert!(orphan.to_string().contains("requires"));
    }

    #[test]
    fn watched_gateway_signing_poll_interval_is_bounded_before_file_access() {
        let urls = vec!["http://127.0.0.1:9910".to_owned()];
        for poll_ms in [24, 60_001] {
            let error = gateway_service_client_from_configuration(
                reqwest::Client::new(),
                &urls,
                Some("inferlab-primary"),
                GatewayServiceAuthenticationConfiguration {
                    service_id: Some("gateway-primary".to_owned()),
                    targets: Some("control-a=http://127.0.0.1:9910".to_owned()),
                    bundle_path: Some(PathBuf::from("does-not-need-to-exist.json")),
                    bundle_poll_ms: Some(poll_ms),
                    ..GatewayServiceAuthenticationConfiguration::default()
                },
            )
            .err()
            .expect("out-of-range polling must fail");
            assert!(error.to_string().contains("between 25 and 60000"));
        }
    }

    #[test]
    fn signing_watch_loop_retries_transient_source_race_but_dedupes_deterministic_input() {
        let path = std::env::temp_dir().join(format!(
            "inferlab-gateway-signing-observation-{}-{}.json",
            std::process::id(),
            now_ms()
        ));
        write_signing_bundle(&path, &encoded_signing_bundle(1, "key-a"));
        let observation = service_signing_bundle_observation(&path);

        let transient_attempts = Cell::new(0_u32);
        let mut transient = ServiceSigningWatchLoopState::default();
        assert!(run_service_signing_watch_iteration(
            &mut transient,
            observation.clone(),
            |last_reported| {
                transient_attempts.set(transient_attempts.get() + 1);
                assert_eq!(last_reported, None);
                Err(ServiceSigningErrorKind::SourceUnavailable)
            }
        ));
        assert!(run_service_signing_watch_iteration(
            &mut transient,
            observation.clone(),
            |last_reported| {
                transient_attempts.set(transient_attempts.get() + 1);
                assert_eq!(
                    last_reported,
                    Some(ServiceSigningErrorKind::SourceUnavailable)
                );
                Ok(ServiceSignerActivationOutcome::Unchanged)
            }
        ));
        assert!(!run_service_signing_watch_iteration(
            &mut transient,
            observation.clone(),
            |_| {
                transient_attempts.set(transient_attempts.get() + 1);
                Ok(ServiceSignerActivationOutcome::Unchanged)
            }
        ));
        assert_eq!(transient_attempts.get(), 2);

        let deterministic_attempts = Cell::new(0_u32);
        let mut deterministic = ServiceSigningWatchLoopState::default();
        assert!(run_service_signing_watch_iteration(
            &mut deterministic,
            observation.clone(),
            |_| {
                deterministic_attempts.set(deterministic_attempts.get() + 1);
                Err(ServiceSigningErrorKind::InvalidJson)
            }
        ));
        assert!(!run_service_signing_watch_iteration(
            &mut deterministic,
            observation,
            |_| {
                deterministic_attempts.set(deterministic_attempts.get() + 1);
                Err(ServiceSigningErrorKind::InvalidJson)
            }
        ));
        assert_eq!(deterministic_attempts.get(), 1);

        fs::remove_file(path).expect("cleanup signing observation");
    }

    #[tokio::test]
    async fn signing_watcher_supervisor_fails_on_completion_and_join_error() {
        let completed = tokio::spawn(async {});
        let completion = service_signing_watcher_finished(completed.await)
            .expect_err("unexpected completion must fail");
        assert!(completion.to_string().contains("exited unexpectedly"));

        let cancelled = tokio::spawn(std::future::pending::<()>());
        cancelled.abort();
        let cancellation = service_signing_watcher_finished(cancelled.await)
            .expect_err("join cancellation must fail");
        assert!(cancellation.to_string().contains("cancelled unexpectedly"));
    }

    #[test]
    fn watched_gateway_signing_rejects_an_invalid_initial_bundle() {
        let path = std::env::temp_dir().join(format!(
            "inferlab-gateway-signing-invalid-{}-{}.json",
            std::process::id(),
            now_ms()
        ));
        write_signing_bundle(&path, b"{not-json");
        let urls = vec!["http://127.0.0.1:9910".to_owned()];
        let error = gateway_service_client_from_configuration(
            reqwest::Client::new(),
            &urls,
            Some("inferlab-primary"),
            GatewayServiceAuthenticationConfiguration {
                service_id: Some("gateway-primary".to_owned()),
                targets: Some("control-a=http://127.0.0.1:9910".to_owned()),
                bundle_path: Some(path.clone()),
                ..GatewayServiceAuthenticationConfiguration::default()
            },
        )
        .err()
        .expect("invalid initial bundle must fail");
        let message = error.to_string();
        assert!(message.contains("not exact valid JSON"));
        assert!(!message.contains("{not-json"));
        assert!(!message.contains(&path.display().to_string()));
        fs::remove_file(path).expect("cleanup invalid signing bundle");
    }

    #[test]
    fn watched_gateway_signing_activates_higher_generation_and_retains_lkg_on_rejection() {
        let path = std::env::temp_dir().join(format!(
            "inferlab-gateway-signing-{}-{}.json",
            std::process::id(),
            now_ms()
        ));
        write_signing_bundle(&path, &encoded_signing_bundle(1, "key-a"));
        let urls = vec!["http://127.0.0.1:9910".to_owned()];
        let bootstrap = gateway_service_client_from_configuration(
            reqwest::Client::new(),
            &urls,
            Some("inferlab-primary"),
            GatewayServiceAuthenticationConfiguration {
                service_id: Some("gateway-primary".to_owned()),
                targets: Some("control-a=http://127.0.0.1:9910".to_owned()),
                bundle_path: Some(path.clone()),
                ..GatewayServiceAuthenticationConfiguration::default()
            },
        )
        .expect("watched signer bootstrap");
        assert_eq!(bootstrap.client.credential_id().as_deref(), Some("key-a"));
        let watcher = bootstrap.signing_watcher.expect("watcher");
        assert_eq!(watcher.poll_interval, Duration::from_millis(100));
        let signer = Arc::clone(&watcher.signer);
        let status = Arc::new(RwLock::new(ControlPlaneStatus {
            service_authentication_enabled: true,
            service_id: Some("gateway-primary".to_owned()),
            service_credential_id: Some("key-a".to_owned()),
            service_signing: bootstrap.signing_status,
            ..ControlPlaneStatus::default()
        }));

        write_signing_bundle(&path, &encoded_signing_bundle(2, "key-b"));
        reload_service_signing_bundle(&watcher, &status).expect("activate key b");
        assert_eq!(signer.snapshot().bundle_generation(), Some(2));
        assert_eq!(signer.snapshot().credential_id(), "key-b");
        {
            let current = status.read().expect("status");
            let signing = current.service_signing.as_ref().expect("signing status");
            assert_eq!(current.service_credential_id.as_deref(), Some("key-b"));
            assert_eq!(signing.successful_activations, 1);
            assert_eq!(signing.rejected_reloads, 0);
            assert_eq!(signing.last_error_kind, None);
        }

        write_signing_bundle(&path, b"{not-json");
        reload_service_signing_bundle(&watcher, &status).expect_err("invalid candidate");
        assert_eq!(signer.snapshot().bundle_generation(), Some(2));
        assert_eq!(signer.snapshot().credential_id(), "key-b");
        {
            let current = status.read().expect("status");
            let signing = current.service_signing.as_ref().expect("signing status");
            assert_eq!(signing.rejected_reloads, 1);
            assert_eq!(signing.last_error_kind.as_deref(), Some("invalid_json"));
        }

        write_signing_bundle(&path, &encoded_signing_bundle(2, "key-b"));
        assert_eq!(
            reload_service_signing_bundle(&watcher, &status).expect("exact current recovery"),
            ServiceSignerActivationOutcome::Unchanged
        );
        assert_eq!(signer.snapshot().bundle_generation(), Some(2));
        assert_eq!(signer.snapshot().credential_id(), "key-b");
        {
            let current = status.read().expect("status");
            let signing = current.service_signing.as_ref().expect("signing status");
            assert_eq!(signing.successful_activations, 1);
            assert_eq!(signing.rejected_reloads, 1);
            assert_eq!(signing.last_error_kind, None);
        }

        write_signing_bundle(&path, &encoded_signing_bundle(1, "key-a"));
        reload_service_signing_bundle(&watcher, &status).expect_err("rollback candidate");
        assert_eq!(signer.snapshot().bundle_generation(), Some(2));
        assert_eq!(signer.snapshot().credential_id(), "key-b");
        {
            let current = status.read().expect("status");
            let signing = current.service_signing.as_ref().expect("signing status");
            assert_eq!(signing.rejected_reloads, 2);
            assert_eq!(signing.last_error_kind.as_deref(), Some("stale_generation"));
        }

        write_signing_bundle(&path, &encoded_signing_bundle(2, "key-a"));
        reload_service_signing_bundle(&watcher, &status).expect_err("fork candidate");
        assert_eq!(signer.snapshot().bundle_generation(), Some(2));
        assert_eq!(signer.snapshot().credential_id(), "key-b");
        {
            let current = status.read().expect("status");
            let signing = current.service_signing.as_ref().expect("signing status");
            assert_eq!(signing.rejected_reloads, 3);
            assert_eq!(signing.last_error_kind.as_deref(), Some("generation_fork"));
        }

        fs::remove_file(path).expect("cleanup signing bundle");
    }

    #[tokio::test]
    async fn fetch_skips_a_foreign_cluster_and_uses_the_expected_cluster() {
        let foreign = spawn_control(committed("inferlab-foreign")).await;
        let primary = spawn_control(committed("inferlab-primary")).await;

        let fetched = fetch_control_configuration(
            &ControlServiceClient::disabled(reqwest::Client::new()),
            &[foreign.clone(), primary.clone()],
            "inferlab-primary",
            &ControlAuthenticator::Disabled,
        )
        .await;

        let verified = fetched.configuration.expect("expected cluster result");
        assert_eq!(verified.source_url, primary);
        assert_eq!(verified.committed.cluster_id, "inferlab-primary");
        assert_eq!(fetched.mismatches.len(), 1);
        assert_eq!(fetched.mismatches[0].source_url, foreign);
        assert_eq!(fetched.mismatches[0].cluster_id, "inferlab-foreign");
    }

    #[tokio::test]
    async fn bootstrap_wait_reports_the_observed_foreign_cluster() {
        let foreign = spawn_control(committed("inferlab-foreign")).await;

        let error = wait_for_control_configuration(
            &ControlServiceClient::disabled(reqwest::Client::new()),
            &[foreign],
            Duration::from_millis(20),
            "inferlab-primary",
            &ControlAuthenticator::Disabled,
        )
        .await
        .expect_err("foreign cluster must not bootstrap");

        assert!(error.to_string().contains("inferlab-primary"));
        assert!(error.to_string().contains("inferlab-foreign"));
    }
}
