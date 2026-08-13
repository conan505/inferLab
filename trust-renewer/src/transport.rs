use std::{fmt, future::Future, pin::Pin, time::Duration};

use futures_util::StreamExt as _;
use reqwest::{StatusCode, header};
use service_auth::ServiceTrustSnapshot;
use transport_security::{MtlsClientPaths, configure_mtls_client};

use crate::snapshot_sha256;

pub const DISTRIBUTOR_SNAPSHOT_PATH: &str = "/v1/service-trust/snapshot";
pub const MAX_DISTRIBUTOR_RESPONSE_BYTES: usize = 1024 * 1024;

pub type TransportFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, TransportError>> + Send + 'a>>;

pub trait DistributorTransport: Send + Sync {
    fn get_snapshot(&self) -> TransportFuture<'_, Option<DistributorSnapshot>>;

    fn publish_snapshot<'a>(&'a self, exact_bytes: &'a [u8])
    -> TransportFuture<'a, PublishOutcome>;
}

#[derive(Clone, Eq, PartialEq)]
pub struct DistributorSnapshot {
    pub snapshot: ServiceTrustSnapshot,
    pub exact_bytes: Vec<u8>,
}

impl fmt::Debug for DistributorSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DistributorSnapshot")
            .field("generation", &self.snapshot.policy.generation)
            .field("snapshot_sha256", &snapshot_sha256(&self.exact_bytes))
            .field("exact_bytes_len", &self.exact_bytes.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishOutcome {
    Accepted,
    Conflict,
    Rejected,
}

#[derive(Clone)]
pub struct MtlsDistributorTransport {
    client: reqwest::Client,
    endpoint: reqwest::Url,
}

impl fmt::Debug for MtlsDistributorTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MtlsDistributorTransport")
            .field("endpoint", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl MtlsDistributorTransport {
    pub fn new(
        endpoint: reqwest::Url,
        mtls: &MtlsClientPaths,
        request_timeout: Duration,
    ) -> Result<Self, TransportBuildError> {
        if endpoint.scheme() != "https"
            || endpoint.path() != DISTRIBUTOR_SNAPSHOT_PATH
            || endpoint.host().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || request_timeout.is_zero()
        {
            return Err(TransportBuildError);
        }
        let builder = reqwest::Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(request_timeout)
            .user_agent("inferlab-trust-renewer/1");
        let client = configure_mtls_client(builder, mtls)
            .and_then(|builder| builder.build().map_err(std::io::Error::other))
            .map_err(|_| TransportBuildError)?;
        Ok(Self { client, endpoint })
    }

    async fn get(&self) -> Result<Option<DistributorSnapshot>, TransportError> {
        let response = self
            .client
            .get(self.endpoint.clone())
            .header(header::ACCEPT, "application/json")
            .header(header::CACHE_CONTROL, "no-cache")
            .send()
            .await
            .map_err(|_| TransportError::transient(TransportErrorKind::RequestFailed))?;
        let status = response.status();
        let bytes = read_bounded_response(response).await?;
        match status {
            StatusCode::OK => parse_snapshot_response(bytes).map(Some),
            StatusCode::NOT_FOUND => Ok(None),
            status if transient_status(status) => Err(TransportError::transient(
                TransportErrorKind::RemoteUnavailable,
            )),
            _ => Err(TransportError::deterministic(
                TransportErrorKind::UnexpectedStatus,
            )),
        }
    }

    async fn post(&self, exact_bytes: &[u8]) -> Result<PublishOutcome, TransportError> {
        if exact_bytes.is_empty() || exact_bytes.len() > MAX_DISTRIBUTOR_RESPONSE_BYTES {
            return Err(TransportError::deterministic(
                TransportErrorKind::InvalidRequestBody,
            ));
        }
        let response = self
            .client
            .post(self.endpoint.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json")
            .body(exact_bytes.to_vec())
            .send()
            .await
            .map_err(|_| TransportError::transient(TransportErrorKind::RequestFailed))?;
        let status = response.status();
        let _ = read_bounded_response(response).await?;
        match status {
            StatusCode::OK | StatusCode::CREATED => Ok(PublishOutcome::Accepted),
            StatusCode::CONFLICT => Ok(PublishOutcome::Conflict),
            status if transient_status(status) => Err(TransportError::transient(
                TransportErrorKind::RemoteUnavailable,
            )),
            status if status.is_client_error() => Ok(PublishOutcome::Rejected),
            _ => Err(TransportError::deterministic(
                TransportErrorKind::UnexpectedStatus,
            )),
        }
    }
}

impl DistributorTransport for MtlsDistributorTransport {
    fn get_snapshot(&self) -> TransportFuture<'_, Option<DistributorSnapshot>> {
        Box::pin(self.get())
    }

    fn publish_snapshot<'a>(
        &'a self,
        exact_bytes: &'a [u8],
    ) -> TransportFuture<'a, PublishOutcome> {
        Box::pin(self.post(exact_bytes))
    }
}

async fn read_bounded_response(response: reqwest::Response) -> Result<Vec<u8>, TransportError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DISTRIBUTOR_RESPONSE_BYTES as u64)
    {
        return Err(TransportError::deterministic(
            TransportErrorKind::ResponseTooLarge,
        ));
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|_| TransportError::transient(TransportErrorKind::ResponseFailed))?;
        let next_length = body
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| TransportError::deterministic(TransportErrorKind::ResponseTooLarge))?;
        if next_length > MAX_DISTRIBUTOR_RESPONSE_BYTES {
            return Err(TransportError::deterministic(
                TransportErrorKind::ResponseTooLarge,
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn parse_snapshot_response(bytes: Vec<u8>) -> Result<DistributorSnapshot, TransportError> {
    if bytes.is_empty() {
        return Err(TransportError::deterministic(
            TransportErrorKind::InvalidSnapshot,
        ));
    }
    let snapshot = serde_json::from_slice::<ServiceTrustSnapshot>(&bytes)
        .map_err(|_| TransportError::deterministic(TransportErrorKind::InvalidSnapshot))?;
    let canonical = serde_json::to_vec(&snapshot)
        .map_err(|_| TransportError::deterministic(TransportErrorKind::InvalidSnapshot))?;
    if canonical != bytes {
        return Err(TransportError::deterministic(
            TransportErrorKind::NonCanonicalSnapshot,
        ));
    }
    Ok(DistributorSnapshot {
        snapshot,
        exact_bytes: bytes,
    })
}

fn transient_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_EARLY
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportErrorKind {
    InvalidRequestBody,
    RequestFailed,
    ResponseFailed,
    ResponseTooLarge,
    RemoteUnavailable,
    UnexpectedStatus,
    InvalidSnapshot,
    NonCanonicalSnapshot,
}

impl TransportErrorKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequestBody => "invalid_request_body",
            Self::RequestFailed => "request_failed",
            Self::ResponseFailed => "response_failed",
            Self::ResponseTooLarge => "response_too_large",
            Self::RemoteUnavailable => "remote_unavailable",
            Self::UnexpectedStatus => "unexpected_status",
            Self::InvalidSnapshot => "invalid_remote_snapshot",
            Self::NonCanonicalSnapshot => "noncanonical_remote_snapshot",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransportError {
    kind: TransportErrorKind,
    transient: bool,
}

impl TransportError {
    pub(crate) const fn transient(kind: TransportErrorKind) -> Self {
        Self {
            kind,
            transient: true,
        }
    }

    pub(crate) const fn deterministic(kind: TransportErrorKind) -> Self {
        Self {
            kind,
            transient: false,
        }
    }

    #[must_use]
    pub const fn kind(self) -> TransportErrorKind {
        self.kind
    }

    #[must_use]
    pub const fn is_transient(self) -> bool {
        self.transient
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.kind.as_str())
    }
}

impl std::error::Error for TransportError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransportBuildError;

impl fmt::Display for TransportBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid static mTLS distributor transport")
    }
}

impl std::error::Error for TransportBuildError {}

#[cfg(test)]
mod tests {
    use service_auth::{
        SERVICE_TRUST_AUTHENTICATION_SCHEMA_V2, SERVICE_TRUST_POLICY_SCHEMA_V2,
        ServiceTrustPolicyPayload, ServiceTrustSnapshotAuthentication,
    };

    use super::*;

    fn snapshot() -> ServiceTrustSnapshot {
        ServiceTrustSnapshot {
            policy: ServiceTrustPolicyPayload {
                schema: SERVICE_TRUST_POLICY_SCHEMA_V2.to_owned(),
                cluster_id: "inferlab-primary".to_owned(),
                generation: 7,
                issued_at_ms: 1_000,
                expires_at_ms: Some(2_000),
                trusted_credentials: vec![],
                revoked_service_ids: vec![],
                revoked_credentials: vec![],
                gateway_service_ids: vec![],
            },
            authentication: ServiceTrustSnapshotAuthentication {
                schema: SERVICE_TRUST_AUTHENTICATION_SCHEMA_V2.to_owned(),
                algorithm: "ed25519".to_owned(),
                key_id: "root-a".to_owned(),
                signature: "fixture".to_owned(),
            },
        }
    }

    #[test]
    fn accepts_only_exact_canonical_snapshot_responses() {
        let bytes = serde_json::to_vec(&snapshot()).expect("serialize");
        let parsed = parse_snapshot_response(bytes.clone()).expect("response");
        assert_eq!(parsed.snapshot, snapshot());
        assert_eq!(parsed.exact_bytes, bytes);

        let pretty = serde_json::to_vec_pretty(&snapshot()).expect("pretty");
        assert_eq!(
            parse_snapshot_response(pretty).unwrap_err().kind(),
            TransportErrorKind::NonCanonicalSnapshot
        );
        assert_eq!(
            parse_snapshot_response(Vec::new()).unwrap_err().kind(),
            TransportErrorKind::InvalidSnapshot
        );
    }

    #[test]
    fn distributor_snapshot_debug_redacts_policy_and_exact_bytes() {
        let bytes = serde_json::to_vec(&snapshot()).expect("serialize");
        let expected_hash = snapshot_sha256(&bytes);
        let parsed = parse_snapshot_response(bytes.clone()).expect("response");
        let debug = format!("{parsed:?}");

        assert!(debug.contains("generation: 7"));
        assert!(debug.contains(&expected_hash));
        assert!(debug.contains(&format!("exact_bytes_len: {}", bytes.len())));
        for forbidden in [
            String::from_utf8(bytes).expect("snapshot JSON"),
            "fixture".to_owned(),
            "root-a".to_owned(),
            "trusted_credentials".to_owned(),
        ] {
            assert!(!debug.contains(&forbidden));
        }
    }

    #[test]
    fn status_classification_is_finite() {
        for status in [
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_EARLY,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
        ] {
            assert!(transient_status(status));
        }
        for status in [
            StatusCode::OK,
            StatusCode::CREATED,
            StatusCode::BAD_REQUEST,
            StatusCode::CONFLICT,
        ] {
            assert!(!transient_status(status));
        }
    }
}
