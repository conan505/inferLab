use std::{
    env, fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use transport_security::MtlsClientPaths;

pub const MIN_POLICY_LIFETIME_MS: u64 = 250;
pub const MAX_POLICY_LIFETIME_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
pub const MIN_INTERVAL_MS: u64 = 10;
pub const MAX_INTERVAL_MS: u64 = 60_000;
const MAX_ENV_BYTES: usize = 4_096;

const STATUS_BIND_ENV: &str = "INFERLAB_TRUST_RENEWER_STATUS_BIND";
const DISTRIBUTOR_URL_ENV: &str = "INFERLAB_TRUST_RENEWER_DISTRIBUTOR_URL";
const CLUSTER_ID_ENV: &str = "INFERLAB_TRUST_RENEWER_CLUSTER_ID";
const TEMPLATE_PATH_ENV: &str = "INFERLAB_TRUST_RENEWER_TEMPLATE_PATH";
const STATE_PATH_ENV: &str = "INFERLAB_TRUST_RENEWER_STATE_PATH";
const ROOT_KEY_ID_ENV: &str = "INFERLAB_TRUST_RENEWER_ROOT_KEY_ID";
const ROOT_PRIVATE_KEY_ENV: &str = "INFERLAB_TRUST_RENEWER_ROOT_PRIVATE_KEY_B64";
const TLS_SERVER_CA_PATH_ENV: &str = "INFERLAB_TRUST_RENEWER_TLS_SERVER_CA_PATH";
const TLS_CLIENT_CERT_PATH_ENV: &str = "INFERLAB_TRUST_RENEWER_TLS_CLIENT_CERT_PATH";
const TLS_CLIENT_KEY_PATH_ENV: &str = "INFERLAB_TRUST_RENEWER_TLS_CLIENT_KEY_PATH";
const POLICY_LIFETIME_MS_ENV: &str = "INFERLAB_TRUST_RENEWER_POLICY_LIFETIME_MS";
const RENEW_BEFORE_MS_ENV: &str = "INFERLAB_TRUST_RENEWER_RENEW_BEFORE_MS";
const POLL_INTERVAL_MS_ENV: &str = "INFERLAB_TRUST_RENEWER_POLL_INTERVAL_MS";
const RETRY_INTERVAL_MS_ENV: &str = "INFERLAB_TRUST_RENEWER_RETRY_INTERVAL_MS";
const REQUEST_TIMEOUT_MS_ENV: &str = "INFERLAB_TRUST_RENEWER_REQUEST_TIMEOUT_MS";

#[derive(Clone, Eq, PartialEq)]
pub struct SecretString(String);

impl SecretString {
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[derive(Clone)]
pub struct RenewerConfig {
    pub status_bind: SocketAddr,
    pub distributor_endpoint: reqwest::Url,
    pub cluster_id: String,
    pub template_path: PathBuf,
    pub state_path: PathBuf,
    pub root_key_id: String,
    pub root_private_key: SecretString,
    pub mtls: MtlsClientPaths,
    pub policy_lifetime: Duration,
    pub renew_before: Duration,
    pub poll_interval: Duration,
    pub retry_interval: Duration,
    pub request_timeout: Duration,
}

impl fmt::Debug for RenewerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RenewerConfig")
            .field("status_bind", &self.status_bind)
            .field("distributor_endpoint", &"<redacted>")
            .field("cluster_id", &self.cluster_id)
            .field("template_path", &"<redacted>")
            .field("state_path", &"<redacted>")
            .field("root_key_id", &self.root_key_id)
            .field("root_private_key", &self.root_private_key)
            .field("mtls", &self.mtls)
            .field("policy_lifetime", &self.policy_lifetime)
            .field("renew_before", &self.renew_before)
            .field("poll_interval", &self.poll_interval)
            .field("retry_interval", &self.retry_interval)
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

impl RenewerConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        RawConfig {
            status_bind: required_env(STATUS_BIND_ENV)?,
            distributor_url: required_env(DISTRIBUTOR_URL_ENV)?,
            cluster_id: required_env(CLUSTER_ID_ENV)?,
            template_path: required_env(TEMPLATE_PATH_ENV)?,
            state_path: required_env(STATE_PATH_ENV)?,
            root_key_id: required_env(ROOT_KEY_ID_ENV)?,
            root_private_key: required_env(ROOT_PRIVATE_KEY_ENV)?,
            tls_server_ca_path: required_env(TLS_SERVER_CA_PATH_ENV)?,
            tls_client_cert_path: required_env(TLS_CLIENT_CERT_PATH_ENV)?,
            tls_client_key_path: required_env(TLS_CLIENT_KEY_PATH_ENV)?,
            policy_lifetime_ms: required_env(POLICY_LIFETIME_MS_ENV)?,
            renew_before_ms: required_env(RENEW_BEFORE_MS_ENV)?,
            poll_interval_ms: required_env(POLL_INTERVAL_MS_ENV)?,
            retry_interval_ms: required_env(RETRY_INTERVAL_MS_ENV)?,
            request_timeout_ms: required_env(REQUEST_TIMEOUT_MS_ENV)?,
        }
        .parse()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RawConfig {
    pub status_bind: String,
    pub distributor_url: String,
    pub cluster_id: String,
    pub template_path: String,
    pub state_path: String,
    pub root_key_id: String,
    pub root_private_key: String,
    pub tls_server_ca_path: String,
    pub tls_client_cert_path: String,
    pub tls_client_key_path: String,
    pub policy_lifetime_ms: String,
    pub renew_before_ms: String,
    pub poll_interval_ms: String,
    pub retry_interval_ms: String,
    pub request_timeout_ms: String,
}

impl fmt::Debug for RawConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawConfig")
            .field("status_bind", &self.status_bind)
            .field("distributor_url", &"<redacted>")
            .field("cluster_id", &self.cluster_id)
            .field("template_path", &"<redacted>")
            .field("state_path", &"<redacted>")
            .field("root_key_id", &self.root_key_id)
            .field("root_private_key", &"<redacted>")
            .field("tls_server_ca_path", &"<redacted>")
            .field("tls_client_cert_path", &"<redacted>")
            .field("tls_client_key_path", &"<redacted>")
            .field("policy_lifetime_ms", &self.policy_lifetime_ms)
            .field("renew_before_ms", &self.renew_before_ms)
            .field("poll_interval_ms", &self.poll_interval_ms)
            .field("retry_interval_ms", &self.retry_interval_ms)
            .field("request_timeout_ms", &self.request_timeout_ms)
            .finish()
    }
}

impl RawConfig {
    pub fn parse(self) -> Result<RenewerConfig, ConfigError> {
        let status_bind = self
            .status_bind
            .parse::<SocketAddr>()
            .map_err(|_| ConfigError::new("status bind must be an explicit socket address"))?;
        if !status_bind.ip().is_loopback() {
            return Err(ConfigError::new("status bind must use a loopback address"));
        }

        let mut distributor_endpoint = reqwest::Url::parse(&self.distributor_url)
            .map_err(|_| ConfigError::new("distributor URL is invalid"))?;
        validate_distributor_origin(&distributor_endpoint)?;
        distributor_endpoint.set_path("/v1/service-trust/snapshot");

        reject_empty(&self.cluster_id, "cluster ID")?;
        reject_empty(&self.root_key_id, "root key ID")?;
        reject_empty(&self.root_private_key, "root private key")?;

        let template_path = parse_path(&self.template_path, "template path")?;
        let state_path = parse_path(&self.state_path, "state path")?;
        if template_path == state_path {
            return Err(ConfigError::new(
                "template and state paths must be distinct",
            ));
        }
        let server_ca = parse_path(&self.tls_server_ca_path, "TLS server CA path")?;
        let certificate_chain =
            parse_path(&self.tls_client_cert_path, "TLS client certificate path")?;
        let private_key = parse_path(&self.tls_client_key_path, "TLS client key path")?;
        if server_ca == certificate_chain
            || server_ca == private_key
            || certificate_chain == private_key
        {
            return Err(ConfigError::new("TLS source paths must be distinct"));
        }

        let policy_lifetime_ms = parse_bounded_ms(
            &self.policy_lifetime_ms,
            "policy lifetime",
            MIN_POLICY_LIFETIME_MS,
            MAX_POLICY_LIFETIME_MS,
        )?;
        let renew_before_ms = parse_u64(&self.renew_before_ms, "renew-before margin")?;
        if renew_before_ms == 0 || renew_before_ms >= policy_lifetime_ms {
            return Err(ConfigError::new(
                "renew-before margin must be positive and below the policy lifetime",
            ));
        }
        let poll_interval_ms = parse_bounded_ms(
            &self.poll_interval_ms,
            "poll interval",
            MIN_INTERVAL_MS,
            MAX_INTERVAL_MS,
        )?;
        let retry_interval_ms = parse_bounded_ms(
            &self.retry_interval_ms,
            "retry interval",
            MIN_INTERVAL_MS,
            MAX_INTERVAL_MS,
        )?;
        let request_timeout_ms = parse_bounded_ms(
            &self.request_timeout_ms,
            "request timeout",
            MIN_INTERVAL_MS,
            MAX_INTERVAL_MS,
        )?;
        let recovery_window = request_timeout_ms
            .checked_add(retry_interval_ms)
            .ok_or_else(|| ConfigError::new("request and retry bounds overflow"))?;
        if renew_before_ms <= recovery_window {
            return Err(ConfigError::new(
                "renew-before margin must cover one request timeout and one retry interval",
            ));
        }

        Ok(RenewerConfig {
            status_bind,
            distributor_endpoint,
            cluster_id: self.cluster_id,
            template_path,
            state_path,
            root_key_id: self.root_key_id,
            root_private_key: SecretString(self.root_private_key),
            mtls: MtlsClientPaths {
                server_ca,
                certificate_chain,
                private_key,
            },
            policy_lifetime: Duration::from_millis(policy_lifetime_ms),
            renew_before: Duration::from_millis(renew_before_ms),
            poll_interval: Duration::from_millis(poll_interval_ms),
            retry_interval: Duration::from_millis(retry_interval_ms),
            request_timeout: Duration::from_millis(request_timeout_ms),
        })
    }
}

fn validate_distributor_origin(url: &reqwest::Url) -> Result<(), ConfigError> {
    if url.scheme() != "https" {
        return Err(ConfigError::new("distributor URL must use HTTPS"));
    }
    if url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err(ConfigError::new(
            "distributor URL must be an HTTPS origin without credentials, path, query, or fragment",
        ));
    }
    Ok(())
}

fn parse_path(value: &str, role: &'static str) -> Result<PathBuf, ConfigError> {
    reject_empty(value, role)?;
    Ok(Path::new(value).to_path_buf())
}

fn reject_empty(value: &str, role: &'static str) -> Result<(), ConfigError> {
    if value.is_empty() {
        return Err(ConfigError::new(format!("{role} cannot be empty")));
    }
    Ok(())
}

fn parse_bounded_ms(
    value: &str,
    role: &'static str,
    minimum: u64,
    maximum: u64,
) -> Result<u64, ConfigError> {
    let parsed = parse_u64(value, role)?;
    if !(minimum..=maximum).contains(&parsed) {
        return Err(ConfigError::new(format!(
            "{role} must be between {minimum} and {maximum} milliseconds"
        )));
    }
    Ok(parsed)
}

fn parse_u64(value: &str, role: &'static str) -> Result<u64, ConfigError> {
    value
        .parse::<u64>()
        .map_err(|_| ConfigError::new(format!("{role} must be an unsigned integer")))
}

fn required_env(name: &'static str) -> Result<String, ConfigError> {
    match env::var(name) {
        Ok(value) if value.len() <= MAX_ENV_BYTES && !value.is_empty() => Ok(value),
        Ok(_) => Err(ConfigError::new(format!(
            "{name} must contain between 1 and {MAX_ENV_BYTES} bytes"
        ))),
        Err(env::VarError::NotPresent) => Err(ConfigError::new(format!("{name} is required"))),
        Err(env::VarError::NotUnicode(_)) => {
            Err(ConfigError::new(format!("{name} must be valid Unicode")))
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_raw() -> RawConfig {
        RawConfig {
            status_bind: "127.0.0.1:8091".to_owned(),
            distributor_url: "https://127.0.0.1:8090".to_owned(),
            cluster_id: "inferlab-primary".to_owned(),
            template_path: "/run/inferlab/renewal-template.json".to_owned(),
            state_path: "/run/inferlab/renewal-state.json".to_owned(),
            root_key_id: "root-a".to_owned(),
            root_private_key: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned(),
            tls_server_ca_path: "/run/inferlab/server-ca.pem".to_owned(),
            tls_client_cert_path: "/run/inferlab/client-cert.pem".to_owned(),
            tls_client_key_path: "/run/inferlab/client-key.pem".to_owned(),
            policy_lifetime_ms: "1000".to_owned(),
            renew_before_ms: "500".to_owned(),
            poll_interval_ms: "25".to_owned(),
            retry_interval_ms: "100".to_owned(),
            request_timeout_ms: "100".to_owned(),
        }
    }

    #[test]
    fn parses_complete_bounded_configuration() {
        let raw = valid_raw();
        let debug = format!("{raw:?}");
        for secret in [
            raw.root_private_key.as_str(),
            raw.distributor_url.as_str(),
            raw.template_path.as_str(),
            raw.state_path.as_str(),
            raw.tls_server_ca_path.as_str(),
            raw.tls_client_cert_path.as_str(),
            raw.tls_client_key_path.as_str(),
        ] {
            assert!(!debug.contains(secret));
        }
        let config = raw.parse().expect("configuration");
        assert_eq!(config.status_bind, "127.0.0.1:8091".parse().unwrap());
        assert_eq!(
            config.distributor_endpoint.as_str(),
            "https://127.0.0.1:8090/v1/service-trust/snapshot"
        );
        assert_eq!(config.policy_lifetime, Duration::from_millis(1_000));
        assert!(!format!("{config:?}").contains(config.root_private_key.expose()));
        assert!(!format!("{config:?}").contains("renewal-template.json"));
    }

    #[test]
    fn rejects_http_and_non_origin_distributor_urls() {
        for url in [
            "http://127.0.0.1:8090",
            "https://user@127.0.0.1:8090",
            "https://127.0.0.1:8090/wrong",
            "https://127.0.0.1:8090?query=1",
        ] {
            let mut raw = valid_raw();
            raw.distributor_url = url.to_owned();
            assert!(raw.parse().is_err(), "accepted {url}");
        }
    }

    #[test]
    fn rejects_non_loopback_status_listener() {
        let mut raw = valid_raw();
        raw.status_bind = "0.0.0.0:8091".to_owned();
        assert_eq!(
            raw.parse().unwrap_err().to_string(),
            "status bind must use a loopback address"
        );
    }

    #[test]
    fn rejects_unrecoverable_renewal_margin() {
        let mut raw = valid_raw();
        raw.renew_before_ms = "200".to_owned();
        assert_eq!(
            raw.parse().unwrap_err().to_string(),
            "renew-before margin must cover one request timeout and one retry interval"
        );

        let mut raw = valid_raw();
        raw.renew_before_ms = "1000".to_owned();
        assert_eq!(
            raw.parse().unwrap_err().to_string(),
            "renew-before margin must be positive and below the policy lifetime"
        );
    }

    #[test]
    fn rejects_aliasing_sensitive_paths() {
        let mut raw = valid_raw();
        raw.state_path = raw.template_path.clone();
        assert_eq!(
            raw.parse().unwrap_err().to_string(),
            "template and state paths must be distinct"
        );

        let mut raw = valid_raw();
        raw.tls_client_key_path = raw.tls_client_cert_path.clone();
        assert_eq!(
            raw.parse().unwrap_err().to_string(),
            "TLS source paths must be distinct"
        );
    }
}
