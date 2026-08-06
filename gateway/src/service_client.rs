use std::{collections::BTreeMap, sync::Arc, time::Duration};

use reqwest::{Client, Response};
use service_auth::{
    HEADER_ALGORITHM, HEADER_AUDIENCE_ID, HEADER_ISSUED_AT_MS, HEADER_NONCE, HEADER_SCHEMA,
    HEADER_SERVICE_ID, HEADER_SIGNATURE, ServiceAuthentication, ServiceSigningIdentity,
    validate_service_id,
};

pub const CONTROL_CONFIGURATION_PATH: &str = "/v1/control/config";

#[derive(Clone, Debug)]
pub struct ControlServiceClient {
    http: Client,
    identity: Option<Arc<ServiceSigningIdentity>>,
    cluster_id: String,
    targets: Arc<BTreeMap<String, String>>,
}

impl ControlServiceClient {
    pub fn disabled(http: Client) -> Self {
        Self {
            http,
            identity: None,
            cluster_id: String::new(),
            targets: Arc::new(BTreeMap::new()),
        }
    }

    pub fn authenticated(
        http: Client,
        identity: Arc<ServiceSigningIdentity>,
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
            identity: Some(identity),
            cluster_id,
            targets: Arc::new(targets),
        })
    }

    pub fn authentication_enabled(&self) -> bool {
        self.identity.is_some()
    }

    pub fn service_id(&self) -> Option<&str> {
        self.identity.as_ref().map(|identity| identity.service_id())
    }

    pub fn credential_id(&self) -> Option<&str> {
        self.identity
            .as_ref()
            .map(|identity| identity.credential_id())
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
        if let Some(identity) = self.identity.as_ref() {
            let audience_id = self.targets.get(&normalized_url).ok_or_else(|| {
                format!("no authenticated service target is configured for {normalized_url}")
            })?;
            let authentication = identity
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
    use super::*;

    const SEED: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";

    #[test]
    fn authenticated_client_requires_an_exact_url_to_service_mapping() {
        let identity = Arc::new(
            ServiceSigningIdentity::from_base64_seed_with_credential(
                "gateway-primary",
                "key-b",
                SEED,
            )
            .expect("identity"),
        );
        let urls = vec!["http://127.0.0.1:9910".to_owned()];
        let missing = ControlServiceClient::authenticated(
            Client::new(),
            Arc::clone(&identity),
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
            identity,
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
            Arc::new(
                ServiceSigningIdentity::from_base64_seed_with_credential(
                    "gateway-primary",
                    "key-b",
                    SEED,
                )
                .expect("identity"),
            ),
            "inferlab-primary",
            targets,
            &urls,
        )
        .expect("client");
        assert_eq!(client.service_id(), Some("gateway-primary"));
        assert_eq!(client.credential_id(), Some("key-b"));
    }
}
