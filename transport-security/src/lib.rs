use std::{
    fs::File,
    io::{self, BufReader, Read},
    path::{Path, PathBuf},
    sync::Arc,
};

use rustls::{
    RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer},
    server::WebPkiClientVerifier,
};
use rustls_pemfile::Item;

mod identity_bundle;

pub use identity_bundle::{
    MAX_TLS_IDENTITY_BUNDLE_BYTES, TLS_IDENTITY_BUNDLE_SCHEMA, TlsIdentity,
    TlsIdentityActivationOutcome, TlsIdentityBundleObservation, TlsIdentityError,
    TlsIdentityErrorKind, TlsIdentityMode, TlsIdentityPollOutcome, TlsIdentityPurpose,
    TlsIdentityReloadError, TlsIdentitySnapshot, TlsIdentityStatus, TlsIdentityWatcherLoop,
    VerifiedTlsIdentityBundle, tls_identity_bundle_observation,
};

pub const MAX_PEM_FILE_BYTES: usize = 256 * 1024;
pub const MAX_CERTIFICATES_PER_FILE: usize = 32;
pub const TLS_1_3_PROTOCOL_NAME: &str = "TLSv1.3";

#[derive(Clone, Eq, PartialEq)]
pub struct MtlsServerPaths {
    pub certificate_chain: PathBuf,
    pub private_key: PathBuf,
    pub client_ca: PathBuf,
}

impl std::fmt::Debug for MtlsServerPaths {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MtlsServerPaths")
            .field("certificate_chain", &"<redacted>")
            .field("private_key", &"<redacted>")
            .field("client_ca", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct MtlsClientPaths {
    pub server_ca: PathBuf,
    pub certificate_chain: PathBuf,
    pub private_key: PathBuf,
}

impl std::fmt::Debug for MtlsClientPaths {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MtlsClientPaths")
            .field("server_ca", &"<redacted>")
            .field("certificate_chain", &"<redacted>")
            .field("private_key", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerTransportStatus {
    Http,
    MutualTls,
}

impl ServerTransportStatus {
    pub const fn mode(self) -> &'static str {
        match self {
            Self::Http => "insecure-http",
            Self::MutualTls => "mutual-tls",
        }
    }

    pub const fn client_certificate_required(self) -> bool {
        matches!(self, Self::MutualTls)
    }

    pub const fn minimum_protocol(self) -> Option<&'static str> {
        match self {
            Self::Http => None,
            Self::MutualTls => Some(TLS_1_3_PROTOCOL_NAME),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServerTransportConfig {
    Http,
    MutualTls(MtlsServerPaths),
}

impl ServerTransportConfig {
    pub fn from_optional_paths(
        certificate_chain: Option<PathBuf>,
        private_key: Option<PathBuf>,
        client_ca: Option<PathBuf>,
    ) -> io::Result<Self> {
        match (certificate_chain, private_key, client_ca) {
            (None, None, None) => Ok(Self::Http),
            (Some(certificate_chain), Some(private_key), Some(client_ca)) => {
                reject_empty_path(&certificate_chain, "server certificate chain")?;
                reject_empty_path(&private_key, "server private key")?;
                reject_empty_path(&client_ca, "client CA certificate")?;
                Ok(Self::MutualTls(MtlsServerPaths {
                    certificate_chain,
                    private_key,
                    client_ca,
                }))
            }
            _ => Err(invalid_input(
                "TLS configuration must provide the certificate chain, private key, and client CA together",
            )),
        }
    }

    pub const fn status(&self) -> ServerTransportStatus {
        match self {
            Self::Http => ServerTransportStatus::Http,
            Self::MutualTls(_) => ServerTransportStatus::MutualTls,
        }
    }

    pub fn load_tls_config(&self) -> io::Result<Option<ServerConfig>> {
        match self {
            Self::Http => Ok(None),
            Self::MutualTls(paths) => load_mtls_server_config(paths).map(Some),
        }
    }
}

pub fn load_mtls_server_config(paths: &MtlsServerPaths) -> io::Result<ServerConfig> {
    let certificate_chain =
        load_certificate_chain(&paths.certificate_chain, "server certificate chain")?;
    let private_key = load_private_key(&paths.private_key, "server private key")?;
    let client_roots = load_root_store(&paths.client_ca, "client CA certificate")?;
    let provider = tls_crypto_provider();
    let client_verifier =
        WebPkiClientVerifier::builder_with_provider(Arc::new(client_roots), Arc::clone(&provider))
            .build()
            .map_err(|_| {
                invalid_data("client CA certificate is not usable for client verification")
            })?;
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| invalid_data("TLS 1.3 is unavailable with the configured crypto provider"))?
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(certificate_chain, private_key)
        .map_err(|_| {
            invalid_data("server certificate chain and private key are invalid or do not match")
        })?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

pub fn load_mtls_server_config_with_identity(
    identity: &VerifiedTlsIdentityBundle,
    client_ca: &Path,
) -> io::Result<ServerConfig> {
    if identity.purpose() != TlsIdentityPurpose::Server {
        return Err(invalid_input(
            "TLS server runtime requires a verified server identity bundle",
        ));
    }
    let client_roots = load_root_store(client_ca, "client CA certificate")?;
    let provider = tls_crypto_provider();
    let client_verifier =
        WebPkiClientVerifier::builder_with_provider(Arc::new(client_roots), Arc::clone(&provider))
            .build()
            .map_err(|_| {
                invalid_data("client CA certificate is not usable for client verification")
            })?;
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| invalid_data("TLS 1.3 is unavailable with the configured crypto provider"))?
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(identity.certificate_chain(), identity.private_key())
        .map_err(|_| invalid_data("verified server identity could not build a TLS runtime"))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

pub fn configure_mtls_client(
    builder: reqwest::ClientBuilder,
    paths: &MtlsClientPaths,
) -> io::Result<reqwest::ClientBuilder> {
    let server_ca_pem = read_bounded(&paths.server_ca, "server CA certificate")?;
    let server_ca_der = parse_certificate_chain(&server_ca_pem, "server CA certificate")?;
    let mut server_root_store = RootCertStore::empty();
    for certificate in server_ca_der {
        server_root_store.add(certificate).map_err(|_| {
            invalid_data("server CA certificate contains an invalid X.509 certificate")
        })?;
    }
    let server_roots = reqwest::Certificate::from_pem_bundle(&server_ca_pem)
        .map_err(|_| invalid_data("server CA certificate contains invalid PEM data"))?;

    let certificate_pem = read_bounded(&paths.certificate_chain, "client certificate chain")?;
    let certificate_chain = parse_certificate_chain(&certificate_pem, "client certificate chain")?;
    let private_key_pem = read_bounded(&paths.private_key, "client private key")?;
    let private_key = parse_private_key(&private_key_pem, "client private key")?;
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    rustls::sign::CertifiedKey::from_der(certificate_chain, private_key, &provider).map_err(
        |_| invalid_data("client certificate chain and private key are invalid or do not match"),
    )?;

    let mut identity_pem = Vec::with_capacity(certificate_pem.len() + private_key_pem.len() + 1);
    identity_pem.extend_from_slice(&certificate_pem);
    if !identity_pem.ends_with(b"\n") {
        identity_pem.push(b'\n');
    }
    identity_pem.extend_from_slice(&private_key_pem);
    let identity = reqwest::Identity::from_pem(&identity_pem)
        .map_err(|_| invalid_data("client certificate chain and private key are invalid"))?;

    Ok(builder
        .tls_certs_only(server_roots)
        .identity(identity)
        .tls_version_min(reqwest::tls::Version::TLS_1_3)
        .tls_version_max(reqwest::tls::Version::TLS_1_3))
}

pub fn configure_mtls_client_with_identity(
    builder: reqwest::ClientBuilder,
    server_ca: &Path,
    identity: &VerifiedTlsIdentityBundle,
) -> io::Result<reqwest::ClientBuilder> {
    if identity.purpose() != TlsIdentityPurpose::Client {
        return Err(invalid_input(
            "TLS client runtime requires a verified client identity bundle",
        ));
    }
    let server_ca_pem = read_bounded(server_ca, "server CA certificate")?;
    let server_ca_der = parse_certificate_chain(&server_ca_pem, "server CA certificate")?;
    let mut server_root_store = RootCertStore::empty();
    for certificate in server_ca_der {
        server_root_store.add(certificate).map_err(|_| {
            invalid_data("server CA certificate contains an invalid X.509 certificate")
        })?;
    }
    let server_roots = reqwest::Certificate::from_pem_bundle(&server_ca_pem)
        .map_err(|_| invalid_data("server CA certificate contains invalid PEM data"))?;
    let mut identity_pem = Vec::with_capacity(
        identity.certificate_chain_pem().len() + identity.private_key_pem().len() + 1,
    );
    identity_pem.extend_from_slice(identity.certificate_chain_pem());
    if !identity_pem.ends_with(b"\n") {
        identity_pem.push(b'\n');
    }
    identity_pem.extend_from_slice(identity.private_key_pem());
    let identity = reqwest::Identity::from_pem(&identity_pem)
        .map_err(|_| invalid_data("verified client identity could not build a TLS runtime"))?;

    Ok(builder
        .tls_certs_only(server_roots)
        .identity(identity)
        .tls_version_min(reqwest::tls::Version::TLS_1_3)
        .tls_version_max(reqwest::tls::Version::TLS_1_3))
}

fn reject_empty_path(path: &Path, role: &'static str) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Err(invalid_input(format!("{role} path cannot be empty")));
    }
    Ok(())
}

fn load_root_store(path: &Path, role: &'static str) -> io::Result<RootCertStore> {
    let certificates = load_certificate_chain(path, role)?;
    let mut roots = RootCertStore::empty();
    for certificate in certificates {
        roots
            .add(certificate)
            .map_err(|_| invalid_data(format!("{role} contains an invalid X.509 certificate")))?;
    }
    Ok(roots)
}

fn load_certificate_chain(
    path: &Path,
    role: &'static str,
) -> io::Result<Vec<CertificateDer<'static>>> {
    let bytes = read_bounded(path, role)?;
    parse_certificate_chain(&bytes, role)
}

pub(crate) fn parse_certificate_chain(
    bytes: &[u8],
    role: &'static str,
) -> io::Result<Vec<CertificateDer<'static>>> {
    let items = parse_pem_items(bytes, role)?;
    let mut certificates = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Item::X509Certificate(certificate) => certificates.push(certificate),
            _ => {
                return Err(invalid_data(format!(
                    "{role} must contain certificate PEM blocks only"
                )));
            }
        }
    }
    if certificates.is_empty() {
        return Err(invalid_data(format!(
            "{role} must contain at least one certificate"
        )));
    }
    if certificates.len() > MAX_CERTIFICATES_PER_FILE {
        return Err(invalid_data(format!(
            "{role} exceeds the {MAX_CERTIFICATES_PER_FILE}-certificate bound"
        )));
    }
    Ok(certificates)
}

fn load_private_key(path: &Path, role: &'static str) -> io::Result<PrivateKeyDer<'static>> {
    let bytes = read_bounded(path, role)?;
    parse_private_key(&bytes, role)
}

pub(crate) fn parse_private_key(
    bytes: &[u8],
    role: &'static str,
) -> io::Result<PrivateKeyDer<'static>> {
    let items = parse_pem_items(bytes, role)?;
    if items.len() != 1 {
        return Err(invalid_data(format!(
            "{role} must contain exactly one private key PEM block"
        )));
    }
    match items.into_iter().next() {
        Some(Item::Pkcs1Key(key)) => Ok(PrivateKeyDer::Pkcs1(key)),
        Some(Item::Pkcs8Key(key)) => Ok(PrivateKeyDer::Pkcs8(key)),
        Some(Item::Sec1Key(key)) => Ok(PrivateKeyDer::Sec1(key)),
        _ => Err(invalid_data(format!(
            "{role} must contain exactly one supported private key PEM block"
        ))),
    }
}

fn read_bounded(path: &Path, role: &'static str) -> io::Result<Vec<u8>> {
    reject_empty_path(path, role)?;
    let file = File::open(path).map_err(|_| invalid_input(format!("{role} cannot be read")))?;
    let metadata = file
        .metadata()
        .map_err(|_| invalid_input(format!("{role} cannot be inspected")))?;
    if !metadata.is_file() {
        return Err(invalid_input(format!("{role} must be a regular file")));
    }
    if metadata.len() > MAX_PEM_FILE_BYTES as u64 {
        return Err(invalid_data(format!(
            "{role} exceeds the {MAX_PEM_FILE_BYTES}-byte bound"
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_PEM_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid_input(format!("{role} cannot be read")))?;
    if bytes.is_empty() {
        return Err(invalid_data(format!("{role} cannot be empty")));
    }
    if bytes.len() > MAX_PEM_FILE_BYTES {
        return Err(invalid_data(format!(
            "{role} exceeds the {MAX_PEM_FILE_BYTES}-byte bound"
        )));
    }
    if bytes.contains(&0) {
        return Err(invalid_data(format!("{role} contains invalid PEM data")));
    }
    Ok(bytes)
}

fn parse_pem_items(bytes: &[u8], role: &'static str) -> io::Result<Vec<Item>> {
    let expected_blocks = validate_strict_pem_framing(bytes, role)?;
    let mut reader = BufReader::new(bytes);
    let items = rustls_pemfile::read_all(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid_data(format!("{role} contains invalid PEM data")))?;
    if items.len() != expected_blocks {
        return Err(invalid_data(format!(
            "{role} contains an unsupported PEM block"
        )));
    }
    Ok(items)
}

fn validate_strict_pem_framing(bytes: &[u8], role: &'static str) -> io::Result<usize> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid_data(format!("{role} contains invalid PEM data")))?;
    let mut open_label: Option<&str> = None;
    let mut blocks = 0_usize;
    for raw_line in text.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        match open_label {
            None if line.is_empty() => {}
            None => {
                let label = line
                    .strip_prefix("-----BEGIN ")
                    .and_then(|line| line.strip_suffix("-----"))
                    .filter(|label| !label.is_empty())
                    .ok_or_else(|| invalid_data(format!("{role} contains non-PEM data")))?;
                open_label = Some(label);
                blocks += 1;
            }
            Some(label) => {
                if line == format!("-----END {label}-----") {
                    open_label = None;
                } else if line.is_empty()
                    || line.starts_with("-----BEGIN ")
                    || line.starts_with("-----END ")
                {
                    return Err(invalid_data(format!("{role} contains invalid PEM framing")));
                }
            }
        }
    }
    if open_label.is_some() || blocks == 0 {
        return Err(invalid_data(format!("{role} contains invalid PEM framing")));
    }
    Ok(blocks)
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

pub(crate) fn tls_crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
        KeyUsagePurpose,
    };

    use super::*;

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    struct TestFiles {
        directory: PathBuf,
    }

    impl TestFiles {
        fn new() -> Self {
            let sequence = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "inferlab-transport-security-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&directory).expect("create test directory");
            Self { directory }
        }

        fn write(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
            let path = self.directory.join(name);
            fs::write(&path, contents).expect("write fixture");
            path
        }
    }

    impl Drop for TestFiles {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn test_material(files: &TestFiles) -> (MtlsServerPaths, MtlsClientPaths) {
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca_key = KeyPair::generate().expect("CA key");
        let ca_cert = ca_params.self_signed(&ca_key).expect("CA cert");
        let issuer = Issuer::new(ca_params, ca_key);

        let mut server_params =
            CertificateParams::new(vec!["localhost".to_owned()]).expect("server params");
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_key = KeyPair::generate().expect("server key");
        let server_cert = server_params
            .signed_by(&server_key, &issuer)
            .expect("server cert");

        let mut client_params =
            CertificateParams::new(Vec::<String>::new()).expect("client params");
        client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let client_key = KeyPair::generate().expect("client key");
        let client_cert = client_params
            .signed_by(&client_key, &issuer)
            .expect("client cert");

        let ca = files.write("ca.pem", ca_cert.pem());
        let server_cert = files.write("server-cert.pem", server_cert.pem());
        let server_key = files.write("server-key.pem", server_key.serialize_pem());
        let client_cert = files.write("client-cert.pem", client_cert.pem());
        let client_key = files.write("client-key.pem", client_key.serialize_pem());
        (
            MtlsServerPaths {
                certificate_chain: server_cert,
                private_key: server_key,
                client_ca: ca.clone(),
            },
            MtlsClientPaths {
                server_ca: ca,
                certificate_chain: client_cert,
                private_key: client_key,
            },
        )
    }

    #[test]
    fn optional_server_paths_are_all_or_nothing() {
        assert_eq!(
            ServerTransportConfig::from_optional_paths(None, None, None).expect("HTTP"),
            ServerTransportConfig::Http
        );
        assert!(
            ServerTransportConfig::from_optional_paths(
                Some(PathBuf::from("cert.pem")),
                None,
                Some(PathBuf::from("ca.pem"))
            )
            .is_err()
        );
    }

    #[test]
    fn valid_server_and_client_material_loads_with_tls13_only() {
        let files = TestFiles::new();
        let (server_paths, client_paths) = test_material(&files);
        let server = load_mtls_server_config(&server_paths).expect("server config");
        let client = configure_mtls_client(reqwest::Client::builder(), &client_paths)
            .expect("client builder")
            .build()
            .expect("client config");
        assert_eq!(
            server.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
        drop(client);
    }

    #[test]
    fn malformed_oversized_mixed_and_mismatched_material_fails_closed() {
        let files = TestFiles::new();
        let (server_paths, client_paths) = test_material(&files);

        let oversized = files.write("oversized.pem", vec![b'x'; MAX_PEM_FILE_BYTES + 1]);
        let mut oversized_paths = server_paths.clone();
        oversized_paths.client_ca = oversized;
        assert!(load_mtls_server_config(&oversized_paths).is_err());

        let mixed = files.write(
            "mixed.pem",
            format!(
                "{}{}",
                fs::read_to_string(&server_paths.certificate_chain).expect("certificate"),
                fs::read_to_string(&server_paths.private_key).expect("key")
            ),
        );
        let mut mixed_paths = server_paths.clone();
        mixed_paths.certificate_chain = mixed;
        assert!(load_mtls_server_config(&mixed_paths).is_err());

        let prefixed = files.write(
            "prefixed.pem",
            format!(
                "secret-prefix\n{}",
                fs::read_to_string(&server_paths.certificate_chain).expect("certificate")
            ),
        );
        let mut prefixed_paths = server_paths.clone();
        prefixed_paths.certificate_chain = prefixed;
        assert!(load_mtls_server_config(&prefixed_paths).is_err());

        let mut mismatched = server_paths;
        mismatched.private_key = client_paths.private_key;
        assert!(load_mtls_server_config(&mismatched).is_err());
    }

    #[test]
    fn errors_never_echo_paths_or_pem_contents() {
        let secret_path = PathBuf::from("/tmp/do-not-expose-secret-cert.pem");
        let error = load_mtls_server_config(&MtlsServerPaths {
            certificate_chain: secret_path.clone(),
            private_key: secret_path.clone(),
            client_ca: secret_path,
        })
        .expect_err("missing certificate must fail")
        .to_string();
        assert!(!error.contains("/tmp/"));
        assert!(!error.contains("do-not-expose"));
    }

    #[test]
    fn path_debug_output_is_redacted() {
        let paths = MtlsClientPaths {
            server_ca: PathBuf::from("/secret/server-ca.pem"),
            certificate_chain: PathBuf::from("/secret/client-cert.pem"),
            private_key: PathBuf::from("/secret/client-key.pem"),
        };
        let debug = format!("{paths:?}");
        assert!(!debug.contains("/secret"));
        assert_eq!(debug.matches("<redacted>").count(), 3);
    }
}
