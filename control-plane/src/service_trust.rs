use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use reqwest::{
    Client, StatusCode, Url,
    header::{CONTENT_LENGTH, ETAG, IF_NONE_MATCH},
    redirect::Policy,
};
use serde::{Deserialize, Serialize};
use service_auth::{
    ServiceSigningIdentity, ServiceTrustApplicationReceipt, ServiceTrustSnapshot,
    TrustedServiceTrustRootKeyRing, VerifiedServiceTrustSnapshot,
};
use tokio::time;
use tracing::{info, warn};

use crate::{ServiceAuthorizer, service_authentication::TrustTransportMode};

pub const SERVICE_TRUST_FLOOR_SCHEMA: &str = "inferlab.service-trust-floor.v1";
pub const SERVICE_TRUST_CACHE_SCHEMA: &str = "inferlab.service-trust-cache.v1";
const MAX_SNAPSHOT_BYTES: u64 = 262_144;
const MAX_FLOOR_BYTES: u64 = 16_384;
const MAX_CACHE_BYTES: u64 = 524_288;
const MAX_ETAG_BYTES: usize = 1_024;
const MAX_DISTRIBUTOR_URL_BYTES: usize = 2_048;
const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REMOTE_BACKOFF: Duration = Duration::from_secs(300);
const MAX_ATOMIC_TEMP_ATTEMPTS: usize = 128;
const SNAPSHOT_ENDPOINT_PATH: &str = "/v1/service-trust/snapshot";
const RECEIPT_ENDPOINT_PATH: &str = "/v1/service-trust/receipts";
static NEXT_ATOMIC_TEMP_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceTrustDistributionMode {
    None,
    LocalFile,
    Remote,
}

pub fn select_service_trust_distribution_mode(
    snapshot_path: Option<&str>,
    distributor_url: Option<&str>,
    remote_only_configuration_present: bool,
    signed_only_configuration_present: bool,
) -> io::Result<ServiceTrustDistributionMode> {
    match (snapshot_path, distributor_url) {
        (Some(_), Some(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_SERVICE_TRUST_SNAPSHOT_PATH and INFERLAB_SERVICE_TRUST_DISTRIBUTOR_URL are mutually exclusive",
        )),
        (Some(_), None) if remote_only_configuration_present => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "service-trust remote cache, timeout, backoff, and TLS client configuration require INFERLAB_SERVICE_TRUST_DISTRIBUTOR_URL",
        )),
        (Some(_), None) => Ok(ServiceTrustDistributionMode::LocalFile),
        (None, Some(_)) => Ok(ServiceTrustDistributionMode::Remote),
        (None, None) if remote_only_configuration_present || signed_only_configuration_present => {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "service-trust poll, cache, timeout, backoff, and TLS client configuration require INFERLAB_SERVICE_TRUST_SNAPSHOT_PATH or INFERLAB_SERVICE_TRUST_DISTRIBUTOR_URL",
            ))
        }
        (None, None) => Ok(ServiceTrustDistributionMode::None),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PersistedServiceTrustFloor {
    pub schema: String,
    pub cluster_id: String,
    pub generation: u64,
    pub signing_key_id: String,
    pub signature: String,
}

impl PersistedServiceTrustFloor {
    fn from_verified(snapshot: &VerifiedServiceTrustSnapshot) -> Self {
        Self {
            schema: SERVICE_TRUST_FLOOR_SCHEMA.to_owned(),
            cluster_id: snapshot.policy.cluster_id.clone(),
            generation: snapshot.policy.generation,
            signing_key_id: snapshot.signing_key_id.clone(),
            signature: snapshot.signature.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ServiceTrustFloorStore {
    path: PathBuf,
}

impl ServiceTrustFloorStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> io::Result<Option<PersistedServiceTrustFloor>> {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if metadata.len() > MAX_FLOOR_BYTES {
            return Err(invalid_data(format!(
                "service-trust floor {} is {} bytes; maximum is {MAX_FLOOR_BYTES}",
                self.path.display(),
                metadata.len()
            )));
        }
        let bytes = fs::read(&self.path)?;
        let floor =
            serde_json::from_slice::<PersistedServiceTrustFloor>(&bytes).map_err(|error| {
                invalid_data(format!(
                    "cannot decode service-trust floor {}: {error}",
                    self.path.display()
                ))
            })?;
        validate_floor(&floor)?;
        Ok(Some(floor))
    }

    pub fn save(&self, floor: &PersistedServiceTrustFloor) -> io::Result<()> {
        validate_floor(floor)?;
        let bytes = serde_json::to_vec_pretty(floor)
            .map_err(|error| io::Error::other(format!("serialize service-trust floor: {error}")))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FLOOR_BYTES {
            return Err(invalid_data(format!(
                "serialized service-trust floor exceeds {MAX_FLOOR_BYTES} bytes"
            )));
        }
        write_atomic(&self.path, "service-trust floor", &bytes)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PersistedServiceTrustCache {
    pub schema: String,
    pub distributor_url: String,
    pub etag: Option<String>,
    pub snapshot: ServiceTrustSnapshot,
}

#[derive(Clone, Debug)]
pub struct ServiceTrustCacheStore {
    path: PathBuf,
}

impl ServiceTrustCacheStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> io::Result<Option<PersistedServiceTrustCache>> {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if metadata.len() > MAX_CACHE_BYTES {
            return Err(invalid_data(format!(
                "service-trust cache {} is {} bytes; maximum is {MAX_CACHE_BYTES}",
                self.path.display(),
                metadata.len()
            )));
        }
        let bytes = fs::read(&self.path)?;
        let cache =
            serde_json::from_slice::<PersistedServiceTrustCache>(&bytes).map_err(|error| {
                invalid_data(format!(
                    "cannot decode service-trust cache {}: {error}",
                    self.path.display()
                ))
            })?;
        validate_cache(&cache)?;
        Ok(Some(cache))
    }

    pub fn save(&self, cache: &PersistedServiceTrustCache) -> io::Result<()> {
        validate_cache(cache)?;
        let bytes = serde_json::to_vec_pretty(cache)
            .map_err(|error| io::Error::other(format!("serialize service-trust cache: {error}")))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_CACHE_BYTES {
            return Err(invalid_data(format!(
                "serialized service-trust cache exceeds {MAX_CACHE_BYTES} bytes"
            )));
        }
        write_atomic(&self.path, "service-trust cache", &bytes)
    }
}

#[derive(Clone, Debug)]
pub struct RemoteServiceTrustConfig {
    distributor_url: Url,
    cache_path: PathBuf,
    poll_interval: Duration,
    request_timeout: Duration,
    max_backoff: Duration,
    tls: Option<RemoteServiceTrustTlsConfig>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct RemoteServiceTrustTlsConfig {
    ca_cert_path: PathBuf,
    client_cert_path: PathBuf,
    client_key_path: PathBuf,
}

impl std::fmt::Debug for RemoteServiceTrustTlsConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteServiceTrustTlsConfig")
            .field("ca_certificate", &"configured")
            .field("client_certificate", &"configured")
            .field("client_private_key", &"configured")
            .finish()
    }
}

impl RemoteServiceTrustTlsConfig {
    pub fn from_optional_paths(
        ca_cert_path: Option<PathBuf>,
        client_cert_path: Option<PathBuf>,
        client_key_path: Option<PathBuf>,
    ) -> io::Result<Option<Self>> {
        match (ca_cert_path, client_cert_path, client_key_path) {
            (None, None, None) => Ok(None),
            (Some(ca_cert_path), Some(client_cert_path), Some(client_key_path)) => {
                if ca_cert_path.as_os_str().is_empty()
                    || client_cert_path.as_os_str().is_empty()
                    || client_key_path.as_os_str().is_empty()
                {
                    return Err(invalid_data(
                        "service-trust TLS certificate and key paths must not be empty",
                    ));
                }
                Ok(Some(Self {
                    ca_cert_path,
                    client_cert_path,
                    client_key_path,
                }))
            }
            _ => Err(invalid_data(
                "INFERLAB_SERVICE_TRUST_TLS_CA_CERT_PATH, INFERLAB_SERVICE_TRUST_TLS_CLIENT_CERT_PATH, and INFERLAB_SERVICE_TRUST_TLS_CLIENT_KEY_PATH must be configured together",
            )),
        }
    }

    fn paths(&self) -> transport_security::MtlsClientPaths {
        transport_security::MtlsClientPaths {
            server_ca: self.ca_cert_path.clone(),
            certificate_chain: self.client_cert_path.clone(),
            private_key: self.client_key_path.clone(),
        }
    }
}

impl RemoteServiceTrustConfig {
    pub fn new(
        distributor_url: &str,
        cache_path: PathBuf,
        poll_interval: Duration,
        request_timeout: Duration,
        max_backoff: Duration,
    ) -> io::Result<Self> {
        Self::new_with_tls(
            distributor_url,
            cache_path,
            poll_interval,
            request_timeout,
            max_backoff,
            None,
        )
    }

    pub fn new_with_tls(
        distributor_url: &str,
        cache_path: PathBuf,
        poll_interval: Duration,
        request_timeout: Duration,
        max_backoff: Duration,
        tls: Option<RemoteServiceTrustTlsConfig>,
    ) -> io::Result<Self> {
        if poll_interval.is_zero() {
            return Err(invalid_data(
                "service-trust snapshot poll interval must be positive",
            ));
        }
        if request_timeout.is_zero() || request_timeout > MAX_REQUEST_TIMEOUT {
            return Err(invalid_data(format!(
                "service-trust request timeout must be between 1 ms and {} ms",
                MAX_REQUEST_TIMEOUT.as_millis()
            )));
        }
        if max_backoff < poll_interval {
            return Err(invalid_data(
                "service-trust maximum backoff must be at least the poll interval",
            ));
        }
        if max_backoff > MAX_REMOTE_BACKOFF {
            return Err(invalid_data(format!(
                "service-trust maximum backoff cannot exceed {} ms",
                MAX_REMOTE_BACKOFF.as_millis()
            )));
        }
        if distributor_url.len() > MAX_DISTRIBUTOR_URL_BYTES {
            return Err(invalid_data(format!(
                "service-trust distributor URL exceeds {MAX_DISTRIBUTOR_URL_BYTES} bytes"
            )));
        }
        let distributor_url = Url::parse(distributor_url).map_err(|error| {
            invalid_data(format!("invalid service-trust distributor URL: {error}"))
        })?;
        match (distributor_url.scheme(), tls.is_some()) {
            ("http", false) | ("https", true) => {}
            ("https", false) => {
                return Err(invalid_data(
                    "https service-trust distributor URL requires the complete TLS client configuration",
                ));
            }
            ("http", true) => {
                return Err(invalid_data(
                    "service-trust TLS client configuration requires an https distributor URL",
                ));
            }
            _ => {
                return Err(invalid_data(
                    "service-trust distributor URL scheme must be http or https",
                ));
            }
        }
        if distributor_url.host_str().is_none() {
            return Err(invalid_data(
                "service-trust distributor URL requires a host",
            ));
        }
        if !distributor_url.username().is_empty() || distributor_url.password().is_some() {
            return Err(invalid_data(
                "service-trust distributor URL must not contain user information",
            ));
        }
        if distributor_url.query().is_some() || distributor_url.fragment().is_some() {
            return Err(invalid_data(
                "service-trust distributor URL must not contain a query or fragment",
            ));
        }
        if distributor_url.path() != "/" && !distributor_url.path().is_empty() {
            return Err(invalid_data(
                "service-trust distributor URL must be an origin without a path",
            ));
        }
        Ok(Self {
            distributor_url,
            cache_path,
            poll_interval,
            request_timeout,
            max_backoff,
            tls,
        })
    }

    fn transport_mode(&self) -> TrustTransportMode {
        if self.tls.is_some() {
            TrustTransportMode::MutualTls
        } else {
            TrustTransportMode::InsecureHttp
        }
    }

    fn endpoint(&self, path: &str) -> io::Result<Url> {
        self.distributor_url
            .join(path)
            .map_err(|error| invalid_data(format!("build service-trust endpoint URL: {error}")))
    }
}

pub struct SignedServiceTrustBootstrap {
    pub authorizer: ServiceAuthorizer,
    pub watcher: ServiceTrustWatcher,
}

#[derive(Debug)]
pub struct ServiceTrustWatcher {
    snapshot_path: PathBuf,
    floor_store: ServiceTrustFloorStore,
    cluster_id: String,
    roots: TrustedServiceTrustRootKeyRing,
    local_service_credential: String,
    poll_interval: Duration,
    accepted_floor: PersistedServiceTrustFloor,
    last_observed_bytes: Vec<u8>,
    last_source_error: Option<String>,
}

impl ServiceTrustWatcher {
    pub async fn run(mut self, authorizer: Arc<ServiceAuthorizer>) {
        let mut interval = time::interval(self.poll_interval);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            match self.reload_once(&authorizer) {
                Ok(Some(generation)) => {
                    info!(
                        generation,
                        snapshot_path = %self.snapshot_path.display(),
                        floor_path = %self.floor_store.path().display(),
                        "applied signed service-trust snapshot"
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    let message = error.to_string();
                    authorizer.record_trust_policy_rejection(message.clone());
                    warn!(
                        error = %message,
                        snapshot_path = %self.snapshot_path.display(),
                        "rejected service-trust snapshot; retaining last known good policy"
                    );
                }
            }
        }
    }

    fn reload_once(&mut self, authorizer: &ServiceAuthorizer) -> io::Result<Option<u64>> {
        let bytes = match read_bounded(&self.snapshot_path, MAX_SNAPSHOT_BYTES) {
            Ok(bytes) => {
                self.last_source_error = None;
                bytes
            }
            Err(error) => {
                let message = error.to_string();
                if self.last_source_error.as_deref() == Some(&message) {
                    return Ok(None);
                }
                self.last_source_error = Some(message);
                return Err(error);
            }
        };
        if bytes == self.last_observed_bytes {
            return Ok(None);
        }
        self.last_observed_bytes = bytes.clone();
        let verified = decode_and_verify(&bytes, &self.cluster_id, &self.roots)?;
        validate_local_credential(&verified, &self.local_service_credential)?;
        validate_candidate_floor(&verified, Some(&self.accepted_floor))?;
        if verified.policy.generation == self.accepted_floor.generation {
            return Ok(None);
        }
        let next_floor = PersistedServiceTrustFloor::from_verified(&verified);
        self.floor_store.save(&next_floor)?;
        let generation = verified.policy.generation;
        if !authorizer
            .apply_signed_snapshot(verified, now_ms())
            .map_err(invalid_data)?
        {
            return Err(invalid_data(format!(
                "service-trust generation {generation} was not newer than the active policy"
            )));
        }
        self.accepted_floor = next_floor;
        Ok(Some(generation))
    }
}

#[allow(clippy::too_many_arguments)]
pub fn bootstrap_signed_service_trust(
    snapshot_path: PathBuf,
    floor_path: PathBuf,
    cluster_id: String,
    roots: TrustedServiceTrustRootKeyRing,
    local_service_credential: String,
    poll_interval: Duration,
    max_age_ms: u64,
    max_future_skew_ms: u64,
) -> io::Result<SignedServiceTrustBootstrap> {
    if poll_interval.is_zero() {
        return Err(invalid_data(
            "service-trust snapshot poll interval must be positive",
        ));
    }
    let bytes = read_bounded(&snapshot_path, MAX_SNAPSHOT_BYTES)?;
    let verified = decode_and_verify(&bytes, &cluster_id, &roots)?;
    validate_local_credential(&verified, &local_service_credential)?;
    let floor_store = ServiceTrustFloorStore::new(floor_path);
    let prior_floor = floor_store.load()?;
    validate_candidate_floor(&verified, prior_floor.as_ref())?;
    let accepted_floor = PersistedServiceTrustFloor::from_verified(&verified);
    if prior_floor.as_ref() != Some(&accepted_floor) {
        floor_store.save(&accepted_floor)?;
    }
    let authorizer = ServiceAuthorizer::required_from_signed_snapshot(
        verified,
        roots.trusted_key_ids(),
        roots.revoked_key_ids(),
        max_age_ms,
        max_future_skew_ms,
        now_ms(),
    )
    .map_err(invalid_data)?;
    authorizer.configure_trust_distribution(
        "local-file",
        "local-file",
        TrustTransportMode::NotApplicable,
        false,
        false,
        false,
    );
    Ok(SignedServiceTrustBootstrap {
        authorizer,
        watcher: ServiceTrustWatcher {
            snapshot_path,
            floor_store,
            cluster_id,
            roots,
            local_service_credential,
            poll_interval,
            accepted_floor,
            last_observed_bytes: bytes,
            last_source_error: None,
        },
    })
}

pub struct RemoteSignedServiceTrustBootstrap {
    pub authorizer: ServiceAuthorizer,
    pub watcher: RemoteServiceTrustWatcher,
}

#[derive(Debug)]
pub struct RemoteServiceTrustWatcher {
    client: Client,
    snapshot_url: Url,
    receipt_url: Url,
    distributor_url: String,
    cache_store: ServiceTrustCacheStore,
    floor_store: ServiceTrustFloorStore,
    cluster_id: String,
    roots: TrustedServiceTrustRootKeyRing,
    local_identity: Arc<ServiceSigningIdentity>,
    poll_interval: Duration,
    request_timeout: Duration,
    max_backoff: Duration,
    accepted_floor: PersistedServiceTrustFloor,
    etag: Option<String>,
    pending_receipt: Option<ServiceTrustApplicationReceipt>,
    persistence_failed: bool,
}

#[derive(Debug)]
struct VerifiedCachedSnapshot {
    cache: PersistedServiceTrustCache,
    verified: VerifiedServiceTrustSnapshot,
}

#[derive(Debug)]
enum RemoteFetch {
    NotModified {
        etag: Option<String>,
    },
    Snapshot {
        bytes: Vec<u8>,
        etag: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemoteReloadOutcome {
    NotModified,
    Unchanged,
    Updated(u64),
}

impl RemoteReloadOutcome {
    fn status_name(self) -> &'static str {
        match self {
            Self::NotModified => "not-modified",
            Self::Unchanged => "unchanged",
            Self::Updated(_) => "updated",
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn bootstrap_remote_signed_service_trust(
    config: RemoteServiceTrustConfig,
    floor_path: PathBuf,
    cluster_id: String,
    roots: TrustedServiceTrustRootKeyRing,
    local_identity: Arc<ServiceSigningIdentity>,
    max_age_ms: u64,
    max_future_skew_ms: u64,
) -> io::Result<RemoteSignedServiceTrustBootstrap> {
    validate_remote_state_paths(&config.cache_path, &floor_path)?;
    validate_remote_tls_state_paths(&config.cache_path, &floor_path, config.tls.as_ref())?;
    let client_builder = Client::builder().redirect(Policy::none()).no_proxy();
    let client_builder = if let Some(tls) = config.tls.as_ref() {
        transport_security::configure_mtls_client(client_builder.https_only(true), &tls.paths())?
    } else {
        client_builder
    };
    let client = client_builder
        .build()
        .map_err(|error| io::Error::other(format!("build service-trust HTTP client: {error}")))?;
    let snapshot_url = config.endpoint(SNAPSHOT_ENDPOINT_PATH)?;
    let receipt_url = config.endpoint(RECEIPT_ENDPOINT_PATH)?;
    let distributor_url = config
        .distributor_url
        .as_str()
        .trim_end_matches('/')
        .to_owned();
    let transport_mode = config.transport_mode();
    let cache_store = ServiceTrustCacheStore::new(config.cache_path.clone());
    let floor_store = ServiceTrustFloorStore::new(floor_path);
    let prior_floor = floor_store.load()?;
    let local_service_credential = format!(
        "{}/{}",
        local_identity.service_id(),
        local_identity.credential_id()
    );

    let cached_result = load_verified_cache(
        &cache_store,
        &cluster_id,
        &roots,
        &local_service_credential,
        prior_floor.as_ref(),
    );
    let request_etag = cached_result
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .filter(|cached| cached.cache.distributor_url == distributor_url)
        .and_then(|cached| cached.cache.etag.as_deref());
    let fetched = fetch_remote_snapshot(
        &client,
        snapshot_url.clone(),
        request_etag,
        config.request_timeout,
    )
    .await;

    let mut initial_fetch_outcome = None;
    let mut initial_fetch_error = None;
    let (verified, accepted_floor, etag, bootstrap_source) = match fetched {
        Ok(RemoteFetch::Snapshot { bytes, etag }) => {
            let candidate = (|| {
                let verified = decode_and_verify(&bytes, &cluster_id, &roots)?;
                validate_local_credential(&verified, &local_service_credential)?;
                validate_candidate_floor(&verified, prior_floor.as_ref())?;
                Ok::<_, io::Error>(verified)
            })();
            match candidate {
                Ok(verified) => {
                    persist_remote_acceptance_redacted(
                        &cache_store,
                        &floor_store,
                        &distributor_url,
                        etag.clone(),
                        &verified,
                    )?;
                    initial_fetch_outcome = Some("updated");
                    let floor = PersistedServiceTrustFloor::from_verified(&verified);
                    (verified, floor, etag, "remote")
                }
                Err(error) => {
                    initial_fetch_error = Some(error.to_string());
                    cached_fallback(
                        cached_result,
                        &floor_store,
                        prior_floor.as_ref(),
                        &distributor_url,
                    )?
                }
            }
        }
        Ok(RemoteFetch::NotModified { etag }) => {
            initial_fetch_outcome = Some("not-modified");
            let (verified, floor, cached_etag, source) = cached_fallback(
                cached_result,
                &floor_store,
                prior_floor.as_ref(),
                &distributor_url,
            )?;
            (verified, floor, etag.or(cached_etag), source)
        }
        Err(error) => {
            initial_fetch_error = Some(error.to_string());
            cached_fallback(
                cached_result,
                &floor_store,
                prior_floor.as_ref(),
                &distributor_url,
            )?
        }
    };

    let activated_at_ms = now_ms();
    let authorizer = ServiceAuthorizer::required_from_signed_snapshot(
        verified.clone(),
        roots.trusted_key_ids(),
        roots.revoked_key_ids(),
        max_age_ms,
        max_future_skew_ms,
        activated_at_ms,
    )
    .map_err(invalid_data)?;
    let pending_receipt = local_identity
        .sign_trust_receipt(&verified, activated_at_ms)
        .map_err(|error| invalid_data(format!("sign service-trust receipt: {error}")))?;
    authorizer.configure_trust_distribution(
        "remote-http",
        bootstrap_source,
        transport_mode,
        true,
        true,
        etag.is_some(),
    );
    if let Some(outcome) = initial_fetch_outcome {
        authorizer.record_trust_fetch_success(outcome, etag.is_some(), now_ms());
    }
    if let Some(error) = initial_fetch_error {
        authorizer.record_trust_policy_rejection(error);
        authorizer.record_trust_fetch_failure(now_ms());
    }

    Ok(RemoteSignedServiceTrustBootstrap {
        authorizer,
        watcher: RemoteServiceTrustWatcher {
            client,
            snapshot_url,
            receipt_url,
            distributor_url,
            cache_store,
            floor_store,
            cluster_id,
            roots,
            local_identity,
            poll_interval: config.poll_interval,
            request_timeout: config.request_timeout,
            max_backoff: config.max_backoff,
            accepted_floor,
            etag,
            pending_receipt: Some(pending_receipt),
            persistence_failed: false,
        },
    })
}

impl RemoteServiceTrustWatcher {
    pub async fn run(mut self, authorizer: Arc<ServiceAuthorizer>) {
        self.post_pending_receipt(&authorizer).await;
        let mut delay = self.poll_interval;
        loop {
            time::sleep(delay).await;
            match self.reload_once(&authorizer).await {
                Ok(outcome) => {
                    authorizer.record_trust_fetch_success(
                        outcome.status_name(),
                        self.etag.is_some(),
                        now_ms(),
                    );
                    delay = self.poll_interval;
                    match outcome {
                        RemoteReloadOutcome::Updated(generation) => {
                            info!(
                                generation,
                                cache_path = %self.cache_store.path().display(),
                                floor_path = %self.floor_store.path().display(),
                                "applied remotely distributed service-trust snapshot"
                            );
                        }
                        RemoteReloadOutcome::NotModified | RemoteReloadOutcome::Unchanged => {}
                    }
                }
                Err(error) => {
                    let message = error.to_string();
                    authorizer.record_trust_policy_rejection(message.clone());
                    let failures = authorizer.record_trust_fetch_failure(now_ms());
                    delay = deterministic_backoff(self.poll_interval, self.max_backoff, failures);
                    warn!(
                        error = %message,
                        retry_delay_ms = delay.as_millis(),
                        "remote service-trust fetch failed; retaining last known good policy"
                    );
                }
            }
            self.post_pending_receipt(&authorizer).await;
        }
    }

    async fn reload_once(
        &mut self,
        authorizer: &ServiceAuthorizer,
    ) -> io::Result<RemoteReloadOutcome> {
        if self.persistence_failed {
            return Err(io::Error::other(
                "remote service-trust persistence previously failed; restart is required before further updates",
            ));
        }
        match fetch_remote_snapshot(
            &self.client,
            self.snapshot_url.clone(),
            self.etag.as_deref(),
            self.request_timeout,
        )
        .await?
        {
            RemoteFetch::NotModified { etag } => {
                self.accept_not_modified(etag)?;
                Ok(RemoteReloadOutcome::NotModified)
            }
            RemoteFetch::Snapshot { bytes, etag } => {
                let verified = decode_and_verify(&bytes, &self.cluster_id, &self.roots)?;
                let local_service_credential = format!(
                    "{}/{}",
                    self.local_identity.service_id(),
                    self.local_identity.credential_id()
                );
                validate_local_credential(&verified, &local_service_credential)?;
                validate_candidate_floor(&verified, Some(&self.accepted_floor))?;
                let generation = verified.policy.generation;
                if let Err(error) = persist_remote_acceptance_redacted(
                    &self.cache_store,
                    &self.floor_store,
                    &self.distributor_url,
                    etag.clone(),
                    &verified,
                ) {
                    self.persistence_failed = true;
                    return Err(error);
                }
                self.etag = etag;
                if generation == self.accepted_floor.generation {
                    self.pending_receipt = Some(
                        self.local_identity
                            .sign_trust_receipt(&verified, now_ms())
                            .map_err(|error| {
                                invalid_data(format!("sign service-trust receipt: {error}"))
                            })?,
                    );
                    return Ok(RemoteReloadOutcome::Unchanged);
                }
                let next_floor = PersistedServiceTrustFloor::from_verified(&verified);
                let activated_at_ms = now_ms();
                if !authorizer
                    .apply_signed_snapshot(verified.clone(), activated_at_ms)
                    .map_err(invalid_data)?
                {
                    return Err(invalid_data(format!(
                        "service-trust generation {generation} was not newer than the active policy"
                    )));
                }
                self.accepted_floor = next_floor;
                self.pending_receipt = None;
                self.pending_receipt = Some(
                    self.local_identity
                        .sign_trust_receipt(&verified, activated_at_ms)
                        .map_err(|error| {
                            invalid_data(format!("sign service-trust receipt: {error}"))
                        })?,
                );
                Ok(RemoteReloadOutcome::Updated(generation))
            }
        }
    }

    fn accept_not_modified(&mut self, response_etag: Option<String>) -> io::Result<()> {
        let Some(response_etag) = response_etag else {
            return Ok(());
        };
        if self.etag.as_deref() == Some(response_etag.as_str()) {
            return Ok(());
        }
        let local_service_credential = format!(
            "{}/{}",
            self.local_identity.service_id(),
            self.local_identity.credential_id()
        );
        let cached = match load_verified_cache(
            &self.cache_store,
            &self.cluster_id,
            &self.roots,
            &local_service_credential,
            Some(&self.accepted_floor),
        ) {
            Ok(cached) => cached,
            Err(error) => {
                warn!(
                    error = %error,
                    cache_path = %self.cache_store.path().display(),
                    "durable service-trust cache verification failed after 304"
                );
                return Err(invalid_data(
                    "durable service-trust cache could not be verified after 304",
                ));
            }
        }
        .ok_or_else(|| {
            invalid_data("remote service-trust returned 304 without a durable cached snapshot")
        })?;
        let cached_floor = PersistedServiceTrustFloor::from_verified(&cached.verified);
        if cached_floor != self.accepted_floor {
            return Err(invalid_data(
                "remote service-trust returned 304 but the durable cache does not match the active snapshot",
            ));
        }
        let mut cache = cached.cache;
        cache.distributor_url.clone_from(&self.distributor_url);
        cache.etag = Some(response_etag.clone());
        if let Err(error) = self.cache_store.save(&cache) {
            warn!(
                error = %error,
                cache_path = %self.cache_store.path().display(),
                "durable service-trust ETag update failed"
            );
            return Err(invalid_data(
                "durable service-trust ETag update could not be persisted",
            ));
        }
        self.etag = Some(response_etag);
        Ok(())
    }

    async fn post_pending_receipt(&mut self, authorizer: &ServiceAuthorizer) {
        let Some(receipt) = self.pending_receipt.as_ref() else {
            return;
        };
        let generation = receipt.payload.generation;
        let result = self
            .client
            .post(self.receipt_url.clone())
            .timeout(self.request_timeout)
            .json(receipt)
            .send()
            .await;
        match result {
            Ok(response)
                if response.status() == StatusCode::OK
                    || response.status() == StatusCode::CREATED =>
            {
                authorizer.record_trust_receipt_success(generation, now_ms());
                self.pending_receipt = None;
            }
            Ok(response) => {
                let message = format!(
                    "service-trust receipt endpoint returned HTTP {}",
                    response.status().as_u16()
                );
                authorizer.record_trust_receipt_failure(message.clone());
                warn!(generation, error = %message, "service-trust receipt was not accepted");
            }
            Err(error) => {
                let message = format!(
                    "post service-trust receipt: {}",
                    request_error_summary(&error)
                );
                authorizer.record_trust_receipt_failure(message.clone());
                warn!(generation, error = %message, "service-trust receipt delivery failed");
            }
        }
    }
}

fn cached_fallback(
    cached_result: io::Result<Option<VerifiedCachedSnapshot>>,
    floor_store: &ServiceTrustFloorStore,
    prior_floor: Option<&PersistedServiceTrustFloor>,
    distributor_url: &str,
) -> io::Result<(
    VerifiedServiceTrustSnapshot,
    PersistedServiceTrustFloor,
    Option<String>,
    &'static str,
)> {
    let cached = cached_result?.ok_or_else(|| {
        invalid_data("remote service-trust is unavailable and no valid cache exists")
    })?;
    let accepted_floor = PersistedServiceTrustFloor::from_verified(&cached.verified);
    if prior_floor != Some(&accepted_floor) {
        floor_store.save(&accepted_floor)?;
    }
    let etag = (cached.cache.distributor_url == distributor_url)
        .then_some(cached.cache.etag)
        .flatten();
    Ok((cached.verified, accepted_floor, etag, "cache"))
}

fn load_verified_cache(
    cache_store: &ServiceTrustCacheStore,
    cluster_id: &str,
    roots: &TrustedServiceTrustRootKeyRing,
    local_service_credential: &str,
    floor: Option<&PersistedServiceTrustFloor>,
) -> io::Result<Option<VerifiedCachedSnapshot>> {
    let Some(cache) = cache_store.load()? else {
        return Ok(None);
    };
    let snapshot_bytes = serde_json::to_vec(&cache.snapshot).map_err(|error| {
        io::Error::other(format!("encode cached service-trust snapshot: {error}"))
    })?;
    if u64::try_from(snapshot_bytes.len()).unwrap_or(u64::MAX) > MAX_SNAPSHOT_BYTES {
        return Err(invalid_data(format!(
            "cached service-trust snapshot exceeds {MAX_SNAPSHOT_BYTES} bytes"
        )));
    }
    let verified = decode_and_verify(&snapshot_bytes, cluster_id, roots)?;
    validate_local_credential(&verified, local_service_credential)?;
    validate_candidate_floor(&verified, floor)?;
    Ok(Some(VerifiedCachedSnapshot { cache, verified }))
}

fn persist_remote_acceptance(
    cache_store: &ServiceTrustCacheStore,
    floor_store: &ServiceTrustFloorStore,
    distributor_url: &str,
    etag: Option<String>,
    snapshot: &VerifiedServiceTrustSnapshot,
) -> io::Result<()> {
    let cache = PersistedServiceTrustCache {
        schema: SERVICE_TRUST_CACHE_SCHEMA.to_owned(),
        distributor_url: distributor_url.to_owned(),
        etag,
        snapshot: ServiceTrustSnapshot {
            policy: snapshot.policy.clone(),
            authentication: service_auth::ServiceTrustSnapshotAuthentication {
                schema: service_auth::SERVICE_TRUST_AUTHENTICATION_SCHEMA.to_owned(),
                algorithm: service_auth::SIGNATURE_ALGORITHM.to_owned(),
                key_id: snapshot.signing_key_id.clone(),
                signature: snapshot.signature.clone(),
            },
        },
    };
    cache_store.save(&cache)?;
    floor_store.save(&PersistedServiceTrustFloor::from_verified(snapshot))?;
    Ok(())
}

fn persist_remote_acceptance_redacted(
    cache_store: &ServiceTrustCacheStore,
    floor_store: &ServiceTrustFloorStore,
    distributor_url: &str,
    etag: Option<String>,
    snapshot: &VerifiedServiceTrustSnapshot,
) -> io::Result<()> {
    persist_remote_acceptance(cache_store, floor_store, distributor_url, etag, snapshot).map_err(
        |error| {
            warn!(
                error = %error,
                cache_path = %cache_store.path().display(),
                floor_path = %floor_store.path().display(),
                "durable service-trust acceptance persistence failed"
            );
            invalid_data("durable service-trust cache and floor could not be persisted")
        },
    )
}

async fn fetch_remote_snapshot(
    client: &Client,
    snapshot_url: Url,
    etag: Option<&str>,
    request_timeout: Duration,
) -> io::Result<RemoteFetch> {
    let mut request = client.get(snapshot_url).timeout(request_timeout);
    if let Some(etag) = etag {
        request = request.header(IF_NONE_MATCH, etag);
    }
    let response = request.send().await.map_err(|error| {
        io::Error::other(format!(
            "fetch remote service-trust snapshot: {}",
            request_error_summary(&error)
        ))
    })?;
    let status = response.status();
    let response_etag = bounded_etag(response.headers().get(ETAG))?;
    if status == StatusCode::NOT_MODIFIED {
        if etag.is_none() {
            return Err(invalid_data(
                "service-trust snapshot endpoint returned 304 to an unconditional request",
            ));
        }
        return Ok(RemoteFetch::NotModified {
            etag: response_etag,
        });
    }
    if status != StatusCode::OK {
        return Err(io::Error::other(format!(
            "service-trust snapshot endpoint returned HTTP {}",
            status.as_u16()
        )));
    }
    if let Some(length) = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        && length > MAX_SNAPSHOT_BYTES
    {
        return Err(invalid_data(format!(
            "remote service-trust snapshot declares {length} bytes; maximum is {MAX_SNAPSHOT_BYTES}"
        )));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            io::Error::other(format!(
                "read remote service-trust snapshot: {}",
                request_error_summary(&error)
            ))
        })?;
        let next_length = bytes.len().saturating_add(chunk.len());
        if u64::try_from(next_length).unwrap_or(u64::MAX) > MAX_SNAPSHOT_BYTES {
            return Err(invalid_data(format!(
                "remote service-trust snapshot exceeds {MAX_SNAPSHOT_BYTES} bytes"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(RemoteFetch::Snapshot {
        bytes,
        etag: response_etag,
    })
}

fn bounded_etag(value: Option<&reqwest::header::HeaderValue>) -> io::Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| invalid_data("service-trust ETag is not valid visible ASCII"))?;
    if value.is_empty()
        || value.len() > MAX_ETAG_BYTES
        || !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        return Err(invalid_data(format!(
            "service-trust ETag must contain 1..={MAX_ETAG_BYTES} bytes"
        )));
    }
    Ok(Some(value.to_owned()))
}

fn deterministic_backoff(base: Duration, maximum: Duration, failures: u64) -> Duration {
    let shifts = failures.saturating_sub(1).min(31) as u32;
    let multiplier = 1_u32 << shifts;
    base.checked_mul(multiplier).unwrap_or(maximum).min(maximum)
}

fn request_error_summary(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "request timed out"
    } else if error.is_connect() {
        "connection failed"
    } else if error.is_request() {
        "request failed"
    } else if error.is_body() {
        "response body failed"
    } else {
        "HTTP transport failed"
    }
}

fn validate_local_credential(
    snapshot: &VerifiedServiceTrustSnapshot,
    local_service_credential: &str,
) -> io::Result<()> {
    if !snapshot
        .compiled
        .keys
        .trusted_service_credentials()
        .iter()
        .any(|credential| credential == local_service_credential)
    {
        return Err(invalid_data(format!(
            "service-trust policy generation {} does not trust local signing credential '{local_service_credential}'",
            snapshot.policy.generation
        )));
    }
    if snapshot
        .compiled
        .keys
        .revoked_service_credentials()
        .iter()
        .any(|credential| credential == local_service_credential)
    {
        return Err(invalid_data(format!(
            "service-trust policy generation {} revokes local signing credential '{local_service_credential}'",
            snapshot.policy.generation
        )));
    }
    let local_service_id = local_service_credential
        .split_once('/')
        .map_or(local_service_credential, |(service_id, _)| service_id);
    if snapshot
        .compiled
        .keys
        .revoked_service_ids()
        .iter()
        .any(|service_id| service_id == local_service_id)
    {
        return Err(invalid_data(format!(
            "service-trust policy generation {} revokes local service identity '{local_service_id}'",
            snapshot.policy.generation
        )));
    }
    Ok(())
}

fn decode_and_verify(
    bytes: &[u8],
    expected_cluster_id: &str,
    roots: &TrustedServiceTrustRootKeyRing,
) -> io::Result<VerifiedServiceTrustSnapshot> {
    let snapshot = serde_json::from_slice::<ServiceTrustSnapshot>(bytes)
        .map_err(|error| invalid_data(format!("cannot decode service-trust snapshot: {error}")))?;
    if snapshot.policy.cluster_id != expected_cluster_id {
        return Err(invalid_data(format!(
            "service-trust cluster mismatch: expected '{expected_cluster_id}', observed '{}'",
            snapshot.policy.cluster_id
        )));
    }
    roots.verify(&snapshot).map_err(invalid_data)
}

fn validate_candidate_floor(
    snapshot: &VerifiedServiceTrustSnapshot,
    floor: Option<&PersistedServiceTrustFloor>,
) -> io::Result<()> {
    let Some(floor) = floor else {
        return Ok(());
    };
    if snapshot.policy.cluster_id != floor.cluster_id {
        return Err(invalid_data(format!(
            "service-trust floor cluster '{}' does not match snapshot cluster '{}'",
            floor.cluster_id, snapshot.policy.cluster_id
        )));
    }
    if snapshot.policy.generation < floor.generation {
        return Err(invalid_data(format!(
            "service-trust rollback rejected: snapshot generation {} is below durable floor {}",
            snapshot.policy.generation, floor.generation
        )));
    }
    if snapshot.policy.generation == floor.generation
        && (snapshot.signing_key_id != floor.signing_key_id
            || snapshot.signature != floor.signature)
    {
        return Err(invalid_data(format!(
            "service-trust generation {} conflicts with the durable accepted snapshot",
            snapshot.policy.generation
        )));
    }
    Ok(())
}

fn validate_floor(floor: &PersistedServiceTrustFloor) -> io::Result<()> {
    if floor.schema != SERVICE_TRUST_FLOOR_SCHEMA {
        return Err(invalid_data(format!(
            "unsupported service-trust floor schema '{}'; expected '{SERVICE_TRUST_FLOOR_SCHEMA}'",
            floor.schema
        )));
    }
    if floor.cluster_id.trim().is_empty()
        || floor.generation == 0
        || floor.signing_key_id.trim().is_empty()
        || floor.signature.trim().is_empty()
    {
        return Err(invalid_data(
            "service-trust floor cluster, generation, key ID, and signature must be present",
        ));
    }
    Ok(())
}

fn validate_cache(cache: &PersistedServiceTrustCache) -> io::Result<()> {
    if cache.schema != SERVICE_TRUST_CACHE_SCHEMA {
        return Err(invalid_data(format!(
            "unsupported service-trust cache schema '{}'; expected '{SERVICE_TRUST_CACHE_SCHEMA}'",
            cache.schema
        )));
    }
    if cache.distributor_url.is_empty() || cache.distributor_url.len() > MAX_DISTRIBUTOR_URL_BYTES {
        return Err(invalid_data(format!(
            "cached service-trust distributor URL must contain 1..={MAX_DISTRIBUTOR_URL_BYTES} bytes"
        )));
    }
    let url = Url::parse(&cache.distributor_url)
        .map_err(|error| invalid_data(format!("invalid cached distributor URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || (url.path() != "/" && !url.path().is_empty())
    {
        return Err(invalid_data(
            "cached service-trust distributor URL is not a valid credential-free HTTP(S) origin",
        ));
    }
    if let Some(etag) = cache.etag.as_deref() {
        if etag.is_empty()
            || etag.len() > MAX_ETAG_BYTES
            || !etag.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        {
            return Err(invalid_data(format!(
                "cached service-trust ETag must contain 1..={MAX_ETAG_BYTES} bytes"
            )));
        }
        reqwest::header::HeaderValue::from_str(etag)
            .map_err(|_| invalid_data("cached service-trust ETag is not valid visible ASCII"))?;
    }
    Ok(())
}

fn validate_remote_state_paths(cache_path: &Path, floor_path: &Path) -> io::Result<()> {
    let cache_target = resolve_path_target(cache_path)?;
    let floor_target = resolve_path_target(floor_path)?;
    if cache_target == floor_target {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "service-trust cache path and durable floor path resolve to the same target",
        ));
    }
    Ok(())
}

fn validate_remote_tls_state_paths(
    cache_path: &Path,
    floor_path: &Path,
    tls: Option<&RemoteServiceTrustTlsConfig>,
) -> io::Result<()> {
    let Some(tls) = tls else {
        return Ok(());
    };
    let cache_target = resolve_path_target(cache_path)?;
    let floor_target = resolve_path_target(floor_path)?;
    for tls_path in [
        &tls.ca_cert_path,
        &tls.client_cert_path,
        &tls.client_key_path,
    ] {
        let tls_target = resolve_path_target(tls_path)?;
        if tls_target == cache_target || tls_target == floor_target {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "service-trust cache and floor paths must not alias TLS certificate or key paths",
            ));
        }
    }
    Ok(())
}

fn resolve_path_target(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let absolute = normalize_path(&absolute);
    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(ancestor) {
            Ok(mut resolved) => {
                for component in suffix.iter().rev() {
                    resolved.push(component);
                }
                return Ok(normalize_path(&resolved));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                let component = ancestor.file_name().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("cannot resolve state path '{}'", path.display()),
                    )
                })?;
                suffix.push(component.to_os_string());
                ancestor = ancestor.parent().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("cannot resolve state path '{}'", path.display()),
                    )
                })?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::Normal(value) => normalized.push(value),
        }
    }
    normalized
}

fn write_atomic(path: &Path, label: &str, bytes: &[u8]) -> io::Result<()> {
    write_atomic_with_sequence(path, label, bytes, &NEXT_ATOMIC_TEMP_SEQUENCE)
}

fn write_atomic_with_sequence(
    path: &Path,
    label: &str,
    bytes: &[u8],
    sequence: &std::sync::atomic::AtomicU64,
) -> io::Result<()> {
    let parent = path.parent().filter(|path| !path.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent)?;
    }
    let directory = parent.unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| invalid_data(format!("{label} path needs a UTF-8 file name")))?;
    let (temporary, mut file) =
        open_unique_atomic_temporary(directory, file_name, label, sequence)?;
    let write_result = file
        .write_all(bytes)
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    sync_directory(directory)
}

fn open_unique_atomic_temporary(
    directory: &Path,
    file_name: &str,
    label: &str,
    sequence: &std::sync::atomic::AtomicU64,
) -> io::Result<(PathBuf, File)> {
    for _ in 0..MAX_ATOMIC_TEMP_ATTEMPTS {
        let nonce = sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temporary = atomic_temporary_path(directory, file_name, nonce);
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "could not allocate a unique temporary file for {label} after {MAX_ATOMIC_TEMP_ATTEMPTS} attempts"
        ),
    ))
}

fn atomic_temporary_path(directory: &Path, file_name: &str, nonce: u64) -> PathBuf {
    directory.join(format!(".{file_name}.{}.{}.tmp", process::id(), nonce))
}

fn read_bounded(path: &Path, maximum: u64) -> io::Result<Vec<u8>> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > maximum {
        return Err(invalid_data(format!(
            "service-trust snapshot {} is {} bytes; maximum is {maximum}",
            path.display(),
            metadata.len()
        )));
    }
    fs::read(path)
}

fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use axum::{
        Json, Router,
        body::Body,
        extract::State,
        http::{HeaderMap, header::LOCATION},
        response::Response,
        routing::{get, post},
    };
    use axum_server::tls_rustls::RustlsConfig;
    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
        KeyUsagePurpose,
    };
    use service_auth::{
        SERVICE_TRUST_POLICY_SCHEMA, ServiceSigningIdentity, ServiceTrustCredential,
        ServiceTrustPolicyPayload, ServiceTrustRootSigningIdentity,
    };
    use tokio::{net::TcpListener, task::JoinHandle};

    const ROOT_SEED: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";
    const SERVICE_SEED: &str = "TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs=";
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "inferlab-service-trust-{}-{name}-{sequence}",
                process::id()
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

    #[derive(Clone, Debug)]
    enum SnapshotResponseMode {
        Conditional,
        AlwaysOk,
        Error(StatusCode),
        Oversized,
        Delayed(Duration),
        NotModified,
        RedirectTo(String),
    }

    #[derive(Debug)]
    struct DistributorState {
        snapshot: Vec<u8>,
        etag: String,
        mode: SnapshotResponseMode,
        observed_etags: Vec<Option<String>>,
        receipts: Vec<ServiceTrustApplicationReceipt>,
    }

    struct TestDistributor {
        url: String,
        state: Arc<Mutex<DistributorState>>,
        task: JoinHandle<()>,
    }

    struct TestTlsDistributor {
        url: String,
        address_url: String,
        state: Arc<Mutex<DistributorState>>,
        task: JoinHandle<()>,
    }

    impl TestDistributor {
        async fn start(snapshot: &ServiceTrustSnapshot, etag: &str) -> Self {
            let state = Arc::new(Mutex::new(DistributorState {
                snapshot: serde_json::to_vec(snapshot).expect("snapshot JSON"),
                etag: etag.to_owned(),
                mode: SnapshotResponseMode::Conditional,
                observed_etags: Vec::new(),
                receipts: Vec::new(),
            }));
            let app = Router::new()
                .route(SNAPSHOT_ENDPOINT_PATH, get(serve_snapshot))
                .route(RECEIPT_ENDPOINT_PATH, post(receive_receipt))
                .with_state(Arc::clone(&state));
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind distributor");
            let address = listener.local_addr().expect("distributor address");
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.expect("serve distributor");
            });
            Self {
                url: format!("http://{address}"),
                state,
                task,
            }
        }

        fn set_snapshot(&self, snapshot: &ServiceTrustSnapshot, etag: &str) {
            let mut state = self.state.lock().expect("distributor state");
            state.snapshot = serde_json::to_vec(snapshot).expect("snapshot JSON");
            state.etag = etag.to_owned();
            state.mode = SnapshotResponseMode::Conditional;
        }

        fn set_mode(&self, mode: SnapshotResponseMode) {
            self.state.lock().expect("distributor state").mode = mode;
        }

        fn receipts(&self) -> Vec<ServiceTrustApplicationReceipt> {
            self.state
                .lock()
                .expect("distributor state")
                .receipts
                .clone()
        }

        fn observed_etags(&self) -> Vec<Option<String>> {
            self.state
                .lock()
                .expect("distributor state")
                .observed_etags
                .clone()
        }

        fn stop(&self) {
            self.task.abort();
        }
    }

    impl Drop for TestDistributor {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl TestTlsDistributor {
        async fn start(
            snapshot: &ServiceTrustSnapshot,
            etag: &str,
            paths: &transport_security::MtlsServerPaths,
        ) -> Self {
            let state = Arc::new(Mutex::new(DistributorState {
                snapshot: serde_json::to_vec(snapshot).expect("snapshot JSON"),
                etag: etag.to_owned(),
                mode: SnapshotResponseMode::Conditional,
                observed_etags: Vec::new(),
                receipts: Vec::new(),
            }));
            let app = Router::new()
                .route(SNAPSHOT_ENDPOINT_PATH, get(serve_snapshot))
                .route(RECEIPT_ENDPOINT_PATH, post(receive_receipt))
                .with_state(Arc::clone(&state));
            let listener =
                std::net::TcpListener::bind("127.0.0.1:0").expect("bind TLS distributor");
            listener
                .set_nonblocking(true)
                .expect("nonblocking TLS listener");
            let address = listener.local_addr().expect("TLS distributor address");
            let server_config =
                transport_security::load_mtls_server_config(paths).expect("TLS server config");
            let rustls = RustlsConfig::from_config(Arc::new(server_config));
            let task = tokio::spawn(async move {
                axum_server::from_tcp_rustls(listener, rustls)
                    .expect("TLS distributor server")
                    .serve(app.into_make_service())
                    .await
                    .expect("serve TLS distributor");
            });
            Self {
                url: format!("https://localhost:{}", address.port()),
                address_url: format!("https://127.0.0.1:{}", address.port()),
                state,
                task,
            }
        }

        fn set_mode(&self, mode: SnapshotResponseMode) {
            self.state.lock().expect("TLS distributor state").mode = mode;
        }

        fn receipts(&self) -> Vec<ServiceTrustApplicationReceipt> {
            self.state
                .lock()
                .expect("TLS distributor state")
                .receipts
                .clone()
        }
    }

    impl Drop for TestTlsDistributor {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    struct TestMtlsMaterial {
        server: transport_security::MtlsServerPaths,
        client: transport_security::MtlsClientPaths,
    }

    fn test_mtls_material(directory: &TestDirectory, prefix: &str) -> TestMtlsMaterial {
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca_key = KeyPair::generate().expect("CA key");
        let ca_cert = ca_params.self_signed(&ca_key).expect("CA certificate");
        let issuer = Issuer::new(ca_params, ca_key);

        let mut server_params =
            CertificateParams::new(vec!["localhost".to_owned()]).expect("server params");
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_key = KeyPair::generate().expect("server key");
        let server_cert = server_params
            .signed_by(&server_key, &issuer)
            .expect("server certificate");

        let mut client_params =
            CertificateParams::new(Vec::<String>::new()).expect("client params");
        client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let client_key = KeyPair::generate().expect("client key");
        let client_cert = client_params
            .signed_by(&client_key, &issuer)
            .expect("client certificate");

        let ca_path = directory.path(&format!("{prefix}-ca.pem"));
        let server_cert_path = directory.path(&format!("{prefix}-server.pem"));
        let server_key_path = directory.path(&format!("{prefix}-server-key.pem"));
        let client_cert_path = directory.path(&format!("{prefix}-client.pem"));
        let client_key_path = directory.path(&format!("{prefix}-client-key.pem"));
        fs::write(&ca_path, ca_cert.pem()).expect("write CA");
        fs::write(&server_cert_path, server_cert.pem()).expect("write server certificate");
        fs::write(&server_key_path, server_key.serialize_pem()).expect("write server key");
        fs::write(&client_cert_path, client_cert.pem()).expect("write client certificate");
        fs::write(&client_key_path, client_key.serialize_pem()).expect("write client key");

        TestMtlsMaterial {
            server: transport_security::MtlsServerPaths {
                certificate_chain: server_cert_path,
                private_key: server_key_path,
                client_ca: ca_path.clone(),
            },
            client: transport_security::MtlsClientPaths {
                server_ca: ca_path,
                certificate_chain: client_cert_path,
                private_key: client_key_path,
            },
        }
    }

    async fn serve_snapshot(
        State(state): State<Arc<Mutex<DistributorState>>>,
        headers: HeaderMap,
    ) -> Response {
        let observed_etag = headers
            .get(IF_NONE_MATCH)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let (mode, snapshot, etag) = {
            let mut state = state.lock().expect("distributor state");
            state.observed_etags.push(observed_etag.clone());
            (
                state.mode.clone(),
                state.snapshot.clone(),
                state.etag.clone(),
            )
        };
        if let SnapshotResponseMode::Delayed(delay) = mode {
            time::sleep(delay).await;
            return response(StatusCode::OK, snapshot, Some(&etag));
        }
        match mode {
            SnapshotResponseMode::Conditional
                if observed_etag.as_deref() == Some(etag.as_str()) =>
            {
                response(StatusCode::NOT_MODIFIED, Vec::new(), Some(&etag))
            }
            SnapshotResponseMode::Conditional | SnapshotResponseMode::AlwaysOk => {
                response(StatusCode::OK, snapshot, Some(&etag))
            }
            SnapshotResponseMode::Error(status) => response(status, Vec::new(), None),
            SnapshotResponseMode::Oversized => response(
                StatusCode::OK,
                vec![b'x'; usize::try_from(MAX_SNAPSHOT_BYTES).expect("bound") + 1],
                Some(&etag),
            ),
            SnapshotResponseMode::NotModified => {
                response(StatusCode::NOT_MODIFIED, Vec::new(), Some(&etag))
            }
            SnapshotResponseMode::RedirectTo(location) => Response::builder()
                .status(StatusCode::TEMPORARY_REDIRECT)
                .header(LOCATION, location)
                .body(Body::empty())
                .expect("redirect response"),
            SnapshotResponseMode::Delayed(_) => unreachable!("handled above"),
        }
    }

    async fn receive_receipt(
        State(state): State<Arc<Mutex<DistributorState>>>,
        Json(receipt): Json<ServiceTrustApplicationReceipt>,
    ) -> StatusCode {
        state
            .lock()
            .expect("distributor state")
            .receipts
            .push(receipt);
        StatusCode::CREATED
    }

    fn response(status: StatusCode, body: Vec<u8>, etag: Option<&str>) -> Response {
        let mut builder = Response::builder().status(status);
        if let Some(etag) = etag {
            builder = builder.header(ETAG, etag);
        }
        builder.body(Body::from(body)).expect("HTTP response")
    }

    fn signed(generation: u64) -> (ServiceTrustSnapshot, TrustedServiceTrustRootKeyRing) {
        let root = ServiceTrustRootSigningIdentity::from_base64_seed("trust-root-a", ROOT_SEED)
            .expect("root");
        let service = ServiceSigningIdentity::from_base64_seed_with_credential(
            "gateway-primary",
            "key-a",
            SERVICE_SEED,
        )
        .expect("service");
        let policy = ServiceTrustPolicyPayload {
            schema: SERVICE_TRUST_POLICY_SCHEMA.to_owned(),
            cluster_id: "inferlab-primary".to_owned(),
            generation,
            issued_at_ms: 1_700_000_000_000 + generation,
            trusted_credentials: vec![ServiceTrustCredential {
                service_id: "gateway-primary".to_owned(),
                credential_id: "key-a".to_owned(),
                public_key_base64: service.public_key_base64(),
            }],
            revoked_service_ids: Vec::new(),
            revoked_credentials: Vec::new(),
            gateway_service_ids: vec!["gateway-primary".to_owned()],
        };
        let snapshot = root.sign(&policy).expect("snapshot");
        let roots = TrustedServiceTrustRootKeyRing::parse(
            &format!("trust-root-a={}", root.public_key_base64()),
            "",
        )
        .expect("roots");
        (snapshot, roots)
    }

    fn service_identity() -> Arc<ServiceSigningIdentity> {
        Arc::new(
            ServiceSigningIdentity::from_base64_seed_with_credential(
                "gateway-primary",
                "key-a",
                SERVICE_SEED,
            )
            .expect("service identity"),
        )
    }

    fn remote_config(url: &str, cache_path: PathBuf) -> RemoteServiceTrustConfig {
        RemoteServiceTrustConfig::new(
            url,
            cache_path,
            Duration::from_millis(10),
            Duration::from_millis(100),
            Duration::from_millis(80),
        )
        .expect("remote config")
    }

    fn tls_config() -> RemoteServiceTrustTlsConfig {
        RemoteServiceTrustTlsConfig::from_optional_paths(
            Some(PathBuf::from("ca.pem")),
            Some(PathBuf::from("client.pem")),
            Some(PathBuf::from("client-key.pem")),
        )
        .expect("complete TLS paths")
        .expect("TLS configuration")
    }

    fn mtls_remote_config(
        url: &str,
        cache_path: PathBuf,
        paths: &transport_security::MtlsClientPaths,
    ) -> RemoteServiceTrustConfig {
        let tls = RemoteServiceTrustTlsConfig::from_optional_paths(
            Some(paths.server_ca.clone()),
            Some(paths.certificate_chain.clone()),
            Some(paths.private_key.clone()),
        )
        .expect("valid TLS path set")
        .expect("TLS configured");
        RemoteServiceTrustConfig::new_with_tls(
            url,
            cache_path,
            Duration::from_millis(10),
            Duration::from_millis(500),
            Duration::from_millis(80),
            Some(tls),
        )
        .expect("mTLS remote config")
    }

    #[test]
    fn durable_floor_rejects_rollback_and_same_generation_fork() {
        let (generation_two, roots) = signed(2);
        let verified_two = roots.verify(&generation_two).expect("verified two");
        let floor = PersistedServiceTrustFloor::from_verified(&verified_two);
        let (generation_one, _) = signed(1);
        let verified_one = roots.verify(&generation_one).expect("verified one");
        assert!(
            validate_candidate_floor(&verified_one, Some(&floor))
                .expect_err("rollback")
                .to_string()
                .contains("rollback")
        );

        let mut forked_policy = generation_two.policy.clone();
        forked_policy.issued_at_ms += 1;
        let root = ServiceTrustRootSigningIdentity::from_base64_seed("trust-root-a", ROOT_SEED)
            .expect("root");
        let forked = root.sign(&forked_policy).expect("forked");
        let verified_fork = roots.verify(&forked).expect("verified fork");
        assert!(
            validate_candidate_floor(&verified_fork, Some(&floor))
                .expect_err("fork")
                .to_string()
                .contains("conflicts")
        );
    }

    #[test]
    fn source_selection_and_remote_bounds_are_explicit() {
        assert_eq!(
            select_service_trust_distribution_mode(None, None, false, false).expect("none"),
            ServiceTrustDistributionMode::None
        );
        assert_eq!(
            select_service_trust_distribution_mode(Some("snapshot.json"), None, false, false)
                .expect("local"),
            ServiceTrustDistributionMode::LocalFile
        );
        assert_eq!(
            select_service_trust_distribution_mode(None, Some("http://distributor"), false, false,)
                .expect("remote"),
            ServiceTrustDistributionMode::Remote
        );
        assert!(
            select_service_trust_distribution_mode(
                Some("snapshot.json"),
                Some("http://distributor"),
                false,
                false,
            )
            .expect_err("exclusive")
            .to_string()
            .contains("mutually exclusive")
        );
        assert!(select_service_trust_distribution_mode(None, None, true, false).is_err());
        assert!(
            select_service_trust_distribution_mode(Some("snapshot.json"), None, true, false)
                .is_err()
        );
        assert!(
            select_service_trust_distribution_mode(None, None, false, true).is_err(),
            "an isolated poll interval must fail closed"
        );
        assert_eq!(
            select_service_trust_distribution_mode(Some("snapshot.json"), None, false, true,)
                .expect("poll is valid in local signed mode"),
            ServiceTrustDistributionMode::LocalFile
        );

        let cache = PathBuf::from("cache.json");
        assert!(
            RemoteServiceTrustConfig::new(
                "ftp://distributor",
                cache.clone(),
                Duration::from_millis(10),
                Duration::from_millis(20),
                Duration::from_millis(40),
            )
            .is_err()
        );
        assert!(
            RemoteServiceTrustConfig::new_with_tls(
                "http://distributor",
                cache.clone(),
                Duration::from_millis(10),
                Duration::from_millis(20),
                Duration::from_millis(40),
                Some(tls_config()),
            )
            .is_err(),
            "TLS paths must not be accepted with plaintext HTTP"
        );
        assert!(
            RemoteServiceTrustConfig::new_with_tls(
                "https://distributor",
                cache.clone(),
                Duration::from_millis(10),
                Duration::from_millis(20),
                Duration::from_millis(40),
                Some(tls_config()),
            )
            .is_ok(),
            "HTTPS accepts a complete mTLS configuration"
        );
        assert!(
            RemoteServiceTrustConfig::new_with_tls(
                "ftp://distributor",
                cache.clone(),
                Duration::from_millis(10),
                Duration::from_millis(20),
                Duration::from_millis(40),
                Some(tls_config()),
            )
            .is_err()
        );
        assert!(
            RemoteServiceTrustConfig::new(
                "https://distributor",
                cache.clone(),
                Duration::from_millis(10),
                Duration::from_millis(20),
                Duration::from_millis(40),
            )
            .is_err()
        );
        assert!(
            RemoteServiceTrustConfig::new(
                "http://user:secret@distributor",
                cache.clone(),
                Duration::from_millis(10),
                Duration::from_millis(20),
                Duration::from_millis(40),
            )
            .is_err()
        );
        assert!(
            RemoteServiceTrustConfig::new(
                "http://distributor/path",
                cache.clone(),
                Duration::from_millis(10),
                Duration::from_millis(20),
                Duration::from_millis(40),
            )
            .is_err()
        );
        assert!(
            RemoteServiceTrustConfig::new(
                "http://distributor",
                cache.clone(),
                Duration::from_millis(10),
                Duration::ZERO,
                Duration::from_millis(40),
            )
            .is_err()
        );
        assert!(
            RemoteServiceTrustConfig::new(
                "http://distributor",
                cache.clone(),
                Duration::from_millis(10),
                MAX_REQUEST_TIMEOUT + Duration::from_millis(1),
                Duration::from_millis(40),
            )
            .is_err()
        );
        assert!(
            RemoteServiceTrustConfig::new(
                "http://distributor",
                cache.clone(),
                Duration::from_millis(20),
                Duration::from_millis(20),
                Duration::from_millis(10),
            )
            .is_err()
        );
        assert!(
            RemoteServiceTrustConfig::new(
                "http://distributor",
                cache,
                Duration::from_millis(20),
                Duration::from_millis(20),
                MAX_REMOTE_BACKOFF + Duration::from_millis(1),
            )
            .is_err()
        );
        assert_eq!(
            deterministic_backoff(Duration::from_millis(100), Duration::from_millis(450), 1,),
            Duration::from_millis(100)
        );
        assert_eq!(
            deterministic_backoff(Duration::from_millis(100), Duration::from_millis(450), 3,),
            Duration::from_millis(400)
        );
        assert_eq!(
            deterministic_backoff(Duration::from_millis(100), Duration::from_millis(450), 20,),
            Duration::from_millis(450)
        );
        assert!(
            validate_remote_state_paths(Path::new("same.json"), Path::new("same.json")).is_err()
        );
        let tls_state_collision = RemoteServiceTrustTlsConfig::from_optional_paths(
            Some(PathBuf::from("ca.pem")),
            Some(PathBuf::from("client.pem")),
            Some(PathBuf::from("cache.json")),
        )
        .expect("complete collision paths")
        .expect("TLS collision config");
        assert!(
            validate_remote_tls_state_paths(
                Path::new("cache.json"),
                Path::new("floor.json"),
                Some(&tls_state_collision),
            )
            .is_err(),
            "durable state must never overwrite TLS material"
        );
        let directory = TestDirectory::new("path-aliases");
        assert!(
            validate_remote_state_paths(
                &directory.path("cache.json"),
                &directory.path("missing/../cache.json"),
            )
            .is_err()
        );
        #[cfg(unix)]
        {
            let real_parent = directory.path("real-parent");
            let linked_parent = directory.path("linked-parent");
            fs::create_dir(&real_parent).expect("real parent");
            std::os::unix::fs::symlink(&real_parent, &linked_parent).expect("parent symlink");
            assert!(
                validate_remote_state_paths(
                    &real_parent.join("cache.json"),
                    &linked_parent.join("cache.json"),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn tls_paths_are_strictly_all_or_none() {
        assert!(
            RemoteServiceTrustTlsConfig::from_optional_paths(None, None, None)
                .expect("absent TLS")
                .is_none()
        );
        for paths in [
            (Some(PathBuf::from("ca.pem")), None, None),
            (
                Some(PathBuf::from("ca.pem")),
                Some(PathBuf::from("client.pem")),
                None,
            ),
            (
                None,
                Some(PathBuf::from("client.pem")),
                Some(PathBuf::from("client-key.pem")),
            ),
        ] {
            let error = RemoteServiceTrustTlsConfig::from_optional_paths(paths.0, paths.1, paths.2)
                .expect_err("partial TLS configuration");
            assert!(error.to_string().contains("must be configured together"));
        }
        assert!(
            RemoteServiceTrustTlsConfig::from_optional_paths(
                Some(PathBuf::new()),
                Some(PathBuf::from("client.pem")),
                Some(PathBuf::from("client-key.pem")),
            )
            .is_err(),
            "empty paths fail closed"
        );
        let debug = format!("{:?}", tls_config());
        assert!(!debug.contains("ca.pem"));
        assert!(!debug.contains("client.pem"));
        assert!(!debug.contains("client-key.pem"));
    }

    #[test]
    fn atomic_persistence_skips_an_existing_tls_temp_candidate() {
        let directory = TestDirectory::new("atomic-temp-collision");
        let cache_path = directory.path("cache.json");
        let sequence = AtomicU64::new(0);
        let tls_key_path = atomic_temporary_path(&directory.0, "cache.json", 0);
        let tls_key_bytes = b"protected TLS private-key material";
        fs::write(&tls_key_path, tls_key_bytes).expect("write protected TLS key fixture");

        write_atomic_with_sequence(
            &cache_path,
            "service-trust cache",
            b"durable cache",
            &sequence,
        )
        .expect("persist through a unique temporary file");

        assert_eq!(
            fs::read(&tls_key_path).expect("read protected TLS key fixture"),
            tls_key_bytes,
            "an existing TLS file at the first temporary candidate must never be truncated or renamed"
        );
        assert_eq!(
            fs::read(&cache_path).expect("read durable cache"),
            b"durable cache\n"
        );
        assert_eq!(sequence.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn remote_bootstrap_persists_full_cache_and_restarts_during_outage() {
        let directory = TestDirectory::new("cache-bootstrap");
        let cache_path = directory.path("cache.json");
        let floor_path = directory.path("floor.json");
        let (snapshot, roots) = signed(1);
        let distributor = TestDistributor::start(&snapshot, "\"generation-1\"").await;
        let bootstrap = bootstrap_remote_signed_service_trust(
            remote_config(&distributor.url, cache_path.clone()),
            floor_path.clone(),
            "inferlab-primary".to_owned(),
            roots.clone(),
            service_identity(),
            5_000,
            1_000,
        )
        .await
        .expect("remote bootstrap");
        assert_eq!(bootstrap.authorizer.trust_policy_generation(), Some(1));
        let status = bootstrap.authorizer.status();
        assert_eq!(status.trust_policy_distribution_mode, "remote-http");
        assert_eq!(status.trust_policy_transport_mode, "insecure-http");
        assert!(!status.trust_policy_server_authentication);
        assert!(!status.trust_policy_client_authentication);
        assert_eq!(
            status.trust_policy_bootstrap_source.as_deref(),
            Some("remote")
        );
        assert!(status.trust_policy_remote_configured);
        assert!(status.trust_policy_cache_configured);
        assert!(status.trust_policy_etag_present);
        let encoded_status = serde_json::to_string(&status).expect("status JSON");
        assert!(!encoded_status.contains(&distributor.url));
        assert!(!encoded_status.contains(&cache_path.display().to_string()));
        assert!(!encoded_status.contains("generation-1"));
        let cache = ServiceTrustCacheStore::new(&cache_path)
            .load()
            .expect("load cache")
            .expect("cache");
        assert_eq!(cache.snapshot, snapshot);
        assert_eq!(cache.etag.as_deref(), Some("\"generation-1\""));
        assert_eq!(
            ServiceTrustFloorStore::new(&floor_path)
                .load()
                .expect("load floor")
                .expect("floor")
                .generation,
            1
        );

        distributor.stop();
        tokio::task::yield_now().await;
        let restarted = bootstrap_remote_signed_service_trust(
            remote_config(&distributor.url, cache_path),
            floor_path.clone(),
            "inferlab-primary".to_owned(),
            roots,
            service_identity(),
            5_000,
            1_000,
        )
        .await
        .expect("cached outage bootstrap");
        assert_eq!(restarted.authorizer.trust_policy_generation(), Some(1));
        let status = restarted.authorizer.status();
        assert_eq!(
            status.trust_policy_bootstrap_source.as_deref(),
            Some("cache")
        );
        assert_eq!(status.trust_policy_consecutive_fetch_failures, 1);
        assert_eq!(
            status.trust_policy_last_fetch_outcome.as_deref(),
            Some("error")
        );
        let error = status.last_trust_policy_error.expect("fetch error");
        assert!(!error.contains("127.0.0.1"));
        assert!(!error.contains(SNAPSHOT_ENDPOINT_PATH));
    }

    #[tokio::test]
    async fn cached_etag_is_never_reused_across_distributor_origins() {
        let directory = TestDirectory::new("etag-origin");
        let cache_path = directory.path("cache.json");
        let floor_path = directory.path("floor.json");
        let (snapshot, roots) = signed(1);
        let first = TestDistributor::start(&snapshot, "\"shared-etag\"").await;
        bootstrap_remote_signed_service_trust(
            remote_config(&first.url, cache_path.clone()),
            floor_path.clone(),
            "inferlab-primary".to_owned(),
            roots.clone(),
            service_identity(),
            5_000,
            1_000,
        )
        .await
        .expect("first bootstrap");

        let second = TestDistributor::start(&snapshot, "\"shared-etag\"").await;
        bootstrap_remote_signed_service_trust(
            remote_config(&second.url, cache_path),
            floor_path,
            "inferlab-primary".to_owned(),
            roots,
            service_identity(),
            5_000,
            1_000,
        )
        .await
        .expect("second bootstrap");
        assert_eq!(second.observed_etags(), vec![None]);
    }

    #[tokio::test]
    async fn remote_etag_update_failure_and_receipt_paths_keep_last_known_good() {
        let directory = TestDirectory::new("etag-update");
        let cache_path = directory.path("cache.json");
        let floor_path = directory.path("floor.json");
        let (generation_one, roots) = signed(1);
        let (generation_two, _) = signed(2);
        let distributor = TestDistributor::start(&generation_one, "\"g1\"").await;
        let bootstrap = bootstrap_remote_signed_service_trust(
            remote_config(&distributor.url, cache_path.clone()),
            floor_path.clone(),
            "inferlab-primary".to_owned(),
            roots.clone(),
            service_identity(),
            5_000,
            1_000,
        )
        .await
        .expect("bootstrap");
        let authorizer = Arc::new(bootstrap.authorizer);
        let mut watcher = bootstrap.watcher;

        watcher.post_pending_receipt(&authorizer).await;
        let receipts = distributor.receipts();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].payload.generation, 1);
        roots
            .verify(&generation_one)
            .expect("verified snapshot")
            .compiled
            .keys
            .verify_trust_receipt(&receipts[0])
            .expect("verified receipt");

        assert_eq!(
            watcher.reload_once(&authorizer).await.expect("304"),
            RemoteReloadOutcome::NotModified
        );
        assert!(watcher.pending_receipt.is_none());
        assert_eq!(
            distributor
                .observed_etags()
                .last()
                .and_then(Option::as_deref),
            Some("\"g1\"")
        );

        distributor.set_snapshot(&generation_two, "\"g2\"");
        assert_eq!(
            watcher
                .reload_once(&authorizer)
                .await
                .expect("generation two"),
            RemoteReloadOutcome::Updated(2)
        );
        assert_eq!(authorizer.trust_policy_generation(), Some(2));
        assert_eq!(
            ServiceTrustCacheStore::new(&cache_path)
                .load()
                .expect("load cache")
                .expect("cache")
                .snapshot
                .policy
                .generation,
            2
        );
        assert_eq!(
            ServiceTrustFloorStore::new(&floor_path)
                .load()
                .expect("load floor")
                .expect("floor")
                .generation,
            2
        );
        watcher.post_pending_receipt(&authorizer).await;
        assert_eq!(distributor.receipts().len(), 2);

        distributor.set_mode(SnapshotResponseMode::AlwaysOk);
        assert_eq!(
            watcher.reload_once(&authorizer).await.expect("same 200"),
            RemoteReloadOutcome::Unchanged
        );
        assert_eq!(
            watcher
                .pending_receipt
                .as_ref()
                .map(|receipt| receipt.payload.generation),
            Some(2)
        );

        distributor.set_mode(SnapshotResponseMode::Error(StatusCode::SERVICE_UNAVAILABLE));
        assert!(watcher.reload_once(&authorizer).await.is_err());
        assert_eq!(authorizer.trust_policy_generation(), Some(2));

        let valid_cache = ServiceTrustCacheStore::new(&cache_path)
            .load()
            .expect("load valid cache")
            .expect("valid cache");
        distributor.set_snapshot(&generation_two, "\"g2-refresh-1\"");
        distributor.set_mode(SnapshotResponseMode::NotModified);
        fs::write(&cache_path, b"{").expect("corrupt cache");
        let corrupt = watcher
            .reload_once(&authorizer)
            .await
            .expect_err("304 verifies cache");
        authorizer.record_trust_policy_rejection(corrupt.to_string());
        authorizer.record_trust_fetch_failure(now_ms());
        let status = authorizer.status();
        let status_error = status.last_trust_policy_error.expect("cache error");
        assert!(!status_error.contains(&cache_path.display().to_string()));
        assert!(status_error.contains("could not be verified"));

        ServiceTrustCacheStore::new(&cache_path)
            .save(&valid_cache)
            .expect("restore cache");
        distributor.set_snapshot(&generation_two, "\"g2-refresh-2\"");
        distributor.set_mode(SnapshotResponseMode::NotModified);
        fs::write(
            &cache_path,
            vec![b'x'; usize::try_from(MAX_CACHE_BYTES).expect("cache bound") + 1],
        )
        .expect("oversized cache");
        let oversized = watcher
            .reload_once(&authorizer)
            .await
            .expect_err("304 bounds cache");
        authorizer.record_trust_policy_rejection(oversized.to_string());
        authorizer.record_trust_fetch_failure(now_ms());
        let status_error = authorizer
            .status()
            .last_trust_policy_error
            .expect("oversized cache error");
        assert!(!status_error.contains(&cache_path.display().to_string()));
        assert!(status_error.contains("could not be verified"));
        assert_eq!(authorizer.trust_policy_generation(), Some(2));
    }

    #[tokio::test]
    async fn remote_receiver_rejects_rollback_fork_and_tamper_without_mutating_lkg() {
        let directory = TestDirectory::new("remote-defense");
        let cache_path = directory.path("cache.json");
        let floor_path = directory.path("floor.json");
        let (generation_three, roots) = signed(3);
        let distributor = TestDistributor::start(&generation_three, "\"g3\"").await;
        let bootstrap = bootstrap_remote_signed_service_trust(
            remote_config(&distributor.url, cache_path.clone()),
            floor_path.clone(),
            "inferlab-primary".to_owned(),
            roots,
            service_identity(),
            5_000,
            1_000,
        )
        .await
        .expect("generation three bootstrap");
        let authorizer = bootstrap.authorizer;
        let mut watcher = bootstrap.watcher;
        watcher.post_pending_receipt(&authorizer).await;
        assert_eq!(distributor.receipts().len(), 1);
        let accepted_cache = ServiceTrustCacheStore::new(&cache_path)
            .load()
            .expect("load cache")
            .expect("cache");
        let accepted_floor = ServiceTrustFloorStore::new(&floor_path)
            .load()
            .expect("load floor")
            .expect("floor");

        let (rollback, _) = signed(2);
        let mut fork_policy = generation_three.policy.clone();
        fork_policy.issued_at_ms = fork_policy.issued_at_ms.saturating_add(1);
        let root = ServiceTrustRootSigningIdentity::from_base64_seed("trust-root-a", ROOT_SEED)
            .expect("root");
        let fork = root.sign(&fork_policy).expect("same-generation fork");
        let (mut tampered, _) = signed(4);
        let replacement = if tampered.authentication.signature.starts_with('A') {
            "B"
        } else {
            "A"
        };
        tampered
            .authentication
            .signature
            .replace_range(0..1, replacement);

        for (label, candidate, etag) in [
            ("rollback", rollback, "\"rollback\""),
            ("fork", fork, "\"fork\""),
            ("tamper", tampered, "\"tamper\""),
        ] {
            distributor.set_snapshot(&candidate, etag);
            let error = watcher.reload_once(&authorizer).await.expect_err(label);
            assert!(
                !error.to_string().is_empty(),
                "{label} must produce a diagnostic"
            );
            assert_eq!(authorizer.trust_policy_generation(), Some(3));
            assert_eq!(
                ServiceTrustCacheStore::new(&cache_path)
                    .load()
                    .expect("reload cache")
                    .expect("cache remains")
                    .snapshot,
                accepted_cache.snapshot,
                "{label} must not replace the durable cache"
            );
            assert_eq!(
                ServiceTrustFloorStore::new(&floor_path)
                    .load()
                    .expect("reload floor")
                    .expect("floor remains"),
                accepted_floor,
                "{label} must not replace the durable floor"
            );
            assert!(watcher.pending_receipt.is_none());
            assert_eq!(distributor.receipts().len(), 1);
        }
    }

    #[tokio::test]
    async fn durable_cache_and_floor_precede_runtime_activation() {
        let directory = TestDirectory::new("ordering");
        let cache_path = directory.path("cache.json");
        let floor_path = directory.path("floor.json");
        let blocked_floor_path = directory.path("blocked-floor");
        fs::create_dir(&blocked_floor_path).expect("blocked floor directory");
        let (generation_one, roots) = signed(1);
        let (generation_two, _) = signed(2);
        let distributor = TestDistributor::start(&generation_one, "\"g1\"").await;
        let bootstrap = bootstrap_remote_signed_service_trust(
            remote_config(&distributor.url, cache_path.clone()),
            floor_path.clone(),
            "inferlab-primary".to_owned(),
            roots,
            service_identity(),
            5_000,
            1_000,
        )
        .await
        .expect("bootstrap");
        let authorizer = bootstrap.authorizer;
        let mut watcher = bootstrap.watcher;
        watcher.floor_store = ServiceTrustFloorStore::new(blocked_floor_path);
        distributor.set_snapshot(&generation_two, "\"g2\"");

        assert!(watcher.reload_once(&authorizer).await.is_err());
        assert!(watcher.persistence_failed);
        assert_eq!(authorizer.trust_policy_generation(), Some(1));
        assert_eq!(
            ServiceTrustCacheStore::new(&cache_path)
                .load()
                .expect("load cache")
                .expect("cache")
                .snapshot
                .policy
                .generation,
            2,
            "the full cache is durable before the floor write is attempted"
        );

        let (generation_three, _) = signed(3);
        watcher.floor_store = ServiceTrustFloorStore::new(floor_path);
        distributor.set_snapshot(&generation_three, "\"g3\"");
        let fail_stopped = watcher
            .reload_once(&authorizer)
            .await
            .expect_err("watcher remains fail-stopped");
        assert!(fail_stopped.to_string().contains("restart is required"));
        assert_eq!(authorizer.trust_policy_generation(), Some(1));
        assert_eq!(
            ServiceTrustCacheStore::new(cache_path)
                .load()
                .expect("load fail-stopped cache")
                .expect("fail-stopped cache")
                .snapshot
                .policy
                .generation,
            2,
            "a later generation must not mutate disk after persistence becomes ambiguous"
        );
    }

    #[tokio::test]
    async fn remote_response_and_timeout_bounds_are_enforced_without_url_disclosure() {
        let (snapshot, _) = signed(1);
        let distributor = TestDistributor::start(&snapshot, "\"g1\"").await;
        let client = Client::builder()
            .redirect(Policy::none())
            .build()
            .expect("client");
        let snapshot_url = Url::parse(&format!("{}{}", distributor.url, SNAPSHOT_ENDPOINT_PATH))
            .expect("snapshot URL");

        distributor.set_mode(SnapshotResponseMode::Oversized);
        let oversized =
            fetch_remote_snapshot(&client, snapshot_url.clone(), None, Duration::from_secs(1))
                .await
                .expect_err("oversized response");
        assert!(
            oversized.to_string().contains("maximum") || oversized.to_string().contains("exceeds")
        );

        distributor.set_mode(SnapshotResponseMode::Delayed(Duration::from_millis(100)));
        let timeout = fetch_remote_snapshot(&client, snapshot_url, None, Duration::from_millis(5))
            .await
            .expect_err("timeout");
        let timeout = timeout.to_string();
        assert!(timeout.contains("timed out"));
        assert!(!timeout.contains("127.0.0.1"));
        assert!(!timeout.contains(SNAPSHOT_ENDPOINT_PATH));
    }

    #[tokio::test]
    async fn receipt_transport_errors_are_redacted_and_304_without_cache_fails_closed() {
        let directory = TestDirectory::new("receipt-redaction");
        let (snapshot, roots) = signed(1);
        let distributor = TestDistributor::start(&snapshot, "\"g1\"").await;
        let bootstrap = bootstrap_remote_signed_service_trust(
            remote_config(&distributor.url, directory.path("cache.json")),
            directory.path("floor.json"),
            "inferlab-primary".to_owned(),
            roots,
            service_identity(),
            5_000,
            1_000,
        )
        .await
        .expect("bootstrap");
        let authorizer = bootstrap.authorizer;
        let mut watcher = bootstrap.watcher;
        distributor.stop();
        tokio::task::yield_now().await;
        watcher.post_pending_receipt(&authorizer).await;
        let status = authorizer.status();
        let error = status
            .trust_policy_last_receipt_error
            .expect("receipt error");
        assert!(!error.contains("127.0.0.1"));
        assert!(!error.contains(RECEIPT_ENDPOINT_PATH));

        let no_cache_directory = TestDirectory::new("no-cache-304");
        let (snapshot, roots) = signed(1);
        let not_modified = TestDistributor::start(&snapshot, "\"g1\"").await;
        not_modified.set_mode(SnapshotResponseMode::NotModified);
        let error = bootstrap_remote_signed_service_trust(
            remote_config(&not_modified.url, no_cache_directory.path("cache.json")),
            no_cache_directory.path("floor.json"),
            "inferlab-primary".to_owned(),
            roots,
            service_identity(),
            5_000,
            1_000,
        )
        .await
        .err()
        .expect("304 without cache");
        assert!(error.to_string().contains("no valid cache"));
    }

    #[tokio::test]
    async fn mutual_tls_bootstrap_and_receipt_are_authenticated_and_redacted() {
        let directory = TestDirectory::new("mtls-bootstrap");
        let material = test_mtls_material(&directory, "valid");
        let (snapshot, roots) = signed(1);
        let distributor = TestTlsDistributor::start(&snapshot, "\"g1\"", &material.server).await;
        let cache_path = directory.path("cache.json");
        let bootstrap = bootstrap_remote_signed_service_trust(
            mtls_remote_config(&distributor.url, cache_path.clone(), &material.client),
            directory.path("floor.json"),
            "inferlab-primary".to_owned(),
            roots.clone(),
            service_identity(),
            5_000,
            1_000,
        )
        .await
        .expect("mTLS bootstrap");
        assert_eq!(bootstrap.authorizer.trust_policy_generation(), Some(1));
        let status = bootstrap.authorizer.status();
        assert_eq!(status.trust_policy_transport_mode, "mutual-tls");
        assert!(status.trust_policy_server_authentication);
        assert!(status.trust_policy_client_authentication);
        let encoded = serde_json::to_string(&status).expect("status JSON");
        assert!(!encoded.contains(&distributor.url));
        assert!(!encoded.contains(&cache_path.display().to_string()));
        assert!(!encoded.contains("BEGIN CERTIFICATE"));

        let mut watcher = bootstrap.watcher;
        watcher.post_pending_receipt(&bootstrap.authorizer).await;
        let receipts = distributor.receipts();
        assert_eq!(receipts.len(), 1);
        roots
            .verify(&snapshot)
            .expect("verified snapshot")
            .compiled
            .keys
            .verify_trust_receipt(&receipts[0])
            .expect("verified mTLS receipt");
    }

    #[tokio::test]
    async fn mutual_tls_rejects_wrong_ca_hostname_and_rogue_client_without_state() {
        let directory = TestDirectory::new("mtls-handshake-rejections");
        let valid = test_mtls_material(&directory, "valid");
        let rogue = test_mtls_material(&directory, "rogue");
        let (snapshot, roots) = signed(1);
        let distributor = TestTlsDistributor::start(&snapshot, "\"g1\"", &valid.server).await;

        let mut wrong_ca = valid.client.clone();
        wrong_ca.server_ca.clone_from(&rogue.client.server_ca);
        let mut rogue_client = rogue.client.clone();
        rogue_client.server_ca.clone_from(&valid.client.server_ca);
        for (label, url, client) in [
            ("wrong-ca", distributor.url.as_str(), &wrong_ca),
            (
                "hostname-mismatch",
                distributor.address_url.as_str(),
                &valid.client,
            ),
            ("rogue-client", distributor.url.as_str(), &rogue_client),
        ] {
            let cache_path = directory.path(&format!("{label}-cache.json"));
            let floor_path = directory.path(&format!("{label}-floor.json"));
            let error = bootstrap_remote_signed_service_trust(
                mtls_remote_config(url, cache_path.clone(), client),
                floor_path.clone(),
                "inferlab-primary".to_owned(),
                roots.clone(),
                service_identity(),
                5_000,
                1_000,
            )
            .await
            .err()
            .unwrap_or_else(|| panic!("{label} must fail"));
            let error = error.to_string();
            assert!(error.contains("no valid cache"), "{label}: {error}");
            assert!(!error.contains(url));
            assert!(!error.contains(&cache_path.display().to_string()));
            assert!(!cache_path.exists(), "{label} must not create a cache");
            assert!(!floor_path.exists(), "{label} must not advance a floor");
        }
    }

    #[tokio::test]
    async fn mutual_tls_handshake_failure_retains_cached_floor_without_advancement() {
        let directory = TestDirectory::new("mtls-lkg");
        let valid = test_mtls_material(&directory, "valid");
        let rogue = test_mtls_material(&directory, "rogue");
        let cache_path = directory.path("cache.json");
        let floor_path = directory.path("floor.json");
        let (generation_one, roots) = signed(1);
        let first = TestTlsDistributor::start(&generation_one, "\"g1\"", &valid.server).await;
        bootstrap_remote_signed_service_trust(
            mtls_remote_config(&first.url, cache_path.clone(), &valid.client),
            floor_path.clone(),
            "inferlab-primary".to_owned(),
            roots.clone(),
            service_identity(),
            5_000,
            1_000,
        )
        .await
        .expect("initial mTLS bootstrap");
        let accepted_cache = fs::read(&cache_path).expect("accepted cache");
        let accepted_floor = fs::read(&floor_path).expect("accepted floor");

        let (generation_two, _) = signed(2);
        let second = TestTlsDistributor::start(&generation_two, "\"g2\"", &valid.server).await;
        let mut wrong_ca = valid.client.clone();
        wrong_ca.server_ca.clone_from(&rogue.client.server_ca);
        let restarted = bootstrap_remote_signed_service_trust(
            mtls_remote_config(&second.url, cache_path.clone(), &wrong_ca),
            floor_path.clone(),
            "inferlab-primary".to_owned(),
            roots,
            service_identity(),
            5_000,
            1_000,
        )
        .await
        .expect("handshake failure falls back to verified LKG");
        assert_eq!(restarted.authorizer.trust_policy_generation(), Some(1));
        assert_eq!(
            fs::read(cache_path).expect("cache after failure"),
            accepted_cache
        );
        assert_eq!(
            fs::read(floor_path).expect("floor after failure"),
            accepted_floor
        );
    }

    #[tokio::test]
    async fn mutual_tls_redirect_cannot_downgrade_or_mutate_state() {
        let directory = TestDirectory::new("mtls-no-downgrade");
        let material = test_mtls_material(&directory, "valid");
        let (snapshot, roots) = signed(1);
        let plaintext = TestDistributor::start(&snapshot, "\"plaintext\"").await;
        let tls = TestTlsDistributor::start(&snapshot, "\"tls\"", &material.server).await;
        tls.set_mode(SnapshotResponseMode::RedirectTo(format!(
            "{}{}",
            plaintext.url, SNAPSHOT_ENDPOINT_PATH
        )));
        let cache_path = directory.path("cache.json");
        let floor_path = directory.path("floor.json");
        let error = bootstrap_remote_signed_service_trust(
            mtls_remote_config(&tls.url, cache_path.clone(), &material.client),
            floor_path.clone(),
            "inferlab-primary".to_owned(),
            roots,
            service_identity(),
            5_000,
            1_000,
        )
        .await
        .err()
        .expect("redirect is not followed");
        assert!(error.to_string().contains("no valid cache"));
        assert!(plaintext.observed_etags().is_empty());
        assert!(!cache_path.exists());
        assert!(!floor_path.exists());
    }
}
