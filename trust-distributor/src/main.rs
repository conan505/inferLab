use std::{env, io, net::SocketAddr, path::PathBuf, sync::Arc};

use observability::{
    HttpMetrics, MetricsRegistry, MetricsServerConfig, Service, init_tracing, serve_metrics,
};
use service_auth::TrustedServiceTrustRootKeyRing;
use tokio::net::TcpListener;
use tracing::info;
use transport_security::{ServerTransportConfig, load_mtls_server_config};
use trust_distributor::{
    DEFAULT_MAX_BODY_BYTES, DistributorConfig, MAX_BODY_BYTES, TrustDistributor,
    TrustDistributorMetrics, app, parse_expected_receivers,
};

const MAX_SMALL_ENV_BYTES: usize = 4096;
const MAX_RECEIVER_ENV_BYTES: usize = 65536;

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
    let expected_receivers = parse_expected_receivers(&required_env(
        "INFERLAB_TRUST_DISTRIBUTOR_EXPECTED_RECEIVERS",
        MAX_RECEIVER_ENV_BYTES,
    )?)
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let max_body_bytes = parse_body_bound()?;
    let transport = ServerTransportConfig::from_optional_paths(
        optional_path_env("INFERLAB_TRUST_DISTRIBUTOR_TLS_CERT_PATH")?,
        optional_path_env("INFERLAB_TRUST_DISTRIBUTOR_TLS_KEY_PATH")?,
        optional_path_env("INFERLAB_TRUST_DISTRIBUTOR_TLS_CLIENT_CA_PATH")?,
    )?;
    let transport_status = transport.status();
    let tls_config = match &transport {
        ServerTransportConfig::Http => None,
        ServerTransportConfig::MutualTls(paths) => Some(load_mtls_server_config(paths)?),
    };
    let trusted_root_key_ids = roots.trusted_key_ids();
    let revoked_root_key_ids = roots.revoked_key_ids();
    let distributor = TrustDistributor::open(
        DistributorConfig {
            cluster_id: cluster_id.clone(),
            state_path: state_path.clone(),
            expected_receivers: expected_receivers.clone(),
            max_body_bytes,
            transport_security: transport_status,
        },
        roots,
    )
    .map_err(io::Error::other)?;

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
        match tls_config {
            None => {
                let listener = TcpListener::from_std(listener)?;
                axum::serve(listener, application).await
            }
            Some(config) => {
                let config = axum_server::tls_rustls::RustlsConfig::from_config(config.into());
                axum_server::from_tcp_rustls(listener, config)
                    .map_err(io::Error::other)?
                    .serve(application.into_make_service())
                    .await
            }
        }
    };
    match metrics_server {
        None => application_server.await,
        Some((config, registry)) => {
            let ((), ()) = tokio::try_join!(application_server, serve_metrics(config, registry))?;
            Ok(())
        }
    }
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
