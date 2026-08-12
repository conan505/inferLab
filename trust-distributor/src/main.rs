use std::{env, future::Future, io, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use observability::{
    HttpMetrics, MetricsRegistry, MetricsServerConfig, Service, init_tracing, serve_metrics,
};
use service_auth::TrustedServiceTrustRootKeyRing;
use tokio::{
    net::TcpListener,
    task::{JoinError, JoinHandle},
};
use tracing::{info, warn};
use transport_security::{
    MtlsClientCertificateVerifier, ServerTransportConfig, ServerTransportStatus, TlsIdentity,
    TlsIdentityPollOutcome, TlsIdentityPurpose, TlsIdentityReloadError, TlsIdentityWatcherLoop,
    VerifiedTlsIdentityBundle, load_mtls_client_certificate_verifier, load_mtls_server_config,
    load_mtls_server_config_with_identity_and_verifier, tls_identity_bundle_observation,
};
use trust_distributor::{
    DEFAULT_MAX_BODY_BYTES, DistributorConfig, MAX_BODY_BYTES, TrustDistributor,
    TrustDistributorMetrics, app, parse_expected_receivers, parse_expected_service_ids,
};

const MAX_SMALL_ENV_BYTES: usize = 4096;
const MAX_RECEIVER_ENV_BYTES: usize = 65536;
const DISTRIBUTOR_TLS_IDENTITY_ID: &str = "trust-distributor";
const DEFAULT_TLS_IDENTITY_BUNDLE_POLL_MS: u64 = 100;
const MIN_TLS_IDENTITY_BUNDLE_POLL_MS: u64 = 25;
const MAX_TLS_IDENTITY_BUNDLE_POLL_MS: u64 = 60_000;

struct DistributorTransportBootstrap {
    status: ServerTransportStatus,
    tls_config: Option<axum_server::tls_rustls::RustlsConfig>,
    identity: Option<Arc<TlsIdentity>>,
    watcher: Option<DistributorTlsIdentityWatcher>,
}

#[derive(Default)]
struct DistributorTransportConfiguration {
    certificate_chain: Option<PathBuf>,
    private_key: Option<PathBuf>,
    client_ca: Option<PathBuf>,
    identity_bundle: Option<PathBuf>,
    identity_bundle_poll_ms: Option<u64>,
    server_name: Option<String>,
}

struct DistributorTlsIdentityWatcher {
    identity: Arc<TlsIdentity>,
    path: PathBuf,
    poll_interval: Duration,
    expected_cluster_id: String,
    expected_server_name: String,
    client_verifier: MtlsClientCertificateVerifier,
    runtime: axum_server::tls_rustls::RustlsConfig,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    init_tracing(Service::TrustDistributor)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let metrics_config = MetricsServerConfig::from_env()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;

    let bind = optional_env(
        "INFERLAB_TRUST_DISTRIBUTOR_BIND",
        "127.0.0.1:8090",
        MAX_SMALL_ENV_BYTES,
    )?;
    let bind_address = bind.parse::<SocketAddr>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("INFERLAB_TRUST_DISTRIBUTOR_BIND must be a socket address: {error}"),
        )
    })?;
    let cluster_id = required_env("INFERLAB_TRUST_DISTRIBUTOR_CLUSTER_ID", MAX_SMALL_ENV_BYTES)?;
    let encoded_roots = required_env("INFERLAB_SERVICE_TRUST_ROOT_KEYS", MAX_SMALL_ENV_BYTES)?;
    let revoked_roots = optional_env(
        "INFERLAB_SERVICE_TRUST_REVOKED_ROOT_KEY_IDS",
        "",
        MAX_SMALL_ENV_BYTES,
    )?;
    let roots = TrustedServiceTrustRootKeyRing::parse(&encoded_roots, &revoked_roots)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let state_path = PathBuf::from(required_env(
        "INFERLAB_TRUST_DISTRIBUTOR_STATE_PATH",
        MAX_SMALL_ENV_BYTES,
    )?);
    let expected_receivers_raw = optional_required_env(
        "INFERLAB_TRUST_DISTRIBUTOR_EXPECTED_RECEIVERS",
        MAX_RECEIVER_ENV_BYTES,
    )?;
    let expected_service_ids_raw = optional_required_env(
        "INFERLAB_TRUST_DISTRIBUTOR_EXPECTED_SERVICE_IDS",
        MAX_RECEIVER_ENV_BYTES,
    )?;
    let expected_receivers =
        parse_expected_receiver_configuration(expected_receivers_raw, expected_service_ids_raw)?;
    let max_body_bytes = parse_body_bound()?;
    let transport = distributor_transport(&cluster_id)?;
    let transport_status = transport.status;
    let trusted_root_key_ids = roots.trusted_key_ids();
    let revoked_root_key_ids = roots.revoked_key_ids();
    let distributor_config = DistributorConfig {
        cluster_id: cluster_id.clone(),
        state_path: state_path.clone(),
        expected_receivers: expected_receivers.clone(),
        max_body_bytes,
        transport_security: transport_status,
    };
    let expected_receiver_mode = distributor_config
        .expected_receiver_mode()
        .map_err(io::Error::other)?;
    let distributor =
        TrustDistributor::open(distributor_config, roots).map_err(io::Error::other)?;
    if let Some(identity) = transport.identity.as_ref() {
        distributor.configure_transport_identity(Arc::clone(identity));
    }

    let listener = std::net::TcpListener::bind(bind_address)?;
    listener.set_nonblocking(true)?;
    let bound_address = listener.local_addr()?;
    info!(
        %bound_address,
        %cluster_id,
        state_path = %state_path.display(),
        trusted_root_key_ids = ?trusted_root_key_ids,
        revoked_root_key_ids = ?revoked_root_key_ids,
        expected_receivers = ?expected_receivers,
        expected_receiver_mode = expected_receiver_mode.as_str(),
        max_body_bytes,
        transport_security_mode = transport_status.mode(),
        client_certificate_required = transport_status.client_certificate_required(),
        minimum_protocol = transport_status.minimum_protocol(),
        snapshot_available = distributor.has_snapshot().await,
        "InferLab trust distributor listening"
    );
    let mut application = app(distributor.clone());
    let metrics_server = match metrics_config {
        None => None,
        Some(config) => {
            let mut registry = MetricsRegistry::new();
            let http = HttpMetrics::register(&mut registry, Service::TrustDistributor)
                .map_err(io::Error::other)?;
            TrustDistributorMetrics::register(&mut registry, distributor)
                .map_err(io::Error::other)?;
            application = http.instrument(application);
            Some((config, Arc::new(registry)))
        }
    };
    let application_server = async move {
        match transport.tls_config {
            None => {
                let listener = TcpListener::from_std(listener)?;
                axum::serve(listener, application).await
            }
            Some(config) => {
                axum_server::from_tcp_rustls(listener, config)
                    .map_err(io::Error::other)?
                    .serve(application.into_make_service())
                    .await
            }
        }
    };
    let application = async move {
        match metrics_server {
            None => application_server.await,
            Some((config, registry)) => {
                let ((), ()) =
                    tokio::try_join!(application_server, serve_metrics(config, registry))?;
                Ok(())
            }
        }
    };
    let tls_identity_background = transport.watcher.map(|watcher| tokio::spawn(watcher.run()));
    supervise_distributor(application, tls_identity_background).await
}

fn distributor_transport(cluster_id: &str) -> io::Result<DistributorTransportBootstrap> {
    let identity_bundle_poll_ms = optional_required_env(
        "INFERLAB_TRUST_DISTRIBUTOR_TLS_IDENTITY_BUNDLE_POLL_MS",
        32,
    )?
    .map(|value| {
        value.parse::<u64>().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "INFERLAB_TRUST_DISTRIBUTOR_TLS_IDENTITY_BUNDLE_POLL_MS must be an integer: {error}"
                ),
            )
        })
    })
    .transpose()?;
    distributor_transport_from_configuration(
        cluster_id,
        DistributorTransportConfiguration {
            certificate_chain: optional_path_env("INFERLAB_TRUST_DISTRIBUTOR_TLS_CERT_PATH")?,
            private_key: optional_path_env("INFERLAB_TRUST_DISTRIBUTOR_TLS_KEY_PATH")?,
            client_ca: optional_path_env("INFERLAB_TRUST_DISTRIBUTOR_TLS_CLIENT_CA_PATH")?,
            identity_bundle: optional_path_env(
                "INFERLAB_TRUST_DISTRIBUTOR_TLS_IDENTITY_BUNDLE_PATH",
            )?,
            identity_bundle_poll_ms,
            server_name: optional_required_env(
                "INFERLAB_TRUST_DISTRIBUTOR_TLS_SERVER_NAME",
                MAX_SMALL_ENV_BYTES,
            )?,
        },
    )
}

fn distributor_transport_from_configuration(
    cluster_id: &str,
    configuration: DistributorTransportConfiguration,
) -> io::Result<DistributorTransportBootstrap> {
    let DistributorTransportConfiguration {
        certificate_chain,
        private_key,
        client_ca,
        identity_bundle,
        identity_bundle_poll_ms,
        server_name,
    } = configuration;

    if identity_bundle.is_none() && (identity_bundle_poll_ms.is_some() || server_name.is_some()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "TLS identity bundle poll and server-name configuration require INFERLAB_TRUST_DISTRIBUTOR_TLS_IDENTITY_BUNDLE_PATH",
        ));
    }
    if identity_bundle.is_some() && (certificate_chain.is_some() || private_key.is_some()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "watched distributor TLS identity bundles are mutually exclusive with legacy certificate and key paths",
        ));
    }

    if let Some(identity_bundle) = identity_bundle {
        let client_ca = client_ca.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "watched distributor TLS identity requires INFERLAB_TRUST_DISTRIBUTOR_TLS_CLIENT_CA_PATH",
            )
        })?;
        let server_name = server_name.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "watched distributor TLS identity requires INFERLAB_TRUST_DISTRIBUTOR_TLS_SERVER_NAME",
            )
        })?;
        let poll_ms = identity_bundle_poll_ms.unwrap_or(DEFAULT_TLS_IDENTITY_BUNDLE_POLL_MS);
        if !(MIN_TLS_IDENTITY_BUNDLE_POLL_MS..=MAX_TLS_IDENTITY_BUNDLE_POLL_MS).contains(&poll_ms) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "INFERLAB_TRUST_DISTRIBUTOR_TLS_IDENTITY_BUNDLE_POLL_MS must be between {MIN_TLS_IDENTITY_BUNDLE_POLL_MS} and {MAX_TLS_IDENTITY_BUNDLE_POLL_MS} milliseconds"
                ),
            ));
        }
        let bundle = VerifiedTlsIdentityBundle::load(
            &identity_bundle,
            cluster_id,
            DISTRIBUTOR_TLS_IDENTITY_ID,
            TlsIdentityPurpose::Server,
            Some(&server_name),
        )
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let client_verifier = load_mtls_client_certificate_verifier(&client_ca)?;
        let server_config =
            load_mtls_server_config_with_identity_and_verifier(&bundle, &client_verifier)?;
        let runtime = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(server_config));
        let identity = Arc::new(TlsIdentity::from_bundle(bundle));
        return Ok(DistributorTransportBootstrap {
            status: ServerTransportStatus::MutualTls,
            tls_config: Some(runtime.clone()),
            identity: Some(Arc::clone(&identity)),
            watcher: Some(DistributorTlsIdentityWatcher {
                identity,
                path: identity_bundle,
                poll_interval: Duration::from_millis(poll_ms),
                expected_cluster_id: cluster_id.to_owned(),
                expected_server_name: server_name,
                client_verifier,
                runtime,
            }),
        });
    }

    let legacy =
        ServerTransportConfig::from_optional_paths(certificate_chain, private_key, client_ca)?;
    match legacy {
        ServerTransportConfig::Http => Ok(DistributorTransportBootstrap {
            status: ServerTransportStatus::Http,
            tls_config: None,
            identity: None,
            watcher: None,
        }),
        ServerTransportConfig::MutualTls(paths) => {
            let server_config = load_mtls_server_config(&paths)?;
            Ok(DistributorTransportBootstrap {
                status: ServerTransportStatus::MutualTls,
                tls_config: Some(axum_server::tls_rustls::RustlsConfig::from_config(
                    Arc::new(server_config),
                )),
                identity: None,
                watcher: None,
            })
        }
    }
}

impl DistributorTlsIdentityWatcher {
    async fn run(self) {
        let mut interval = tokio::time::interval(self.poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        let mut reload_loop = TlsIdentityWatcherLoop::default();
        loop {
            interval.tick().await;
            let observation = tls_identity_bundle_observation(&self.path);
            match reload_loop.poll(observation, &self.identity, || self.reload_once()) {
                TlsIdentityPollOutcome::Activated => {
                    let status = self.identity.status();
                    info!(
                        generation = ?status.bundle_generation,
                        "trust distributor activated a TLS server identity bundle"
                    );
                }
                TlsIdentityPollOutcome::Rejected { kind, report: true } => {
                    warn!(
                        reason = kind.as_str(),
                        "trust distributor retained its last-known-good TLS server identity"
                    );
                }
                TlsIdentityPollOutcome::Skipped
                | TlsIdentityPollOutcome::Unchanged
                | TlsIdentityPollOutcome::Rejected { report: false, .. } => {}
            }
        }
    }

    fn reload_once(
        &self,
    ) -> Result<transport_security::TlsIdentityActivationOutcome, TlsIdentityReloadError> {
        let candidate = VerifiedTlsIdentityBundle::load(
            &self.path,
            &self.expected_cluster_id,
            DISTRIBUTOR_TLS_IDENTITY_ID,
            TlsIdentityPurpose::Server,
            Some(&self.expected_server_name),
        )
        .map_err(TlsIdentityReloadError::Source)?;
        self.identity
            .activate_bundle(candidate, |candidate| {
                let config = load_mtls_server_config_with_identity_and_verifier(
                    candidate,
                    &self.client_verifier,
                )
                .map_err(|_| ())?;
                self.runtime.reload_from_config(Arc::new(config));
                Ok(())
            })
            .map_err(TlsIdentityReloadError::Activation)
    }
}

async fn supervise_distributor<F>(
    application: F,
    mut tls_identity_background: Option<JoinHandle<()>>,
) -> io::Result<()>
where
    F: Future<Output = io::Result<()>>,
{
    tokio::pin!(application);
    let result = tokio::select! {
        result = &mut application => result,
        result = await_optional_background(&mut tls_identity_background) => {
            Err(unexpected_background_exit("TLS identity bundle watcher", result))
        }
    };
    if let Some(background) = tls_identity_background.as_ref() {
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

fn parse_expected_receiver_configuration(
    expected_receivers_raw: Option<String>,
    expected_service_ids_raw: Option<String>,
) -> io::Result<std::collections::BTreeSet<String>> {
    match (expected_receivers_raw, expected_service_ids_raw) {
        (Some(receivers), None) => parse_expected_receivers(&receivers),
        (None, Some(services)) => parse_expected_service_ids(&services),
        (None, None) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "exactly one of INFERLAB_TRUST_DISTRIBUTOR_EXPECTED_RECEIVERS or INFERLAB_TRUST_DISTRIBUTOR_EXPECTED_SERVICE_IDS is required",
            ));
        }
        (Some(_), Some(_)) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "INFERLAB_TRUST_DISTRIBUTOR_EXPECTED_RECEIVERS and INFERLAB_TRUST_DISTRIBUTOR_EXPECTED_SERVICE_IDS are mutually exclusive",
            ));
        }
    }
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

fn optional_path_env(name: &str) -> io::Result<Option<PathBuf>> {
    match env::var(name) {
        Ok(value) => validate_env(name, value, MAX_SMALL_ENV_BYTES, false)
            .map(PathBuf::from)
            .map(Some),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must contain valid Unicode"),
        )),
    }
}

fn required_env(name: &str, max_bytes: usize) -> io::Result<String> {
    let value = env::var(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{name} is required")))?;
    validate_env(name, value, max_bytes, false)
}

fn optional_required_env(name: &str, max_bytes: usize) -> io::Result<Option<String>> {
    match env::var(name) {
        Ok(value) => validate_env(name, value, max_bytes, false).map(Some),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must contain valid Unicode"),
        )),
    }
}

fn optional_env(name: &str, default: &str, max_bytes: usize) -> io::Result<String> {
    let value = env::var(name).unwrap_or_else(|_| default.to_owned());
    validate_env(name, value, max_bytes, true)
}

fn validate_env(
    name: &str,
    value: String,
    max_bytes: usize,
    allow_empty: bool,
) -> io::Result<String> {
    if (!allow_empty && value.trim().is_empty()) || value.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{name} must contain {} to {max_bytes} bytes",
                usize::from(!allow_empty)
            ),
        ));
    }
    Ok(value)
}

fn parse_body_bound() -> io::Result<usize> {
    let raw = optional_env(
        "INFERLAB_TRUST_DISTRIBUTOR_MAX_BODY_BYTES",
        &DEFAULT_MAX_BODY_BYTES.to_string(),
        32,
    )?;
    let value = raw.parse::<usize>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("INFERLAB_TRUST_DISTRIBUTOR_MAX_BODY_BYTES must be an integer: {error}"),
        )
    })?;
    if !(1..=MAX_BODY_BYTES).contains(&value) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "INFERLAB_TRUST_DISTRIBUTOR_MAX_BODY_BYTES must be between 1 and {MAX_BODY_BYTES}"
            ),
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_receiver_configuration_is_explicit_and_homogeneous() {
        assert_eq!(
            parse_expected_receiver_configuration(
                Some("control-a/key-a,control-b/key-a".to_owned()),
                None,
            )
            .expect("qualified receivers")
            .into_iter()
            .collect::<Vec<_>>(),
            ["control-a/key-a", "control-b/key-a"]
        );
        assert_eq!(
            parse_expected_receiver_configuration(None, Some("control-a,control-b".to_owned()),)
                .expect("service receivers")
                .into_iter()
                .collect::<Vec<_>>(),
            ["control-a", "control-b"]
        );
        assert!(parse_expected_receiver_configuration(None, None).is_err());
        assert!(
            parse_expected_receiver_configuration(
                Some("control-a/key-a".to_owned()),
                Some("control-a".to_owned()),
            )
            .is_err()
        );
        assert!(parse_expected_receiver_configuration(Some("control-a".to_owned()), None).is_err());
        assert!(
            parse_expected_receiver_configuration(None, Some("control-a/key-a".to_owned()),)
                .is_err()
        );
    }

    #[test]
    fn watched_tls_identity_configuration_is_strictly_separate_and_bounded() {
        let mixed = distributor_transport_from_configuration(
            "inferlab-primary",
            DistributorTransportConfiguration {
                certificate_chain: Some(PathBuf::from("server.pem")),
                client_ca: Some(PathBuf::from("ca.pem")),
                identity_bundle: Some(PathBuf::from("identity.json")),
                server_name: Some("localhost".to_owned()),
                ..DistributorTransportConfiguration::default()
            },
        )
        .err()
        .expect("watched and static sources must not mix");
        assert_eq!(mixed.kind(), io::ErrorKind::InvalidInput);

        let missing_name = distributor_transport_from_configuration(
            "inferlab-primary",
            DistributorTransportConfiguration {
                client_ca: Some(PathBuf::from("ca.pem")),
                identity_bundle: Some(PathBuf::from("identity.json")),
                ..DistributorTransportConfiguration::default()
            },
        )
        .err()
        .expect("watched identity needs a bound server name");
        assert!(missing_name.to_string().contains("TLS_SERVER_NAME"));

        let invalid_poll = distributor_transport_from_configuration(
            "inferlab-primary",
            DistributorTransportConfiguration {
                client_ca: Some(PathBuf::from("ca.pem")),
                identity_bundle: Some(PathBuf::from("identity.json")),
                identity_bundle_poll_ms: Some(MIN_TLS_IDENTITY_BUNDLE_POLL_MS - 1),
                server_name: Some("localhost".to_owned()),
                ..DistributorTransportConfiguration::default()
            },
        )
        .err()
        .expect("poll interval is bounded before source load");
        assert!(invalid_poll.to_string().contains("must be between"));
    }

    #[tokio::test]
    async fn tls_identity_watcher_completion_is_process_supervised() {
        let error = supervise_distributor(std::future::pending(), Some(tokio::spawn(async {})))
            .await
            .expect_err("watcher completion must fail the process");
        assert_eq!(
            error.to_string(),
            "TLS identity bundle watcher stopped unexpectedly"
        );
    }

    #[tokio::test]
    async fn absent_tls_identity_watcher_does_not_block_application_completion() {
        supervise_distributor(async { Ok(()) }, None)
            .await
            .expect("static and HTTP modes have no watcher");
    }
}
