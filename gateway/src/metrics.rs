use std::{
    fmt,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Instant,
};

use axum::{body::Bytes, http::StatusCode};
use futures_util::Stream;
use observability::{FIXED_HISTOGRAM_BUCKETS, MetricsRegistry, RequestId};
use prometheus_client::{
    encoding::{EncodeLabelSet, EncodeLabelValue, LabelValueEncoder},
    metrics::{counter::Counter, family::Family, gauge::Gauge, histogram::Histogram},
    registry::Unit,
};

use crate::{
    SharedControlPlaneStatus, SharedRoutingSnapshot, admission::AdmissionController,
    resilience::ResilienceController, routing::RoutingPolicy, routing_lease::SharedRoutingLease,
};

type UnsignedGauge = Gauge<u64, AtomicU64>;
type AdmissionGaugeFamily = Family<AdmissionLabels, UnsignedGauge>;
type RetryCounterFamily = Family<RetryLabels, Counter>;
type CircuitGaugeFamily = Family<CircuitLabels, UnsignedGauge>;
type CompletionHistogramFamily = Family<CompletionLabels, Histogram, fn() -> Histogram>;

#[derive(Clone, Copy, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct AdmissionLabels {
    state: AdmissionState,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum AdmissionState {
    Outstanding,
    Executing,
    Queued,
}

impl EncodeLabelValue for AdmissionState {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        EncodeLabelValue::encode(
            &match self {
                Self::Outstanding => "outstanding",
                Self::Executing => "executing",
                Self::Queued => "queued",
            },
            encoder,
        )
    }
}

#[derive(Clone, Copy, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct RetryLabels {
    decision: RetryDecision,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum RetryDecision {
    Granted,
    BudgetDenied,
    LimitExhausted,
}

impl EncodeLabelValue for RetryDecision {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        EncodeLabelValue::encode(
            &match self {
                Self::Granted => "granted",
                Self::BudgetDenied => "budget_denied",
                Self::LimitExhausted => "limit_exhausted",
            },
            encoder,
        )
    }
}

#[derive(Clone, Copy, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct CircuitLabels {
    state: CircuitState,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

impl EncodeLabelValue for CircuitState {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        EncodeLabelValue::encode(
            &match self {
                Self::Closed => "closed",
                Self::Open => "open",
                Self::HalfOpen => "half_open",
            },
            encoder,
        )
    }
}

#[derive(Clone, Copy, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct CompletionLabels {
    outcome: CompletionOutcome,
}

#[derive(Clone)]
pub(crate) struct GatewayMetrics {
    completion: CompletionHistograms,
    pub(crate) public_edge_rejections: Option<Counter>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompletionMode {
    Json,
    Stream,
}

impl CompletionMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Stream => "stream",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum CompletionOutcome {
    Success,
    Error,
    Cancelled,
    Deadline,
}

impl CompletionOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
            Self::Deadline => "deadline",
        }
    }
}

impl EncodeLabelValue for CompletionOutcome {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        EncodeLabelValue::encode(&self.as_str(), encoder)
    }
}

#[derive(Clone)]
struct CompletionHistograms {
    success: Histogram,
    error: Histogram,
    cancelled: Histogram,
    deadline: Histogram,
}

impl CompletionHistograms {
    fn get(&self, outcome: CompletionOutcome) -> &Histogram {
        match outcome {
            CompletionOutcome::Success => &self.success,
            CompletionOutcome::Error => &self.error,
            CompletionOutcome::Cancelled => &self.cancelled,
            CompletionOutcome::Deadline => &self.deadline,
        }
    }
}

#[derive(Clone)]
struct CounterMirror {
    metric: Counter,
    observed: Arc<Mutex<u64>>,
}

impl CounterMirror {
    fn new(metric: Counter) -> Self {
        Self {
            metric,
            observed: Arc::new(Mutex::new(0)),
        }
    }

    fn update(&self, observed: u64) {
        let mut previous = self
            .observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if observed > *previous {
            self.metric.inc_by(observed - *previous);
            *previous = observed;
        }
    }
}

pub(crate) fn register(
    registry: &mut MetricsRegistry,
    routing: SharedRoutingSnapshot,
    _control_plane: Option<SharedControlPlaneStatus>,
    routing_lease: Option<SharedRoutingLease>,
    admission: Arc<AdmissionController>,
    resilience: Arc<ResilienceController>,
    hosted_public_edge: bool,
) -> Result<GatewayMetrics, String> {
    let admission_current = AdmissionGaugeFamily::default();
    let admission_outstanding = admission_current.get_or_create_owned(&AdmissionLabels {
        state: AdmissionState::Outstanding,
    });
    let admission_executing = admission_current.get_or_create_owned(&AdmissionLabels {
        state: AdmissionState::Executing,
    });
    let admission_queued = admission_current.get_or_create_owned(&AdmissionLabels {
        state: AdmissionState::Queued,
    });
    registry
        .register(
            "inferlab_gateway_admission_current",
            "Current gateway requests by admission state",
            admission_current,
        )
        .map_err(|error| error.to_string())?;

    let admission_rejections = Counter::default();
    registry
        .register(
            "inferlab_gateway_admission_rejections",
            "Gateway requests rejected by bounded admission",
            admission_rejections.clone(),
        )
        .map_err(|error| error.to_string())?;

    let requests = Counter::default();
    registry
        .register(
            "inferlab_gateway_requests",
            "Original completion requests admitted to gateway resilience handling",
            requests.clone(),
        )
        .map_err(|error| error.to_string())?;

    let attempts = Counter::default();
    registry
        .register(
            "inferlab_gateway_attempts",
            "Gateway worker attempts",
            attempts.clone(),
        )
        .map_err(|error| error.to_string())?;

    let transient_failures = Counter::default();
    registry
        .register(
            "inferlab_gateway_transient_failures",
            "Transient gateway worker-attempt failures",
            transient_failures.clone(),
        )
        .map_err(|error| error.to_string())?;

    let retries = RetryCounterFamily::default();
    let retries_granted = retries.get_or_create_owned(&RetryLabels {
        decision: RetryDecision::Granted,
    });
    let retries_budget_denied = retries.get_or_create_owned(&RetryLabels {
        decision: RetryDecision::BudgetDenied,
    });
    let retries_limit_exhausted = retries.get_or_create_owned(&RetryLabels {
        decision: RetryDecision::LimitExhausted,
    });
    registry
        .register(
            "inferlab_gateway_retries",
            "Gateway retry decisions",
            retries,
        )
        .map_err(|error| error.to_string())?;

    let deadlines_exceeded = Counter::default();
    registry
        .register(
            "inferlab_gateway_deadlines_exceeded",
            "Gateway request deadlines exceeded",
            deadlines_exceeded.clone(),
        )
        .map_err(|error| error.to_string())?;

    let workers = UnsignedGauge::default();
    registry
        .register(
            "inferlab_gateway_workers",
            "Workers in the active immutable routing snapshot",
            workers.clone(),
        )
        .map_err(|error| error.to_string())?;

    let worker_requests_in_flight = UnsignedGauge::default();
    registry
        .register(
            "inferlab_gateway_worker_requests_in_flight",
            "Gateway requests holding a worker routing lease",
            worker_requests_in_flight.clone(),
        )
        .map_err(|error| error.to_string())?;

    let worker_circuits = CircuitGaugeFamily::default();
    let circuits_closed = worker_circuits.get_or_create_owned(&CircuitLabels {
        state: CircuitState::Closed,
    });
    let circuits_open = worker_circuits.get_or_create_owned(&CircuitLabels {
        state: CircuitState::Open,
    });
    let circuits_half_open = worker_circuits.get_or_create_owned(&CircuitLabels {
        state: CircuitState::HalfOpen,
    });
    registry
        .register(
            "inferlab_gateway_worker_circuits",
            "Gateway worker circuits by bounded state",
            worker_circuits,
        )
        .map_err(|error| error.to_string())?;

    let routing_lease_ready = UnsignedGauge::default();
    registry
        .register(
            "inferlab_gateway_routing_lease_ready",
            "Whether the routing lease permits new requests",
            routing_lease_ready.clone(),
        )
        .map_err(|error| error.to_string())?;

    let control_revision = UnsignedGauge::default();
    registry
        .register(
            "inferlab_gateway_control_revision",
            "Control revision in the active routing snapshot, or zero when static",
            control_revision.clone(),
        )
        .map_err(|error| error.to_string())?;

    let completion_family = CompletionHistogramFamily::new_with_constructor(fixed_histogram);
    let completion = CompletionHistograms {
        success: completion_histogram(&completion_family, CompletionOutcome::Success),
        error: completion_histogram(&completion_family, CompletionOutcome::Error),
        cancelled: completion_histogram(&completion_family, CompletionOutcome::Cancelled),
        deadline: completion_histogram(&completion_family, CompletionOutcome::Deadline),
    };
    registry
        .register_with_unit(
            "inferlab_gateway_completion_duration",
            "Gateway completion lifetime through downstream body completion",
            Unit::Seconds,
            completion_family,
        )
        .map_err(|error| error.to_string())?;

    let public_edge_rejections = hosted_public_edge.then(Counter::default);
    if let Some(counter) = public_edge_rejections.as_ref() {
        registry
            .register(
                "inferlab_gateway_public_edge_rejections",
                "Hosted completion-gate authentication, body, input, rate, and admission rejections",
                counter.clone(),
            )
            .map_err(|error| error.to_string())?;
    }

    let admission_rejections = CounterMirror::new(admission_rejections);
    let requests = CounterMirror::new(requests);
    let attempts = CounterMirror::new(attempts);
    let transient_failures = CounterMirror::new(transient_failures);
    let retries_granted = CounterMirror::new(retries_granted);
    let retries_budget_denied = CounterMirror::new(retries_budget_denied);
    let retries_limit_exhausted = CounterMirror::new(retries_limit_exhausted);
    let deadlines_exceeded = CounterMirror::new(deadlines_exceeded);
    let refresh = Mutex::new(());
    registry
        .set_before_render(move || {
            let _refresh = refresh
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let admission_snapshot = admission.snapshot();
            let resilience_snapshot = resilience.snapshot();
            let active_routing = routing
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            let routing_snapshot = active_routing.workers.metrics_snapshot();
            let lease_ready = routing_lease
                .as_ref()
                .is_none_or(|lease| lease.snapshot().accepting_new_requests);

            admission_outstanding.set(admission_snapshot.outstanding as u64);
            admission_executing.set(admission_snapshot.executing as u64);
            admission_queued.set(admission_snapshot.queued as u64);
            admission_rejections.update(admission_snapshot.rejected_total as u64);
            requests.update(resilience_snapshot.original_requests);
            attempts.update(resilience_snapshot.attempts);
            transient_failures.update(resilience_snapshot.transient_failures);
            retries_granted.update(resilience_snapshot.retries_granted);
            retries_budget_denied.update(resilience_snapshot.retries_denied_budget);
            retries_limit_exhausted.update(resilience_snapshot.retry_limit_exhausted);
            deadlines_exceeded.update(resilience_snapshot.deadline_exceeded);
            workers.set(routing_snapshot.workers as u64);
            worker_requests_in_flight.set(routing_snapshot.in_flight as u64);
            circuits_closed.set(routing_snapshot.circuits_closed as u64);
            circuits_open.set(routing_snapshot.circuits_open as u64);
            circuits_half_open.set(routing_snapshot.circuits_half_open as u64);
            routing_lease_ready.set(u64::from(lease_ready));
            control_revision.set(active_routing.control_revision.unwrap_or(0));
        })
        .map_err(|error| error.to_string())?;

    Ok(GatewayMetrics {
        completion,
        public_edge_rejections,
    })
}

pub(crate) struct CompletionTimer {
    histograms: CompletionHistograms,
    request_id: RequestId,
    request_number: u64,
    mode: CompletionMode,
    policy: RoutingPolicy,
    started: Instant,
    deadline: Instant,
    deadline_fired: Arc<AtomicBool>,
    body: Option<BodyContext>,
    finished: bool,
}

struct BodyContext {
    worker_id: String,
    attempt: usize,
    status: StatusCode,
}

impl CompletionTimer {
    pub(crate) fn start(
        metrics: &GatewayMetrics,
        request_id: RequestId,
        request_number: u64,
        mode: CompletionMode,
        policy: RoutingPolicy,
        deadline: Instant,
    ) -> Self {
        tracing::info!(
            service = "gateway",
            event = "completion_started",
            request_id = request_id.as_str(),
            request_number,
            mode = mode.as_str(),
            policy = %policy,
        );
        Self {
            histograms: metrics.completion.clone(),
            request_id,
            request_number,
            mode,
            policy,
            started: Instant::now(),
            deadline,
            deadline_fired: Arc::new(AtomicBool::new(false)),
            body: None,
            finished: false,
        }
    }

    pub(crate) fn deadline_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.deadline_fired)
    }

    pub(crate) fn error(&mut self) {
        self.finish(CompletionOutcome::Error);
    }

    pub(crate) fn deadline(&mut self) {
        self.deadline_fired.store(true, Ordering::Relaxed);
        self.finish(CompletionOutcome::Deadline);
    }

    pub(crate) fn into_stream<S>(
        mut self,
        stream: S,
        worker_id: String,
        attempt: usize,
        status: StatusCode,
    ) -> CompletionStream
    where
        S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    {
        self.body = Some(BodyContext {
            worker_id,
            attempt,
            status,
        });
        CompletionStream {
            inner: Box::pin(stream),
            timer: Some(self),
        }
    }

    fn deadline_reached(&self) -> bool {
        self.deadline_fired.load(Ordering::Relaxed) || Instant::now() >= self.deadline
    }

    fn finish(&mut self, outcome: CompletionOutcome) {
        if self.finished {
            return;
        }
        self.finished = true;
        let elapsed = self.started.elapsed();
        self.histograms.get(outcome).observe(elapsed.as_secs_f64());
        let (worker_id, attempt, status) =
            self.body.as_ref().map_or(("unselected", 0, 0), |body| {
                (body.worker_id.as_str(), body.attempt, body.status.as_u16())
            });
        tracing::info!(
            service = "gateway",
            event = "completion_terminal",
            request_id = self.request_id.as_str(),
            request_number = self.request_number,
            mode = self.mode.as_str(),
            outcome = outcome.as_str(),
            policy = %self.policy,
            worker_id,
            attempt,
            status,
            duration_ms = elapsed.as_secs_f64() * 1_000.0,
        );
    }
}

impl Drop for CompletionTimer {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let outcome = if self.deadline_reached() {
            CompletionOutcome::Deadline
        } else if self.body.is_some() {
            CompletionOutcome::Cancelled
        } else {
            // Before a response body exists, dropping the handler future means the
            // downstream caller cancelled it. Normal early returns explicitly mark
            // their error or deadline outcome before this guard is dropped.
            CompletionOutcome::Cancelled
        };
        self.finish(outcome);
    }
}

pub(crate) struct CompletionStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    timer: Option<CompletionTimer>,
}

impl Stream for CompletionStream {
    type Item = Result<Bytes, reqwest::Error>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let next = self.inner.as_mut().poll_next(context);
        match &next {
            Poll::Ready(None) => {
                if let Some(timer) = self.timer.as_mut() {
                    let outcome = if timer.deadline_reached() {
                        CompletionOutcome::Deadline
                    } else if timer
                        .body
                        .as_ref()
                        .is_some_and(|body| body.status.is_success())
                    {
                        CompletionOutcome::Success
                    } else {
                        CompletionOutcome::Error
                    };
                    timer.finish(outcome);
                }
            }
            Poll::Ready(Some(Err(_))) => {
                if let Some(timer) = self.timer.as_mut() {
                    let outcome = if timer.deadline_reached() {
                        CompletionOutcome::Deadline
                    } else {
                        CompletionOutcome::Error
                    };
                    timer.finish(outcome);
                }
            }
            Poll::Pending | Poll::Ready(Some(Ok(_))) => {}
        }
        next
    }
}

fn completion_histogram(
    family: &CompletionHistogramFamily,
    outcome: CompletionOutcome,
) -> Histogram {
    family.get_or_create_owned(&CompletionLabels { outcome })
}

fn fixed_histogram() -> Histogram {
    Histogram::new(FIXED_HISTOGRAM_BUCKETS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use observability::TOTAL_SERIES_BUDGET_PER_TARGET;

    #[test]
    fn completion_label_space_is_exactly_four_series() {
        let mut labels = Vec::new();
        for outcome in [
            CompletionOutcome::Success,
            CompletionOutcome::Error,
            CompletionOutcome::Cancelled,
            CompletionOutcome::Deadline,
        ] {
            labels.push(outcome.as_str());
        }
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), 4);
    }

    #[test]
    fn pre_header_drop_is_cancelled_and_normal_returns_are_explicit() {
        let mut registry = MetricsRegistry::new();
        let family = CompletionHistogramFamily::new_with_constructor(fixed_histogram);
        let completion = CompletionHistograms {
            success: completion_histogram(&family, CompletionOutcome::Success),
            error: completion_histogram(&family, CompletionOutcome::Error),
            cancelled: completion_histogram(&family, CompletionOutcome::Cancelled),
            deadline: completion_histogram(&family, CompletionOutcome::Deadline),
        };
        registry
            .register(
                "test_gateway_completion_duration_seconds",
                "Test-only completion lifetime",
                family,
            )
            .expect("register completion family");
        let metrics = GatewayMetrics {
            completion,
            public_edge_rejections: None,
        };
        let start = |request_id: &str| {
            CompletionTimer::start(
                &metrics,
                RequestId::parse(request_id).expect("request ID"),
                1,
                CompletionMode::Json,
                RoutingPolicy::RoundRobin,
                Instant::now() + std::time::Duration::from_secs(1),
            )
        };

        drop(start("cancel-before-headers"));
        let mut error = start("explicit-error-before-headers");
        error.error();
        drop(error);
        let mut deadline = start("explicit-deadline-before-headers");
        deadline.deadline();
        drop(deadline);

        let rendered = registry.render().expect("metrics");
        assert!(
            rendered.contains(
                "test_gateway_completion_duration_seconds_count{outcome=\"cancelled\"} 1"
            )
        );
        assert!(
            rendered
                .contains("test_gateway_completion_duration_seconds_count{outcome=\"error\"} 1")
        );
        assert!(
            rendered
                .contains("test_gateway_completion_duration_seconds_count{outcome=\"deadline\"} 1")
        );
    }

    #[test]
    fn gateway_theoretical_series_stay_within_the_hard_target_budget() {
        const SCALAR_AND_COUNTER_SERIES: usize = 18;
        const HISTOGRAM_SERIES: usize = 4 * (FIXED_HISTOGRAM_BUCKETS.len() + 3);
        let local_total = observability::Service::Gateway.max_http_series()
            + SCALAR_AND_COUNTER_SERIES
            + HISTOGRAM_SERIES;
        let hosted_total = local_total + 1;

        assert_eq!(local_total, 255);
        assert_eq!(hosted_total, 256);
        assert!(hosted_total <= TOTAL_SERIES_BUDGET_PER_TARGET);
    }
}
