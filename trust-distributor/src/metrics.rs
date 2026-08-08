use std::{
    fmt::Write,
    sync::{Arc, Mutex, atomic::AtomicU64},
};

use observability::{
    MetricsRegistry, RegistryError,
    prometheus_client::{
        encoding::{EncodeLabelSet, EncodeLabelValue, LabelValueEncoder},
        metrics::{counter::Counter, family::Family, gauge::Gauge},
    },
};

use crate::TrustDistributor;

#[cfg(test)]
const TRUST_DOMAIN_SERIES: usize = 16;
type UnsignedGauge = Gauge<u64, AtomicU64>;

macro_rules! bounded_label_value {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Clone, Debug, Eq, Hash, PartialEq)]
        enum $name { $($variant),+ }
        impl EncodeLabelValue for $name {
            fn encode(&self, encoder: &mut LabelValueEncoder<'_>) -> std::fmt::Result {
                encoder.write_str(match self { $(Self::$variant => $value),+ })
            }
        }
    };
}

bounded_label_value!(SnapshotOutcome {
    Served => "served",
    NotModified => "not_modified",
    Unavailable => "unavailable",
});
bounded_label_value!(PublishOutcome {
    Published => "published",
    Unchanged => "unchanged",
    Rejected => "rejected",
    StorageError => "storage_error",
});
bounded_label_value!(ReceiptOutcome {
    Recorded => "recorded",
    Duplicate => "duplicate",
    Rejected => "rejected",
    StorageError => "storage_error",
});
bounded_label_value!(ReceiverState {
    Expected => "expected",
    Acked => "acked",
    Pending => "pending",
});

#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct SnapshotLabel {
    outcome: SnapshotOutcome,
}
#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct PublishLabel {
    outcome: PublishOutcome,
}
#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct ReceiptLabel {
    outcome: ReceiptOutcome,
}
#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct ReceiverLabel {
    state: ReceiverState,
}

pub struct TrustDistributorMetrics {
    distributor: TrustDistributor,
    refresh_lock: Mutex<()>,
    snapshot_requests: Family<SnapshotLabel, Counter>,
    snapshot_publish: Family<PublishLabel, Counter>,
    receipts: Family<ReceiptLabel, Counter>,
    generation: UnsignedGauge,
    receivers: Family<ReceiverLabel, UnsignedGauge>,
    storage_healthy: UnsignedGauge,
}

impl TrustDistributorMetrics {
    pub fn register(
        registry: &mut MetricsRegistry,
        distributor: TrustDistributor,
    ) -> Result<Arc<Self>, RegistryError> {
        let metrics = Arc::new(Self {
            distributor,
            refresh_lock: Mutex::new(()),
            snapshot_requests: Family::default(),
            snapshot_publish: Family::default(),
            receipts: Family::default(),
            generation: Gauge::default(),
            receivers: Family::default(),
            storage_healthy: Gauge::default(),
        });
        register(
            registry,
            "inferlab_trust_snapshot_requests",
            "Service-trust snapshot read outcomes.",
            &metrics.snapshot_requests,
        )?;
        register(
            registry,
            "inferlab_trust_snapshot_publish",
            "Service-trust snapshot publication outcomes.",
            &metrics.snapshot_publish,
        )?;
        register(
            registry,
            "inferlab_trust_receipts",
            "Service-trust receipt acceptance outcomes.",
            &metrics.receipts,
        )?;
        register(
            registry,
            "inferlab_trust_snapshot_generation",
            "Current published service-trust snapshot generation, or zero before publication.",
            &metrics.generation,
        )?;
        register(
            registry,
            "inferlab_trust_receivers",
            "Current bounded convergence receiver counts.",
            &metrics.receivers,
        )?;
        register(
            registry,
            "inferlab_trust_storage_healthy",
            "Whether distributor durable mutations are currently healthy.",
            &metrics.storage_healthy,
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
        let Some(snapshot) = self.distributor.try_metrics_snapshot() else {
            return;
        };
        sync_family(
            &self.snapshot_requests,
            SnapshotLabel {
                outcome: SnapshotOutcome::Served,
            },
            snapshot.snapshot_served,
        );
        sync_family(
            &self.snapshot_requests,
            SnapshotLabel {
                outcome: SnapshotOutcome::NotModified,
            },
            snapshot.snapshot_not_modified,
        );
        sync_family(
            &self.snapshot_requests,
            SnapshotLabel {
                outcome: SnapshotOutcome::Unavailable,
            },
            snapshot.snapshot_unavailable,
        );
        sync_family(
            &self.snapshot_publish,
            PublishLabel {
                outcome: PublishOutcome::Published,
            },
            snapshot.snapshot_published,
        );
        sync_family(
            &self.snapshot_publish,
            PublishLabel {
                outcome: PublishOutcome::Unchanged,
            },
            snapshot.snapshot_unchanged,
        );
        sync_family(
            &self.snapshot_publish,
            PublishLabel {
                outcome: PublishOutcome::Rejected,
            },
            snapshot.snapshot_rejected,
        );
        sync_family(
            &self.snapshot_publish,
            PublishLabel {
                outcome: PublishOutcome::StorageError,
            },
            snapshot.snapshot_storage_errors,
        );
        sync_family(
            &self.receipts,
            ReceiptLabel {
                outcome: ReceiptOutcome::Recorded,
            },
            snapshot.receipts_recorded,
        );
        sync_family(
            &self.receipts,
            ReceiptLabel {
                outcome: ReceiptOutcome::Duplicate,
            },
            snapshot.receipts_duplicate,
        );
        sync_family(
            &self.receipts,
            ReceiptLabel {
                outcome: ReceiptOutcome::Rejected,
            },
            snapshot.receipts_rejected,
        );
        sync_family(
            &self.receipts,
            ReceiptLabel {
                outcome: ReceiptOutcome::StorageError,
            },
            snapshot.receipt_storage_errors,
        );
        set_gauge(&self.generation, snapshot.generation);
        set_family_gauge(
            &self.receivers,
            ReceiverLabel {
                state: ReceiverState::Expected,
            },
            snapshot.expected_receivers,
        );
        set_family_gauge(
            &self.receivers,
            ReceiverLabel {
                state: ReceiverState::Acked,
            },
            snapshot.acked_receivers,
        );
        set_family_gauge(
            &self.receivers,
            ReceiverLabel {
                state: ReceiverState::Pending,
            },
            snapshot.pending_receivers,
        );
        set_gauge(&self.storage_healthy, u64::from(snapshot.storage_healthy));
    }
}

fn register<M: observability::prometheus_client::registry::Metric + Clone>(
    registry: &mut MetricsRegistry,
    name: &str,
    help: &str,
    metric: &M,
) -> Result<(), RegistryError> {
    registry.register(name, help, metric.clone())
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

fn sync_family<L>(family: &Family<L, Counter>, label: L, value: u64)
where
    L: Clone + Eq + std::hash::Hash + EncodeLabelSet + Send + Sync + 'static,
{
    sync_counter(&family.get_or_create(&label), value);
}

fn set_family_gauge<L>(family: &Family<L, UnsignedGauge>, label: L, value: u64)
where
    L: Clone + Eq + std::hash::Hash + EncodeLabelSet + Send + Sync + 'static,
{
    set_gauge(&family.get_or_create(&label), value);
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use service_auth::{ServiceSigningIdentity, TrustedServiceTrustRootKeyRing};
    use transport_security::ServerTransportStatus;

    use crate::{DEFAULT_MAX_BODY_BYTES, DistributorConfig};

    use super::*;

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn empty_distributor_metrics_are_bounded_and_identity_free() {
        let sequence = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "inferlab-trust-metrics-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create metrics directory");
        let receiver = ServiceSigningIdentity::from_base64_seed_with_credential(
            "control-secret",
            "credential-secret",
            "TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs=",
        )
        .expect("receiver");
        let roots = TrustedServiceTrustRootKeyRing::parse(
            &format!("root-secret={}", receiver.public_key_base64()),
            "",
        )
        .expect("root parser accepts an Ed25519 public key");
        let distributor = TrustDistributor::open(
            DistributorConfig {
                cluster_id: "cluster-secret".to_owned(),
                state_path: directory.join("state-secret.json"),
                expected_receivers: BTreeSet::from(["control-secret/credential-secret".to_owned()]),
                max_body_bytes: DEFAULT_MAX_BODY_BYTES,
                transport_security: ServerTransportStatus::Http,
            },
            roots,
        )
        .expect("open distributor");

        let mut registry = MetricsRegistry::new();
        TrustDistributorMetrics::register(&mut registry, distributor).expect("register metrics");
        let output = registry.render().expect("render metrics");
        assert!(output.contains("inferlab_trust_receivers{state=\"expected\"} 1"));
        assert!(output.contains("inferlab_trust_storage_healthy 1"));
        for forbidden in [
            "cluster-secret",
            "root-secret",
            "control-secret",
            "credential-secret",
            "state-secret.json",
        ] {
            assert!(!output.contains(forbidden), "leaked {forbidden}");
        }
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn trust_common_and_domain_series_fit_the_hard_target_budget() {
        assert_eq!(TRUST_DOMAIN_SERIES, 16);
        assert!(
            observability::Service::TrustDistributor.max_http_series() + TRUST_DOMAIN_SERIES <= 256
        );
    }
}
