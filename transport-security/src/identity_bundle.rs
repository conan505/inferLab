use std::{
    fmt,
    fs::File,
    io::{self, Read},
    path::Path,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use rustls::{
    RootCertStore,
    client::{WebPkiServerVerifier, danger::ServerCertVerifier as _},
    pki_types::{CertificateDer, DnsName, PrivateKeyDer, ServerName, UnixTime},
    server::WebPkiClientVerifier,
};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use x509_parser::parse_x509_certificate;

use crate::{MAX_PEM_FILE_BYTES, parse_certificate_chain, parse_private_key, tls_crypto_provider};

pub const TLS_IDENTITY_BUNDLE_SCHEMA: &str = "inferlab.tls-identity-bundle.v1";
pub const MAX_TLS_IDENTITY_BUNDLE_BYTES: usize = 512 * 1024;
const MAX_TLS_IDENTITY_ID_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TlsIdentityPurpose {
    Server,
    Client,
}

impl TlsIdentityPurpose {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Client => "client",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlsIdentityErrorKind {
    SourceUnavailable,
    NotRegularFile,
    UnsafePermissions,
    BundleTooLarge,
    InvalidJson,
    InvalidSchema,
    InvalidClusterId,
    InvalidIdentityId,
    InvalidGeneration,
    InvalidPurpose,
    InvalidServerName,
    InvalidCertificate,
    InvalidPrivateKey,
    PrivateKeyMismatch,
    InvalidIssuerCa,
    CertificateExpired,
    CertificateNotYetValid,
    WrongHostname,
    WrongEku,
    WrongCa,
    CertificateValidation,
    ClusterMismatch,
    IdentityMismatch,
    PurposeMismatch,
    ServerNameMismatch,
    IssuerCaMismatch,
    StaleGeneration,
    GenerationFork,
    RuntimeConfiguration,
}

impl TlsIdentityErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SourceUnavailable => "source_unavailable",
            Self::NotRegularFile => "not_regular_file",
            Self::UnsafePermissions => "unsafe_permissions",
            Self::BundleTooLarge => "bundle_too_large",
            Self::InvalidJson => "invalid_json",
            Self::InvalidSchema => "invalid_schema",
            Self::InvalidClusterId => "invalid_cluster_id",
            Self::InvalidIdentityId => "invalid_identity_id",
            Self::InvalidGeneration => "invalid_generation",
            Self::InvalidPurpose => "invalid_purpose",
            Self::InvalidServerName => "invalid_server_name",
            Self::InvalidCertificate => "invalid_certificate",
            Self::InvalidPrivateKey => "invalid_private_key",
            Self::PrivateKeyMismatch => "private_key_mismatch",
            Self::InvalidIssuerCa => "invalid_issuer_ca",
            Self::CertificateExpired => "certificate_expired",
            Self::CertificateNotYetValid => "certificate_not_yet_valid",
            Self::WrongHostname => "wrong_hostname",
            Self::WrongEku => "wrong_eku",
            Self::WrongCa => "wrong_ca",
            Self::CertificateValidation => "certificate_validation",
            Self::ClusterMismatch => "cluster_mismatch",
            Self::IdentityMismatch => "identity_mismatch",
            Self::PurposeMismatch => "purpose_mismatch",
            Self::ServerNameMismatch => "server_name_mismatch",
            Self::IssuerCaMismatch => "issuer_ca_mismatch",
            Self::StaleGeneration => "stale_generation",
            Self::GenerationFork => "generation_fork",
            Self::RuntimeConfiguration => "runtime_configuration",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TlsIdentityError {
    kind: TlsIdentityErrorKind,
    message: &'static str,
}

impl TlsIdentityError {
    const fn new(kind: TlsIdentityErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    pub const fn kind(&self) -> TlsIdentityErrorKind {
        self.kind
    }

    pub const fn runtime_configuration() -> Self {
        Self::new(
            TlsIdentityErrorKind::RuntimeConfiguration,
            "TLS identity runtime configuration could not be built",
        )
    }
}

impl fmt::Display for TlsIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for TlsIdentityError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EncodedTlsIdentityBundle {
    schema: String,
    cluster_id: String,
    generation: u64,
    identity_id: String,
    purpose: TlsIdentityPurpose,
    server_name: Option<String>,
    certificate_chain_pem: String,
    private_key_pem: String,
    issuer_ca_pem: String,
}

pub struct VerifiedTlsIdentityBundle {
    cluster_id: String,
    generation: u64,
    identity_id: String,
    purpose: TlsIdentityPurpose,
    server_name: Option<String>,
    certificate_chain: Vec<CertificateDer<'static>>,
    private_key: PrivateKeyDer<'static>,
    issuer_ca: Vec<CertificateDer<'static>>,
    certificate_chain_pem: Vec<u8>,
    private_key_pem: Vec<u8>,
}

impl fmt::Debug for VerifiedTlsIdentityBundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedTlsIdentityBundle")
            .field("cluster_id", &self.cluster_id)
            .field("generation", &self.generation)
            .field("identity_id", &self.identity_id)
            .field("purpose", &self.purpose)
            .field("server_name", &self.server_name)
            .field("certificate_chain_length", &self.certificate_chain.len())
            .field("issuer_ca_count", &self.issuer_ca.len())
            .finish_non_exhaustive()
    }
}

impl VerifiedTlsIdentityBundle {
    pub fn load(
        path: impl AsRef<Path>,
        expected_cluster_id: &str,
        expected_identity_id: &str,
        expected_purpose: TlsIdentityPurpose,
        expected_server_name: Option<&str>,
    ) -> Result<Self, TlsIdentityError> {
        let bytes = read_bundle_file(path.as_ref())?;
        Self::decode(
            &bytes,
            expected_cluster_id,
            expected_identity_id,
            expected_purpose,
            expected_server_name,
        )
    }

    pub fn decode(
        bytes: &[u8],
        expected_cluster_id: &str,
        expected_identity_id: &str,
        expected_purpose: TlsIdentityPurpose,
        expected_server_name: Option<&str>,
    ) -> Result<Self, TlsIdentityError> {
        Self::decode_at(
            bytes,
            expected_cluster_id,
            expected_identity_id,
            expected_purpose,
            expected_server_name,
            UnixTime::now(),
        )
    }

    fn decode_at(
        bytes: &[u8],
        expected_cluster_id: &str,
        expected_identity_id: &str,
        expected_purpose: TlsIdentityPurpose,
        expected_server_name: Option<&str>,
        now: UnixTime,
    ) -> Result<Self, TlsIdentityError> {
        if bytes.len() > MAX_TLS_IDENTITY_BUNDLE_BYTES {
            return Err(TlsIdentityError::new(
                TlsIdentityErrorKind::BundleTooLarge,
                "TLS identity bundle exceeds the byte limit",
            ));
        }
        validate_identity_id(expected_cluster_id, TlsIdentityErrorKind::InvalidClusterId)?;
        validate_identity_id(
            expected_identity_id,
            TlsIdentityErrorKind::InvalidIdentityId,
        )?;
        validate_expected_server_name(expected_purpose, expected_server_name)?;

        let encoded: EncodedTlsIdentityBundle = serde_json::from_slice(bytes).map_err(|_| {
            TlsIdentityError::new(
                TlsIdentityErrorKind::InvalidJson,
                "TLS identity bundle is not exact valid JSON",
            )
        })?;
        if encoded.schema != TLS_IDENTITY_BUNDLE_SCHEMA {
            return Err(TlsIdentityError::new(
                TlsIdentityErrorKind::InvalidSchema,
                "TLS identity bundle schema is unsupported",
            ));
        }
        validate_identity_id(&encoded.cluster_id, TlsIdentityErrorKind::InvalidClusterId)?;
        if encoded.cluster_id != expected_cluster_id {
            return Err(TlsIdentityError::new(
                TlsIdentityErrorKind::ClusterMismatch,
                "TLS identity bundle cluster ID does not match this process",
            ));
        }
        validate_identity_id(
            &encoded.identity_id,
            TlsIdentityErrorKind::InvalidIdentityId,
        )?;
        if encoded.identity_id != expected_identity_id {
            return Err(TlsIdentityError::new(
                TlsIdentityErrorKind::IdentityMismatch,
                "TLS identity bundle identity ID does not match this process",
            ));
        }
        if encoded.generation == 0 {
            return Err(TlsIdentityError::new(
                TlsIdentityErrorKind::InvalidGeneration,
                "TLS identity bundle generation must be positive",
            ));
        }
        if encoded.purpose != expected_purpose {
            return Err(TlsIdentityError::new(
                TlsIdentityErrorKind::PurposeMismatch,
                "TLS identity bundle purpose does not match this process",
            ));
        }
        validate_bundle_server_name(
            encoded.purpose,
            encoded.server_name.as_deref(),
            expected_server_name,
        )?;
        for pem in [
            encoded.certificate_chain_pem.as_bytes(),
            encoded.private_key_pem.as_bytes(),
            encoded.issuer_ca_pem.as_bytes(),
        ] {
            if pem.len() > MAX_PEM_FILE_BYTES {
                return Err(TlsIdentityError::new(
                    TlsIdentityErrorKind::BundleTooLarge,
                    "TLS identity bundle contains an oversized PEM component",
                ));
            }
        }

        let certificate_chain = parse_certificate_chain(
            encoded.certificate_chain_pem.as_bytes(),
            "TLS identity certificate chain",
        )
        .map_err(|_| {
            TlsIdentityError::new(
                TlsIdentityErrorKind::InvalidCertificate,
                "TLS identity bundle contains an invalid certificate chain",
            )
        })?;
        let private_key = parse_private_key(
            encoded.private_key_pem.as_bytes(),
            "TLS identity private key",
        )
        .map_err(|_| {
            TlsIdentityError::new(
                TlsIdentityErrorKind::InvalidPrivateKey,
                "TLS identity bundle contains an invalid private key",
            )
        })?;
        let mut issuer_ca =
            parse_certificate_chain(encoded.issuer_ca_pem.as_bytes(), "TLS identity issuer CA")
                .map_err(|_| {
                    TlsIdentityError::new(
                        TlsIdentityErrorKind::InvalidIssuerCa,
                        "TLS identity bundle contains an invalid issuer CA",
                    )
                })?;
        validate_issuer_ca(&issuer_ca)?;
        issuer_ca.sort_by(|left, right| left.as_ref().cmp(right.as_ref()));
        if issuer_ca
            .windows(2)
            .any(|pair| pair[0].as_ref() == pair[1].as_ref())
        {
            return Err(TlsIdentityError::new(
                TlsIdentityErrorKind::InvalidIssuerCa,
                "TLS identity bundle issuer CA certificates must be unique",
            ));
        }

        let provider = tls_crypto_provider();
        rustls::sign::CertifiedKey::from_der(
            certificate_chain.clone(),
            private_key.clone_key(),
            provider.as_ref(),
        )
        .map_err(|_| {
            TlsIdentityError::new(
                TlsIdentityErrorKind::PrivateKeyMismatch,
                "TLS identity certificate chain and private key do not match",
            )
        })?;
        validate_certificate(
            &certificate_chain,
            &issuer_ca,
            encoded.purpose,
            encoded.server_name.as_deref(),
            now,
        )?;

        Ok(Self {
            cluster_id: encoded.cluster_id,
            generation: encoded.generation,
            identity_id: encoded.identity_id,
            purpose: encoded.purpose,
            server_name: encoded.server_name,
            certificate_chain,
            private_key,
            issuer_ca,
            certificate_chain_pem: encoded.certificate_chain_pem.into_bytes(),
            private_key_pem: encoded.private_key_pem.into_bytes(),
        })
    }

    pub fn cluster_id(&self) -> &str {
        &self.cluster_id
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn identity_id(&self) -> &str {
        &self.identity_id
    }

    pub const fn purpose(&self) -> TlsIdentityPurpose {
        self.purpose
    }

    pub fn server_name(&self) -> Option<&str> {
        self.server_name.as_deref()
    }

    pub fn certificate_chain_length(&self) -> usize {
        self.certificate_chain.len()
    }

    pub fn issuer_ca_count(&self) -> usize {
        self.issuer_ca.len()
    }

    pub fn certificate_chain(&self) -> Vec<CertificateDer<'static>> {
        self.certificate_chain.clone()
    }

    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        self.private_key.clone_key()
    }

    pub fn certificate_chain_pem(&self) -> &[u8] {
        &self.certificate_chain_pem
    }

    pub fn private_key_pem(&self) -> &[u8] {
        &self.private_key_pem
    }

    fn semantically_matches(&self, other: &Self) -> bool {
        self.cluster_id == other.cluster_id
            && self.identity_id == other.identity_id
            && self.purpose == other.purpose
            && self.server_name == other.server_name
            && certificate_slices_equal(&self.certificate_chain, &other.certificate_chain)
            && certificate_slices_equal(&self.issuer_ca, &other.issuer_ca)
    }

    fn issuer_ca_matches(&self, other: &Self) -> bool {
        certificate_slices_equal(&self.issuer_ca, &other.issuer_ca)
    }
}

fn validate_identity_id(value: &str, kind: TlsIdentityErrorKind) -> Result<(), TlsIdentityError> {
    let valid = !value.is_empty()
        && value.len() <= MAX_TLS_IDENTITY_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(TlsIdentityError::new(
            kind,
            "TLS identity binding contains an invalid identifier",
        ))
    }
}

fn validate_expected_server_name(
    purpose: TlsIdentityPurpose,
    server_name: Option<&str>,
) -> Result<(), TlsIdentityError> {
    match (purpose, server_name) {
        (TlsIdentityPurpose::Server, Some(server_name)) => parse_dns_name(server_name).map(|_| ()),
        (TlsIdentityPurpose::Client, None) => Ok(()),
        _ => Err(TlsIdentityError::new(
            TlsIdentityErrorKind::InvalidPurpose,
            "server TLS identities require one DNS name and client identities forbid it",
        )),
    }
}

fn validate_bundle_server_name(
    purpose: TlsIdentityPurpose,
    bundle_name: Option<&str>,
    expected_name: Option<&str>,
) -> Result<(), TlsIdentityError> {
    match (purpose, bundle_name, expected_name) {
        (TlsIdentityPurpose::Server, Some(bundle_name), Some(expected_name)) => {
            parse_dns_name(bundle_name)?;
            if bundle_name == expected_name {
                Ok(())
            } else {
                Err(TlsIdentityError::new(
                    TlsIdentityErrorKind::ServerNameMismatch,
                    "TLS identity bundle server name does not match this process",
                ))
            }
        }
        (TlsIdentityPurpose::Client, None, None) => Ok(()),
        _ => Err(TlsIdentityError::new(
            TlsIdentityErrorKind::InvalidServerName,
            "TLS identity bundle has an invalid server-name contract",
        )),
    }
}

fn parse_dns_name(name: &str) -> Result<ServerName<'static>, TlsIdentityError> {
    DnsName::try_from(name.to_owned())
        .map(ServerName::DnsName)
        .map_err(|_| {
            TlsIdentityError::new(
                TlsIdentityErrorKind::InvalidServerName,
                "TLS identity server name must be a valid DNS name",
            )
        })
}

fn validate_certificate(
    certificate_chain: &[CertificateDer<'static>],
    issuer_ca: &[CertificateDer<'static>],
    purpose: TlsIdentityPurpose,
    server_name: Option<&str>,
    now: UnixTime,
) -> Result<(), TlsIdentityError> {
    let mut roots = RootCertStore::empty();
    for certificate in issuer_ca {
        roots.add(certificate.clone()).map_err(|_| {
            TlsIdentityError::new(
                TlsIdentityErrorKind::InvalidIssuerCa,
                "TLS identity issuer CA cannot be used as a trust anchor",
            )
        })?;
    }
    let end_entity = certificate_chain.first().ok_or_else(|| {
        TlsIdentityError::new(
            TlsIdentityErrorKind::InvalidCertificate,
            "TLS identity certificate chain is empty",
        )
    })?;
    validate_required_extended_key_usage(end_entity, purpose)?;
    let intermediates = &certificate_chain[1..];
    let provider = tls_crypto_provider();
    let result = match purpose {
        TlsIdentityPurpose::Server => {
            let verifier =
                WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
                    .build()
                    .map_err(|_| {
                        TlsIdentityError::new(
                            TlsIdentityErrorKind::InvalidIssuerCa,
                            "TLS identity issuer CA cannot verify a server certificate",
                        )
                    })?;
            verifier
                .verify_server_cert(
                    end_entity,
                    intermediates,
                    &parse_dns_name(server_name.expect("validated server name"))?,
                    &[],
                    now,
                )
                .map(|_| ())
        }
        TlsIdentityPurpose::Client => {
            let verifier =
                WebPkiClientVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
                    .build()
                    .map_err(|_| {
                        TlsIdentityError::new(
                            TlsIdentityErrorKind::InvalidIssuerCa,
                            "TLS identity issuer CA cannot verify a client certificate",
                        )
                    })?;
            verifier
                .verify_client_cert(end_entity, intermediates, now)
                .map(|_| ())
        }
    };
    result.map_err(classify_certificate_validation_error)
}

fn validate_issuer_ca(certificates: &[CertificateDer<'_>]) -> Result<(), TlsIdentityError> {
    for certificate_der in certificates {
        let (remaining, certificate) =
            parse_x509_certificate(certificate_der.as_ref()).map_err(|_| {
                TlsIdentityError::new(
                    TlsIdentityErrorKind::InvalidIssuerCa,
                    "TLS identity issuer CA certificate cannot be parsed",
                )
            })?;
        if !remaining.is_empty() {
            return Err(TlsIdentityError::new(
                TlsIdentityErrorKind::InvalidIssuerCa,
                "TLS identity issuer CA certificate contains trailing bytes",
            ));
        }
        let is_ca = certificate
            .basic_constraints()
            .map_err(|_| {
                TlsIdentityError::new(
                    TlsIdentityErrorKind::InvalidIssuerCa,
                    "TLS identity issuer CA has invalid basic constraints",
                )
            })?
            .is_some_and(|constraints| constraints.value.ca);
        if !is_ca {
            return Err(TlsIdentityError::new(
                TlsIdentityErrorKind::InvalidIssuerCa,
                "TLS identity issuer certificate is not a certificate authority",
            ));
        }
        if certificate
            .key_usage()
            .map_err(|_| {
                TlsIdentityError::new(
                    TlsIdentityErrorKind::InvalidIssuerCa,
                    "TLS identity issuer CA has an invalid key-usage extension",
                )
            })?
            .is_some_and(|usage| !usage.value.key_cert_sign())
        {
            return Err(TlsIdentityError::new(
                TlsIdentityErrorKind::InvalidIssuerCa,
                "TLS identity issuer CA is not permitted to sign certificates",
            ));
        }
    }
    Ok(())
}

fn validate_required_extended_key_usage(
    end_entity: &CertificateDer<'_>,
    purpose: TlsIdentityPurpose,
) -> Result<(), TlsIdentityError> {
    let (remaining, certificate) = parse_x509_certificate(end_entity.as_ref()).map_err(|_| {
        TlsIdentityError::new(
            TlsIdentityErrorKind::InvalidCertificate,
            "TLS identity leaf certificate cannot be parsed",
        )
    })?;
    if !remaining.is_empty() {
        return Err(TlsIdentityError::new(
            TlsIdentityErrorKind::InvalidCertificate,
            "TLS identity leaf certificate contains trailing bytes",
        ));
    }
    let extended_key_usage = certificate
        .extended_key_usage()
        .map_err(|_| {
            TlsIdentityError::new(
                TlsIdentityErrorKind::InvalidCertificate,
                "TLS identity leaf certificate has an invalid extended-key-usage extension",
            )
        })?
        .ok_or_else(|| {
            TlsIdentityError::new(
                TlsIdentityErrorKind::WrongEku,
                "TLS identity leaf certificate omits its required extended key usage",
            )
        })?;
    let has_required_usage = match purpose {
        TlsIdentityPurpose::Server => extended_key_usage.value.server_auth,
        TlsIdentityPurpose::Client => extended_key_usage.value.client_auth,
    };
    if !has_required_usage {
        return Err(TlsIdentityError::new(
            TlsIdentityErrorKind::WrongEku,
            "TLS identity leaf certificate omits its required extended key usage",
        ));
    }
    Ok(())
}

fn classify_certificate_validation_error(error: rustls::Error) -> TlsIdentityError {
    use rustls::CertificateError;

    let kind = match error {
        rustls::Error::InvalidCertificate(
            CertificateError::Expired | CertificateError::ExpiredContext { .. },
        ) => TlsIdentityErrorKind::CertificateExpired,
        rustls::Error::InvalidCertificate(
            CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. },
        ) => TlsIdentityErrorKind::CertificateNotYetValid,
        rustls::Error::InvalidCertificate(
            CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
        ) => TlsIdentityErrorKind::WrongHostname,
        rustls::Error::InvalidCertificate(
            CertificateError::InvalidPurpose | CertificateError::InvalidPurposeContext { .. },
        ) => TlsIdentityErrorKind::WrongEku,
        rustls::Error::InvalidCertificate(
            CertificateError::UnknownIssuer | CertificateError::BadSignature,
        ) => TlsIdentityErrorKind::WrongCa,
        _ => TlsIdentityErrorKind::CertificateValidation,
    };
    TlsIdentityError::new(
        kind,
        "TLS identity certificate is not valid for its CA, time, purpose, or server name",
    )
}

fn certificate_slices_equal(
    left: &[CertificateDer<'static>],
    right: &[CertificateDer<'static>],
) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.as_ref() == right.as_ref())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlsIdentityMode {
    StaticPaths,
    WatchedBundle,
}

impl TlsIdentityMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StaticPaths => "static-paths",
            Self::WatchedBundle => "watched-bundle",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TlsIdentityStatus {
    pub mode: TlsIdentityMode,
    pub identity_id: Option<String>,
    pub purpose: TlsIdentityPurpose,
    pub server_name: Option<String>,
    pub bundle_generation: Option<u64>,
    pub leaf_certificate_sha256: Option<String>,
    pub certificate_chain_length: usize,
    pub issuer_ca_count: usize,
    pub successful_activations: u64,
    pub rejected_reloads: u64,
    pub last_error_kind: Option<TlsIdentityErrorKind>,
}

impl TlsIdentityStatus {
    pub const fn static_paths(purpose: TlsIdentityPurpose) -> Self {
        Self {
            mode: TlsIdentityMode::StaticPaths,
            identity_id: None,
            purpose,
            server_name: None,
            bundle_generation: None,
            leaf_certificate_sha256: None,
            certificate_chain_length: 0,
            issuer_ca_count: 0,
            successful_activations: 0,
            rejected_reloads: 0,
            last_error_kind: None,
        }
    }
}

struct TlsIdentityState {
    bundle: VerifiedTlsIdentityBundle,
}

struct TlsIdentityInner {
    state: RwLock<TlsIdentityState>,
    successful_activations: AtomicU64,
    rejected_reloads: AtomicU64,
    last_error: RwLock<Option<TlsIdentityErrorKind>>,
}

#[derive(Clone)]
pub struct TlsIdentity {
    inner: Arc<TlsIdentityInner>,
}

impl fmt::Debug for TlsIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TlsIdentity")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct TlsIdentitySnapshot {
    pub cluster_id: String,
    pub generation: u64,
    pub identity_id: String,
    pub purpose: TlsIdentityPurpose,
    pub server_name: Option<String>,
    pub leaf_certificate_der: Vec<u8>,
}

impl fmt::Debug for TlsIdentitySnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TlsIdentitySnapshot")
            .field("cluster_id", &self.cluster_id)
            .field("generation", &self.generation)
            .field("identity_id", &self.identity_id)
            .field("purpose", &self.purpose)
            .field("server_name", &self.server_name)
            .field(
                "leaf_certificate_sha256",
                &sha256_hex(&self.leaf_certificate_der),
            )
            .field("leaf_certificate_length", &self.leaf_certificate_der.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlsIdentityActivationOutcome {
    Activated,
    Unchanged,
}

impl TlsIdentity {
    pub fn from_bundle(bundle: VerifiedTlsIdentityBundle) -> Self {
        Self {
            inner: Arc::new(TlsIdentityInner {
                state: RwLock::new(TlsIdentityState { bundle }),
                successful_activations: AtomicU64::new(0),
                rejected_reloads: AtomicU64::new(0),
                last_error: RwLock::new(None),
            }),
        }
    }

    pub fn snapshot(&self) -> TlsIdentitySnapshot {
        let state = self
            .inner
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        TlsIdentitySnapshot {
            cluster_id: state.bundle.cluster_id.clone(),
            generation: state.bundle.generation,
            identity_id: state.bundle.identity_id.clone(),
            purpose: state.bundle.purpose,
            server_name: state.bundle.server_name.clone(),
            leaf_certificate_der: state.bundle.certificate_chain[0].as_ref().to_vec(),
        }
    }

    pub fn activate_bundle(
        &self,
        candidate: VerifiedTlsIdentityBundle,
        publish: impl FnOnce(&VerifiedTlsIdentityBundle) -> Result<(), ()>,
    ) -> Result<TlsIdentityActivationOutcome, TlsIdentityError> {
        let mut state = self
            .inner
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = (|| {
            if candidate.cluster_id != state.bundle.cluster_id {
                return Err(TlsIdentityError::new(
                    TlsIdentityErrorKind::ClusterMismatch,
                    "TLS identity candidate cluster differs from the active identity",
                ));
            }
            if candidate.identity_id != state.bundle.identity_id {
                return Err(TlsIdentityError::new(
                    TlsIdentityErrorKind::IdentityMismatch,
                    "TLS identity candidate identity differs from the active identity",
                ));
            }
            if candidate.purpose != state.bundle.purpose {
                return Err(TlsIdentityError::new(
                    TlsIdentityErrorKind::PurposeMismatch,
                    "TLS identity candidate purpose differs from the active identity",
                ));
            }
            if candidate.server_name != state.bundle.server_name {
                return Err(TlsIdentityError::new(
                    TlsIdentityErrorKind::ServerNameMismatch,
                    "TLS identity candidate server name differs from the active identity",
                ));
            }
            if candidate.generation < state.bundle.generation {
                return Err(TlsIdentityError::new(
                    TlsIdentityErrorKind::StaleGeneration,
                    "TLS identity bundle generation is older than the active generation",
                ));
            }
            if candidate.generation == state.bundle.generation {
                if candidate.semantically_matches(&state.bundle) {
                    return Ok(TlsIdentityActivationOutcome::Unchanged);
                }
                return Err(TlsIdentityError::new(
                    TlsIdentityErrorKind::GenerationFork,
                    "TLS identity bundle reuses the active generation with different contents",
                ));
            }
            if !candidate.issuer_ca_matches(&state.bundle) {
                return Err(TlsIdentityError::new(
                    TlsIdentityErrorKind::IssuerCaMismatch,
                    "TLS identity bundle changes the process-pinned issuer CA",
                ));
            }
            publish(&candidate).map_err(|()| TlsIdentityError::runtime_configuration())?;
            state.bundle = candidate;
            Ok(TlsIdentityActivationOutcome::Activated)
        })();

        match &result {
            Ok(TlsIdentityActivationOutcome::Activated) => {
                self.inner
                    .successful_activations
                    .fetch_add(1, Ordering::Relaxed);
                self.clear_error();
            }
            Ok(TlsIdentityActivationOutcome::Unchanged) => self.clear_error(),
            Err(error) => self.record_rejection(error.kind()),
        }
        result
    }

    pub fn record_rejection(&self, kind: TlsIdentityErrorKind) {
        self.inner.rejected_reloads.fetch_add(1, Ordering::Relaxed);
        *self
            .inner
            .last_error
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(kind);
    }

    pub fn status(&self) -> TlsIdentityStatus {
        let state = self
            .inner
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        TlsIdentityStatus {
            mode: TlsIdentityMode::WatchedBundle,
            identity_id: Some(state.bundle.identity_id.clone()),
            purpose: state.bundle.purpose,
            server_name: state.bundle.server_name.clone(),
            bundle_generation: Some(state.bundle.generation),
            leaf_certificate_sha256: Some(sha256_hex(state.bundle.certificate_chain[0].as_ref())),
            certificate_chain_length: state.bundle.certificate_chain.len(),
            issuer_ca_count: state.bundle.issuer_ca.len(),
            successful_activations: self.inner.successful_activations.load(Ordering::Relaxed),
            rejected_reloads: self.inner.rejected_reloads.load(Ordering::Relaxed),
            last_error_kind: *self
                .inner
                .last_error
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        }
    }

    fn clear_error(&self) {
        *self
            .inner
            .last_error
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TlsIdentityBundleObservation(ObservationKind);

#[derive(Clone, Debug, Eq, PartialEq)]
enum ObservationKind {
    Present(TlsIdentityBundleFileStamp),
    Unavailable(io::ErrorKind),
}

#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct TlsIdentityBundleFileStamp {
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
struct TlsIdentityBundleFileStamp {
    size: u64,
    modified: Option<std::time::SystemTime>,
    readonly: bool,
}

#[cfg(unix)]
pub fn tls_identity_bundle_observation(path: &Path) -> TlsIdentityBundleObservation {
    use std::os::unix::fs::MetadataExt as _;

    let observation = match std::fs::metadata(path) {
        Ok(metadata) => ObservationKind::Present(TlsIdentityBundleFileStamp {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }),
        Err(error) => ObservationKind::Unavailable(error.kind()),
    };
    TlsIdentityBundleObservation(observation)
}

#[cfg(not(unix))]
pub fn tls_identity_bundle_observation(path: &Path) -> TlsIdentityBundleObservation {
    let observation = match std::fs::metadata(path) {
        Ok(metadata) => ObservationKind::Present(TlsIdentityBundleFileStamp {
            size: metadata.len(),
            modified: metadata.modified().ok(),
            readonly: metadata.permissions().readonly(),
        }),
        Err(error) => ObservationKind::Unavailable(error.kind()),
    };
    TlsIdentityBundleObservation(observation)
}

#[derive(Debug)]
pub enum TlsIdentityReloadError {
    Source(TlsIdentityError),
    Activation(TlsIdentityError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlsIdentityPollOutcome {
    Skipped,
    Activated,
    Unchanged,
    Rejected {
        kind: TlsIdentityErrorKind,
        report: bool,
    },
}

#[derive(Default)]
pub struct TlsIdentityWatcherLoop {
    completed_source_observation: Option<TlsIdentityBundleObservation>,
    reported_source_failure: Option<(TlsIdentityBundleObservation, TlsIdentityErrorKind)>,
}

impl TlsIdentityWatcherLoop {
    pub fn poll(
        &mut self,
        observation: TlsIdentityBundleObservation,
        identity: &TlsIdentity,
        reload: impl FnOnce() -> Result<TlsIdentityActivationOutcome, TlsIdentityReloadError>,
    ) -> TlsIdentityPollOutcome {
        if self.completed_source_observation.as_ref() == Some(&observation) {
            return TlsIdentityPollOutcome::Skipped;
        }
        if self.completed_source_observation.as_ref() != Some(&observation) {
            self.completed_source_observation = None;
        }
        match reload() {
            Ok(TlsIdentityActivationOutcome::Activated) => {
                self.completed_source_observation = Some(observation);
                self.reported_source_failure = None;
                TlsIdentityPollOutcome::Activated
            }
            Ok(TlsIdentityActivationOutcome::Unchanged) => {
                self.completed_source_observation = Some(observation);
                self.reported_source_failure = None;
                TlsIdentityPollOutcome::Unchanged
            }
            Err(TlsIdentityReloadError::Source(error)) => {
                let kind = error.kind();
                let report =
                    self.reported_source_failure.as_ref() != Some(&(observation.clone(), kind));
                if report {
                    identity.record_rejection(kind);
                    self.reported_source_failure = Some((observation.clone(), kind));
                }
                if !matches!(
                    kind,
                    TlsIdentityErrorKind::SourceUnavailable
                        | TlsIdentityErrorKind::CertificateNotYetValid
                ) {
                    self.completed_source_observation = Some(observation);
                }
                TlsIdentityPollOutcome::Rejected { kind, report }
            }
            Err(TlsIdentityReloadError::Activation(error)) => {
                let kind = error.kind();
                self.completed_source_observation = Some(observation);
                self.reported_source_failure = None;
                TlsIdentityPollOutcome::Rejected { kind, report: true }
            }
        }
    }
}

fn read_bundle_file(path: &Path) -> Result<Vec<u8>, TlsIdentityError> {
    let source_metadata = std::fs::symlink_metadata(path).map_err(|_| {
        TlsIdentityError::new(
            TlsIdentityErrorKind::SourceUnavailable,
            "TLS identity bundle metadata is unavailable",
        )
    })?;
    if source_metadata.file_type().is_symlink() || !source_metadata.file_type().is_file() {
        return Err(TlsIdentityError::new(
            TlsIdentityErrorKind::NotRegularFile,
            "TLS identity bundle must be a regular file and not a symbolic link",
        ));
    }
    let mut file = open_bundle_file(path)?;
    let metadata = file.metadata().map_err(|_| {
        TlsIdentityError::new(
            TlsIdentityErrorKind::SourceUnavailable,
            "TLS identity bundle metadata is unavailable",
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(TlsIdentityError::new(
            TlsIdentityErrorKind::NotRegularFile,
            "TLS identity bundle must be a regular file",
        ));
    }
    validate_opened_file_identity(&source_metadata, &metadata)?;
    validate_file_permissions(&metadata)?;
    if metadata.len() > MAX_TLS_IDENTITY_BUNDLE_BYTES as u64 {
        return Err(TlsIdentityError::new(
            TlsIdentityErrorKind::BundleTooLarge,
            "TLS identity bundle exceeds the byte limit",
        ));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take((MAX_TLS_IDENTITY_BUNDLE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            TlsIdentityError::new(
                TlsIdentityErrorKind::SourceUnavailable,
                "TLS identity bundle file could not be read",
            )
        })?;
    if bytes.len() > MAX_TLS_IDENTITY_BUNDLE_BYTES {
        return Err(TlsIdentityError::new(
            TlsIdentityErrorKind::BundleTooLarge,
            "TLS identity bundle exceeds the byte limit",
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn open_bundle_file(path: &Path) -> Result<File, TlsIdentityError> {
    use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt as _};

    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| {
            TlsIdentityError::new(
                TlsIdentityErrorKind::SourceUnavailable,
                "TLS identity bundle file is unavailable",
            )
        })
}

#[cfg(not(unix))]
fn open_bundle_file(path: &Path) -> Result<File, TlsIdentityError> {
    File::open(path).map_err(|_| {
        TlsIdentityError::new(
            TlsIdentityErrorKind::SourceUnavailable,
            "TLS identity bundle file is unavailable",
        )
    })
}

#[cfg(unix)]
fn validate_opened_file_identity(
    source_metadata: &std::fs::Metadata,
    opened_metadata: &std::fs::Metadata,
) -> Result<(), TlsIdentityError> {
    use std::os::unix::fs::MetadataExt as _;

    if source_metadata.dev() != opened_metadata.dev()
        || source_metadata.ino() != opened_metadata.ino()
    {
        return Err(TlsIdentityError::new(
            TlsIdentityErrorKind::SourceUnavailable,
            "TLS identity bundle source changed before it could be read",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_opened_file_identity(
    _source_metadata: &std::fs::Metadata,
    _opened_metadata: &std::fs::Metadata,
) -> Result<(), TlsIdentityError> {
    Ok(())
}

#[cfg(unix)]
fn validate_file_permissions(metadata: &std::fs::Metadata) -> Result<(), TlsIdentityError> {
    use std::os::unix::fs::PermissionsExt as _;

    if metadata.permissions().mode() & 0o777 != 0o600 {
        return Err(TlsIdentityError::new(
            TlsIdentityErrorKind::UnsafePermissions,
            "TLS identity bundle permissions must be exactly 0600",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_file_permissions(_metadata: &std::fs::Metadata) -> Result<(), TlsIdentityError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Arc, Barrier},
        thread,
        time::Duration,
    };

    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
        KeyUsagePurpose,
    };
    use serde_json::json;

    use super::*;

    struct TestDirectory(std::path::PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "inferlab-tls-identity-{name}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }

        fn write_bundle(&self, name: &str, bytes: &[u8]) -> std::path::PathBuf {
            let path = self.0.join(name);
            fs::write(&path, bytes).expect("write bundle");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                    .expect("set bundle mode");
            }
            path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct TestCa {
        certificate_pem: String,
        issuer: Issuer<'static, KeyPair>,
    }

    fn test_ca() -> TestCa {
        let mut parameters = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        parameters.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        parameters.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let key = KeyPair::generate().expect("CA key");
        let certificate = parameters.self_signed(&key).expect("CA cert");
        TestCa {
            certificate_pem: certificate.pem(),
            issuer: Issuer::new(parameters, key),
        }
    }

    fn test_non_ca_issuer() -> TestCa {
        let parameters = CertificateParams::new(Vec::<String>::new()).expect("issuer params");
        let key = KeyPair::generate().expect("issuer key");
        let certificate = parameters.self_signed(&key).expect("issuer cert");
        TestCa {
            certificate_pem: certificate.pem(),
            issuer: Issuer::new(parameters, key),
        }
    }

    fn leaf(ca: &TestCa, purpose: TlsIdentityPurpose, name: &str) -> (String, String) {
        let mut parameters = match purpose {
            TlsIdentityPurpose::Server => {
                CertificateParams::new(vec![name.to_owned()]).expect("server params")
            }
            TlsIdentityPurpose::Client => {
                CertificateParams::new(Vec::<String>::new()).expect("client params")
            }
        };
        parameters.extended_key_usages = vec![match purpose {
            TlsIdentityPurpose::Server => ExtendedKeyUsagePurpose::ServerAuth,
            TlsIdentityPurpose::Client => ExtendedKeyUsagePurpose::ClientAuth,
        }];
        let key = KeyPair::generate().expect("leaf key");
        let certificate = parameters
            .signed_by(&key, &ca.issuer)
            .expect("leaf certificate");
        (certificate.pem(), key.serialize_pem())
    }

    fn leaf_without_eku(ca: &TestCa, purpose: TlsIdentityPurpose, name: &str) -> (String, String) {
        let parameters = match purpose {
            TlsIdentityPurpose::Server => {
                CertificateParams::new(vec![name.to_owned()]).expect("server params")
            }
            TlsIdentityPurpose::Client => {
                CertificateParams::new(Vec::<String>::new()).expect("client params")
            }
        };
        let key = KeyPair::generate().expect("leaf key");
        let certificate = parameters
            .signed_by(&key, &ca.issuer)
            .expect("leaf certificate");
        (certificate.pem(), key.serialize_pem())
    }

    fn bundle_bytes(
        ca: &TestCa,
        generation: u64,
        identity_id: &str,
        purpose: TlsIdentityPurpose,
        name: Option<&str>,
    ) -> Vec<u8> {
        let (certificate, key) = leaf(ca, purpose, name.unwrap_or(identity_id));
        serde_json::to_vec(&json!({
            "schema": TLS_IDENTITY_BUNDLE_SCHEMA,
            "cluster_id": "inferlab-primary",
            "generation": generation,
            "identity_id": identity_id,
            "purpose": purpose.as_str(),
            "server_name": name,
            "certificate_chain_pem": certificate,
            "private_key_pem": key,
            "issuer_ca_pem": ca.certificate_pem,
        }))
        .expect("encode bundle")
    }

    fn decode(
        bytes: &[u8],
        identity_id: &str,
        purpose: TlsIdentityPurpose,
        name: Option<&str>,
    ) -> VerifiedTlsIdentityBundle {
        VerifiedTlsIdentityBundle::decode(bytes, "inferlab-primary", identity_id, purpose, name)
            .expect("verified bundle")
    }

    #[test]
    fn strict_bundle_is_bound_bounded_and_redacted() {
        let ca = test_ca();
        let valid = bundle_bytes(
            &ca,
            1,
            "trust-distributor",
            TlsIdentityPurpose::Server,
            Some("localhost"),
        );
        let verified = decode(
            &valid,
            "trust-distributor",
            TlsIdentityPurpose::Server,
            Some("localhost"),
        );
        assert_eq!(verified.generation(), 1);
        assert_eq!(verified.certificate_chain_length(), 1);
        assert_eq!(verified.issuer_ca_count(), 1);

        let debug = format!("{verified:?}");
        assert!(!debug.contains("BEGIN"));
        assert!(!debug.contains("PRIVATE"));

        let wrong_identity = VerifiedTlsIdentityBundle::decode(
            &valid,
            "inferlab-primary",
            "other",
            TlsIdentityPurpose::Server,
            Some("localhost"),
        )
        .expect_err("identity mismatch");
        assert_eq!(
            wrong_identity.kind(),
            TlsIdentityErrorKind::IdentityMismatch
        );

        assert_eq!(
            VerifiedTlsIdentityBundle::decode(
                &vec![b' '; MAX_TLS_IDENTITY_BUNDLE_BYTES + 1],
                "inferlab-primary",
                "trust-distributor",
                TlsIdentityPurpose::Server,
                Some("localhost"),
            )
            .expect_err("oversized")
            .kind(),
            TlsIdentityErrorKind::BundleTooLarge
        );
    }

    #[test]
    fn purpose_hostname_ca_and_private_key_fail_closed() {
        let ca = test_ca();
        let other_ca = test_ca();
        let server = bundle_bytes(
            &ca,
            1,
            "trust-distributor",
            TlsIdentityPurpose::Server,
            Some("localhost"),
        );
        assert_eq!(
            VerifiedTlsIdentityBundle::decode(
                &server,
                "inferlab-primary",
                "trust-distributor",
                TlsIdentityPurpose::Client,
                None,
            )
            .expect_err("wrong purpose")
            .kind(),
            TlsIdentityErrorKind::PurposeMismatch
        );
        assert_eq!(
            VerifiedTlsIdentityBundle::decode(
                &server,
                "inferlab-primary",
                "trust-distributor",
                TlsIdentityPurpose::Server,
                Some("example.test"),
            )
            .expect_err("wrong expected name")
            .kind(),
            TlsIdentityErrorKind::ServerNameMismatch
        );

        let mut wrong_ca: serde_json::Value = serde_json::from_slice(&server).expect("JSON");
        wrong_ca["issuer_ca_pem"] = json!(other_ca.certificate_pem);
        assert_eq!(
            VerifiedTlsIdentityBundle::decode(
                &serde_json::to_vec(&wrong_ca).expect("JSON"),
                "inferlab-primary",
                "trust-distributor",
                TlsIdentityPurpose::Server,
                Some("localhost"),
            )
            .expect_err("wrong CA")
            .kind(),
            TlsIdentityErrorKind::WrongCa
        );

        let client = bundle_bytes(&ca, 1, "node-a", TlsIdentityPurpose::Client, None);
        let client_value: serde_json::Value = serde_json::from_slice(&client).expect("JSON");
        let mut mismatched: serde_json::Value = serde_json::from_slice(&server).expect("JSON");
        mismatched["private_key_pem"] = client_value["private_key_pem"].clone();
        assert_eq!(
            VerifiedTlsIdentityBundle::decode(
                &serde_json::to_vec(&mismatched).expect("JSON"),
                "inferlab-primary",
                "trust-distributor",
                TlsIdentityPurpose::Server,
                Some("localhost"),
            )
            .expect_err("key mismatch")
            .kind(),
            TlsIdentityErrorKind::PrivateKeyMismatch
        );

        let non_ca = test_non_ca_issuer();
        let non_ca_bundle = bundle_bytes(
            &non_ca,
            1,
            "trust-distributor",
            TlsIdentityPurpose::Server,
            Some("localhost"),
        );
        assert_eq!(
            VerifiedTlsIdentityBundle::decode(
                &non_ca_bundle,
                "inferlab-primary",
                "trust-distributor",
                TlsIdentityPurpose::Server,
                Some("localhost"),
            )
            .expect_err("non-CA trust anchor")
            .kind(),
            TlsIdentityErrorKind::InvalidIssuerCa
        );
    }

    #[test]
    fn current_time_and_eku_are_verified() {
        let ca = test_ca();
        let client = bundle_bytes(&ca, 1, "node-a", TlsIdentityPurpose::Client, None);
        assert_eq!(
            VerifiedTlsIdentityBundle::decode_at(
                &client,
                "inferlab-primary",
                "node-a",
                TlsIdentityPurpose::Client,
                None,
                UnixTime::since_unix_epoch(Duration::from_secs(70_000_000_000)),
            )
            .expect_err("expired at future time")
            .kind(),
            TlsIdentityErrorKind::CertificateExpired
        );

        let server_usage = bundle_bytes(
            &ca,
            1,
            "node-a",
            TlsIdentityPurpose::Server,
            Some("localhost"),
        );
        let mut wrong_usage: serde_json::Value =
            serde_json::from_slice(&client).expect("client JSON");
        let server_value: serde_json::Value =
            serde_json::from_slice(&server_usage).expect("server JSON");
        wrong_usage["certificate_chain_pem"] = server_value["certificate_chain_pem"].clone();
        wrong_usage["private_key_pem"] = server_value["private_key_pem"].clone();
        assert_eq!(
            VerifiedTlsIdentityBundle::decode(
                &serde_json::to_vec(&wrong_usage).expect("JSON"),
                "inferlab-primary",
                "node-a",
                TlsIdentityPurpose::Client,
                None,
            )
            .expect_err("wrong EKU")
            .kind(),
            TlsIdentityErrorKind::WrongEku
        );

        for purpose in [TlsIdentityPurpose::Server, TlsIdentityPurpose::Client] {
            let identity_id = match purpose {
                TlsIdentityPurpose::Server => "trust-distributor",
                TlsIdentityPurpose::Client => "node-a",
            };
            let server_name = (purpose == TlsIdentityPurpose::Server).then_some("localhost");
            let (certificate, key) =
                leaf_without_eku(&ca, purpose, server_name.unwrap_or(identity_id));
            let missing_eku = serde_json::to_vec(&json!({
                "schema": TLS_IDENTITY_BUNDLE_SCHEMA,
                "cluster_id": "inferlab-primary",
                "generation": 1,
                "identity_id": identity_id,
                "purpose": purpose.as_str(),
                "server_name": server_name,
                "certificate_chain_pem": certificate,
                "private_key_pem": key,
                "issuer_ca_pem": ca.certificate_pem,
            }))
            .expect("encode bundle");
            assert_eq!(
                VerifiedTlsIdentityBundle::decode(
                    &missing_eku,
                    "inferlab-primary",
                    identity_id,
                    purpose,
                    server_name,
                )
                .expect_err("missing EKU")
                .kind(),
                TlsIdentityErrorKind::WrongEku
            );
        }
    }

    #[test]
    fn file_loader_rejects_permissions_and_symlinks() {
        let directory = TestDirectory::new("file-custody");
        let ca = test_ca();
        let bytes = bundle_bytes(&ca, 1, "node-a", TlsIdentityPurpose::Client, None);
        let path = directory.write_bundle("identity.json", &bytes);
        VerifiedTlsIdentityBundle::load(
            &path,
            "inferlab-primary",
            "node-a",
            TlsIdentityPurpose::Client,
            None,
        )
        .expect("safe bundle");

        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt as _, symlink};
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("unsafe mode");
            assert_eq!(
                VerifiedTlsIdentityBundle::load(
                    &path,
                    "inferlab-primary",
                    "node-a",
                    TlsIdentityPurpose::Client,
                    None,
                )
                .expect_err("unsafe mode")
                .kind(),
                TlsIdentityErrorKind::UnsafePermissions
            );
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("safe mode");
            let link = directory.0.join("identity-link.json");
            symlink(&path, &link).expect("symlink");
            assert_eq!(
                VerifiedTlsIdentityBundle::load(
                    &link,
                    "inferlab-primary",
                    "node-a",
                    TlsIdentityPurpose::Client,
                    None,
                )
                .expect_err("symlink")
                .kind(),
                TlsIdentityErrorKind::NotRegularFile
            );

            let fifo = directory.0.join("identity-fifo.json");
            let status = std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("invoke mkfifo");
            assert!(status.success());
            assert_eq!(
                VerifiedTlsIdentityBundle::load(
                    &fifo,
                    "inferlab-primary",
                    "node-a",
                    TlsIdentityPurpose::Client,
                    None,
                )
                .expect_err("FIFO")
                .kind(),
                TlsIdentityErrorKind::NotRegularFile
            );
        }
    }

    #[test]
    fn activation_rejects_rollback_fork_ca_change_and_runtime_failure() {
        let ca = test_ca();
        let other_ca = test_ca();
        let first = bundle_bytes(&ca, 1, "node-a", TlsIdentityPurpose::Client, None);
        let unchanged = first.clone();
        let fork = bundle_bytes(&ca, 1, "node-a", TlsIdentityPurpose::Client, None);
        let second = bundle_bytes(&ca, 2, "node-a", TlsIdentityPurpose::Client, None);
        let wrong_ca = bundle_bytes(&other_ca, 3, "node-a", TlsIdentityPurpose::Client, None);
        let identity =
            TlsIdentity::from_bundle(decode(&first, "node-a", TlsIdentityPurpose::Client, None));

        assert_eq!(
            identity
                .activate_bundle(
                    decode(&unchanged, "node-a", TlsIdentityPurpose::Client, None,),
                    |_| Ok(()),
                )
                .expect("unchanged"),
            TlsIdentityActivationOutcome::Unchanged
        );
        assert_eq!(
            identity
                .activate_bundle(
                    decode(&fork, "node-a", TlsIdentityPurpose::Client, None),
                    |_| Ok(()),
                )
                .expect_err("fork")
                .kind(),
            TlsIdentityErrorKind::GenerationFork
        );
        assert_eq!(
            identity
                .activate_bundle(
                    decode(&second, "node-a", TlsIdentityPurpose::Client, None),
                    |_| Err(()),
                )
                .expect_err("runtime failure")
                .kind(),
            TlsIdentityErrorKind::RuntimeConfiguration
        );
        assert_eq!(identity.snapshot().generation, 1);
        assert_eq!(
            identity
                .activate_bundle(
                    decode(&second, "node-a", TlsIdentityPurpose::Client, None),
                    |_| Ok(()),
                )
                .expect("activate"),
            TlsIdentityActivationOutcome::Activated
        );
        assert_eq!(
            identity
                .activate_bundle(
                    decode(&wrong_ca, "node-a", TlsIdentityPurpose::Client, None,),
                    |_| Ok(()),
                )
                .expect_err("CA migration")
                .kind(),
            TlsIdentityErrorKind::IssuerCaMismatch
        );
        let status = identity.status();
        assert_eq!(status.bundle_generation, Some(2));
        assert_eq!(status.successful_activations, 1);
        assert_eq!(status.rejected_reloads, 3);
        assert_eq!(
            status.last_error_kind,
            Some(TlsIdentityErrorKind::IssuerCaMismatch)
        );
    }

    #[test]
    fn concurrent_snapshots_are_entirely_old_or_new() {
        let ca = test_ca();
        let first = bundle_bytes(&ca, 1, "node-a", TlsIdentityPurpose::Client, None);
        let second = bundle_bytes(&ca, 2, "node-a", TlsIdentityPurpose::Client, None);
        let identity = Arc::new(TlsIdentity::from_bundle(decode(
            &first,
            "node-a",
            TlsIdentityPurpose::Client,
            None,
        )));
        let old = identity.snapshot();
        let old_debug = format!("{old:?}");
        assert!(old_debug.contains("leaf_certificate_sha256"));
        assert!(!old_debug.contains(&format!("{:?}", old.leaf_certificate_der)));
        let barrier = Arc::new(Barrier::new(2));
        let reader_identity = Arc::clone(&identity);
        let reader_barrier = Arc::clone(&barrier);
        let reader = thread::spawn(move || {
            reader_barrier.wait();
            (0..10_000)
                .map(|_| reader_identity.snapshot())
                .collect::<Vec<_>>()
        });
        barrier.wait();
        identity
            .activate_bundle(
                decode(&second, "node-a", TlsIdentityPurpose::Client, None),
                |_| Ok(()),
            )
            .expect("activate");
        let new = identity.snapshot();
        let snapshots = reader.join().expect("reader");
        assert!(
            snapshots
                .iter()
                .all(|snapshot| { snapshot == &old || snapshot == &new })
        );
        assert_ne!(old.leaf_certificate_der, new.leaf_certificate_der);
    }

    #[test]
    fn watcher_loop_deduplicates_deterministic_errors_and_retries_time_dependent_sources() {
        let directory = TestDirectory::new("watcher-loop");
        let ca = test_ca();
        let bytes = bundle_bytes(&ca, 1, "node-a", TlsIdentityPurpose::Client, None);
        let path = directory.write_bundle("identity.json", &bytes);
        let identity =
            TlsIdentity::from_bundle(decode(&bytes, "node-a", TlsIdentityPurpose::Client, None));
        let mut watcher = TlsIdentityWatcherLoop::default();
        let observation = tls_identity_bundle_observation(&path);
        let first = watcher.poll(observation.clone(), &identity, || {
            Err(TlsIdentityReloadError::Source(TlsIdentityError::new(
                TlsIdentityErrorKind::InvalidJson,
                "invalid",
            )))
        });
        assert_eq!(
            first,
            TlsIdentityPollOutcome::Rejected {
                kind: TlsIdentityErrorKind::InvalidJson,
                report: true,
            }
        );
        assert_eq!(
            watcher.poll(observation, &identity, || panic!("deduplicated")),
            TlsIdentityPollOutcome::Skipped
        );
        assert_eq!(identity.status().rejected_reloads, 1);

        let missing = directory.0.join("missing.json");
        let unavailable = tls_identity_bundle_observation(&missing);
        for expected_report in [true, false] {
            assert_eq!(
                watcher.poll(unavailable.clone(), &identity, || {
                    Err(TlsIdentityReloadError::Source(TlsIdentityError::new(
                        TlsIdentityErrorKind::SourceUnavailable,
                        "unavailable",
                    )))
                }),
                TlsIdentityPollOutcome::Rejected {
                    kind: TlsIdentityErrorKind::SourceUnavailable,
                    report: expected_report,
                }
            );
        }
        assert_eq!(identity.status().rejected_reloads, 2);

        let time_dependent = tls_identity_bundle_observation(&path);
        for expected_report in [true, false] {
            assert_eq!(
                watcher.poll(time_dependent.clone(), &identity, || {
                    Err(TlsIdentityReloadError::Source(TlsIdentityError::new(
                        TlsIdentityErrorKind::CertificateNotYetValid,
                        "not yet valid",
                    )))
                }),
                TlsIdentityPollOutcome::Rejected {
                    kind: TlsIdentityErrorKind::CertificateNotYetValid,
                    report: expected_report,
                }
            );
        }
        assert_eq!(identity.status().rejected_reloads, 3);
    }
}
