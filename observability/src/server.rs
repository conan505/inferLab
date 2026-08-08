use std::{env, error::Error, fmt, io, net::SocketAddr, sync::Arc};

use axum::{
    Router,
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use tokio::net::TcpListener;

use crate::MetricsRegistry;

const METRICS_BIND_ENV: &str = "INFERLAB_METRICS_BIND";
const NON_LOOPBACK_OPT_IN_ENV: &str = "INFERLAB_METRICS_ALLOW_NON_LOOPBACK";
const OPENMETRICS_CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// Validated configuration for the isolated metrics listener.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MetricsServerConfig {
    bind_addr: SocketAddr,
}

impl MetricsServerConfig {
    /// Read optional listener configuration from the process environment.
    pub fn from_env() -> Result<Option<Self>, MetricsServerConfigError> {
        let bind = match env::var(METRICS_BIND_ENV) {
            Ok(bind) => bind,
            Err(env::VarError::NotPresent) => return Ok(None),
            Err(env::VarError::NotUnicode(_)) => {
                return Err(MetricsServerConfigError::NonUnicodeBind);
            }
        };
        let allow_non_loopback = match env::var(NON_LOOPBACK_OPT_IN_ENV) {
            Ok(value) => Some(value),
            Err(env::VarError::NotPresent) => None,
            Err(env::VarError::NotUnicode(_)) => {
                return Err(MetricsServerConfigError::NonUnicodeOptIn);
            }
        };
        Self::from_values(Some(&bind), allow_non_loopback.as_deref())
    }

    /// Parse explicit values without mutating the process environment.
    pub fn from_values(
        bind: Option<&str>,
        allow_non_loopback: Option<&str>,
    ) -> Result<Option<Self>, MetricsServerConfigError> {
        let Some(bind) = bind else {
            return Ok(None);
        };
        let bind_addr = bind
            .parse::<SocketAddr>()
            .map_err(|_| MetricsServerConfigError::InvalidBind(bind.to_owned()))?;
        if !bind_addr.ip().is_loopback() && allow_non_loopback != Some("1") {
            return Err(MetricsServerConfigError::NonLoopbackRequiresOptIn(
                bind_addr,
            ));
        }
        Ok(Some(Self { bind_addr }))
    }

    #[must_use]
    pub const fn bind_addr(self) -> SocketAddr {
        self.bind_addr
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetricsServerConfigError {
    NonUnicodeBind,
    NonUnicodeOptIn,
    InvalidBind(String),
    NonLoopbackRequiresOptIn(SocketAddr),
}

impl fmt::Display for MetricsServerConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonUnicodeBind => write!(formatter, "{METRICS_BIND_ENV} is not valid Unicode"),
            Self::NonUnicodeOptIn => {
                write!(formatter, "{NON_LOOPBACK_OPT_IN_ENV} is not valid Unicode")
            }
            Self::InvalidBind(value) => write!(
                formatter,
                "invalid {METRICS_BIND_ENV} value `{value}`; expected an explicit socket address"
            ),
            Self::NonLoopbackRequiresOptIn(address) => write!(
                formatter,
                "metrics bind {address} is not loopback; set {NON_LOOPBACK_OPT_IN_ENV}=1 explicitly"
            ),
        }
    }
}

impl Error for MetricsServerConfigError {}

/// Build the intentionally tiny, uninstrumented metrics application.
pub fn metrics_router(registry: Arc<MetricsRegistry>) -> Router {
    Router::new()
        .route("/healthz", get(metrics_health))
        .route("/metrics", get(render_metrics))
        .with_state(registry)
}

/// Bind and serve until the listener fails or the caller cancels this future.
pub async fn serve_metrics(
    config: MetricsServerConfig,
    registry: Arc<MetricsRegistry>,
) -> io::Result<()> {
    let listener = TcpListener::bind(config.bind_addr()).await?;
    axum::serve(listener, metrics_router(registry)).await
}

async fn metrics_health() -> StatusCode {
    StatusCode::OK
}

async fn render_metrics(State(registry): State<Arc<MetricsRegistry>>) -> Response {
    match registry.render() {
        Ok(body) => (
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static(OPENMETRICS_CONTENT_TYPE),
                ),
                (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
            ],
            body,
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request},
    };
    use prometheus_client::metrics::counter::Counter;
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn listener_is_disabled_when_unset() {
        assert_eq!(MetricsServerConfig::from_values(None, None), Ok(None));
    }

    #[test]
    fn loopback_does_not_need_public_bind_opt_in() {
        let config = MetricsServerConfig::from_values(Some("127.0.0.1:9091"), None)
            .unwrap()
            .unwrap();
        assert_eq!(config.bind_addr(), "127.0.0.1:9091".parse().unwrap());

        let config = MetricsServerConfig::from_values(Some("[::1]:9091"), Some("garbage"))
            .unwrap()
            .unwrap();
        assert!(config.bind_addr().ip().is_loopback());
    }

    #[test]
    fn non_loopback_requires_exact_opt_in() {
        let address: SocketAddr = "0.0.0.0:9091".parse().unwrap();
        for opt_in in [None, Some("0"), Some("true"), Some(" 1")] {
            assert_eq!(
                MetricsServerConfig::from_values(Some("0.0.0.0:9091"), opt_in),
                Err(MetricsServerConfigError::NonLoopbackRequiresOptIn(address))
            );
        }
        assert_eq!(
            MetricsServerConfig::from_values(Some("0.0.0.0:9091"), Some("1"))
                .unwrap()
                .unwrap()
                .bind_addr(),
            address
        );
    }

    #[test]
    fn bind_must_be_an_explicit_socket_address() {
        for invalid in ["", "localhost:9091", "127.0.0.1", " 127.0.0.1:9091"] {
            assert_eq!(
                MetricsServerConfig::from_values(Some(invalid), None),
                Err(MetricsServerConfigError::InvalidBind(invalid.to_owned()))
            );
        }
    }

    #[tokio::test]
    async fn endpoint_is_openmetrics_and_not_self_instrumented() {
        let mut registry = MetricsRegistry::new();
        let counter: Counter = Counter::default();
        registry
            .register("inferlab_test_events", "Test events", counter.clone())
            .unwrap();
        counter.inc();
        let app = metrics_router(Arc::new(registry));

        let response = app
            .clone()
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            OPENMETRICS_CONTENT_TYPE
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("# TYPE inferlab_test_events counter"));
        assert!(body.contains("inferlab_test_events_total 1"));
        assert!(body.ends_with("# EOF\n"));
        assert!(!body.contains("inferlab_http_requests"));

        let missing = app
            .clone()
            .oneshot(Request::get("/private").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let wrong_method = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
