use std::{env, io, path::PathBuf, sync::Arc, time::Duration};

use control_auth::{SigningIdentity, TrustedWriterKeyRing};
use control_plane::{
    ControlMetrics, NodeConfig, Peer, RaftNode, ServiceAuthorizer, WriteAuthorizer,
    app_with_authentication,
    model::DEFAULT_CLUSTER_ID,
    service_trust::{
        RemoteServiceTrustConfig, RemoteServiceTrustTlsConfig, RemoteServiceTrustWatcher,
        ServiceTrustDistributionMode, ServiceTrustWatcher, bootstrap_remote_signed_service_trust,
        bootstrap_signed_service_trust, select_service_trust_distribution_mode,
    },
};
use observability::{
    HttpMetrics, MetricsRegistry, MetricsServerConfig, Service, init_tracing, serve_metrics,
};
use service_auth::{
    LEGACY_CREDENTIAL_ID, ServiceSigningIdentity, TrustedServiceKeyRing,
    TrustedServiceTrustRootKeyRing,
};
use tokio::net::TcpListener;
use tracing::info;

#[tokio::main]
async fn main() -> io::Result<()> {
    init_tracing(Service::ControlPlane)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let metrics_config = MetricsServerConfig::from_env()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;

    let node_id = required_env("INFERLAB_RAFT_NODE_ID")?;
    let cluster_id =
        env::var("INFERLAB_RAFT_CLUSTER_ID").unwrap_or_else(|_| DEFAULT_CLUSTER_ID.to_owned());
    let signer = control_signer()?;
    let writer_authorizer = Arc::new(control_writer_authorizer()?);
    let writer_status = writer_authorizer.status();
    let service_identity = control_service_identity()?;
    validate_local_service_identity(&node_id, service_identity.as_deref())?;
    let data_directory = env::var("INFERLAB_RAFT_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./data/raft").join(&node_id));
    let (service_authorizer, service_trust_watcher) =
        control_service_authorizer(&cluster_id, &data_directory, service_identity.clone()).await?;
    let service_authorizer = Arc::new(service_authorizer);
    let service_status = service_authorizer.status();
    if service_status.required && service_identity.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "service authentication requires INFERLAB_SERVICE_ID and INFERLAB_SERVICE_PRIVATE_KEY_B64 on every control node",
        ));
    }
    if let Some(identity) = service_identity.as_ref() {
        let qualified = format!("{}/{}", identity.service_id(), identity.credential_id());
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
                .any(|service_id| service_id == identity.service_id())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("local service signing credential '{qualified}' is revoked"),
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
    let node = RaftNode::open_with_service_identity(
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
        service_identity.clone(),
    )
    .map_err(io::Error::other)?;
    let _background = node.spawn_background();
    let _service_trust_background = service_trust_watcher
        .map(|watcher| tokio::spawn(watcher.run(Arc::clone(&service_authorizer))));
    let listener = TcpListener::bind(&bind).await?;
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
        service_id = service_identity.as_ref().map(|identity| identity.service_id()),
        service_credential_id = service_identity.as_ref().map(|identity| identity.credential_id()),
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

fn control_service_identity() -> io::Result<Option<Arc<ServiceSigningIdentity>>> {
    let service_id = env::var("INFERLAB_SERVICE_ID").ok();
    let credential_id = env::var("INFERLAB_SERVICE_CREDENTIAL_ID").ok();
    let private_key = env::var("INFERLAB_SERVICE_PRIVATE_KEY_B64").ok();
    match (service_id, credential_id, private_key) {
        (None, None, None) => Ok(None),
        (Some(service_id), credential_id, Some(private_key)) => {
            ServiceSigningIdentity::from_base64_seed_with_credential(
                service_id,
                credential_id.unwrap_or_else(|| LEGACY_CREDENTIAL_ID.to_owned()),
                &private_key,
            )
            .map(Arc::new)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_SERVICE_ID and INFERLAB_SERVICE_PRIVATE_KEY_B64 must be configured together; INFERLAB_SERVICE_CREDENTIAL_ID is optional only when both are present",
        )),
    }
}

fn validate_local_service_identity(
    node_id: &str,
    identity: Option<&ServiceSigningIdentity>,
) -> io::Result<()> {
    if let Some(identity) = identity
        && identity.service_id() != node_id
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "INFERLAB_SERVICE_ID '{}' must match INFERLAB_RAFT_NODE_ID '{node_id}'",
                identity.service_id()
            ),
        ));
    }
    Ok(())
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
    local_identity: Option<Arc<ServiceSigningIdentity>>,
) -> io::Result<(ServiceAuthorizer, Option<ConfiguredServiceTrustWatcher>)> {
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
    let tls_ca_cert_path = optional_path_env("INFERLAB_SERVICE_TRUST_TLS_CA_CERT_PATH")?;
    let tls_client_cert_path = optional_path_env("INFERLAB_SERVICE_TRUST_TLS_CLIENT_CERT_PATH")?;
    let tls_client_key_path = optional_path_env("INFERLAB_SERVICE_TRUST_TLS_CLIENT_KEY_PATH")?;
    let tls = RemoteServiceTrustTlsConfig::from_optional_paths(
        tls_ca_cert_path,
        tls_client_cert_path,
        tls_client_key_path,
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
        poll_interval.is_some(),
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
        let local_identity = local_identity.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "signed service-trust snapshots require a local service signing identity",
            )
        })?;
        let roots = TrustedServiceTrustRootKeyRing::parse(&root_keys, &revoked_root_keys)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
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
            let bootstrap = bootstrap_signed_service_trust(
                PathBuf::from(snapshot_path.expect("local-file mode selected")),
                floor_path,
                cluster_id.to_owned(),
                roots,
                format!(
                    "{}/{}",
                    local_identity.service_id(),
                    local_identity.credential_id()
                ),
                poll_interval,
                max_age_ms,
                max_future_skew_ms,
            )?;
            return Ok((
                bootstrap.authorizer,
                Some(ConfiguredServiceTrustWatcher::Local(Box::new(
                    bootstrap.watcher,
                ))),
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
        let bootstrap = bootstrap_remote_signed_service_trust(
            config,
            floor_path,
            cluster_id.to_owned(),
            roots,
            local_identity,
            max_age_ms,
            max_future_skew_ms,
        )
        .await?;
        return Ok((
            bootstrap.authorizer,
            Some(ConfiguredServiceTrustWatcher::Remote(Box::new(
                bootstrap.watcher,
            ))),
        ));
    }

    if !root_keys.trim().is_empty()
        || !revoked_root_keys.trim().is_empty()
        || floor_path.is_some()
        || cache_path.is_some()
        || request_timeout.is_some()
        || max_backoff.is_some()
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
        return Ok((ServiceAuthorizer::disabled(), None));
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
        .map(|authorizer| (authorizer, None))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

fn required_env(name: &str) -> io::Result<String> {
    env::var(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{name} is required")))
}

fn optional_path_env(name: &str) -> io::Result<Option<PathBuf>> {
    optional_path_from_env_result(name, env::var(name))
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
    use super::*;

    const SERVICE_SEED: &str = "TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs=";

    #[test]
    fn local_service_identity_is_bound_before_trust_bootstrap() {
        let identity = ServiceSigningIdentity::from_base64_seed_with_credential(
            "node-b",
            "key-a",
            SERVICE_SEED,
        )
        .expect("identity");
        let error =
            validate_local_service_identity("node-a", Some(&identity)).expect_err("node mismatch");
        assert!(error.to_string().contains("must match"));
        validate_local_service_identity("node-b", Some(&identity)).expect("matching node");
        validate_local_service_identity("node-a", None).expect("unsigned compatibility mode");
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
}
