use std::{env, future::Future, io, path::PathBuf, sync::Arc, time::Duration};

use control_auth::{SigningIdentity, TrustedWriterKeyRing};
use control_plane::{
    ControlMetrics, NodeConfig, Peer, RaftNode, ServiceAuthorizer, WriteAuthorizer,
    app_with_authentication,
    model::DEFAULT_CLUSTER_ID,
    service_trust::{
        RemoteServiceTrustConfig, RemoteServiceTrustTlsConfig,
        RemoteServiceTrustTlsIdentityWatcher, RemoteServiceTrustWatcher,
        ServiceTrustDistributionMode, ServiceTrustWatcher,
        bootstrap_remote_signed_service_trust_with_signer,
        bootstrap_signed_service_trust_with_signer, select_service_trust_distribution_mode,
    },
};
use observability::{
    HttpMetrics, MetricsRegistry, MetricsServerConfig, Service, init_tracing, serve_metrics,
};
use service_auth::{
    LEGACY_CREDENTIAL_ID, ServiceSigner, ServiceSignerActivationOutcome, ServiceSigningErrorKind,
    ServiceSigningIdentity, ServiceTrustReceiverValidityConfig, TrustedServiceKeyRing,
    TrustedServiceTrustRootKeyRing, VerifiedServiceSigningBundle,
};
use tokio::{
    net::TcpListener,
    task::{JoinError, JoinHandle},
};
use tracing::{info, warn};

const DEFAULT_SERVICE_TRUST_MAX_POLICY_LIFETIME_MS: u64 = 86_400_000;
const MIN_SERVICE_TRUST_MAX_POLICY_LIFETIME_MS: u64 = 250;
const MAX_SERVICE_TRUST_MAX_POLICY_LIFETIME_MS: u64 = 604_800_000;
const DEFAULT_SERVICE_TRUST_POLICY_MAX_FUTURE_SKEW_MS: u64 = 5_000;
const MAX_SERVICE_TRUST_POLICY_MAX_FUTURE_SKEW_MS: u64 = 300_000;
const DEFAULT_SERVICE_SIGNING_BUNDLE_POLL_MS: u64 = 100;
const MIN_SERVICE_SIGNING_BUNDLE_POLL_MS: u64 = 25;
const MAX_SERVICE_SIGNING_BUNDLE_POLL_MS: u64 = 60_000;

struct ControlServiceSignerBootstrap {
    signer: Option<Arc<ServiceSigner>>,
    watcher: Option<ServiceSigningBundleWatcher>,
}

struct ServiceSigningBundleWatcher {
    signer: Arc<ServiceSigner>,
    path: PathBuf,
    poll_interval: Duration,
    expected_cluster_id: String,
    expected_service_id: String,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    init_tracing(Service::ControlPlane)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let metrics_config = MetricsServerConfig::from_env()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;

    let node_id = required_env("INFERLAB_RAFT_NODE_ID")?;
    let cluster_id = raft_cluster_id()?;
    let signer = control_signer()?;
    let writer_authorizer = Arc::new(control_writer_authorizer()?);
    let writer_status = writer_authorizer.status();
    let ControlServiceSignerBootstrap {
        signer: service_signer,
        watcher: service_signing_watcher,
    } = control_service_signer(&cluster_id)?;
    validate_local_service_signer(&node_id, service_signer.as_deref())?;
    let data_directory = env::var("INFERLAB_RAFT_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./data/raft").join(&node_id));
    let (service_authorizer, service_trust_watcher, service_trust_tls_identity_watcher) =
        control_service_authorizer(&cluster_id, &data_directory, service_signer.clone()).await?;
    let service_authorizer = Arc::new(service_authorizer);
    let service_status = service_authorizer.status();
    if service_status.required && service_signer.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "service authentication requires INFERLAB_SERVICE_ID and exactly one local signing source on every control node",
        ));
    }
    if let Some(service_signer) = service_signer.as_ref() {
        let snapshot = service_signer.snapshot();
        let qualified = format!("{}/{}", snapshot.service_id(), snapshot.credential_id());
        if service_status.required
            && !service_status
                .trusted_service_credentials
                .iter()
                .any(|credential| credential == &qualified)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("local service signing credential '{qualified}' is not trusted"),
            ));
        }
        if service_status
            .revoked_service_credentials
            .iter()
            .any(|credential| credential == &qualified)
            || service_status
                .revoked_service_ids
                .iter()
                .any(|service_id| service_id == snapshot.service_id())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("local service signing credential '{qualified}' is revoked"),
            ));
        }
        if service_status.required
            && !service_authorizer.service_signer_is_eligible(&snapshot, &cluster_id)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "local service signing credential '{qualified}' does not match the exact active trust-policy key"
                ),
            ));
        }
    }
    let bind = required_env("INFERLAB_RAFT_BIND")?;
    let peers = parse_peers(&required_env("INFERLAB_RAFT_PEERS")?)?;
    let election_timeout_min =
        Duration::from_millis(parse_env("INFERLAB_RAFT_ELECTION_MIN_MS", 300_u64)?);
    let election_timeout_max =
        Duration::from_millis(parse_env("INFERLAB_RAFT_ELECTION_MAX_MS", 600_u64)?);
    let heartbeat_interval =
        Duration::from_millis(parse_env("INFERLAB_RAFT_HEARTBEAT_MS", 100_u64)?);
    let rpc_timeout = Duration::from_millis(parse_env("INFERLAB_RAFT_RPC_TIMEOUT_MS", 150_u64)?);
    let commit_timeout =
        Duration::from_millis(parse_env("INFERLAB_RAFT_COMMIT_TIMEOUT_MS", 2_000_u64)?);
    let node = RaftNode::open_with_service_signer(
        NodeConfig {
            node_id: node_id.clone(),
            cluster_id: cluster_id.clone(),
            peers,
            state_path: data_directory.join("state.json"),
            event_path: data_directory.join("events.jsonl"),
            election_timeout_min,
            election_timeout_max,
            heartbeat_interval,
            rpc_timeout,
            commit_timeout,
        },
        service_signer.clone(),
    )
    .map_err(io::Error::other)?;
    let background = node.spawn_background();
    let service_signing_background = service_signing_watcher
        .map(|watcher| tokio::spawn(watcher.run(Arc::clone(&service_authorizer))));
    let service_trust_background = service_trust_watcher
        .map(|watcher| tokio::spawn(watcher.run(Arc::clone(&service_authorizer))));
    let service_trust_tls_identity_background =
        service_trust_tls_identity_watcher.map(|watcher| tokio::spawn(watcher.run()));
    let listener = TcpListener::bind(&bind).await?;
    let service_signer_status = service_signer.as_ref().map(|signer| signer.status());
    info!(
        %node_id,
        %cluster_id,
        signing_key_id = signer.as_ref().map(|signer| signer.key_id()),
        writer_authorization_required = writer_status.required,
        trusted_writer_ids = ?writer_status.trusted_writer_ids,
        revoked_writer_ids = ?writer_status.revoked_writer_ids,
        write_max_age_ms = writer_status.max_age_ms,
        write_max_future_skew_ms = writer_status.max_future_skew_ms,
        service_authentication_required = service_status.required,
        service_id = service_signer_status.as_ref().map(|status| status.service_id.as_str()),
        service_credential_id = service_signer_status.as_ref().map(|status| status.active_credential_id.as_str()),
        service_signing_mode = service_signer_status.as_ref().map(|status| status.mode.as_str()),
        service_signing_bundle_generation = service_signer_status.as_ref().and_then(|status| status.bundle_generation),
        service_signing_configured_credential_count = service_signer_status.as_ref().map(|status| status.configured_credential_count),
        trusted_service_ids = ?service_status.trusted_service_ids,
        trusted_service_credentials = ?service_status.trusted_service_credentials,
        revoked_service_ids = ?service_status.revoked_service_ids,
        revoked_service_credentials = ?service_status.revoked_service_credentials,
        gateway_service_ids = ?service_status.gateway_service_ids,
        service_request_max_age_ms = service_status.max_age_ms,
        service_request_max_future_skew_ms = service_status.max_future_skew_ms,
        service_trust_policy_source = %service_status.trust_policy_source,
        service_trust_policy_generation = service_status.trust_policy_generation,
        service_trust_policy_signing_key_id = service_status.trust_policy_signing_key_id,
        trusted_service_trust_signing_key_ids = ?service_status.trusted_trust_policy_signing_key_ids,
        revoked_service_trust_signing_key_ids = ?service_status.revoked_trust_policy_signing_key_ids,
        %bind,
        data_directory = %data_directory.display(),
        election_timeout_min_ms = election_timeout_min.as_millis(),
        election_timeout_max_ms = election_timeout_max.as_millis(),
        heartbeat_interval_ms = heartbeat_interval.as_millis(),
        "InferLab Raft control-plane node listening"
    );
    let application = async move {
        match metrics_config {
            None => {
                axum::serve(
                    listener,
                    app_with_authentication(node, signer, writer_authorizer, service_authorizer),
                )
                .await
            }
            Some(metrics_config) => {
                let mut registry = MetricsRegistry::new();
                let http = HttpMetrics::register(&mut registry, Service::ControlPlane)
                    .map_err(io::Error::other)?;
                ControlMetrics::register(
                    &mut registry,
                    Arc::clone(&node),
                    Arc::clone(&writer_authorizer),
                    Arc::clone(&service_authorizer),
                )
                .map_err(io::Error::other)?;
                let registry = Arc::new(registry);
                let application = http.instrument(app_with_authentication(
                    node,
                    signer,
                    writer_authorizer,
                    service_authorizer,
                ));
                let ((), ()) = tokio::try_join!(
                    async { axum::serve(listener, application).await },
                    serve_metrics(metrics_config, registry),
                )?;
                Ok(())
            }
        }
    };
    supervise_control_plane(
        application,
        background,
        service_signing_background,
        service_trust_background,
        service_trust_tls_identity_background,
    )
    .await
}

async fn supervise_control_plane<F>(
    application: F,
    mut raft_background: JoinHandle<()>,
    mut service_signing_background: Option<JoinHandle<()>>,
    mut service_trust_background: Option<JoinHandle<()>>,
    mut service_trust_tls_identity_background: Option<JoinHandle<()>>,
) -> io::Result<()>
where
    F: Future<Output = io::Result<()>>,
{
    tokio::pin!(application);
    let result = tokio::select! {
        result = &mut application => result,
        result = &mut raft_background => {
            Err(unexpected_background_exit("Raft background loop", result))
        }
        result = await_optional_background(&mut service_signing_background) => {
            Err(unexpected_background_exit("service-signing bundle watcher", result))
        }
        result = await_optional_background(&mut service_trust_background) => {
            Err(unexpected_background_exit("service-trust watcher", result))
        }
        result = await_optional_background(&mut service_trust_tls_identity_background) => {
            Err(unexpected_background_exit("service-trust TLS identity watcher", result))
        }
    };
    raft_background.abort();
    if let Some(background) = service_signing_background.as_ref() {
        background.abort();
    }
    if let Some(background) = service_trust_background.as_ref() {
        background.abort();
    }
    if let Some(background) = service_trust_tls_identity_background.as_ref() {
        background.abort();
    }
    result
}

async fn await_optional_background(
    background: &mut Option<JoinHandle<()>>,
) -> Result<(), JoinError> {
    match background {
        Some(background) => background.await,
        None => std::future::pending().await,
    }
}

fn unexpected_background_exit(name: &str, result: Result<(), JoinError>) -> io::Error {
    match result {
        Ok(()) => io::Error::other(format!("{name} stopped unexpectedly")),
        Err(error) if error.is_panic() => io::Error::other(format!("{name} failed")),
        Err(_) => io::Error::other(format!("{name} was cancelled unexpectedly")),
    }
}

fn control_signer() -> io::Result<Option<Arc<SigningIdentity>>> {
    let key_id = env::var("INFERLAB_CONTROL_SIGNING_KEY_ID").ok();
    let private_key = env::var("INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64").ok();
    match (key_id, private_key) {
        (None, None) => Ok(None),
        (Some(key_id), Some(private_key)) => {
            SigningIdentity::from_base64_seed(key_id, &private_key)
                .map(Arc::new)
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_CONTROL_SIGNING_KEY_ID and INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64 must be configured together",
        )),
    }
}

fn control_writer_authorizer() -> io::Result<WriteAuthorizer> {
    let encoded_keys = env::var("INFERLAB_CONTROL_WRITER_KEYS").unwrap_or_default();
    let revoked_writer_ids = env::var("INFERLAB_CONTROL_REVOKED_WRITER_IDS").unwrap_or_default();
    if encoded_keys.trim().is_empty() {
        if !revoked_writer_ids.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "INFERLAB_CONTROL_REVOKED_WRITER_IDS requires INFERLAB_CONTROL_WRITER_KEYS",
            ));
        }
        return Ok(WriteAuthorizer::disabled());
    }
    let keys = TrustedWriterKeyRing::parse(&encoded_keys, &revoked_writer_ids)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let max_age_ms = parse_env("INFERLAB_CONTROL_WRITE_MAX_AGE_MS", 30_000_u64)?;
    let max_future_skew_ms = parse_env("INFERLAB_CONTROL_WRITE_MAX_FUTURE_SKEW_MS", 5_000_u64)?;
    if max_age_ms == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_CONTROL_WRITE_MAX_AGE_MS must be positive",
        ));
    }
    Ok(WriteAuthorizer::required(
        keys,
        max_age_ms,
        max_future_skew_ms,
    ))
}

#[derive(Default)]
struct ControlServiceSigningConfiguration {
    service_id: Option<String>,
    credential_id: Option<String>,
    private_key: Option<String>,
    bundle_path: Option<PathBuf>,
    bundle_poll_ms: Option<u64>,
}

fn control_service_signer(cluster_id: &str) -> io::Result<ControlServiceSignerBootstrap> {
    let service_id = optional_string_env("INFERLAB_SERVICE_ID")?;
    let credential_id = optional_string_env("INFERLAB_SERVICE_CREDENTIAL_ID")?;
    let private_key = optional_string_env("INFERLAB_SERVICE_PRIVATE_KEY_B64")?;
    let bundle_path = optional_path_env("INFERLAB_SERVICE_SIGNING_BUNDLE_PATH")?;
    let bundle_poll_ms = optional_string_env("INFERLAB_SERVICE_SIGNING_BUNDLE_POLL_MS")?
        .map(|value| parse_value("INFERLAB_SERVICE_SIGNING_BUNDLE_POLL_MS", &value))
        .transpose()?;
    control_service_signer_from_configuration(
        cluster_id,
        ControlServiceSigningConfiguration {
            service_id,
            credential_id,
            private_key,
            bundle_path,
            bundle_poll_ms,
        },
    )
}

fn control_service_signer_from_configuration(
    cluster_id: &str,
    configuration: ControlServiceSigningConfiguration,
) -> io::Result<ControlServiceSignerBootstrap> {
    let ControlServiceSigningConfiguration {
        service_id,
        credential_id,
        private_key,
        bundle_path,
        bundle_poll_ms,
    } = configuration;
    if bundle_path.is_none() && bundle_poll_ms.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_SERVICE_SIGNING_BUNDLE_POLL_MS requires INFERLAB_SERVICE_SIGNING_BUNDLE_PATH",
        ));
    }
    if bundle_path.is_some() && (credential_id.is_some() || private_key.is_some()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_SERVICE_SIGNING_BUNDLE_PATH is mutually exclusive with INFERLAB_SERVICE_CREDENTIAL_ID and INFERLAB_SERVICE_PRIVATE_KEY_B64",
        ));
    }
    match (service_id, credential_id, private_key, bundle_path) {
        (None, None, None, None) => Ok(ControlServiceSignerBootstrap {
            signer: None,
            watcher: None,
        }),
        (Some(service_id), credential_id, Some(private_key), None) => {
            let identity = ServiceSigningIdentity::from_base64_seed_with_credential(
                service_id,
                credential_id.unwrap_or_else(|| LEGACY_CREDENTIAL_ID.to_owned()),
                &private_key,
            )
            .map(Arc::new)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            Ok(ControlServiceSignerBootstrap {
                signer: Some(Arc::new(ServiceSigner::from_static(identity))),
                watcher: None,
            })
        }
        (Some(service_id), None, None, Some(bundle_path)) => {
            if bundle_path.as_os_str().is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "INFERLAB_SERVICE_SIGNING_BUNDLE_PATH must not be empty",
                ));
            }
            let poll_ms = bundle_poll_ms.unwrap_or(DEFAULT_SERVICE_SIGNING_BUNDLE_POLL_MS);
            if !(MIN_SERVICE_SIGNING_BUNDLE_POLL_MS..=MAX_SERVICE_SIGNING_BUNDLE_POLL_MS)
                .contains(&poll_ms)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "INFERLAB_SERVICE_SIGNING_BUNDLE_POLL_MS must be between {MIN_SERVICE_SIGNING_BUNDLE_POLL_MS} and {MAX_SERVICE_SIGNING_BUNDLE_POLL_MS} milliseconds"
                    ),
                ));
            }
            let bundle = VerifiedServiceSigningBundle::load(&bundle_path, cluster_id, &service_id)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let signer = Arc::new(ServiceSigner::from_bundle(bundle));
            Ok(ControlServiceSignerBootstrap {
                signer: Some(Arc::clone(&signer)),
                watcher: Some(ServiceSigningBundleWatcher {
                    signer,
                    path: bundle_path,
                    poll_interval: Duration::from_millis(poll_ms),
                    expected_cluster_id: cluster_id.to_owned(),
                    expected_service_id: service_id,
                }),
            })
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_SERVICE_ID must be configured with exactly one signing source: INFERLAB_SERVICE_SIGNING_BUNDLE_PATH, or legacy INFERLAB_SERVICE_PRIVATE_KEY_B64 with optional INFERLAB_SERVICE_CREDENTIAL_ID",
        )),
    }
}

fn validate_local_service_signer(node_id: &str, signer: Option<&ServiceSigner>) -> io::Result<()> {
    if let Some(signer) = signer {
        let service_id = signer.snapshot().service_id().to_owned();
        if service_id == node_id {
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "INFERLAB_SERVICE_ID '{}' must match INFERLAB_RAFT_NODE_ID '{node_id}'",
                service_id
            ),
        ));
    }
    Ok(())
}

impl ServiceSigningBundleWatcher {
    async fn run(self, authorizer: Arc<ServiceAuthorizer>) {
        let mut interval = tokio::time::interval(self.poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        let mut reload_loop = ServiceSigningWatcherLoop::default();
        loop {
            interval.tick().await;
            let observation = service_signing_bundle_observation(&self.path);
            match reload_loop.poll(
                observation,
                authorizer.trust_policy_generation(),
                &self.signer,
                || reload_service_signing_bundle(&self, &authorizer),
            ) {
                ServiceSigningPollOutcome::Activated => {
                    let snapshot = self.signer.snapshot();
                    info!(
                        generation = ?snapshot.bundle_generation(),
                        credential_id = snapshot.credential_id(),
                        "control plane activated a service-signing bundle"
                    );
                }
                ServiceSigningPollOutcome::Rejected { kind, report: true } => {
                    warn!(
                        reason = service_signing_error_kind_name(kind),
                        "control plane retained the last-known-good service signer"
                    );
                }
                ServiceSigningPollOutcome::Skipped
                | ServiceSigningPollOutcome::Unchanged
                | ServiceSigningPollOutcome::Rejected { report: false, .. } => {}
            }
        }
    }
}

#[derive(Default)]
struct ServiceSigningWatcherLoop {
    completed_source_observation: Option<ServiceSigningBundleObservation>,
    rejected_candidate_observation: Option<(ServiceSigningBundleObservation, Option<u64>)>,
    reported_source_failure: Option<(ServiceSigningBundleObservation, ServiceSigningErrorKind)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServiceSigningPollOutcome {
    Skipped,
    Activated,
    Unchanged,
    Rejected {
        kind: ServiceSigningErrorKind,
        report: bool,
    },
}

#[derive(Debug)]
enum ServiceSigningReloadError {
    Source(service_auth::ServiceSigningError),
    Activation(service_auth::ServiceSigningError),
}

impl ServiceSigningWatcherLoop {
    fn poll(
        &mut self,
        observation: ServiceSigningBundleObservation,
        trust_policy_generation: Option<u64>,
        signer: &ServiceSigner,
        reload: impl FnOnce() -> Result<ServiceSignerActivationOutcome, ServiceSigningReloadError>,
    ) -> ServiceSigningPollOutcome {
        if self.completed_source_observation.as_ref() == Some(&observation)
            || self.rejected_candidate_observation.as_ref()
                == Some(&(observation.clone(), trust_policy_generation))
        {
            return ServiceSigningPollOutcome::Skipped;
        }
        if self.completed_source_observation.as_ref() != Some(&observation) {
            self.completed_source_observation = None;
        }
        if self
            .rejected_candidate_observation
            .as_ref()
            .is_some_and(|(candidate, _)| candidate != &observation)
        {
            self.rejected_candidate_observation = None;
        }
        match reload() {
            Ok(ServiceSignerActivationOutcome::Activated) => {
                self.completed_source_observation = Some(observation);
                self.rejected_candidate_observation = None;
                self.reported_source_failure = None;
                ServiceSigningPollOutcome::Activated
            }
            Ok(ServiceSignerActivationOutcome::Unchanged) => {
                self.completed_source_observation = Some(observation);
                self.rejected_candidate_observation = None;
                self.reported_source_failure = None;
                ServiceSigningPollOutcome::Unchanged
            }
            Err(ServiceSigningReloadError::Source(error)) => {
                let kind = error.kind();
                let report =
                    self.reported_source_failure.as_ref() != Some(&(observation.clone(), kind));
                if report {
                    signer.record_rejection(kind);
                    self.reported_source_failure = Some((observation.clone(), kind));
                }
                if kind != ServiceSigningErrorKind::SourceUnavailable {
                    self.completed_source_observation = Some(observation);
                }
                self.rejected_candidate_observation = None;
                ServiceSigningPollOutcome::Rejected { kind, report }
            }
            Err(ServiceSigningReloadError::Activation(error)) => {
                let kind = error.kind();
                if kind == ServiceSigningErrorKind::CandidateRejected {
                    self.rejected_candidate_observation =
                        Some((observation, trust_policy_generation));
                } else {
                    self.completed_source_observation = Some(observation);
                    self.rejected_candidate_observation = None;
                }
                self.reported_source_failure = None;
                ServiceSigningPollOutcome::Rejected { kind, report: true }
            }
        }
    }
}

fn reload_service_signing_bundle(
    watcher: &ServiceSigningBundleWatcher,
    authorizer: &ServiceAuthorizer,
) -> Result<ServiceSignerActivationOutcome, ServiceSigningReloadError> {
    let candidate = match VerifiedServiceSigningBundle::load(
        &watcher.path,
        &watcher.expected_cluster_id,
        &watcher.expected_service_id,
    ) {
        Ok(candidate) => candidate,
        Err(error) => return Err(ServiceSigningReloadError::Source(error)),
    };
    watcher
        .signer
        .activate_bundle(candidate, |snapshot| {
            authorizer.service_signer_is_eligible(snapshot, &watcher.expected_cluster_id)
        })
        .map_err(ServiceSigningReloadError::Activation)
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

enum ConfiguredServiceTrustWatcher {
    Local(Box<ServiceTrustWatcher>),
    Remote(Box<RemoteServiceTrustWatcher>),
}

impl ConfiguredServiceTrustWatcher {
    async fn run(self, authorizer: Arc<ServiceAuthorizer>) {
        match self {
            Self::Local(watcher) => watcher.run(authorizer).await,
            Self::Remote(watcher) => watcher.run(authorizer).await,
        }
    }
}

async fn control_service_authorizer(
    cluster_id: &str,
    data_directory: &std::path::Path,
    local_signer: Option<Arc<ServiceSigner>>,
) -> io::Result<(
    ServiceAuthorizer,
    Option<ConfiguredServiceTrustWatcher>,
    Option<RemoteServiceTrustTlsIdentityWatcher>,
)> {
    let encoded_keys = env::var("INFERLAB_SERVICE_TRUSTED_KEYS").unwrap_or_default();
    let revoked_service_ids = env::var("INFERLAB_SERVICE_REVOKED_IDS").unwrap_or_default();
    let revoked_credentials = env::var("INFERLAB_SERVICE_REVOKED_CREDENTIALS").unwrap_or_default();
    let gateway_service_ids = env::var("INFERLAB_GATEWAY_SERVICE_IDS").unwrap_or_default();
    let snapshot_path = env::var("INFERLAB_SERVICE_TRUST_SNAPSHOT_PATH").ok();
    let distributor_url = env::var("INFERLAB_SERVICE_TRUST_DISTRIBUTOR_URL").ok();
    let cache_path = env::var("INFERLAB_SERVICE_TRUST_CACHE_PATH").ok();
    let poll_interval = env::var("INFERLAB_SERVICE_TRUST_POLL_MS").ok();
    let request_timeout = env::var("INFERLAB_SERVICE_TRUST_REQUEST_TIMEOUT_MS").ok();
    let max_backoff = env::var("INFERLAB_SERVICE_TRUST_MAX_BACKOFF_MS").ok();
    let allow_legacy_v1 = optional_string_env("INFERLAB_SERVICE_TRUST_ALLOW_LEGACY_V1")?;
    let max_policy_lifetime = optional_string_env("INFERLAB_SERVICE_TRUST_MAX_POLICY_LIFETIME_MS")?;
    let policy_max_future_skew =
        optional_string_env("INFERLAB_SERVICE_TRUST_POLICY_MAX_FUTURE_SKEW_MS")?;
    let tls_ca_cert_path = optional_path_env("INFERLAB_SERVICE_TRUST_TLS_CA_CERT_PATH")?;
    let tls_client_cert_path = optional_path_env("INFERLAB_SERVICE_TRUST_TLS_CLIENT_CERT_PATH")?;
    let tls_client_key_path = optional_path_env("INFERLAB_SERVICE_TRUST_TLS_CLIENT_KEY_PATH")?;
    let tls_client_identity_bundle_path =
        optional_path_env("INFERLAB_SERVICE_TRUST_TLS_CLIENT_IDENTITY_BUNDLE_PATH")?;
    let tls_client_identity_bundle_poll_ms =
        optional_string_env("INFERLAB_SERVICE_TRUST_TLS_CLIENT_IDENTITY_BUNDLE_POLL_MS")?
            .map(|value| {
                parse_value(
                    "INFERLAB_SERVICE_TRUST_TLS_CLIENT_IDENTITY_BUNDLE_POLL_MS",
                    &value,
                )
            })
            .transpose()?;
    let tls = RemoteServiceTrustTlsConfig::from_optional_sources(
        tls_ca_cert_path,
        tls_client_cert_path,
        tls_client_key_path,
        tls_client_identity_bundle_path,
        tls_client_identity_bundle_poll_ms,
    )?;
    let root_keys = env::var("INFERLAB_SERVICE_TRUST_ROOT_KEYS").unwrap_or_default();
    let revoked_root_keys =
        env::var("INFERLAB_SERVICE_TRUST_REVOKED_ROOT_KEY_IDS").unwrap_or_default();
    let floor_path = env::var("INFERLAB_SERVICE_TRUST_STATE_PATH").ok();
    let max_age_ms = parse_env("INFERLAB_SERVICE_AUTH_MAX_AGE_MS", 5_000_u64)?;
    let max_future_skew_ms = parse_env("INFERLAB_SERVICE_AUTH_MAX_FUTURE_SKEW_MS", 1_000_u64)?;

    let distribution_mode = select_service_trust_distribution_mode(
        snapshot_path.as_deref(),
        distributor_url.as_deref(),
        cache_path.is_some() || request_timeout.is_some() || max_backoff.is_some() || tls.is_some(),
        poll_interval.is_some()
            || allow_legacy_v1.is_some()
            || max_policy_lifetime.is_some()
            || policy_max_future_skew.is_some(),
    )?;

    if distribution_mode != ServiceTrustDistributionMode::None {
        if !encoded_keys.trim().is_empty()
            || !revoked_service_ids.trim().is_empty()
            || !revoked_credentials.trim().is_empty()
            || !gateway_service_ids.trim().is_empty()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "signed service-trust file or distributor mode cannot be combined with static service trusted, revoked, or gateway ID configuration",
            ));
        }
        if root_keys.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "signed service-trust file or distributor mode requires INFERLAB_SERVICE_TRUST_ROOT_KEYS",
            ));
        }
        let local_signer = local_signer.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "signed service-trust snapshots require a local service signing identity",
            )
        })?;
        let roots = TrustedServiceTrustRootKeyRing::parse(&root_keys, &revoked_root_keys)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let validity_config = signed_service_trust_validity_config(
            allow_legacy_v1.as_deref(),
            max_policy_lifetime.as_deref(),
            policy_max_future_skew.as_deref(),
        )?;
        if validity_config.allow_v1() {
            warn!(
                "legacy non-expiring service-trust policy v1 compatibility is enabled; policy lifetime is unbounded"
            );
        }
        let poll_interval = Duration::from_millis(
            poll_interval
                .map(|value| parse_value("INFERLAB_SERVICE_TRUST_POLL_MS", &value))
                .transpose()?
                .unwrap_or(100_u64),
        );
        let floor_path = floor_path
            .map(PathBuf::from)
            .unwrap_or_else(|| data_directory.join("service-trust-floor.json"));
        if distribution_mode == ServiceTrustDistributionMode::LocalFile {
            let bootstrap = bootstrap_signed_service_trust_with_signer(
                PathBuf::from(snapshot_path.expect("local-file mode selected")),
                floor_path,
                cluster_id.to_owned(),
                roots,
                local_signer,
                validity_config,
                poll_interval,
                max_age_ms,
                max_future_skew_ms,
            )?;
            return Ok((
                bootstrap.authorizer,
                Some(ConfiguredServiceTrustWatcher::Local(Box::new(
                    bootstrap.watcher,
                ))),
                None,
            ));
        }

        let request_timeout = request_timeout
            .map(|value| parse_value("INFERLAB_SERVICE_TRUST_REQUEST_TIMEOUT_MS", &value))
            .transpose()?
            .unwrap_or(2_000_u64);
        let max_backoff = max_backoff
            .map(|value| parse_value("INFERLAB_SERVICE_TRUST_MAX_BACKOFF_MS", &value))
            .transpose()?
            .unwrap_or(10_000_u64);
        let config = RemoteServiceTrustConfig::new_with_tls(
            distributor_url.as_deref().expect("remote mode selected"),
            cache_path
                .map(PathBuf::from)
                .unwrap_or_else(|| data_directory.join("service-trust-cache.json")),
            poll_interval,
            Duration::from_millis(request_timeout),
            Duration::from_millis(max_backoff),
            tls,
        )?;
        let bootstrap = bootstrap_remote_signed_service_trust_with_signer(
            config,
            floor_path,
            cluster_id.to_owned(),
            roots,
            local_signer,
            validity_config,
            max_age_ms,
            max_future_skew_ms,
        )
        .await?;
        return Ok((
            bootstrap.authorizer,
            Some(ConfiguredServiceTrustWatcher::Remote(Box::new(
                bootstrap.watcher,
            ))),
            bootstrap.tls_identity_watcher,
        ));
    }

    if !root_keys.trim().is_empty()
        || !revoked_root_keys.trim().is_empty()
        || floor_path.is_some()
        || cache_path.is_some()
        || request_timeout.is_some()
        || max_backoff.is_some()
        || allow_legacy_v1.is_some()
        || max_policy_lifetime.is_some()
        || policy_max_future_skew.is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "service-trust roots and signed-distribution state, cache, timeout, or backoff configuration require INFERLAB_SERVICE_TRUST_SNAPSHOT_PATH or INFERLAB_SERVICE_TRUST_DISTRIBUTOR_URL",
        ));
    }
    if encoded_keys.trim().is_empty() {
        if !revoked_service_ids.trim().is_empty()
            || !revoked_credentials.trim().is_empty()
            || !gateway_service_ids.trim().is_empty()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "INFERLAB_SERVICE_REVOKED_IDS, INFERLAB_SERVICE_REVOKED_CREDENTIALS, and INFERLAB_GATEWAY_SERVICE_IDS require INFERLAB_SERVICE_TRUSTED_KEYS",
            ));
        }
        return Ok((ServiceAuthorizer::disabled(), None, None));
    }
    let keys = TrustedServiceKeyRing::parse_with_revoked_credentials(
        &encoded_keys,
        &revoked_service_ids,
        &revoked_credentials,
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let gateway_service_ids = gateway_service_ids
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if max_age_ms == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_SERVICE_AUTH_MAX_AGE_MS must be positive",
        ));
    }
    ServiceAuthorizer::required(keys, gateway_service_ids, max_age_ms, max_future_skew_ms)
        .map(|authorizer| (authorizer, None, None))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

fn signed_service_trust_validity_config(
    allow_legacy_v1: Option<&str>,
    max_policy_lifetime: Option<&str>,
    policy_max_future_skew: Option<&str>,
) -> io::Result<ServiceTrustReceiverValidityConfig> {
    let allow_legacy_v1 = match allow_legacy_v1 {
        None | Some("0") => false,
        Some("1") => true,
        Some(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "INFERLAB_SERVICE_TRUST_ALLOW_LEGACY_V1 must be 0 or 1",
            ));
        }
    };
    let max_policy_lifetime = max_policy_lifetime
        .map(|value| parse_value("INFERLAB_SERVICE_TRUST_MAX_POLICY_LIFETIME_MS", value))
        .transpose()?
        .unwrap_or(DEFAULT_SERVICE_TRUST_MAX_POLICY_LIFETIME_MS);
    if !(MIN_SERVICE_TRUST_MAX_POLICY_LIFETIME_MS..=MAX_SERVICE_TRUST_MAX_POLICY_LIFETIME_MS)
        .contains(&max_policy_lifetime)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "INFERLAB_SERVICE_TRUST_MAX_POLICY_LIFETIME_MS must be between {MIN_SERVICE_TRUST_MAX_POLICY_LIFETIME_MS} and {MAX_SERVICE_TRUST_MAX_POLICY_LIFETIME_MS}"
            ),
        ));
    }
    let policy_max_future_skew = policy_max_future_skew
        .map(|value| parse_value("INFERLAB_SERVICE_TRUST_POLICY_MAX_FUTURE_SKEW_MS", value))
        .transpose()?
        .unwrap_or(DEFAULT_SERVICE_TRUST_POLICY_MAX_FUTURE_SKEW_MS);
    if policy_max_future_skew > MAX_SERVICE_TRUST_POLICY_MAX_FUTURE_SKEW_MS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "INFERLAB_SERVICE_TRUST_POLICY_MAX_FUTURE_SKEW_MS must be between 0 and {MAX_SERVICE_TRUST_POLICY_MAX_FUTURE_SKEW_MS}"
            ),
        ));
    }
    ServiceTrustReceiverValidityConfig::new(
        allow_legacy_v1,
        policy_max_future_skew,
        max_policy_lifetime,
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

fn required_env(name: &str) -> io::Result<String> {
    env::var(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{name} is required")))
}

fn raft_cluster_id() -> io::Result<String> {
    raft_cluster_id_from_env_result(env::var("INFERLAB_RAFT_CLUSTER_ID"))
}

fn raft_cluster_id_from_env_result(value: Result<String, env::VarError>) -> io::Result<String> {
    optional_string_from_env_result("INFERLAB_RAFT_CLUSTER_ID", value)
        .map(|value| value.unwrap_or_else(|| DEFAULT_CLUSTER_ID.to_owned()))
}

fn optional_path_env(name: &str) -> io::Result<Option<PathBuf>> {
    optional_path_from_env_result(name, env::var(name))
}

fn optional_string_env(name: &str) -> io::Result<Option<String>> {
    optional_string_from_env_result(name, env::var(name))
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

fn optional_path_from_env_result(
    name: &str,
    value: Result<String, env::VarError>,
) -> io::Result<Option<PathBuf>> {
    match value {
        Ok(value) => Ok(Some(PathBuf::from(value))),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must contain valid Unicode"),
        )),
    }
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

fn parse_value<T>(name: &str, value: &str) -> io::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value.parse::<T>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} has an invalid value: {error}"),
        )
    })
}

fn parse_peers(raw: &str) -> io::Result<Vec<Peer>> {
    raw.split(',')
        .map(|entry| {
            let (id, base_url) = entry.split_once('=').ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid peer '{entry}'; expected id=http://host:port"),
                )
            })?;
            Ok(Peer {
                id: id.trim().to_owned(),
                base_url: base_url.trim().trim_end_matches('/').to_owned(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    const SERVICE_SEED: &str = "TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs=";
    const SERVICE_SEED_B: &str = "oRHYnSe9L9fS2eMjpnvZPZ7tg09poPfRXpAMlzsqHkg=";
    const SERVICE_SEED_WRONG: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "inferlab-control-signing-{}-{name}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_bundle(
        path: &std::path::Path,
        generation: u64,
        active_credential_id: &str,
        key_b_seed: &str,
    ) {
        let document = serde_json::json!({
            "schema": service_auth::SERVICE_SIGNING_BUNDLE_SCHEMA,
            "cluster_id": "inferlab-test",
            "generation": generation,
            "service_id": "node-a",
            "active_credential_id": active_credential_id,
            "credentials": [
                {
                    "credential_id": "key-a",
                    "private_key_base64": SERVICE_SEED
                },
                {
                    "credential_id": "key-b",
                    "private_key_base64": key_b_seed
                }
            ]
        });
        fs::write(path, serde_json::to_vec(&document).expect("encode bundle"))
            .expect("write bundle");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .expect("set bundle permissions");
        }
    }

    #[test]
    fn local_service_identity_is_bound_before_trust_bootstrap() {
        let identity = ServiceSigningIdentity::from_base64_seed_with_credential(
            "node-b",
            "key-a",
            SERVICE_SEED,
        )
        .expect("identity");
        let signer = ServiceSigner::from_static(Arc::new(identity));
        let error =
            validate_local_service_signer("node-a", Some(&signer)).expect_err("node mismatch");
        assert!(error.to_string().contains("must match"));
        validate_local_service_signer("node-b", Some(&signer)).expect("matching node");
        validate_local_service_signer("node-a", None).expect("unsigned compatibility mode");
    }

    #[test]
    fn raft_cluster_id_defaults_only_when_absent_and_rejects_malformed_unicode() {
        assert_eq!(
            raft_cluster_id_from_env_result(Err(env::VarError::NotPresent))
                .expect("absent cluster ID uses the compatibility default"),
            DEFAULT_CLUSTER_ID
        );
        assert_eq!(
            raft_cluster_id_from_env_result(Ok("inferlab-custom".to_owned()))
                .expect("explicit cluster ID"),
            "inferlab-custom"
        );
        let error = raft_cluster_id_from_env_result(Err(env::VarError::NotUnicode(
            std::ffi::OsString::from("malformed-cluster"),
        )))
        .expect_err("malformed Unicode cluster ID must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "INFERLAB_RAFT_CLUSTER_ID must contain valid Unicode"
        );
        assert!(!error.to_string().contains("malformed-cluster"));
    }

    #[test]
    fn static_service_signer_configuration_remains_compatible() {
        let bootstrap = control_service_signer_from_configuration(
            "inferlab-test",
            ControlServiceSigningConfiguration {
                service_id: Some("node-a".to_owned()),
                private_key: Some(SERVICE_SEED.to_owned()),
                ..ControlServiceSigningConfiguration::default()
            },
        )
        .expect("static signer");
        let status = bootstrap.signer.expect("signer").status();
        assert_eq!(status.mode.as_str(), "static");
        assert_eq!(status.service_id, "node-a");
        assert_eq!(status.active_credential_id, LEGACY_CREDENTIAL_ID);
        assert_eq!(status.bundle_generation, None);
        assert!(bootstrap.watcher.is_none());
    }

    #[test]
    fn watched_service_signer_configuration_is_exclusive_and_bounded() {
        let mixed = control_service_signer_from_configuration(
            "inferlab-test",
            ControlServiceSigningConfiguration {
                service_id: Some("node-a".to_owned()),
                credential_id: Some("key-a".to_owned()),
                private_key: Some(SERVICE_SEED.to_owned()),
                bundle_path: Some(PathBuf::from("bundle.json")),
                bundle_poll_ms: Some(100),
            },
        )
        .err()
        .expect("mixed sources must fail");
        assert!(mixed.to_string().contains("mutually exclusive"));

        let orphan = control_service_signer_from_configuration(
            "inferlab-test",
            ControlServiceSigningConfiguration {
                bundle_poll_ms: Some(100),
                ..ControlServiceSigningConfiguration::default()
            },
        )
        .err()
        .expect("orphan poll must fail");
        assert!(orphan.to_string().contains("requires"));

        for poll_ms in [24, 60_001] {
            let error = control_service_signer_from_configuration(
                "inferlab-test",
                ControlServiceSigningConfiguration {
                    service_id: Some("node-a".to_owned()),
                    bundle_path: Some(PathBuf::from("does-not-need-to-exist.json")),
                    bundle_poll_ms: Some(poll_ms),
                    ..ControlServiceSigningConfiguration::default()
                },
            )
            .err()
            .expect("out-of-range poll must fail before file access");
            assert!(error.to_string().contains("between 25 and 60000"));
        }
    }

    #[test]
    fn watched_service_signer_activates_only_the_exact_policy_key() {
        let directory = TestDirectory::new("policy-eligibility");
        let path = directory.path("bundle.json");
        write_bundle(&path, 1, "key-a", SERVICE_SEED_B);
        let bootstrap = control_service_signer_from_configuration(
            "inferlab-test",
            ControlServiceSigningConfiguration {
                service_id: Some("node-a".to_owned()),
                bundle_path: Some(path.clone()),
                bundle_poll_ms: Some(100),
                ..ControlServiceSigningConfiguration::default()
            },
        )
        .expect("watched signer");
        let signer = bootstrap.signer.expect("signer");
        let watcher = bootstrap.watcher.expect("watcher");
        let key_a = ServiceSigningIdentity::from_base64_seed_with_credential(
            "node-a",
            "key-a",
            SERVICE_SEED,
        )
        .expect("key a");
        let key_b = ServiceSigningIdentity::from_base64_seed_with_credential(
            "node-a",
            "key-b",
            SERVICE_SEED_B,
        )
        .expect("key b");
        let trusted = format!(
            "node-a/key-a={},node-a/key-b={}",
            key_a.public_key_base64(),
            key_b.public_key_base64()
        );
        let keys = TrustedServiceKeyRing::parse_with_revoked_credentials(&trusted, "", "")
            .expect("trusted overlap");
        let authorizer = ServiceAuthorizer::required(keys, vec!["node-a".to_owned()], 5_000, 1_000)
            .expect("authorizer");

        write_bundle(&path, 2, "key-b", SERVICE_SEED_B);
        assert_eq!(
            reload_service_signing_bundle(&watcher, &authorizer).expect("activate key b"),
            ServiceSignerActivationOutcome::Activated
        );
        let status = signer.status();
        assert_eq!(status.bundle_generation, Some(2));
        assert_eq!(status.active_credential_id, "key-b");
        assert_eq!(status.successful_activations, 1);

        let mut reload_loop = ServiceSigningWatcherLoop::default();
        write_bundle(&path, 3, "key-b", SERVICE_SEED_WRONG);
        let rejected_observation = service_signing_bundle_observation(&path);
        assert_eq!(
            reload_loop.poll(rejected_observation.clone(), Some(10), &signer, || {
                reload_service_signing_bundle(&watcher, &authorizer)
            },),
            ServiceSigningPollOutcome::Rejected {
                kind: ServiceSigningErrorKind::CandidateRejected,
                report: true,
            }
        );
        let status = signer.status();
        assert_eq!(status.bundle_generation, Some(2));
        assert_eq!(status.active_credential_id, "key-b");
        assert_eq!(status.successful_activations, 1);
        assert_eq!(status.rejected_reloads, 1);
        assert_eq!(
            status.last_error_kind,
            Some(ServiceSigningErrorKind::CandidateRejected)
        );
        assert_eq!(
            reload_loop.poll(rejected_observation.clone(), Some(10), &signer, || {
                panic!("candidate rejection must be deduplicated within one policy generation")
            }),
            ServiceSigningPollOutcome::Skipped
        );
        assert_eq!(
            reload_loop.poll(rejected_observation, Some(11), &signer, || {
                reload_service_signing_bundle(&watcher, &authorizer)
            }),
            ServiceSigningPollOutcome::Rejected {
                kind: ServiceSigningErrorKind::CandidateRejected,
                report: true,
            },
            "a policy-generation change must retry an eligibility rejection"
        );
        assert_eq!(signer.status().rejected_reloads, 2);

        fs::write(&path, b"{").expect("write malformed bundle");
        let observation = service_signing_bundle_observation(&path);
        assert_eq!(
            reload_loop.poll(
                observation.clone(),
                authorizer.trust_policy_generation(),
                &signer,
                || reload_service_signing_bundle(&watcher, &authorizer),
            ),
            ServiceSigningPollOutcome::Rejected {
                kind: ServiceSigningErrorKind::InvalidJson,
                report: true,
            }
        );
        let status = signer.status();
        assert_eq!(status.bundle_generation, Some(2));
        assert_eq!(status.active_credential_id, "key-b");
        assert_eq!(status.rejected_reloads, 3);
        assert_eq!(
            status.last_error_kind,
            Some(ServiceSigningErrorKind::InvalidJson)
        );
        assert_eq!(
            reload_loop.poll(observation, Some(999), &signer, || {
                panic!(
                    "unchanged deterministic invalid bundle must ignore policy-generation changes"
                )
            }),
            ServiceSigningPollOutcome::Skipped
        );
        assert_eq!(signer.status().rejected_reloads, 3);

        write_bundle(&path, 2, "key-b", SERVICE_SEED_B);
        assert_eq!(
            reload_loop.poll(
                service_signing_bundle_observation(&path),
                authorizer.trust_policy_generation(),
                &signer,
                || reload_service_signing_bundle(&watcher, &authorizer),
            ),
            ServiceSigningPollOutcome::Unchanged
        );
        assert_eq!(signer.status().last_error_kind, None);
    }

    #[test]
    fn watcher_loop_retries_transient_open_race_without_recounting_or_relogging() {
        let directory = TestDirectory::new("transient-open-race");
        let path = directory.path("bundle.json");
        write_bundle(&path, 1, "key-a", SERVICE_SEED_B);
        let signer = Arc::new(ServiceSigner::from_bundle(
            VerifiedServiceSigningBundle::load(&path, "inferlab-test", "node-a")
                .expect("initial bundle"),
        ));
        let observation = service_signing_bundle_observation(&path);
        assert!(matches!(
            &observation,
            ServiceSigningBundleObservation::Present(_)
        ));
        let source_error = VerifiedServiceSigningBundle::load(
            directory.path("temporarily-unavailable.json"),
            "inferlab-test",
            "node-a",
        )
        .expect_err("missing source");
        assert_eq!(
            source_error.kind(),
            ServiceSigningErrorKind::SourceUnavailable
        );
        let mut reload_loop = ServiceSigningWatcherLoop::default();
        let mut attempts = 0_u64;

        for (report, policy_generation) in [(true, None), (false, Some(99))] {
            assert_eq!(
                reload_loop.poll(observation.clone(), policy_generation, &signer, || {
                    attempts += 1;
                    Err(ServiceSigningReloadError::Source(source_error.clone()))
                },),
                ServiceSigningPollOutcome::Rejected {
                    kind: ServiceSigningErrorKind::SourceUnavailable,
                    report,
                }
            );
        }
        assert_eq!(attempts, 2, "unchanged transient source must be retried");
        let status = signer.status();
        assert_eq!(status.rejected_reloads, 1);
        assert_eq!(
            status.last_error_kind,
            Some(ServiceSigningErrorKind::SourceUnavailable)
        );

        assert_eq!(
            reload_loop.poll(observation.clone(), Some(100), &signer, || {
                attempts += 1;
                signer
                    .activate_bundle(
                        VerifiedServiceSigningBundle::load(&path, "inferlab-test", "node-a")
                            .expect("source recovered"),
                        |_| true,
                    )
                    .map_err(ServiceSigningReloadError::Activation)
            }),
            ServiceSigningPollOutcome::Unchanged
        );
        assert_eq!(attempts, 3);
        assert_eq!(signer.status().rejected_reloads, 1);
        assert_eq!(signer.status().last_error_kind, None);

        assert_eq!(
            reload_loop.poll(observation, Some(101), &signer, || {
                panic!("completed unchanged observation must be deduplicated")
            }),
            ServiceSigningPollOutcome::Skipped
        );
    }

    #[test]
    fn watcher_loop_reloads_a_prior_valid_observation_after_intervening_failures() {
        let directory = TestDirectory::new("prior-valid-recovery");
        let path = directory.path("bundle.json");
        write_bundle(&path, 1, "key-a", SERVICE_SEED_B);
        let signer = Arc::new(ServiceSigner::from_bundle(
            VerifiedServiceSigningBundle::load(&path, "inferlab-test", "node-a")
                .expect("initial bundle"),
        ));
        let valid_observation = service_signing_bundle_observation(&path);
        let mut reload_loop = ServiceSigningWatcherLoop::default();
        assert_eq!(
            reload_loop.poll(valid_observation.clone(), Some(1), &signer, || {
                signer
                    .activate_bundle(
                        VerifiedServiceSigningBundle::load(&path, "inferlab-test", "node-a")
                            .expect("current bundle"),
                        |_| true,
                    )
                    .map_err(ServiceSigningReloadError::Activation)
            }),
            ServiceSigningPollOutcome::Unchanged
        );

        let source_error = VerifiedServiceSigningBundle::load(
            directory.path("unavailable.json"),
            "inferlab-test",
            "node-a",
        )
        .expect_err("missing source");
        assert_eq!(
            reload_loop.poll(
                ServiceSigningBundleObservation::Unavailable(io::ErrorKind::NotFound),
                Some(1),
                &signer,
                || Err(ServiceSigningReloadError::Source(source_error)),
            ),
            ServiceSigningPollOutcome::Rejected {
                kind: ServiceSigningErrorKind::SourceUnavailable,
                report: true,
            }
        );
        assert_eq!(
            reload_loop.poll(valid_observation.clone(), Some(1), &signer, || {
                signer
                    .activate_bundle(
                        VerifiedServiceSigningBundle::load(&path, "inferlab-test", "node-a")
                            .expect("restored source"),
                        |_| true,
                    )
                    .map_err(ServiceSigningReloadError::Activation)
            }),
            ServiceSigningPollOutcome::Unchanged,
            "returning to the prior valid source must clear a transient error"
        );
        assert_eq!(signer.status().last_error_kind, None);

        write_bundle(&path, 2, "key-b", SERVICE_SEED_B);
        let rejected_observation = service_signing_bundle_observation(&path);
        assert_eq!(
            reload_loop.poll(rejected_observation, Some(1), &signer, || {
                signer
                    .activate_bundle(
                        VerifiedServiceSigningBundle::load(&path, "inferlab-test", "node-a")
                            .expect("candidate bundle"),
                        |_| false,
                    )
                    .map_err(ServiceSigningReloadError::Activation)
            }),
            ServiceSigningPollOutcome::Rejected {
                kind: ServiceSigningErrorKind::CandidateRejected,
                report: true,
            }
        );
        write_bundle(&path, 1, "key-a", SERVICE_SEED_B);
        assert_eq!(
            reload_loop.poll(valid_observation, Some(1), &signer, || {
                signer
                    .activate_bundle(
                        VerifiedServiceSigningBundle::load(&path, "inferlab-test", "node-a")
                            .expect("restored last-known-good bundle"),
                        |_| true,
                    )
                    .map_err(ServiceSigningReloadError::Activation)
            }),
            ServiceSigningPollOutcome::Unchanged,
            "returning to the prior valid source must clear an eligibility error"
        );
        let status = signer.status();
        assert_eq!(status.bundle_generation, Some(1));
        assert_eq!(status.active_credential_id, "key-a");
        assert_eq!(status.rejected_reloads, 2);
        assert_eq!(status.last_error_kind, None);
    }

    #[tokio::test]
    async fn supervisor_fails_when_the_service_signing_watcher_completes_or_panics() {
        for (panic_watcher, expected) in [
            (false, "service-signing bundle watcher stopped unexpectedly"),
            (true, "service-signing bundle watcher failed"),
        ] {
            let raft_background = tokio::spawn(std::future::pending::<()>());
            let signing_background = tokio::spawn(async move {
                assert!(!panic_watcher, "injected watcher panic");
            });
            let error = supervise_control_plane(
                std::future::pending::<io::Result<()>>(),
                raft_background,
                Some(signing_background),
                None,
                None,
            )
            .await
            .expect_err("unexpected watcher exit must fail the process");
            assert_eq!(error.kind(), io::ErrorKind::Other);
            assert_eq!(error.to_string(), expected);
        }
    }

    #[tokio::test]
    async fn supervisor_fails_when_the_tls_identity_watcher_completes() {
        let error = supervise_control_plane(
            std::future::pending::<io::Result<()>>(),
            tokio::spawn(std::future::pending::<()>()),
            None,
            None,
            Some(tokio::spawn(async {})),
        )
        .await
        .expect_err("unexpected TLS identity watcher exit must fail the process");
        assert_eq!(
            error.to_string(),
            "service-trust TLS identity watcher stopped unexpectedly"
        );
    }

    #[test]
    fn malformed_unicode_tls_path_fails_closed() {
        let error = optional_path_from_env_result(
            "INFERLAB_SERVICE_TRUST_TLS_CLIENT_KEY_PATH",
            Err(env::VarError::NotUnicode(std::ffi::OsString::from(
                "malformed-value",
            ))),
        )
        .expect_err("non-Unicode TLS path must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("must contain valid Unicode"));
        assert!(!error.to_string().contains("malformed-value"));
    }

    #[test]
    fn signed_service_trust_validity_configuration_is_bounded_and_explicit() {
        let defaults = signed_service_trust_validity_config(None, None, None).expect("defaults");
        assert!(!defaults.allow_v1());
        assert_eq!(
            defaults.max_lifetime_ms(),
            DEFAULT_SERVICE_TRUST_MAX_POLICY_LIFETIME_MS
        );
        assert_eq!(
            defaults.max_future_skew_ms(),
            DEFAULT_SERVICE_TRUST_POLICY_MAX_FUTURE_SKEW_MS
        );

        let legacy = signed_service_trust_validity_config(Some("1"), Some("250"), Some("0"))
            .expect("bounded legacy override");
        assert!(legacy.allow_v1());
        assert_eq!(legacy.max_lifetime_ms(), 250);
        assert_eq!(legacy.max_future_skew_ms(), 0);

        for (legacy, lifetime, skew) in [
            (Some("true"), None, None),
            (None, Some("249"), None),
            (None, Some("604800001"), None),
            (None, None, Some("300001")),
        ] {
            assert!(
                signed_service_trust_validity_config(legacy, lifetime, skew).is_err(),
                "out-of-contract validity configuration must fail"
            );
        }
    }

    #[test]
    fn malformed_unicode_validity_setting_fails_closed() {
        let error = optional_string_from_env_result(
            "INFERLAB_SERVICE_TRUST_MAX_POLICY_LIFETIME_MS",
            Err(env::VarError::NotUnicode(std::ffi::OsString::from(
                "malformed-value",
            ))),
        )
        .expect_err("non-Unicode validity value must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("must contain valid Unicode"));
        assert!(!error.to_string().contains("malformed-value"));
    }
}
