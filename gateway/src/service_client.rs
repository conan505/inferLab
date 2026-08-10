use std::{collections::BTreeMap, sync::Arc, time::Duration};

use reqwest::{Client, Response};
use service_auth::{
    HEADER_ALGORITHM, HEADER_AUDIENCE_ID, HEADER_ISSUED_AT_MS, HEADER_NONCE, HEADER_SCHEMA,
    HEADER_SERVICE_ID, HEADER_SIGNATURE, ServiceAuthentication, ServiceSigner, validate_service_id,
};

pub const CONTROL_CONFIGURATION_PATH: &str = "/v1/control/config";

#[derive(Clone, Debug)]
pub struct ControlServiceClient {
    http: Client,
    signer: Option<Arc<ServiceSigner>>,
    cluster_id: String,
    targets: Arc<BTreeMap<String, String>>,
}

impl ControlServiceClient {
    pub fn disabled(http: Client) -> Self {
        Self {
            http,
            signer: None,
            cluster_id: String::new(),
            targets: Arc::new(BTreeMap::new()),
        }
    }

    pub fn authenticated(
        http: Client,
        signer: Arc<ServiceSigner>,
        cluster_id: impl Into<String>,
        targets: BTreeMap<String, String>,
        control_urls: &[String],
    ) -> Result<Self, String> {
        let cluster_id = cluster_id.into();
        if cluster_id.trim().is_empty() {
            return Err("service-authenticated control requests require a cluster ID".to_owned());
        }
        let configured_urls = control_urls
            .iter()
            .map(|url| normalize_url(url))
            .collect::<Vec<_>>();
        let missing = configured_urls
            .iter()
            .filter(|url| !targets.contains_key(*url))
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(format!(
                "INFERLAB_CONTROL_SERVICE_TARGETS is missing control URL(s): {}",
                missing.join(", ")
            ));
        }
        let extra = targets
            .keys()
            .filter(|url| !configured_urls.contains(url))
            .cloned()
            .collect::<Vec<_>>();
        if !extra.is_empty() {
            return Err(format!(
                "INFERLAB_CONTROL_SERVICE_TARGETS contains URL(s) not present in INFERLAB_CONTROL_PLANE_URLS: {}",
                extra.join(", ")
            ));
        }
        Ok(Self {
            http,
            signer: Some(signer),
            cluster_id,
            targets: Arc::new(targets),
        })
    }

    pub fn authentication_enabled(&self) -> bool {
        self.signer.is_some()
    }

    pub fn service_id(&self) -> Option<String> {
        self.signer
            .as_ref()
            .map(|signer| signer.snapshot().service_id().to_owned())
    }

    pub fn credential_id(&self) -> Option<String> {
        self.signer
            .as_ref()
            .map(|signer| signer.snapshot().credential_id().to_owned())
    }

    pub fn configured_targets(&self) -> Vec<String> {
        self.targets
            .iter()
            .map(|(url, service_id)| format!("{service_id}={url}"))
            .collect()
    }

    pub async fn get_configuration(&self, base_url: &str) -> Result<Response, String> {
        let normalized_url = normalize_url(base_url);
        let mut request = self
            .http
            .get(format!("{normalized_url}{CONTROL_CONFIGURATION_PATH}"))
            .timeout(Duration::from_millis(250));
        if let Some(signer) = self.signer.as_ref() {
            let audience_id = self.targets.get(&normalized_url).ok_or_else(|| {
                format!("no authenticated service target is configured for {normalized_url}")
            })?;
            // One immutable snapshot owns the credential for the complete request-signing
            // operation. A concurrent bundle activation can affect the next request, never
            // splice two credentials into this one.
            let signer = signer.snapshot();
            let authentication = signer
                .authenticate_now(
                    "GET",
                    CONTROL_CONFIGURATION_PATH,
                    &self.cluster_id,
                    audience_id,
                    b"",
                )
                .map_err(|error| format!("sign control service request: {error}"))?;
            request = add_authentication_headers(request, &authentication);
        }
        request
            .send()
            .await
            .map_err(|error| format!("send control service request: {error}"))
    }
}

pub fn parse_control_service_targets(raw: &str) -> Result<BTreeMap<String, String>, String> {
    let mut targets = BTreeMap::new();
    for raw_entry in raw.split(',') {
        let entry = raw_entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (service_id, url) = entry.split_once('=').ok_or_else(|| {
            format!(
                "invalid control service target '{entry}'; expected service-id=http://host:port"
            )
        })?;
        let service_id = service_id.trim();
        let url = normalize_url(url);
        if service_id.is_empty() || url.is_empty() {
            return Err(format!("invalid control service target '{entry}'"));
        }
        validate_service_id(service_id)
            .map_err(|error| format!("invalid control service target '{entry}': {error}"))?;
        if targets.insert(url.clone(), service_id.to_owned()).is_some() {
            return Err(format!("control service URL '{url}' is duplicated"));
        }
    }
    if targets.is_empty() {
        return Err("at least one control service target is required".to_owned());
    }
    Ok(targets)
}

fn normalize_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_owned()
}

fn add_authentication_headers(
    request: reqwest::RequestBuilder,
    authentication: &ServiceAuthentication,
) -> reqwest::RequestBuilder {
    request
        .header(HEADER_SCHEMA, &authentication.schema)
        .header(HEADER_ALGORITHM, &authentication.algorithm)
        .header(HEADER_SERVICE_ID, &authentication.service_id)
        .header(HEADER_AUDIENCE_ID, &authentication.audience_id)
        .header(HEADER_ISSUED_AT_MS, authentication.issued_at_ms.to_string())
        .header(HEADER_NONCE, &authentication.nonce)
        .header(HEADER_SIGNATURE, &authentication.signature)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use axum::{Json, Router, extract::State, http::HeaderMap, routing::get};
    use serde_json::json;
    use service_auth::{
        SERVICE_SIGNING_BUNDLE_SCHEMA, ServiceRequestPayload, ServiceSigner,
        ServiceSigningIdentity, TrustedServiceKeyRing, VerifiedServiceSigningBundle,
    };
    use tokio::{
        net::TcpListener,
        sync::{Notify, mpsc},
    };

    const SEED_A: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";
    const SEED_B: &str = "oRHYnSe9L9fS2eMjpnvZPZ7tg09poPfRXpAMlzsqHkg=";

    fn bundle(generation: u64, active: &str) -> VerifiedServiceSigningBundle {
        let encoded = format!(
            r#"{{"schema":"{SERVICE_SIGNING_BUNDLE_SCHEMA}","cluster_id":"inferlab-primary","generation":{generation},"service_id":"gateway-primary","active_credential_id":"{active}","credentials":[{{"credential_id":"key-a","private_key_base64":"{SEED_A}"}},{{"credential_id":"key-b","private_key_base64":"{SEED_B}"}}]}}"#
        );
        VerifiedServiceSigningBundle::decode(
            encoded.as_bytes(),
            "inferlab-primary",
            "gateway-primary",
        )
        .expect("bundle")
    }

    #[test]
    fn authenticated_client_requires_an_exact_url_to_service_mapping() {
        let identity = Arc::new(
            ServiceSigningIdentity::from_base64_seed_with_credential(
                "gateway-primary",
                "key-b",
                SEED_A,
            )
            .expect("identity"),
        );
        let signer = Arc::new(ServiceSigner::from_static(Arc::clone(&identity)));
        let urls = vec!["http://127.0.0.1:9910".to_owned()];
        let missing = ControlServiceClient::authenticated(
            Client::new(),
            Arc::clone(&signer),
            "inferlab-primary",
            BTreeMap::new(),
            &urls,
        )
        .expect_err("missing mapping");
        assert!(missing.contains("missing control URL"));

        let extra = parse_control_service_targets(
            "node-a=http://127.0.0.1:9910,node-b=http://127.0.0.1:9911",
        )
        .expect("targets");
        let error = ControlServiceClient::authenticated(
            Client::new(),
            signer,
            "inferlab-primary",
            extra,
            &urls,
        )
        .expect_err("extra mapping");
        assert!(error.contains("not present"));

        let targets =
            parse_control_service_targets("node-a=http://127.0.0.1:9910").expect("one target");
        let client = ControlServiceClient::authenticated(
            Client::new(),
            Arc::new(ServiceSigner::from_static(Arc::new(
                ServiceSigningIdentity::from_base64_seed_with_credential(
                    "gateway-primary",
                    "key-b",
                    SEED_A,
                )
                .expect("identity"),
            ))),
            "inferlab-primary",
            targets,
            &urls,
        )
        .expect("client");
        assert_eq!(client.service_id().as_deref(), Some("gateway-primary"));
        assert_eq!(client.credential_id().as_deref(), Some("key-b"));
    }

    #[derive(Clone)]
    struct SigningFixture {
        ring: Arc<TrustedServiceKeyRing>,
        observed: mpsc::UnboundedSender<String>,
        release_first: Arc<Notify>,
        calls: Arc<AtomicUsize>,
    }

    async fn observe_signed_request(
        State(fixture): State<SigningFixture>,
        headers: HeaderMap,
    ) -> Json<serde_json::Value> {
        let header = |name| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .expect("signed header")
                .to_owned()
        };
        let authentication = ServiceAuthentication {
            schema: header(HEADER_SCHEMA),
            algorithm: header(HEADER_ALGORITHM),
            service_id: header(HEADER_SERVICE_ID),
            audience_id: header(HEADER_AUDIENCE_ID),
            issued_at_ms: header(HEADER_ISSUED_AT_MS).parse().expect("issued at"),
            nonce: header(HEADER_NONCE),
            signature: header(HEADER_SIGNATURE),
        };
        let verified = fixture
            .ring
            .verify(
                &ServiceRequestPayload {
                    method: "GET",
                    path: CONTROL_CONFIGURATION_PATH,
                    cluster_id: "inferlab-primary",
                    audience_id: "control-a",
                    issued_at_ms: authentication.issued_at_ms,
                    nonce: &authentication.nonce,
                    body: b"",
                },
                &authentication,
            )
            .expect("valid signed request");
        fixture
            .observed
            .send(verified.credential_id)
            .expect("observer");
        if fixture.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            fixture.release_first.notified().await;
        }
        Json(json!({"status": "ok"}))
    }

    #[tokio::test]
    async fn in_flight_request_keeps_its_snapshot_and_the_next_request_uses_the_handoff() {
        let key_a = ServiceSigningIdentity::from_base64_seed_with_credential(
            "gateway-primary",
            "key-a",
            SEED_A,
        )
        .expect("key a");
        let key_b = ServiceSigningIdentity::from_base64_seed_with_credential(
            "gateway-primary",
            "key-b",
            SEED_B,
        )
        .expect("key b");
        let ring = Arc::new(
            TrustedServiceKeyRing::parse(
                &format!(
                    "gateway-primary/key-a={},gateway-primary/key-b={}",
                    key_a.public_key_base64(),
                    key_b.public_key_base64()
                ),
                "",
            )
            .expect("trusted keys"),
        );
        let signer = Arc::new(ServiceSigner::from_bundle(bundle(1, "key-a")));
        let (observed, mut observations) = mpsc::unbounded_channel();
        let release_first = Arc::new(Notify::new());
        let fixture = SigningFixture {
            ring,
            observed,
            release_first: Arc::clone(&release_first),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route(CONTROL_CONFIGURATION_PATH, get(observe_signed_request))
            .with_state(fixture);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server");
        });
        let base_url = format!("http://{address}");
        let urls = vec![base_url.clone()];
        let client = ControlServiceClient::authenticated(
            Client::new(),
            Arc::clone(&signer),
            "inferlab-primary",
            parse_control_service_targets(&format!("control-a={base_url}")).expect("targets"),
            &urls,
        )
        .expect("client");

        let in_flight_client = client.clone();
        let in_flight_url = base_url.clone();
        let first = tokio::spawn(async move {
            in_flight_client
                .get_configuration(&in_flight_url)
                .await
                .expect("first response")
        });
        assert_eq!(observations.recv().await.as_deref(), Some("key-a"));
        signer
            .activate_bundle(bundle(2, "key-b"), |_| true)
            .expect("activate key b");
        release_first.notify_one();
        first.await.expect("first task");

        client
            .get_configuration(&base_url)
            .await
            .expect("second response");
        assert_eq!(observations.recv().await.as_deref(), Some("key-b"));
    }
}
