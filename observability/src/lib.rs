//! Shared, bounded-cardinality observability primitives for InferLab services.
//!
//! The crate deliberately separates an application's public listener from its
//! opt-in metrics listener. Metrics labels are finite enums and matched routes
//! are checked against a service-specific allowlist before they are exported.

mod http;
mod logging;
mod registry;
mod request_id;
mod server;

pub use http::{
    DOMAIN_SERIES_BUDGET_PER_TARGET, HttpDurationLabels, HttpMethod, HttpMetrics,
    HttpRequestLabels, HttpRoute, MAX_HTTP_SERIES_PER_TARGET, Service, ServiceLabels, StatusClass,
    TOTAL_SERIES_BUDGET_PER_TARGET,
};
pub use logging::{LogFormat, LogFormatError, TracingInitError, init_tracing};
pub use prometheus_client;
pub use registry::{
    FIXED_HISTOGRAM_BUCKETS, FixedHistogramConstructor, MetricsRegistry, RegistryError,
    fixed_histogram,
};
pub use request_id::{REQUEST_ID_HEADER, RequestId, RequestIdError, request_id_middleware};
pub use server::{MetricsServerConfig, MetricsServerConfigError, metrics_router, serve_metrics};
