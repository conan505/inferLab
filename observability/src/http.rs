use std::{fmt, time::Instant};

use axum::{
    Router,
    extract::{MatchedPath, Request, State},
    http::{Method, StatusCode},
    middleware::{self, Next},
    response::Response,
};
use prometheus_client::{
    encoding::{EncodeLabelSet, EncodeLabelValue, LabelValueEncoder},
    metrics::{counter::Counter, family::Family, gauge::Gauge, histogram::Histogram},
    registry::Unit,
};

use crate::{
    FixedHistogramConstructor, MetricsRegistry, RegistryError, RequestId, request_id_middleware,
};

type RequestFamily = Family<HttpRequestLabels, Counter>;
type DurationFamily = Family<HttpDurationLabels, Histogram, FixedHistogramConstructor>;
type InFlightFamily = Family<ServiceLabels, Gauge>;

/// Hard theoretical maximum for the shared HTTP series on any one target.
pub const MAX_HTTP_SERIES_PER_TARGET: usize = 190;

/// Per-target series budget left for service-specific metrics.
pub const DOMAIN_SERIES_BUDGET_PER_TARGET: usize = 66;

/// Locked total series budget for any individual scrape target.
pub const TOTAL_SERIES_BUDGET_PER_TARGET: usize = 256;

/// Stable service identities allowed in shared HTTP metrics.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Service {
    Gateway,
    CpuWorker,
    BatchQueue,
    ControlPlane,
    TrustDistributor,
    TrustRenewer,
    RaftLinkProxy,
}

impl Service {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::CpuWorker => "cpu-worker",
            Self::BatchQueue => "batch-queue",
            Self::ControlPlane => "control-plane",
            Self::TrustDistributor => "trust-distributor",
            Self::TrustRenewer => "trust-renewer",
            Self::RaftLinkProxy => "raft-link-proxy",
        }
    }

    /// Maximum route/method combinations after allowlist collapsing.
    #[must_use]
    pub const fn max_http_route_method_pairs(self) -> usize {
        match self {
            Self::Gateway => 8,
            Self::CpuWorker => 5,
            Self::BatchQueue => 9,
            Self::ControlPlane | Self::TrustDistributor => 7,
            Self::TrustRenewer => 4,
            Self::RaftLinkProxy => 6,
        }
    }

    /// Maximum sample series emitted by the three shared HTTP families.
    #[must_use]
    pub const fn max_http_series(self) -> usize {
        // Four status-class counters plus 14 finite histogram buckets, +Inf,
        // sum, and count for each allowed route/method pair; one service gauge.
        self.max_http_route_method_pairs() * (4 + 17) + 1
    }
}

impl fmt::Display for Service {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl EncodeLabelValue for Service {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        EncodeLabelValue::encode(&self.as_str(), encoder)
    }
}

/// Finite method labels. No extension method can create another series.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Other,
}

impl HttpMethod {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Other => "other",
        }
    }
}

impl From<&Method> for HttpMethod {
    fn from(method: &Method) -> Self {
        match *method {
            Method::GET => Self::Get,
            Method::POST => Self::Post,
            Method::PUT => Self::Put,
            _ => Self::Other,
        }
    }
}

impl EncodeLabelValue for HttpMethod {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        EncodeLabelValue::encode(&self.as_str(), encoder)
    }
}

/// Finite final-response status classes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StatusClass {
    Success,
    Redirection,
    ClientError,
    ServerError,
}

impl StatusClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "2xx",
            Self::Redirection => "3xx",
            Self::ClientError => "4xx",
            Self::ServerError => "5xx",
        }
    }
}

impl From<StatusCode> for StatusClass {
    fn from(status: StatusCode) -> Self {
        match status.as_u16() / 100 {
            2 => Self::Success,
            3 => Self::Redirection,
            4 => Self::ClientError,
            // An informational or non-standard final response is a server-side
            // contract violation and is conservatively grouped with 5xx.
            _ => Self::ServerError,
        }
    }
}

impl EncodeLabelValue for StatusClass {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        EncodeLabelValue::encode(&self.as_str(), encoder)
    }
}

/// Every route template permitted in HTTP metrics.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HttpRoute {
    Root,
    SocialCard,
    Health,
    Healthz,
    Readyz,
    ShowcaseStatus,
    InternalWorkers,
    ChatCompletions,
    InternalScheduler,
    InternalCache,
    BatchJobs,
    BatchClaim,
    BatchJob,
    BatchJobAck,
    BatchJobFail,
    BatchDeadLetter,
    InternalStatus,
    RaftRequestVote,
    RaftAppendEntries,
    ControlStatus,
    ControlConfig,
    TrustStatus,
    TrustRenewalStatus,
    TrustSnapshot,
    TrustReceipts,
    LinkStatus,
    LinkMode,
    Unmatched,
}

impl HttpRoute {
    /// Map a framework-provided matched template through the service allowlist.
    /// Raw URI paths are never retained or exported.
    #[must_use]
    pub fn from_matched_path(service: Service, matched_path: Option<&str>) -> Self {
        let Some(path) = matched_path else {
            return Self::Unmatched;
        };
        match (service, path) {
            (Service::Gateway, "/") => Self::Root,
            (Service::Gateway, "/assets/og-inferlab.png") => Self::SocialCard,
            (Service::Gateway, "/health") => Self::Health,
            (Service::Gateway, "/readyz") => Self::Readyz,
            (Service::Gateway, "/showcase/status") => Self::ShowcaseStatus,
            (Service::Gateway, "/internal/workers") => Self::InternalWorkers,
            (Service::Gateway, "/v1/chat/completions") => Self::ChatCompletions,

            (Service::CpuWorker, "/health") => Self::Health,
            (Service::CpuWorker, "/internal/scheduler") => Self::InternalScheduler,
            (Service::CpuWorker, "/internal/cache") => Self::InternalCache,
            (Service::CpuWorker, "/v1/chat/completions") => Self::ChatCompletions,

            (Service::BatchQueue, "/healthz") => Self::Healthz,
            (Service::BatchQueue, "/v1/batch/jobs") => Self::BatchJobs,
            (Service::BatchQueue, "/v1/batch/claim") => Self::BatchClaim,
            (Service::BatchQueue, "/v1/batch/jobs/{job_id}") => Self::BatchJob,
            (Service::BatchQueue, "/v1/batch/jobs/{job_id}/ack") => Self::BatchJobAck,
            (Service::BatchQueue, "/v1/batch/jobs/{job_id}/fail") => Self::BatchJobFail,
            (Service::BatchQueue, "/v1/batch/dead-letter") => Self::BatchDeadLetter,
            (Service::BatchQueue, "/internal/status") => Self::InternalStatus,

            (Service::ControlPlane, "/healthz") => Self::Healthz,
            (Service::ControlPlane, "/raft/request-vote") => Self::RaftRequestVote,
            (Service::ControlPlane, "/raft/append-entries") => Self::RaftAppendEntries,
            (Service::ControlPlane, "/v1/control/status") => Self::ControlStatus,
            (Service::ControlPlane, "/v1/control/config") => Self::ControlConfig,

            (Service::TrustDistributor, "/health") => Self::Health,
            (Service::TrustDistributor, "/readyz") => Self::Readyz,
            (Service::TrustDistributor, "/v1/service-trust/status") => Self::TrustStatus,
            (Service::TrustDistributor, "/v1/service-trust/snapshot") => Self::TrustSnapshot,
            (Service::TrustDistributor, "/v1/service-trust/receipts") => Self::TrustReceipts,

            (Service::TrustRenewer, "/health") => Self::Health,
            (Service::TrustRenewer, "/readyz") => Self::Readyz,
            (Service::TrustRenewer, "/v1/service-trust/renewal/status") => Self::TrustRenewalStatus,

            (Service::RaftLinkProxy, "/healthz") => Self::Healthz,
            (Service::RaftLinkProxy, "/v1/link/status") => Self::LinkStatus,
            (Service::RaftLinkProxy, "/v1/link/mode") => Self::LinkMode,
            (Service::RaftLinkProxy, "/raft/request-vote") => Self::RaftRequestVote,
            (Service::RaftLinkProxy, "/raft/append-entries") => Self::RaftAppendEntries,
            _ => Self::Unmatched,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Root => "/",
            Self::SocialCard => "/assets/og-inferlab.png",
            Self::Health => "/health",
            Self::Healthz => "/healthz",
            Self::Readyz => "/readyz",
            Self::ShowcaseStatus => "/showcase/status",
            Self::InternalWorkers => "/internal/workers",
            Self::ChatCompletions => "/v1/chat/completions",
            Self::InternalScheduler => "/internal/scheduler",
            Self::InternalCache => "/internal/cache",
            Self::BatchJobs => "/v1/batch/jobs",
            Self::BatchClaim => "/v1/batch/claim",
            Self::BatchJob => "/v1/batch/jobs/{job_id}",
            Self::BatchJobAck => "/v1/batch/jobs/{job_id}/ack",
            Self::BatchJobFail => "/v1/batch/jobs/{job_id}/fail",
            Self::BatchDeadLetter => "/v1/batch/dead-letter",
            Self::InternalStatus => "/internal/status",
            Self::RaftRequestVote => "/raft/request-vote",
            Self::RaftAppendEntries => "/raft/append-entries",
            Self::ControlStatus => "/v1/control/status",
            Self::ControlConfig => "/v1/control/config",
            Self::TrustStatus => "/v1/service-trust/status",
            Self::TrustRenewalStatus => "/v1/service-trust/renewal/status",
            Self::TrustSnapshot => "/v1/service-trust/snapshot",
            Self::TrustReceipts => "/v1/service-trust/receipts",
            Self::LinkStatus => "/v1/link/status",
            Self::LinkMode => "/v1/link/mode",
            Self::Unmatched => "unmatched",
        }
    }

    /// Whether a route/method pair is part of the public API contract.
    ///
    /// Known paths called with unsupported methods are recorded as
    /// `unmatched`. This preserves the method signal while preventing an
    /// attacker from multiplying every route by every possible method.
    #[must_use]
    pub const fn allows_method(self, method: HttpMethod) -> bool {
        match self {
            Self::Root
            | Self::SocialCard
            | Self::Health
            | Self::Healthz
            | Self::Readyz
            | Self::ShowcaseStatus
            | Self::InternalWorkers
            | Self::InternalScheduler
            | Self::InternalCache
            | Self::BatchJob
            | Self::BatchDeadLetter
            | Self::InternalStatus
            | Self::ControlStatus
            | Self::TrustStatus
            | Self::TrustRenewalStatus
            | Self::LinkStatus => matches!(method, HttpMethod::Get),
            Self::ChatCompletions
            | Self::BatchJobs
            | Self::BatchClaim
            | Self::BatchJobAck
            | Self::BatchJobFail
            | Self::RaftRequestVote
            | Self::RaftAppendEntries
            | Self::TrustReceipts => matches!(method, HttpMethod::Post),
            Self::ControlConfig => matches!(method, HttpMethod::Get | HttpMethod::Put),
            Self::TrustSnapshot => matches!(method, HttpMethod::Get | HttpMethod::Post),
            Self::LinkMode => matches!(method, HttpMethod::Put),
            Self::Unmatched => true,
        }
    }
}

impl EncodeLabelValue for HttpRoute {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        EncodeLabelValue::encode(&self.as_str(), encoder)
    }
}

#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
pub struct HttpRequestLabels {
    pub service: Service,
    pub route: HttpRoute,
    pub method: HttpMethod,
    pub status_class: StatusClass,
}

#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
pub struct HttpDurationLabels {
    pub service: Service,
    pub route: HttpRoute,
    pub method: HttpMethod,
}

#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
pub struct ServiceLabels {
    pub service: Service,
}

/// Shared HTTP metric handles for one service.
#[derive(Clone, Debug)]
pub struct HttpMetrics {
    service: Service,
    requests: RequestFamily,
    handler_duration: DurationFamily,
    in_flight: InFlightFamily,
}

impl HttpMetrics {
    /// Register the three shared HTTP metric families.
    pub fn register(
        registry: &mut MetricsRegistry,
        service: Service,
    ) -> Result<Self, RegistryError> {
        let requests = RequestFamily::default();
        let handler_duration = DurationFamily::new_with_constructor(FixedHistogramConstructor);
        let in_flight = InFlightFamily::default();

        registry.register(
            "inferlab_http_requests",
            "HTTP responses returned by an InferLab service",
            requests.clone(),
        )?;
        registry.register_with_unit(
            "inferlab_http_handler_duration",
            "Time from request receipt until response headers are available",
            Unit::Seconds,
            handler_duration.clone(),
        )?;
        registry.register(
            "inferlab_http_requests_in_flight",
            "HTTP requests currently executing in an InferLab service",
            in_flight.clone(),
        )?;

        Ok(Self {
            service,
            requests,
            handler_duration,
            in_flight,
        })
    }

    /// Apply request-ID assignment and bounded HTTP instrumentation.
    ///
    /// The request-ID layer is outermost, so the canonical ID exists before
    /// inner authentication, body-limit, and handler layers run.
    pub fn instrument(&self, router: Router) -> Router {
        router
            .layer(middleware::from_fn_with_state(self.clone(), observe_http))
            .layer(middleware::from_fn(request_id_middleware))
    }

    #[must_use]
    pub const fn service(&self) -> Service {
        self.service
    }
}

async fn observe_http(
    State(metrics): State<HttpMetrics>,
    request: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let requested_method = HttpMethod::from(request.method());
    let matched_path = request
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str);
    let matched_route = HttpRoute::from_matched_path(metrics.service, matched_path);
    let (route, method) =
        if matched_route != HttpRoute::Unmatched && matched_route.allows_method(requested_method) {
            (matched_route, requested_method)
        } else {
            (HttpRoute::Unmatched, HttpMethod::Other)
        };
    let request_id = request.extensions().get::<RequestId>().cloned();

    let in_flight = metrics
        .in_flight
        .get_or_create(&ServiceLabels {
            service: metrics.service,
        })
        .clone();
    in_flight.inc();
    let _in_flight_guard = InFlightGuard(in_flight);

    let response = next.run(request).await;
    let elapsed = started.elapsed();
    let status = response.status();
    let status_class = StatusClass::from(status);

    metrics
        .requests
        .get_or_create(&HttpRequestLabels {
            service: metrics.service,
            route,
            method,
            status_class,
        })
        .inc();
    metrics
        .handler_duration
        .get_or_create(&HttpDurationLabels {
            service: metrics.service,
            route,
            method,
        })
        .observe(elapsed.as_secs_f64());

    tracing::info!(
        service = metrics.service.as_str(),
        event = "http_response_headers",
        request_id = request_id.as_ref().map_or("missing", RequestId::as_str),
        route = route.as_str(),
        method = method.as_str(),
        status = status.as_u16(),
        duration_ms = elapsed.as_secs_f64() * 1_000.0,
    );

    response
}

struct InFlightGuard(Gauge);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.dec();
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        Extension,
        body::{Body, to_bytes},
        http::Request as HttpRequest,
        response::IntoResponse,
        routing::get,
    };
    use tower::ServiceExt;

    use super::*;
    use crate::REQUEST_ID_HEADER;

    async fn echo_request_id(Extension(request_id): Extension<RequestId>) -> impl IntoResponse {
        request_id.as_str().to_owned()
    }

    fn test_app() -> (Router, MetricsRegistry) {
        let mut registry = MetricsRegistry::new();
        let http = HttpMetrics::register(&mut registry, Service::Gateway).unwrap();
        let app = http.instrument(Router::new().route("/health", get(echo_request_id)));
        (app, registry)
    }

    #[tokio::test]
    async fn valid_id_is_preserved_and_echoed() {
        let (app, _) = test_app();
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/health")
                    .header(&REQUEST_ID_HEADER, "caller-123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers().get(&REQUEST_ID_HEADER).unwrap(),
            "caller-123"
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"caller-123");
    }

    #[tokio::test]
    async fn invalid_id_is_replaced_before_inner_service_and_never_reflected() {
        let (app, _) = test_app();
        let invalid = "private/raw/id";
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/health")
                    .header(&REQUEST_ID_HEADER, invalid)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let echoed = response
            .headers()
            .get(&REQUEST_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert_ne!(echoed, invalid);
        assert!(RequestId::parse(&echoed).is_ok());
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], echoed.as_bytes());
    }

    #[tokio::test]
    async fn generated_id_is_echoed_on_inner_error_response() {
        let mut registry = MetricsRegistry::new();
        let http = HttpMetrics::register(&mut registry, Service::Gateway).unwrap();
        let app = http
            .instrument(Router::new().route("/health", get(|| async { StatusCode::UNAUTHORIZED })));
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let echoed = response
            .headers()
            .get(&REQUEST_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(RequestId::parse(echoed).is_ok());
    }

    #[tokio::test]
    async fn matched_template_and_finite_labels_are_rendered_exactly() {
        let (app, registry) = test_app();
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let output = registry.render().unwrap();
        assert!(output.contains(
            "inferlab_http_requests_total{service=\"gateway\",route=\"/health\",method=\"GET\",status_class=\"2xx\"} 1"
        ));
        assert!(output.contains("inferlab_http_requests_in_flight{service=\"gateway\"} 0"));
    }

    #[tokio::test]
    async fn raw_unmatched_paths_collapse_to_one_finite_series() {
        let (app, registry) = test_app();
        let methods = [Method::GET, Method::POST, Method::PUT, Method::PATCH];
        for index in 0..100 {
            let uri = format!("/private/{index}/prompt-fragment");
            let request = HttpRequest::builder()
                .method(methods[index % methods.len()].clone())
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::POST)
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);

        let output = registry.render().unwrap();
        assert!(!output.contains("private"));
        assert!(!output.contains("prompt-fragment"));
        let unmatched_request_series = output
            .lines()
            .filter(|line| {
                line.starts_with("inferlab_http_requests_total{")
                    && line.contains("route=\"unmatched\"")
            })
            .collect::<Vec<_>>();
        assert_eq!(unmatched_request_series.len(), 1);
        assert!(unmatched_request_series[0].contains("method=\"other\""));
        assert!(unmatched_request_series[0].ends_with(" 101"));
        for forbidden_method in ["GET", "POST", "PUT"] {
            assert!(!output.lines().any(|line| {
                line.contains("route=\"unmatched\"")
                    && line.contains(&format!("method=\"{forbidden_method}\""))
            }));
        }
    }

    #[test]
    fn allowlist_rejects_cross_service_and_unknown_templates() {
        assert_eq!(
            HttpRoute::from_matched_path(Service::Gateway, Some("/health")),
            HttpRoute::Health
        );
        assert_eq!(
            HttpRoute::from_matched_path(Service::CpuWorker, Some("/readyz")),
            HttpRoute::Unmatched
        );
        assert_eq!(
            HttpRoute::from_matched_path(Service::Gateway, Some("/users/{user_id}")),
            HttpRoute::Unmatched
        );
        assert_eq!(
            HttpRoute::from_matched_path(
                Service::TrustRenewer,
                Some("/v1/service-trust/renewal/status"),
            ),
            HttpRoute::TrustRenewalStatus
        );
        assert_eq!(
            HttpRoute::from_matched_path(
                Service::TrustDistributor,
                Some("/v1/service-trust/renewal/status"),
            ),
            HttpRoute::Unmatched
        );
        assert!(HttpRoute::ControlConfig.allows_method(HttpMethod::Get));
        assert!(HttpRoute::ControlConfig.allows_method(HttpMethod::Put));
        assert!(!HttpRoute::ControlConfig.allows_method(HttpMethod::Post));
    }

    #[test]
    fn theoretical_common_http_cardinality_is_hard_bounded() {
        let services = [
            Service::Gateway,
            Service::CpuWorker,
            Service::BatchQueue,
            Service::ControlPlane,
            Service::TrustDistributor,
            Service::TrustRenewer,
            Service::RaftLinkProxy,
        ];
        assert_eq!(Service::BatchQueue.max_http_series(), 190);
        assert!(
            services
                .into_iter()
                .all(|service| { service.max_http_series() <= MAX_HTTP_SERIES_PER_TARGET })
        );
        assert_eq!(
            MAX_HTTP_SERIES_PER_TARGET + DOMAIN_SERIES_BUDGET_PER_TARGET,
            TOTAL_SERIES_BUDGET_PER_TARGET
        );
    }
}
