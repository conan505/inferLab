use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use rcgen::{
    BasicConstraints, Certificate, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer,
    KeyPair, KeyUsagePurpose,
};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use service_auth::{
    SERVICE_TRUST_POLICY_SCHEMA, ServiceSigningIdentity, ServiceTrustCredential,
    ServiceTrustPolicyPayload, ServiceTrustRootSigningIdentity, TrustedServiceTrustRootKeyRing,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    task::JoinHandle,
    time::timeout,
};
use transport_security::{
    MtlsClientPaths, MtlsServerPaths, ServerTransportStatus, configure_mtls_client,
    load_mtls_server_config,
};
use trust_distributor::{DEFAULT_MAX_BODY_BYTES, DistributorConfig, TrustDistributor, app};

const ROOT_SEED: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";
const RECEIVER_SEED: &str = "TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs=";
static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    directory: PathBuf,
    server_paths: MtlsServerPaths,
    valid_client_paths: MtlsClientPaths,
    rogue_client_paths: MtlsClientPaths,
    distributor_config: DistributorConfig,
    trust_root: ServiceTrustRootSigningIdentity,
    receiver: ServiceSigningIdentity,
}

impl Fixture {
    fn new() -> Self {
        let sequence = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "inferlab-trust-distributor-mtls-test-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create test directory");

        let (ca_cert, issuer) = certificate_authority();
        let (server_cert, server_key) = leaf_certificate(
            &issuer,
            vec!["localhost".to_owned(), "127.0.0.1".to_owned()],
            ExtendedKeyUsagePurpose::ServerAuth,
        );
        let (client_cert, client_key) =
            leaf_certificate(&issuer, Vec::new(), ExtendedKeyUsagePurpose::ClientAuth);
        let (_rogue_ca_cert, rogue_issuer) = certificate_authority();
        let (rogue_client_cert, rogue_client_key) = leaf_certificate(
            &rogue_issuer,
            Vec::new(),
            ExtendedKeyUsagePurpose::ClientAuth,
        );

        let ca_path = write(&directory, "ca.pem", ca_cert.pem());
        let server_paths = MtlsServerPaths {
            certificate_chain: write(&directory, "server-cert.pem", server_cert.pem()),
            private_key: write(&directory, "server-key.pem", server_key.serialize_pem()),
            client_ca: ca_path.clone(),
        };
        let valid_client_paths = MtlsClientPaths {
            server_ca: ca_path.clone(),
            certificate_chain: write(&directory, "client-cert.pem", client_cert.pem()),
            private_key: write(&directory, "client-key.pem", client_key.serialize_pem()),
        };
        let rogue_client_paths = MtlsClientPaths {
            server_ca: ca_path,
            certificate_chain: write(&directory, "rogue-client-cert.pem", rogue_client_cert.pem()),
            private_key: write(
                &directory,
                "rogue-client-key.pem",
                rogue_client_key.serialize_pem(),
            ),
        };

        Self {
            server_paths,
            valid_client_paths,
            rogue_client_paths,
            distributor_config: DistributorConfig {
                cluster_id: "inferlab-primary".to_owned(),
                state_path: directory.join("state.json"),
                expected_receivers: BTreeSet::from(["control-a/key-a".to_owned()]),
                max_body_bytes: DEFAULT_MAX_BODY_BYTES,
                transport_security: ServerTransportStatus::MutualTls,
            },
            trust_root: ServiceTrustRootSigningIdentity::from_base64_seed("root-a", ROOT_SEED)
                .expect("trust root"),
            receiver: ServiceSigningIdentity::from_base64_seed_with_credential(
                "control-a",
                "key-a",
                RECEIVER_SEED,
            )
            .expect("receiver"),
            directory,
        }
    }

    fn roots(&self) -> TrustedServiceTrustRootKeyRing {
        TrustedServiceTrustRootKeyRing::parse(
            &format!("root-a={}", self.trust_root.public_key_base64()),
            "",
        )
        .expect("roots")
    }

    fn distributor(&self) -> TrustDistributor {
        TrustDistributor::open(self.distributor_config.clone(), self.roots())
            .expect("open distributor")
    }

    fn snapshot(&self) -> service_auth::ServiceTrustSnapshot {
        self.trust_root
            .sign(&ServiceTrustPolicyPayload {
                schema: SERVICE_TRUST_POLICY_SCHEMA.to_owned(),
                cluster_id: "inferlab-primary".to_owned(),
                generation: 1,
                issued_at_ms: 1_700_000_000_001,
                expires_at_ms: None,
                trusted_credentials: vec![ServiceTrustCredential {
                    service_id: "control-a".to_owned(),
                    credential_id: "key-a".to_owned(),
                    public_key_base64: self.receiver.public_key_base64(),
                }],
                revoked_service_ids: Vec::new(),
                revoked_credentials: Vec::new(),
                gateway_service_ids: vec!["control-a".to_owned()],
            })
            .expect("snapshot")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn certificate_authority() -> (Certificate, Issuer<'static, KeyPair>) {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let key = KeyPair::generate().expect("CA key");
    let certificate = params.self_signed(&key).expect("CA certificate");
    (certificate, Issuer::new(params, key))
}

fn leaf_certificate(
    issuer: &Issuer<'_, KeyPair>,
    names: Vec<String>,
    usage: ExtendedKeyUsagePurpose,
) -> (Certificate, KeyPair) {
    let mut params = CertificateParams::new(names).expect("leaf params");
    params.extended_key_usages = vec![usage];
    let key = KeyPair::generate().expect("leaf key");
    let certificate = params.signed_by(&key, issuer).expect("leaf certificate");
    (certificate, key)
}

fn write(directory: &Path, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, contents).expect("write test material");
    path
}

async fn serve(
    distributor: TrustDistributor,
    server_paths: &MtlsServerPaths,
) -> (String, std::net::SocketAddr, JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind TLS listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("listener address");
    let config = load_mtls_server_config(server_paths).expect("TLS server config");
    let config = axum_server::tls_rustls::RustlsConfig::from_config(config.into());
    let task = tokio::spawn(async move {
        axum_server::from_tcp_rustls(listener, config)
            .expect("TLS server")
            .serve(app(distributor).into_make_service())
            .await
            .expect("serve distributor");
    });
    (
        format!("https://localhost:{}", address.port()),
        address,
        task,
    )
}

fn mtls_client(paths: &MtlsClientPaths) -> Client {
    configure_mtls_client(Client::builder(), paths)
        .expect("configure mTLS client")
        .no_proxy()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(2))
        .build()
        .expect("build mTLS client")
}

fn client_without_identity(ca_path: &Path) -> Client {
    let roots = reqwest::Certificate::from_pem_bundle(&fs::read(ca_path).expect("read CA"))
        .expect("CA bundle");
    Client::builder()
        .tls_certs_only(roots)
        .tls_version_min(reqwest::tls::Version::TLS_1_3)
        .tls_version_max(reqwest::tls::Version::TLS_1_3)
        .no_proxy()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(2))
        .build()
        .expect("build no-identity client")
}

#[tokio::test]
async fn mtls_requires_ca_approved_clients_rejects_plaintext_and_preserves_storage() {
    let fixture = Fixture::new();
    let (base, address, task) = serve(fixture.distributor(), &fixture.server_paths).await;
    let client = mtls_client(&fixture.valid_client_paths);

    let health = client
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("approved client health");
    assert_eq!(health.status(), StatusCode::OK);

    let status = client
        .get(format!("{base}/v1/service-trust/status"))
        .send()
        .await
        .expect("approved client status")
        .json::<Value>()
        .await
        .expect("status JSON");
    assert_eq!(status["transport_security"]["mode"], "mutual-tls");
    assert_eq!(
        status["transport_security"]["client_certificate_required"],
        true
    );
    assert_eq!(status["transport_security"]["minimum_protocol"], "TLSv1.3");

    assert_eq!(
        client
            .post(format!("{base}/v1/service-trust/snapshot"))
            .json(&fixture.snapshot())
            .send()
            .await
            .expect("publish over mTLS")
            .status(),
        StatusCode::CREATED
    );

    let no_identity = client_without_identity(&fixture.valid_client_paths.server_ca);
    assert!(
        no_identity
            .get(format!("{base}/health"))
            .send()
            .await
            .is_err(),
        "server must reject a client with no certificate"
    );

    let rogue_client = mtls_client(&fixture.rogue_client_paths);
    assert!(
        rogue_client
            .get(format!("{base}/health"))
            .send()
            .await
            .is_err(),
        "server must reject a certificate signed by a rogue CA"
    );

    let mut plaintext = TcpStream::connect(address)
        .await
        .expect("plaintext connection");
    plaintext
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("write plaintext request");
    let mut response = [0_u8; 64];
    let read = timeout(Duration::from_secs(2), plaintext.read(&mut response)).await;
    assert!(
        !matches!(read, Ok(Ok(length)) if length > 0 && response[..length].starts_with(b"HTTP/")),
        "TLS listener must never answer a plaintext HTTP request"
    );

    task.abort();
    task.await.expect_err("aborted first server");

    let (restarted_base, _, restarted_task) =
        serve(fixture.distributor(), &fixture.server_paths).await;
    let recovered = client
        .get(format!("{restarted_base}/v1/service-trust/status"))
        .send()
        .await
        .expect("restarted status")
        .json::<Value>()
        .await
        .expect("restarted JSON");
    assert_eq!(recovered["snapshot"]["generation"], 1);
    assert_eq!(recovered["storage"]["mutation_poisoned"], false);
    restarted_task.abort();
}

#[test]
fn io_error_type_remains_usable_by_binary_startup() {
    let error: io::Error = transport_security::ServerTransportConfig::from_optional_paths(
        Some(PathBuf::from("cert.pem")),
        None,
        None,
    )
    .expect_err("partial TLS config");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}
