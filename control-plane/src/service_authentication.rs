use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use axum::http::HeaderMap;
use serde::Serialize;
use service_auth::{
    HEADER_ALGORITHM, HEADER_AUDIENCE_ID, HEADER_ISSUED_AT_MS, HEADER_NONCE, HEADER_SCHEMA,
    HEADER_SERVICE_ID, HEADER_SIGNATURE, ServiceAuthentication, ServiceRequestPayload,
    TrustedServiceKeyRing,
};

const MAX_REPLAY_ENTRIES: usize = 10_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceRejectionKind {
    Authentication,
    Freshness,
    Replay,
    Authorization,
}

#[derive(Debug)]
pub struct ServiceAuthorizationError {
    pub kind: ServiceRejectionKind,
    pub service_id: Option<String>,
    pub message: String,
}

#[derive(Clone, Copy, Debug)]
pub struct ServiceRequestContext<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub cluster_id: &'a str,
    pub audience_id: &'a str,
    pub body: &'a [u8],
    pub now_ms: u64,
}

enum Mode {
    Disabled,
    Required {
        keys: TrustedServiceKeyRing,
        gateway_service_ids: BTreeSet<String>,
        max_age_ms: u64,
        max_future_skew_ms: u64,
    },
}

pub struct ServiceAuthorizer {
    mode: Mode,
    replay_cache: Mutex<HashMap<(String, String), u64>>,
    verifications: AtomicU64,
    authentication_rejections: AtomicU64,
    freshness_rejections: AtomicU64,
    replay_rejections: AtomicU64,
    authorization_rejections: AtomicU64,
    authorized_peer_rpcs: AtomicU64,
    authorized_gateway_reads: AtomicU64,
    last_verified_service_id: Mutex<Option<String>>,
    last_rejected_service_id: Mutex<Option<String>>,
    last_error: Mutex<Option<String>>,
}

#[derive(Debug, Serialize)]
pub struct ServiceAuthenticationStatus {
    pub required: bool,
    pub trusted_service_ids: Vec<String>,
    pub revoked_service_ids: Vec<String>,
    pub gateway_service_ids: Vec<String>,
    pub max_age_ms: Option<u64>,
    pub max_future_skew_ms: Option<u64>,
    pub verifications: u64,
    pub authentication_rejections: u64,
    pub freshness_rejections: u64,
    pub replay_rejections: u64,
    pub authorization_rejections: u64,
    pub authorized_peer_rpcs: u64,
    pub authorized_gateway_reads: u64,
    pub replay_cache_entries: usize,
    pub last_verified_service_id: Option<String>,
    pub last_rejected_service_id: Option<String>,
    pub last_error: Option<String>,
}

impl ServiceAuthorizer {
    pub fn disabled() -> Self {
        Self::new(Mode::Disabled)
    }

    pub fn required(
        keys: TrustedServiceKeyRing,
        gateway_service_ids: impl IntoIterator<Item = String>,
        max_age_ms: u64,
        max_future_skew_ms: u64,
    ) -> Result<Self, String> {
        if max_age_ms == 0 {
            return Err("service request maximum age must be positive".to_owned());
        }
        let gateway_service_ids = gateway_service_ids.into_iter().collect::<BTreeSet<_>>();
        if gateway_service_ids.is_empty() {
            return Err("at least one gateway service ID is required".to_owned());
        }
        let trusted = keys
            .trusted_service_ids()
            .into_iter()
            .collect::<BTreeSet<_>>();
        if let Some(untrusted) = gateway_service_ids.difference(&trusted).next() {
            return Err(format!(
                "gateway service ID '{untrusted}' is missing from the trusted service keys"
            ));
        }
        Ok(Self::new(Mode::Required {
            keys,
            gateway_service_ids,
            max_age_ms,
            max_future_skew_ms,
        }))
    }

    fn new(mode: Mode) -> Self {
        Self {
            mode,
            replay_cache: Mutex::new(HashMap::new()),
            verifications: AtomicU64::new(0),
            authentication_rejections: AtomicU64::new(0),
            freshness_rejections: AtomicU64::new(0),
            replay_rejections: AtomicU64::new(0),
            authorization_rejections: AtomicU64::new(0),
            authorized_peer_rpcs: AtomicU64::new(0),
            authorized_gateway_reads: AtomicU64::new(0),
            last_verified_service_id: Mutex::new(None),
            last_rejected_service_id: Mutex::new(None),
            last_error: Mutex::new(None),
        }
    }

    pub fn authenticate(
        &self,
        authentication: Option<ServiceAuthentication>,
        context: ServiceRequestContext<'_>,
    ) -> Result<Option<String>, ServiceAuthorizationError> {
        match (&self.mode, authentication) {
            (Mode::Disabled, None) => Ok(None),
            (Mode::Disabled, Some(authentication)) => Err(self.reject(
                ServiceRejectionKind::Authentication,
                Some(authentication.service_id),
                "service authentication is not configured; refusing signed service headers"
                    .to_owned(),
            )),
            (Mode::Required { .. }, None) => Err(self.reject(
                ServiceRejectionKind::Authentication,
                None,
                "service authentication is required".to_owned(),
            )),
            (
                Mode::Required {
                    keys,
                    max_age_ms,
                    max_future_skew_ms,
                    ..
                },
                Some(authentication),
            ) => {
                let service_id = authentication.service_id.clone();
                let payload = ServiceRequestPayload {
                    method: context.method,
                    path: context.path,
                    cluster_id: context.cluster_id,
                    audience_id: context.audience_id,
                    issued_at_ms: authentication.issued_at_ms,
                    nonce: &authentication.nonce,
                    body: context.body,
                };
                if let Err(error) = keys.verify(&payload, &authentication) {
                    return Err(self.reject(
                        ServiceRejectionKind::Authentication,
                        Some(service_id),
                        error.to_string(),
                    ));
                }
                let latest_acceptable = context.now_ms.saturating_add(*max_future_skew_ms);
                if authentication.issued_at_ms > latest_acceptable {
                    return Err(self.reject(
                        ServiceRejectionKind::Freshness,
                        Some(service_id),
                        format!(
                            "service request was issued {} ms in the future; maximum future skew is {} ms",
                            authentication.issued_at_ms.saturating_sub(context.now_ms),
                            max_future_skew_ms
                        ),
                    ));
                }
                let age_ms = context.now_ms.saturating_sub(authentication.issued_at_ms);
                if age_ms > *max_age_ms {
                    return Err(self.reject(
                        ServiceRejectionKind::Freshness,
                        Some(service_id),
                        format!(
                            "service request is {age_ms} ms old; maximum age is {max_age_ms} ms"
                        ),
                    ));
                }
                let expires_at_ms = authentication
                    .issued_at_ms
                    .saturating_add(*max_age_ms)
                    .saturating_add(*max_future_skew_ms);
                let mut cache = self
                    .replay_cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                cache.retain(|_, expiry| *expiry >= context.now_ms);
                let key = (service_id.clone(), authentication.nonce);
                if cache.contains_key(&key) {
                    drop(cache);
                    return Err(self.reject(
                        ServiceRejectionKind::Replay,
                        Some(service_id),
                        "service request nonce was already accepted".to_owned(),
                    ));
                }
                if cache.len() >= MAX_REPLAY_ENTRIES {
                    drop(cache);
                    return Err(self.reject(
                        ServiceRejectionKind::Replay,
                        Some(service_id),
                        "service replay cache capacity is exhausted".to_owned(),
                    ));
                }
                cache.insert(key, expires_at_ms);
                drop(cache);
                self.verifications.fetch_add(1, Ordering::Relaxed);
                replace(&self.last_verified_service_id, Some(service_id.clone()));
                replace(&self.last_error, None);
                Ok(Some(service_id))
            }
        }
    }

    pub fn authorize_gateway(
        &self,
        service_id: Option<&str>,
    ) -> Result<(), ServiceAuthorizationError> {
        match (&self.mode, service_id) {
            (Mode::Disabled, _) => Ok(()),
            (
                Mode::Required {
                    gateway_service_ids,
                    ..
                },
                Some(service_id),
            ) if gateway_service_ids.contains(service_id) => Ok(()),
            (Mode::Required { .. }, service_id) => Err(self.reject(
                ServiceRejectionKind::Authorization,
                service_id.map(str::to_owned),
                format!(
                    "service identity '{}' is not authorized as a gateway",
                    service_id.unwrap_or("<missing>")
                ),
            )),
        }
    }

    pub fn record_peer_authorization_rejection(&self, service_id: Option<&str>, message: String) {
        let _ = self.reject(
            ServiceRejectionKind::Authorization,
            service_id.map(str::to_owned),
            message,
        );
    }

    pub fn record_header_rejection(&self, message: String) {
        let _ = self.reject(ServiceRejectionKind::Authentication, None, message);
    }

    pub fn record_authorized_peer_rpc(&self, service_id: Option<&str>) {
        if service_id.is_some() {
            self.authorized_peer_rpcs.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_authorized_gateway_read(&self, service_id: Option<&str>) {
        if service_id.is_some() {
            self.authorized_gateway_reads
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn required_mode(&self) -> bool {
        matches!(self.mode, Mode::Required { .. })
    }

    pub fn status(&self) -> ServiceAuthenticationStatus {
        let (required, trusted, revoked, gateways, max_age_ms, max_future_skew_ms) =
            match &self.mode {
                Mode::Disabled => (false, Vec::new(), Vec::new(), Vec::new(), None, None),
                Mode::Required {
                    keys,
                    gateway_service_ids,
                    max_age_ms,
                    max_future_skew_ms,
                } => (
                    true,
                    keys.trusted_service_ids(),
                    keys.revoked_service_ids(),
                    gateway_service_ids.iter().cloned().collect(),
                    Some(*max_age_ms),
                    Some(*max_future_skew_ms),
                ),
            };
        ServiceAuthenticationStatus {
            required,
            trusted_service_ids: trusted,
            revoked_service_ids: revoked,
            gateway_service_ids: gateways,
            max_age_ms,
            max_future_skew_ms,
            verifications: self.verifications.load(Ordering::Relaxed),
            authentication_rejections: self.authentication_rejections.load(Ordering::Relaxed),
            freshness_rejections: self.freshness_rejections.load(Ordering::Relaxed),
            replay_rejections: self.replay_rejections.load(Ordering::Relaxed),
            authorization_rejections: self.authorization_rejections.load(Ordering::Relaxed),
            authorized_peer_rpcs: self.authorized_peer_rpcs.load(Ordering::Relaxed),
            authorized_gateway_reads: self.authorized_gateway_reads.load(Ordering::Relaxed),
            replay_cache_entries: self
                .replay_cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            last_verified_service_id: clone_locked(&self.last_verified_service_id),
            last_rejected_service_id: clone_locked(&self.last_rejected_service_id),
            last_error: clone_locked(&self.last_error),
        }
    }

    fn reject(
        &self,
        kind: ServiceRejectionKind,
        service_id: Option<String>,
        message: String,
    ) -> ServiceAuthorizationError {
        match kind {
            ServiceRejectionKind::Authentication => {
                self.authentication_rejections
                    .fetch_add(1, Ordering::Relaxed);
            }
            ServiceRejectionKind::Freshness => {
                self.freshness_rejections.fetch_add(1, Ordering::Relaxed);
            }
            ServiceRejectionKind::Replay => {
                self.replay_rejections.fetch_add(1, Ordering::Relaxed);
            }
            ServiceRejectionKind::Authorization => {
                self.authorization_rejections
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        replace(&self.last_rejected_service_id, service_id.clone());
        replace(&self.last_error, Some(message.clone()));
        ServiceAuthorizationError {
            kind,
            service_id,
            message,
        }
    }
}

pub fn authentication_from_headers(
    headers: &HeaderMap,
) -> Result<Option<ServiceAuthentication>, String> {
    let names = [
        HEADER_SCHEMA,
        HEADER_ALGORITHM,
        HEADER_SERVICE_ID,
        HEADER_AUDIENCE_ID,
        HEADER_ISSUED_AT_MS,
        HEADER_NONCE,
        HEADER_SIGNATURE,
    ];
    let present = names
        .iter()
        .filter(|name| headers.contains_key(**name))
        .count();
    if present == 0 {
        return Ok(None);
    }
    if present != names.len() {
        return Err("service authentication headers are incomplete".to_owned());
    }
    let value = |name: &'static str| -> Result<String, String> {
        headers
            .get(name)
            .ok_or_else(|| format!("missing {name}"))?
            .to_str()
            .map(str::to_owned)
            .map_err(|_| format!("{name} is not valid ASCII"))
    };
    let issued_at_ms = value(HEADER_ISSUED_AT_MS)?
        .parse::<u64>()
        .map_err(|error| format!("{HEADER_ISSUED_AT_MS} is invalid: {error}"))?;
    Ok(Some(ServiceAuthentication {
        schema: value(HEADER_SCHEMA)?,
        algorithm: value(HEADER_ALGORITHM)?,
        service_id: value(HEADER_SERVICE_ID)?,
        audience_id: value(HEADER_AUDIENCE_ID)?,
        issued_at_ms,
        nonce: value(HEADER_NONCE)?,
        signature: value(HEADER_SIGNATURE)?,
    }))
}

fn replace<T>(slot: &Mutex<T>, value: T) {
    *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = value;
}

fn clone_locked<T: Clone>(slot: &Mutex<T>) -> T {
    slot.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

#[cfg(test)]
mod tests {
    use service_auth::{ServiceRequestPayload, ServiceSigningIdentity};

    use super::*;

    const SEED: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";

    fn required_authorizer() -> (ServiceAuthorizer, ServiceSigningIdentity) {
        let identity =
            ServiceSigningIdentity::from_base64_seed("gateway-primary", SEED).expect("identity");
        let keys = TrustedServiceKeyRing::parse(
            &format!("gateway-primary={}", identity.public_key_base64()),
            "",
        )
        .expect("keys");
        let authorizer =
            ServiceAuthorizer::required(keys, ["gateway-primary".to_owned()], 1_000, 100)
                .expect("authorizer");
        (authorizer, identity)
    }

    fn authentication(
        identity: &ServiceSigningIdentity,
        issued_at_ms: u64,
        nonce: &str,
    ) -> ServiceAuthentication {
        identity
            .authenticate(&ServiceRequestPayload {
                method: "GET",
                path: "/v1/control/config",
                cluster_id: "inferlab-primary",
                audience_id: "node-a",
                issued_at_ms,
                nonce,
                body: b"",
            })
            .expect("authentication")
    }

    fn context(method: &'static str, now_ms: u64) -> ServiceRequestContext<'static> {
        ServiceRequestContext {
            method,
            path: "/v1/control/config",
            cluster_id: "inferlab-primary",
            audience_id: "node-a",
            body: b"",
            now_ms,
        }
    }

    #[test]
    fn accepts_once_then_rejects_the_same_nonce_as_a_replay() {
        let (authorizer, identity) = required_authorizer();
        let authentication = authentication(&identity, 10_000, "gateway-primary.10000.1");
        let service_id = authorizer
            .authenticate(Some(authentication.clone()), context("GET", 10_010))
            .expect("fresh request");
        authorizer
            .authorize_gateway(service_id.as_deref())
            .expect("gateway authorization");
        let replay = authorizer
            .authenticate(Some(authentication), context("GET", 10_020))
            .expect_err("replay");
        assert_eq!(replay.kind, ServiceRejectionKind::Replay);
        assert_eq!(authorizer.status().replay_rejections, 1);
    }

    #[test]
    fn distinguishes_freshness_from_signature_failure() {
        let (authorizer, identity) = required_authorizer();
        let stale = authorizer
            .authenticate(
                Some(authentication(&identity, 8_000, "gateway-primary.8000.1")),
                context("GET", 10_000),
            )
            .expect_err("stale");
        assert_eq!(stale.kind, ServiceRejectionKind::Freshness);

        let tampered = authorizer
            .authenticate(
                Some(authentication(&identity, 10_000, "gateway-primary.10000.2")),
                context("POST", 10_000),
            )
            .expect_err("tampered method");
        assert_eq!(tampered.kind, ServiceRejectionKind::Authentication);
    }

    #[test]
    fn disabled_mode_refuses_signature_shaped_headers() {
        let (_, identity) = required_authorizer();
        let error = ServiceAuthorizer::disabled()
            .authenticate(
                Some(authentication(&identity, 10_000, "gateway-primary.10000.3")),
                context("GET", 10_000),
            )
            .expect_err("unchecked signed headers must not pass");
        assert_eq!(error.kind, ServiceRejectionKind::Authentication);
    }
}
