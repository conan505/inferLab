use std::sync::{Arc, Mutex, atomic::AtomicU64};

use observability::{
    MetricsRegistry, RegistryError,
    prometheus_client::{
        encoding::{EncodeLabelSet, EncodeLabelValue, LabelValueEncoder},
        metrics::{counter::Counter, family::Family, gauge::Gauge},
        registry::Unit,
    },
};

use crate::QueueStore;

#[cfg(test)]
const QUEUE_DOMAIN_SERIES: usize = 12;
type UnsignedGauge = Gauge<u64, AtomicU64>;

#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct JobStateLabel {
    state: JobState,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum JobState {
    Pending,
    Claimed,
    Completed,
    DeadLetter,
}

impl EncodeLabelValue for JobState {
    fn encode(&self, encoder: &mut LabelValueEncoder<'_>) -> std::fmt::Result {
        use std::fmt::Write;
        encoder.write_str(match self {
            Self::Pending => "pending",
            Self::Claimed => "claimed",
            Self::Completed => "completed",
            Self::DeadLetter => "dead_letter",
        })
    }
}

#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct FailureKindLabel {
    kind: FailureKind,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum FailureKind {
    Explicit,
    DeadLettered,
    TornTail,
}

impl EncodeLabelValue for FailureKind {
    fn encode(&self, encoder: &mut LabelValueEncoder<'_>) -> std::fmt::Result {
        use std::fmt::Write;
        encoder.write_str(match self {
            Self::Explicit => "explicit",
            Self::DeadLettered => "dead_lettered",
            Self::TornTail => "torn_tail",
        })
    }
}

/// A bounded projection of queue state. All label values are compile-time
/// enums, and refresh reads the store's O(1) scalar snapshot only.
pub struct QueueMetrics {
    store: Arc<QueueStore>,
    refresh_lock: Mutex<()>,
    jobs: Family<JobStateLabel, UnsignedGauge>,
    wal_bytes: UnsignedGauge,
    wal_events: Counter,
    claims: Counter,
    acknowledgments: Counter,
    redeliveries: Counter,
    failures: Family<FailureKindLabel, Counter>,
}

impl QueueMetrics {
    pub fn register(
        registry: &mut MetricsRegistry,
        store: Arc<QueueStore>,
    ) -> Result<Arc<Self>, RegistryError> {
        let metrics = Arc::new(Self {
            store,
            refresh_lock: Mutex::new(()),
            jobs: Family::default(),
            wal_bytes: Gauge::default(),
            wal_events: Counter::default(),
            claims: Counter::default(),
            acknowledgments: Counter::default(),
            redeliveries: Counter::default(),
            failures: Family::default(),
        });
        registry.register(
            "inferlab_queue_jobs",
            "Current durable queue jobs by lifecycle state.",
            metrics.jobs.clone(),
        )?;
        registry.register_with_unit(
            "inferlab_queue_wal",
            "Current durable queue write-ahead log size in bytes.",
            Unit::Bytes,
            metrics.wal_bytes.clone(),
        )?;
        registry.register(
            "inferlab_queue_wal_events",
            "Durable queue write-ahead log events written.",
            metrics.wal_events.clone(),
        )?;
        registry.register(
            "inferlab_queue_claims",
            "Durable queue claims granted.",
            metrics.claims.clone(),
        )?;
        registry.register(
            "inferlab_queue_acknowledgments",
            "Durable queue jobs acknowledged.",
            metrics.acknowledgments.clone(),
        )?;
        registry.register(
            "inferlab_queue_redeliveries",
            "Durable queue claims issued after a prior attempt.",
            metrics.redeliveries.clone(),
        )?;
        registry.register(
            "inferlab_queue_failures",
            "Durable queue failure transitions by bounded kind.",
            metrics.failures.clone(),
        )?;

        let refresh = Arc::clone(&metrics);
        registry.set_before_render(move || refresh.refresh())?;
        metrics.refresh();
        Ok(metrics)
    }

    pub fn refresh(&self) {
        let Ok(_guard) = self.refresh_lock.lock() else {
            return;
        };
        let Ok(snapshot) = self.store.metrics_snapshot() else {
            return;
        };
        set_gauge(
            &self.jobs.get_or_create(&JobStateLabel {
                state: JobState::Pending,
            }),
            snapshot.pending,
        );
        set_gauge(
            &self.jobs.get_or_create(&JobStateLabel {
                state: JobState::Claimed,
            }),
            snapshot.claimed,
        );
        set_gauge(
            &self.jobs.get_or_create(&JobStateLabel {
                state: JobState::Completed,
            }),
            snapshot.completed,
        );
        set_gauge(
            &self.jobs.get_or_create(&JobStateLabel {
                state: JobState::DeadLetter,
            }),
            snapshot.dead_letter,
        );
        set_gauge(&self.wal_bytes, snapshot.wal_bytes);
        sync_counter(&self.wal_events, snapshot.wal_events);
        sync_counter(&self.claims, snapshot.claims_total);
        sync_counter(&self.acknowledgments, snapshot.acknowledgments_total);
        sync_counter(&self.redeliveries, snapshot.redeliveries_total);
        sync_counter(
            &self.failures.get_or_create(&FailureKindLabel {
                kind: FailureKind::Explicit,
            }),
            snapshot.explicit_failures_total,
        );
        sync_counter(
            &self.failures.get_or_create(&FailureKindLabel {
                kind: FailureKind::DeadLettered,
            }),
            snapshot.dead_lettered_total,
        );
        sync_counter(
            &self.failures.get_or_create(&FailureKindLabel {
                kind: FailureKind::TornTail,
            }),
            snapshot.torn_tail_records_discarded,
        );
    }
}

fn set_gauge(gauge: &UnsignedGauge, value: u64) {
    gauge.set(value);
}

fn sync_counter(counter: &Counter, value: u64) {
    let current = counter.get();
    if value > current {
        counter.inc_by(value - current);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use serde_json::json;

    use crate::model::EnqueueRequest;

    use super::*;

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn renders_only_bounded_queue_state_without_payload_or_path() {
        let sequence = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "inferlab-queue-metrics-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create metrics directory");
        let wal_path = directory.join("secret-name.wal");
        let store = QueueStore::open(&wal_path).expect("open queue");
        store
            .enqueue(
                EnqueueRequest {
                    idempotency_key: "private-job-key".to_owned(),
                    payload: json!({"prompt": "private prompt"}),
                    max_attempts: 2,
                },
                100,
            )
            .expect("enqueue");

        let mut registry = MetricsRegistry::new();
        QueueMetrics::register(&mut registry, store).expect("register metrics");
        let output = registry.render().expect("render metrics");
        assert!(output.contains("inferlab_queue_jobs{state=\"pending\"} 1"));
        assert!(output.contains("inferlab_queue_wal_events_total 1"));
        for forbidden in ["private-job-key", "private prompt", "secret-name.wal"] {
            assert!(!output.contains(forbidden), "leaked {forbidden}");
        }

        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn queue_common_and_domain_series_fit_the_hard_target_budget() {
        assert_eq!(QUEUE_DOMAIN_SERIES, 12);
        assert!(observability::Service::BatchQueue.max_http_series() + QUEUE_DOMAIN_SERIES <= 256);
    }
}
