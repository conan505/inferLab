use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use observability::{FIXED_HISTOGRAM_BUCKETS, MetricsRegistry, RequestId};
use prometheus_client::{
    encoding::{EncodeLabelSet, EncodeLabelValue, LabelValueEncoder},
    metrics::{counter::Counter, family::Family, gauge::Gauge, histogram::Histogram},
    registry::Unit,
};

use crate::scheduler::ContinuousBatchScheduler;

type UnsignedGauge = Gauge<u64, AtomicU64>;
type SchedulerCurrentFamily = Family<SchedulerCurrentLabels, UnsignedGauge>;
type SchedulerRequestFamily = Family<SchedulerRequestLabels, Counter>;
type BatchSlotFamily = Family<BatchSlotLabels, Counter>;
type GenerationHistogramFamily = Family<GenerationLabels, Histogram, fn() -> Histogram>;

#[derive(Clone, Copy, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct SchedulerCurrentLabels {
    state: SchedulerCurrentState,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum SchedulerCurrentState {
    Queued,
    Active,
}

impl EncodeLabelValue for SchedulerCurrentState {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        EncodeLabelValue::encode(
            &match self {
                Self::Queued => "queued",
                Self::Active => "active",
            },
            encoder,
        )
    }
}

#[derive(Clone, Copy, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct SchedulerRequestLabels {
    outcome: SchedulerRequestOutcome,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum SchedulerRequestOutcome {
    Admitted,
    Completed,
    Cancelled,
    Failed,
}

impl EncodeLabelValue for SchedulerRequestOutcome {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        EncodeLabelValue::encode(
            &match self {
                Self::Admitted => "admitted",
                Self::Completed => "completed",
                Self::Cancelled => "cancelled",
                Self::Failed => "failed",
            },
            encoder,
        )
    }
}

#[derive(Clone, Copy, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct BatchSlotLabels {
    state: BatchSlotState,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum BatchSlotState {
    Used,
    Available,
}

impl EncodeLabelValue for BatchSlotState {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        EncodeLabelValue::encode(
            &match self {
                Self::Used => "used",
                Self::Available => "available",
            },
            encoder,
        )
    }
}

#[derive(Clone, Copy, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct GenerationLabels {
    outcome: GenerationOutcome,
}

#[derive(Clone)]
pub(crate) struct WorkerMetrics {
    generation: GenerationHistograms,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GenerationMode {
    Json,
    Stream,
}

impl GenerationMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Stream => "stream",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum GenerationOutcome {
    Success,
    Error,
    Cancelled,
}

impl GenerationOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

impl EncodeLabelValue for GenerationOutcome {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        EncodeLabelValue::encode(&self.as_str(), encoder)
    }
}

#[derive(Clone)]
struct GenerationHistograms {
    success: Histogram,
    error: Histogram,
    cancelled: Histogram,
}

impl GenerationHistograms {
    fn get(&self, outcome: GenerationOutcome) -> &Histogram {
        match outcome {
            GenerationOutcome::Success => &self.success,
            GenerationOutcome::Error => &self.error,
            GenerationOutcome::Cancelled => &self.cancelled,
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
    requests_source: Arc<AtomicU64>,
    scheduler: ContinuousBatchScheduler,
) -> Result<WorkerMetrics, String> {
    let requests = Counter::default();
    registry
        .register(
            "inferlab_worker_requests",
            "CPU worker completion requests accepted by JSON extraction",
            requests.clone(),
        )
        .map_err(|error| error.to_string())?;

    let scheduler_current = SchedulerCurrentFamily::default();
    let scheduler_queued = scheduler_current.get_or_create_owned(&SchedulerCurrentLabels {
        state: SchedulerCurrentState::Queued,
    });
    let scheduler_active = scheduler_current.get_or_create_owned(&SchedulerCurrentLabels {
        state: SchedulerCurrentState::Active,
    });
    registry
        .register(
            "inferlab_worker_scheduler_current",
            "Current CPU scheduler requests by bounded state",
            scheduler_current,
        )
        .map_err(|error| error.to_string())?;

    let scheduler_requests = SchedulerRequestFamily::default();
    let admitted = scheduler_requests.get_or_create_owned(&SchedulerRequestLabels {
        outcome: SchedulerRequestOutcome::Admitted,
    });
    let completed = scheduler_requests.get_or_create_owned(&SchedulerRequestLabels {
        outcome: SchedulerRequestOutcome::Completed,
    });
    let cancelled = scheduler_requests.get_or_create_owned(&SchedulerRequestLabels {
        outcome: SchedulerRequestOutcome::Cancelled,
    });
    let failed = scheduler_requests.get_or_create_owned(&SchedulerRequestLabels {
        outcome: SchedulerRequestOutcome::Failed,
    });
    registry
        .register(
            "inferlab_worker_scheduler_requests",
            "CPU scheduler requests by terminal or admission outcome",
            scheduler_requests,
        )
        .map_err(|error| error.to_string())?;

    let batches = Counter::default();
    registry
        .register(
            "inferlab_worker_scheduler_batches",
            "Continuous scheduler batches executed",
            batches.clone(),
        )
        .map_err(|error| error.to_string())?;

    let tokens = Counter::default();
    registry
        .register(
            "inferlab_worker_tokens",
            "CPU scheduler generation token steps, including terminal steps",
            tokens.clone(),
        )
        .map_err(|error| error.to_string())?;

    let batch_slots = BatchSlotFamily::default();
    let slots_used = batch_slots.get_or_create_owned(&BatchSlotLabels {
        state: BatchSlotState::Used,
    });
    let slots_available = batch_slots.get_or_create_owned(&BatchSlotLabels {
        state: BatchSlotState::Available,
    });
    registry
        .register(
            "inferlab_worker_batch_slots",
            "Continuous scheduler batch slots by used or available state",
            batch_slots,
        )
        .map_err(|error| error.to_string())?;

    let generation_family = GenerationHistogramFamily::new_with_constructor(fixed_histogram);
    let generation = GenerationHistograms {
        success: histogram(&generation_family, GenerationOutcome::Success),
        error: histogram(&generation_family, GenerationOutcome::Error),
        cancelled: histogram(&generation_family, GenerationOutcome::Cancelled),
    };
    registry
        .register_with_unit(
            "inferlab_worker_generation_duration",
            "CPU generation duration through scheduler terminal outcome",
            Unit::Seconds,
            generation_family,
        )
        .map_err(|error| error.to_string())?;

    let requests = CounterMirror::new(requests);
    let admitted = CounterMirror::new(admitted);
    let completed = CounterMirror::new(completed);
    let cancelled = CounterMirror::new(cancelled);
    let failed = CounterMirror::new(failed);
    let batches = CounterMirror::new(batches);
    let tokens = CounterMirror::new(tokens);
    let slots_used = CounterMirror::new(slots_used);
    let slots_available = CounterMirror::new(slots_available);
    let refresh = Mutex::new(());
    registry
        .set_before_render(move || {
            let _refresh = refresh
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let snapshot = scheduler.metrics_snapshot();
            scheduler_queued.set(snapshot.queued as u64);
            scheduler_active.set(snapshot.active as u64);
            requests.update(requests_source.load(Ordering::Relaxed));
            admitted.update(snapshot.admitted);
            completed.update(snapshot.completed);
            cancelled.update(snapshot.cancelled);
            failed.update(snapshot.failed);
            batches.update(snapshot.batches);
            tokens.update(snapshot.token_steps);
            slots_used.update(snapshot.slots_used);
            slots_available.update(snapshot.slots_available);
        })
        .map_err(|error| error.to_string())?;

    Ok(WorkerMetrics { generation })
}

pub(crate) struct GenerationTimer {
    histograms: GenerationHistograms,
    request_id: RequestId,
    worker_id: String,
    request_number: u64,
    mode: GenerationMode,
    started: Instant,
    finished: bool,
}

impl GenerationTimer {
    pub(crate) fn start(
        metrics: &WorkerMetrics,
        request_id: RequestId,
        worker_id: String,
        request_number: u64,
        mode: GenerationMode,
    ) -> Self {
        tracing::info!(
            service = "cpu-worker",
            event = "generation_started",
            request_id = request_id.as_str(),
            worker_id,
            request_number,
            mode = mode.as_str(),
        );
        Self {
            histograms: metrics.generation.clone(),
            request_id,
            worker_id,
            request_number,
            mode,
            started: Instant::now(),
            finished: false,
        }
    }

    pub(crate) fn success(&mut self) {
        self.finish(GenerationOutcome::Success);
    }

    pub(crate) fn error(&mut self) {
        self.finish(GenerationOutcome::Error);
    }

    fn finish(&mut self, outcome: GenerationOutcome) {
        if self.finished {
            return;
        }
        self.finished = true;
        let elapsed = self.started.elapsed();
        self.histograms.get(outcome).observe(elapsed.as_secs_f64());
        tracing::info!(
            service = "cpu-worker",
            event = "generation_terminal",
            request_id = self.request_id.as_str(),
            worker_id = self.worker_id,
            request_number = self.request_number,
            mode = self.mode.as_str(),
            outcome = outcome.as_str(),
            duration_ms = elapsed.as_secs_f64() * 1_000.0,
        );
    }
}

impl Drop for GenerationTimer {
    fn drop(&mut self) {
        if !self.finished {
            self.finish(GenerationOutcome::Cancelled);
        }
    }
}

fn histogram(family: &GenerationHistogramFamily, outcome: GenerationOutcome) -> Histogram {
    family.get_or_create_owned(&GenerationLabels { outcome })
}

fn fixed_histogram() -> Histogram {
    Histogram::new(FIXED_HISTOGRAM_BUCKETS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_theoretical_series_stay_within_the_hard_target_budget() {
        const SCALAR_AND_COUNTER_SERIES: usize = 11;
        const HISTOGRAM_SERIES: usize = 3 * (FIXED_HISTOGRAM_BUCKETS.len() + 3);
        let total = observability::Service::CpuWorker.max_http_series()
            + SCALAR_AND_COUNTER_SERIES
            + HISTOGRAM_SERIES;

        assert_eq!(total, 168);
        assert!(total <= observability::TOTAL_SERIES_BUDGET_PER_TARGET);
    }
}
