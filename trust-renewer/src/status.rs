use std::sync::{Arc, RwLock};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use prometheus_client::metrics::{counter::Counter, gauge::Gauge};
use serde::Serialize;
use serde_json::json;

use observability::{MetricsRegistry, RegistryError};

use crate::RenewalCounters;

pub const TRUST_RENEWER_STATUS_SCHEMA: &str = "inferlab.trust-renewer-status.v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RenewerPhase {
    Reconciling,
    Waiting,
    Publishing,
    RetryWaiting,
    FailedClosed,
}

impl RenewerPhase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reconciling => "reconciling",
            Self::Waiting => "waiting",
            Self::Publishing => "publishing",
            Self::RetryWaiting => "retry_waiting",
            Self::FailedClosed => "failed_closed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RenewerErrorKind {
    Configuration,
    Template,
    State,
    StateDurabilityUncertain,
    SingleWriter,
    Transport,
    RemoteSnapshot,
    TemplateMismatch,
    AuthorityMismatch,
    GenerationFork,
    DistributorRollback,
    PendingOutsideValidity,
    PublicationRejected,
    Clock,
    Internal,
}

impl RenewerErrorKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Configuration => "configuration",
            Self::Template => "template",
            Self::State => "state",
            Self::StateDurabilityUncertain => "state_durability_uncertain",
            Self::SingleWriter => "single_writer",
            Self::Transport => "transport",
            Self::RemoteSnapshot => "remote_snapshot",
            Self::TemplateMismatch => "template_mismatch",
            Self::AuthorityMismatch => "authority_mismatch",
            Self::GenerationFork => "generation_fork",
            Self::DistributorRollback => "distributor_rollback",
            Self::PendingOutsideValidity => "pending_outside_validity",
            Self::PublicationRejected => "publication_rejected",
            Self::Clock => "clock",
            Self::Internal => "internal",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RenewerStatus {
    pub schema: &'static str,
    pub service: &'static str,
    pub mode: &'static str,
    pub phase: RenewerPhase,
    pub ready: bool,
    pub transport: &'static str,
    pub template_fingerprint: String,
    pub authority_fingerprint: String,
    pub distributor_generation: Option<u64>,
    pub committed_generation: Option<u64>,
    pub pending_generation: Option<u64>,
    pub current_expires_at_ms: Option<u64>,
    pub renewal_deadline_ms: Option<u64>,
    pub remaining_margin_ms: Option<i64>,
    pub attempts: u64,
    pub successful_renewals: u64,
    pub transient_failures: u64,
    pub rejected_states: u64,
    pub late_recoveries: u64,
    pub last_error_kind: Option<RenewerErrorKind>,
}

impl RenewerStatus {
    #[must_use]
    pub fn starting(template_fingerprint: String, authority_fingerprint: String) -> Self {
        Self {
            schema: TRUST_RENEWER_STATUS_SCHEMA,
            service: "inferlab-trust-renewer",
            mode: "automatic-renewal",
            phase: RenewerPhase::Reconciling,
            ready: false,
            transport: "mutual-tls",
            template_fingerprint,
            authority_fingerprint,
            distributor_generation: None,
            committed_generation: None,
            pending_generation: None,
            current_expires_at_ms: None,
            renewal_deadline_ms: None,
            remaining_margin_ms: None,
            attempts: 0,
            successful_renewals: 0,
            transient_failures: 0,
            rejected_states: 0,
            late_recoveries: 0,
            last_error_kind: None,
        }
    }

    pub fn set_counters(&mut self, counters: &RenewalCounters) {
        self.attempts = counters.attempts;
        self.successful_renewals = counters.successful_renewals;
        self.transient_failures = counters.transient_failures;
        self.rejected_states = counters.rejected_states;
        self.late_recoveries = counters.late_recoveries;
    }
}

#[derive(Clone, Debug)]
pub struct SharedRenewerStatus {
    inner: Arc<RwLock<RenewerStatus>>,
}

impl SharedRenewerStatus {
    #[must_use]
    pub fn new(status: RenewerStatus) -> Self {
        Self {
            inner: Arc::new(RwLock::new(status)),
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> RenewerStatus {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn update(&self, update: impl FnOnce(&mut RenewerStatus)) {
        let mut status = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        update(&mut status);
    }
}

pub fn status_app(status: SharedRenewerStatus) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/readyz", get(readiness))
        .route("/v1/service-trust/renewal/status", get(status_response))
        .with_state(status)
}

async fn health(State(status): State<SharedRenewerStatus>) -> Response {
    let snapshot = status.snapshot();
    Json(json!({
        "status": "ok",
        "service": "inferlab-trust-renewer",
        "phase": snapshot.phase,
    }))
    .into_response()
}

async fn readiness(State(status): State<SharedRenewerStatus>) -> Response {
    let snapshot = status.snapshot();
    if snapshot.ready {
        Json(json!({
            "status": "ready",
            "phase": snapshot.phase,
            "distributor_generation": snapshot.distributor_generation,
        }))
        .into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status": "not-ready",
                "phase": snapshot.phase,
                "last_error_kind": snapshot.last_error_kind,
            })),
        )
            .into_response()
    }
}

async fn status_response(State(status): State<SharedRenewerStatus>) -> Json<RenewerStatus> {
    Json(status.snapshot())
}

#[derive(Clone, Debug)]
pub struct TrustRenewerMetrics {
    status: SharedRenewerStatus,
    ready: Gauge,
    pending: Gauge,
    attempts: Counter,
    successful_renewals: Counter,
    transient_failures: Counter,
    rejected_states: Counter,
    late_recoveries: Counter,
}

impl TrustRenewerMetrics {
    pub fn register(
        registry: &mut MetricsRegistry,
        status: SharedRenewerStatus,
    ) -> Result<Self, RegistryError> {
        let metrics = Self {
            status,
            ready: Gauge::default(),
            pending: Gauge::default(),
            attempts: Counter::default(),
            successful_renewals: Counter::default(),
            transient_failures: Counter::default(),
            rejected_states: Counter::default(),
            late_recoveries: Counter::default(),
        };
        registry.register(
            "inferlab_trust_renewer_ready",
            "Whether the renewer has reconciled a current distributor generation.",
            metrics.ready.clone(),
        )?;
        registry.register(
            "inferlab_trust_renewer_pending",
            "Whether one exact signed snapshot is pending reconciliation.",
            metrics.pending.clone(),
        )?;
        registry.register(
            "inferlab_trust_renewer_attempts",
            "Durably recorded publication attempts.",
            metrics.attempts.clone(),
        )?;
        registry.register(
            "inferlab_trust_renewer_successful_renewals",
            "Durably reconciled automatic renewal cycles.",
            metrics.successful_renewals.clone(),
        )?;
        registry.register(
            "inferlab_trust_renewer_transient_failures",
            "Durably recorded transient distributor failures.",
            metrics.transient_failures.clone(),
        )?;
        registry.register(
            "inferlab_trust_renewer_rejected_states",
            "Durably recorded deterministic renewal rejections.",
            metrics.rejected_states.clone(),
        )?;
        registry.register(
            "inferlab_trust_renewer_late_recoveries",
            "Durably reconciled renewals published after the prior expiry.",
            metrics.late_recoveries.clone(),
        )?;
        let refresh = metrics.clone();
        registry.set_before_render(move || refresh.refresh())?;
        Ok(metrics)
    }

    pub fn refresh(&self) {
        let status = self.status.snapshot();
        self.ready.set(i64::from(status.ready));
        self.pending
            .set(i64::from(status.pending_generation.is_some()));
        set_counter(&self.attempts, status.attempts);
        set_counter(&self.successful_renewals, status.successful_renewals);
        set_counter(&self.transient_failures, status.transient_failures);
        set_counter(&self.rejected_states, status.rejected_states);
        set_counter(&self.late_recoveries, status.late_recoveries);
    }
}

fn set_counter(counter: &Counter, value: u64) {
    let current = counter.get();
    if value > current {
        counter.inc_by(value - current);
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;

    use super::*;

    fn status() -> SharedRenewerStatus {
        SharedRenewerStatus::new(RenewerStatus::starting("a".repeat(64), "b".repeat(64)))
    }

    #[tokio::test]
    async fn status_is_bounded_and_redacted() {
        let response = status_app(status())
            .oneshot(
                Request::builder()
                    .uri("/v1/service-trust/renewal/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .expect("body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["schema"], TRUST_RENEWER_STATUS_SCHEMA);
        assert_eq!(value["transport"], "mutual-tls");
        assert_eq!(value["attempts"], 0);
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        for forbidden in ["private_key", "signature", "credential", "template_path"] {
            assert!(!text.contains(forbidden));
        }
    }

    #[tokio::test]
    async fn readiness_and_health_reflect_finite_phase() {
        let status = status();
        let not_ready = status_app(status.clone())
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(not_ready.status(), StatusCode::SERVICE_UNAVAILABLE);

        status.update(|snapshot| {
            snapshot.phase = RenewerPhase::FailedClosed;
            snapshot.last_error_kind = Some(RenewerErrorKind::GenerationFork);
        });
        let unhealthy = status_app(status)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(unhealthy.status(), StatusCode::OK);
    }

    #[test]
    fn metrics_are_inferlab_prefixed_and_bounded() {
        let mut registry = MetricsRegistry::new();
        TrustRenewerMetrics::register(&mut registry, status()).expect("metrics");
        let rendered = registry.render().expect("render");
        for name in [
            "inferlab_trust_renewer_ready",
            "inferlab_trust_renewer_pending",
            "inferlab_trust_renewer_attempts_total",
            "inferlab_trust_renewer_successful_renewals_total",
            "inferlab_trust_renewer_transient_failures_total",
            "inferlab_trust_renewer_rejected_states_total",
            "inferlab_trust_renewer_late_recoveries_total",
        ] {
            assert!(rendered.contains(name), "missing {name}");
        }
        assert!(!rendered.contains("template_fingerprint"));
        assert!(!rendered.contains("authority_fingerprint"));
    }
}
