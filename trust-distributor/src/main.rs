use std::{env, io, net::SocketAddr, path::PathBuf};

use service_auth::TrustedServiceTrustRootKeyRing;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;
use trust_distributor::{
    DEFAULT_MAX_BODY_BYTES, DistributorConfig, MAX_BODY_BYTES, TrustDistributor, app,
    parse_expected_receivers,
};

const MAX_SMALL_ENV_BYTES: usize = 4096;
const MAX_RECEIVER_ENV_BYTES: usize = 65536;

#[tokio::main]
async fn main() -> io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

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
    let trusted_root_key_ids = roots.trusted_key_ids();
    let revoked_root_key_ids = roots.revoked_key_ids();
    let distributor = TrustDistributor::open(
        DistributorConfig {
            cluster_id: cluster_id.clone(),
            state_path: state_path.clone(),
            expected_receivers: expected_receivers.clone(),
            max_body_bytes,
        },
        roots,
    )
    .map_err(io::Error::other)?;

    let listener = TcpListener::bind(bind_address).await?;
    info!(
        %bind_address,
        %cluster_id,
        state_path = %state_path.display(),
        trusted_root_key_ids = ?trusted_root_key_ids,
        revoked_root_key_ids = ?revoked_root_key_ids,
        expected_receivers = ?expected_receivers,
        max_body_bytes,
        snapshot_available = distributor.has_snapshot().await,
        "InferLab trust distributor listening"
    );
    axum::serve(listener, app(distributor)).await
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
