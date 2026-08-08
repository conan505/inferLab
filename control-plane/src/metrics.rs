use std::{
    fmt::Write,
    sync::{Arc, Mutex, atomic::AtomicU64},
};

use observability::{
    MetricsRegistry, RegistryError,
    prometheus_client::{
        encoding::{EncodeLabelSet, EncodeLabelValue, LabelValueEncoder},
        metrics::{counter::Counter, family::Family, gauge::Gauge},
        registry::Unit,
    },
};

use crate::{
    RaftError, RaftNode, ServiceAuthorizer, WriteAuthorizer,
    link_proxy::{LinkMode, LinkProxy},
    model::{RaftMetricsSnapshot, Role},
};

#[cfg(test)]
const CONTROL_DOMAIN_SERIES: usize = 33;
#[cfg(test)]
const LINK_DOMAIN_SERIES: usize = 7;
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

bounded_label_value!(ControlRole { Follower => "follower", Candidate => "candidate", Leader => "leader" });
bounded_label_value!(BinaryResult { Accepted => "accepted", Rejected => "rejected" });
bounded_label_value!(ReplicationResult { Success => "success", Failure => "failure" });
bounded_label_value!(WriteResult {
    Verified => "verified",
    Committed => "committed",
    AuthRejected => "auth_rejected",
    FreshnessRejected => "freshness_rejected",
    RevisionConflict => "revision_conflict",
});
bounded_label_value!(ServiceResult {
    Verified => "verified",
    AuthRejected => "auth_rejected",
    FreshnessRejected => "freshness_rejected",
    ReplayRejected => "replay_rejected",
    AuthorizationRejected => "authorization_rejected",
    CredentialRevoked => "credential_revoked",
    PeerAuthorized => "peer_authorized",
    GatewayAuthorized => "gateway_authorized",
});
bounded_label_value!(TrustResult { Reloaded => "reloaded", Rejected => "rejected" });
bounded_label_value!(ReceiptResult { Posted => "posted", Failed => "failed" });
bounded_label_value!(LinkModeValue { Allow => "allow", Drop => "drop" });
bounded_label_value!(LinkRequestOutcome {
    Forwarded => "forwarded",
    Dropped => "dropped",
    UpstreamFailure => "upstream_failure",
});

#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct RoleLabel {
    role: ControlRole,
}
#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct ResultLabel {
    result: BinaryResult,
}
#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct ReplicationLabel {
    result: ReplicationResult,
}
#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct WriteLabel {
    result: WriteResult,
}
#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct ServiceLabel {
    result: ServiceResult,
}
#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct TrustLabel {
    result: TrustResult,
}
#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct ReceiptLabel {
    result: ReceiptResult,
}
#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct LinkModeLabel {
    mode: LinkModeValue,
}
#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct LinkOutcomeLabel {
    outcome: LinkRequestOutcome,
}

pub struct ControlMetrics {
    node: Arc<RaftNode>,
    writer: Arc<WriteAuthorizer>,
    service: Arc<ServiceAuthorizer>,
    refresh_lock: Mutex<()>,
    role: Family<RoleLabel, UnsignedGauge>,
    term: UnsignedGauge,
    commit_index: UnsignedGauge,
    last_applied: UnsignedGauge,
    last_log_index: UnsignedGauge,
    storage_healthy: UnsignedGauge,
    elections: Counter,
    leadership_terms: Counter,
    votes_granted: Counter,
    append_entries: Family<ResultLabel, Counter>,
    replication: Family<ReplicationLabel, Counter>,
    writes: Family<WriteLabel, Counter>,
    service_authentication: Family<ServiceLabel, Counter>,
    trust_policy: Family<TrustLabel, Counter>,
    trust_fetch_consecutive_failures: UnsignedGauge,
    trust_receipts: Family<ReceiptLabel, Counter>,
}

impl ControlMetrics {
    pub fn register(
        registry: &mut MetricsRegistry,
        node: Arc<RaftNode>,
        writer: Arc<WriteAuthorizer>,
        service: Arc<ServiceAuthorizer>,
    ) -> Result<Arc<Self>, RegistryError> {
        let metrics = Arc::new(Self {
            node,
            writer,
            service,
            refresh_lock: Mutex::new(()),
            role: Family::default(),
            term: Gauge::default(),
            commit_index: Gauge::default(),
            last_applied: Gauge::default(),
            last_log_index: Gauge::default(),
            storage_healthy: Gauge::default(),
            elections: Counter::default(),
            leadership_terms: Counter::default(),
            votes_granted: Counter::default(),
            append_entries: Family::default(),
            replication: Family::default(),
            writes: Family::default(),
            service_authentication: Family::default(),
            trust_policy: Family::default(),
            trust_fetch_consecutive_failures: Gauge::default(),
            trust_receipts: Family::default(),
        });
        register(
            registry,
            "inferlab_control_role",
            "Current Raft role as a bounded one-hot gauge.",
            &metrics.role,
        )?;
        register(
            registry,
            "inferlab_control_term",
            "Current durable Raft term.",
            &metrics.term,
        )?;
        register(
            registry,
            "inferlab_control_commit_index",
            "Current committed Raft log index.",
            &metrics.commit_index,
        )?;
        register(
            registry,
            "inferlab_control_last_applied",
            "Current last applied Raft log index.",
            &metrics.last_applied,
        )?;
        register(
            registry,
            "inferlab_control_last_log_index",
            "Current last durable Raft log index.",
            &metrics.last_log_index,
        )?;
        register(
            registry,
            "inferlab_control_storage_healthy",
            "Whether Raft storage is currently healthy.",
            &metrics.storage_healthy,
        )?;
        register(
            registry,
            "inferlab_control_elections",
            "Raft elections started.",
            &metrics.elections,
        )?;
        register(
            registry,
            "inferlab_control_leadership_terms",
            "Raft leadership terms entered.",
            &metrics.leadership_terms,
        )?;
        register(
            registry,
            "inferlab_control_votes_granted",
            "Raft votes granted.",
            &metrics.votes_granted,
        )?;
        register(
            registry,
            "inferlab_control_append_entries",
            "AppendEntries RPC outcomes.",
            &metrics.append_entries,
        )?;
        register(
            registry,
            "inferlab_control_replication",
            "Leader replication outcomes.",
            &metrics.replication,
        )?;
        register(
            registry,
            "inferlab_control_write_authorization",
            "Control write authorization outcomes.",
            &metrics.writes,
        )?;
        register(
            registry,
            "inferlab_control_service_authentication",
            "Service request authentication and authorization outcomes.",
            &metrics.service_authentication,
        )?;
        register(
            registry,
            "inferlab_control_trust_policy",
            "Signed service-trust policy reload outcomes.",
            &metrics.trust_policy,
        )?;
        register(
            registry,
            "inferlab_control_trust_fetch_consecutive_failures",
            "Current consecutive remote service-trust fetch failures.",
            &metrics.trust_fetch_consecutive_failures,
        )?;
        register(
            registry,
            "inferlab_control_trust_receipts",
            "Service-trust convergence receipt posting outcomes.",
            &metrics.trust_receipts,
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
        let Some(raft) =
            raft_snapshot_or_mark_unhealthy(self.node.metrics_snapshot(), &self.storage_healthy)
        else {
            return;
        };
        let writer = self.writer.metrics_snapshot();
        let service = self.service.metrics_snapshot();

        for (role, value) in [
            (
                ControlRole::Follower,
                u64::from(raft.role == Role::Follower),
            ),
            (
                ControlRole::Candidate,
                u64::from(raft.role == Role::Candidate),
            ),
            (ControlRole::Leader, u64::from(raft.role == Role::Leader)),
        ] {
            set_gauge(&self.role.get_or_create(&RoleLabel { role }), value);
        }
        set_gauge(&self.term, raft.term);
        set_gauge(&self.commit_index, raft.commit_index);
        set_gauge(&self.last_applied, raft.last_applied);
        set_gauge(&self.last_log_index, raft.last_log_index);
        set_gauge(&self.storage_healthy, u64::from(raft.storage_healthy));
        sync_counter(&self.elections, raft.elections_started);
        sync_counter(&self.leadership_terms, raft.leadership_terms);
        sync_counter(&self.votes_granted, raft.votes_granted);
        sync_family(
            &self.append_entries,
            ResultLabel {
                result: BinaryResult::Accepted,
            },
            raft.append_entries_accepted,
        );
        sync_family(
            &self.append_entries,
            ResultLabel {
                result: BinaryResult::Rejected,
            },
            raft.append_entries_rejected,
        );
        sync_family(
            &self.replication,
            ReplicationLabel {
                result: ReplicationResult::Success,
            },
            raft.replication_successes,
        );
        sync_family(
            &self.replication,
            ReplicationLabel {
                result: ReplicationResult::Failure,
            },
            raft.replication_failures,
        );
        sync_family(
            &self.writes,
            WriteLabel {
                result: WriteResult::Verified,
            },
            writer.verified_intents,
        );
        sync_family(
            &self.writes,
            WriteLabel {
                result: WriteResult::Committed,
            },
            writer.committed_writes,
        );
        sync_family(
            &self.writes,
            WriteLabel {
                result: WriteResult::AuthRejected,
            },
            writer.authentication_rejections,
        );
        sync_family(
            &self.writes,
            WriteLabel {
                result: WriteResult::FreshnessRejected,
            },
            writer.freshness_rejections,
        );
        sync_family(
            &self.writes,
            WriteLabel {
                result: WriteResult::RevisionConflict,
            },
            writer.revision_conflicts,
        );
        sync_family(
            &self.service_authentication,
            ServiceLabel {
                result: ServiceResult::Verified,
            },
            service.verifications,
        );
        sync_family(
            &self.service_authentication,
            ServiceLabel {
                result: ServiceResult::AuthRejected,
            },
            service.authentication_rejections,
        );
        sync_family(
            &self.service_authentication,
            ServiceLabel {
                result: ServiceResult::FreshnessRejected,
            },
            service.freshness_rejections,
        );
        sync_family(
            &self.service_authentication,
            ServiceLabel {
                result: ServiceResult::ReplayRejected,
            },
            service.replay_rejections,
        );
        sync_family(
            &self.service_authentication,
            ServiceLabel {
                result: ServiceResult::AuthorizationRejected,
            },
            service.authorization_rejections,
        );
        sync_family(
            &self.service_authentication,
            ServiceLabel {
                result: ServiceResult::CredentialRevoked,
            },
            service.credential_revocation_rejections,
        );
        sync_family(
            &self.service_authentication,
            ServiceLabel {
                result: ServiceResult::PeerAuthorized,
            },
            service.authorized_peer_rpcs,
        );
        sync_family(
            &self.service_authentication,
            ServiceLabel {
                result: ServiceResult::GatewayAuthorized,
            },
            service.authorized_gateway_reads,
        );
        sync_family(
            &self.trust_policy,
            TrustLabel {
                result: TrustResult::Reloaded,
            },
            service.trust_policy_reloads,
        );
        sync_family(
            &self.trust_policy,
            TrustLabel {
                result: TrustResult::Rejected,
            },
            service.trust_policy_rejections,
        );
        set_gauge(
            &self.trust_fetch_consecutive_failures,
            service.trust_policy_consecutive_fetch_failures,
        );
        sync_family(
            &self.trust_receipts,
            ReceiptLabel {
                result: ReceiptResult::Posted,
            },
            service.trust_policy_receipts_posted,
        );
        sync_family(
            &self.trust_receipts,
            ReceiptLabel {
                result: ReceiptResult::Failed,
            },
            service.trust_policy_receipt_failures,
        );
    }
}

fn raft_snapshot_or_mark_unhealthy(
    result: Result<RaftMetricsSnapshot, RaftError>,
    storage_healthy: &UnsignedGauge,
) -> Option<RaftMetricsSnapshot> {
    match result {
        Ok(snapshot) => Some(snapshot),
        Err(_) => {
            // Preserve last-known informational gauges, but never preserve a
            // healthy claim when the Raft mutex itself can no longer be read.
            set_gauge(storage_healthy, 0);
            None
        }
    }
}

pub struct LinkMetrics {
    proxy: Arc<LinkProxy>,
    refresh_lock: Mutex<()>,
    mode: Family<LinkModeLabel, UnsignedGauge>,
    mode_changes: Counter,
    requests: Family<LinkOutcomeLabel, Counter>,
    last_transition_timestamp_seconds: UnsignedGauge,
}

impl LinkMetrics {
    pub fn register(
        registry: &mut MetricsRegistry,
        proxy: Arc<LinkProxy>,
    ) -> Result<Arc<Self>, RegistryError> {
        let metrics = Arc::new(Self {
            proxy,
            refresh_lock: Mutex::new(()),
            mode: Family::default(),
            mode_changes: Counter::default(),
            requests: Family::default(),
            last_transition_timestamp_seconds: Gauge::default(),
        });
        register(
            registry,
            "inferlab_raft_link_mode",
            "Current directed Raft link mode as a bounded one-hot gauge.",
            &metrics.mode,
        )?;
        register(
            registry,
            "inferlab_raft_link_mode_changes",
            "Directed Raft link mode changes.",
            &metrics.mode_changes,
        )?;
        register(
            registry,
            "inferlab_raft_link_requests",
            "Directed Raft link request outcomes.",
            &metrics.requests,
        )?;
        registry.register_with_unit(
            "inferlab_raft_link_last_transition_timestamp",
            "Unix timestamp in seconds of the last directed link transition.",
            Unit::Seconds,
            metrics.last_transition_timestamp_seconds.clone(),
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
        let Ok(snapshot) = self.proxy.metrics_snapshot() else {
            return;
        };
        for (mode, value) in [
            (
                LinkModeValue::Allow,
                u64::from(snapshot.mode == LinkMode::Allow),
            ),
            (
                LinkModeValue::Drop,
                u64::from(snapshot.mode == LinkMode::Drop),
            ),
        ] {
            set_gauge(&self.mode.get_or_create(&LinkModeLabel { mode }), value);
        }
        sync_counter(&self.mode_changes, snapshot.mode_changes);
        sync_family(
            &self.requests,
            LinkOutcomeLabel {
                outcome: LinkRequestOutcome::Forwarded,
            },
            snapshot.forwarded_requests,
        );
        sync_family(
            &self.requests,
            LinkOutcomeLabel {
                outcome: LinkRequestOutcome::Dropped,
            },
            snapshot.dropped_requests,
        );
        sync_family(
            &self.requests,
            LinkOutcomeLabel {
                outcome: LinkRequestOutcome::UpstreamFailure,
            },
            snapshot.upstream_failures,
        );
        set_gauge(
            &self.last_transition_timestamp_seconds,
            snapshot.last_transition_at_ms / 1_000,
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_raft_snapshot_clears_the_storage_health_claim() {
        let storage_healthy = UnsignedGauge::default();
        storage_healthy.set(1);
        let snapshot = raft_snapshot_or_mark_unhealthy(
            Err(RaftError::Storage("injected poisoned state".to_owned())),
            &storage_healthy,
        );
        assert!(snapshot.is_none());
        assert_eq!(storage_healthy.get(), 0);
    }

    #[test]
    fn control_and_link_series_fit_the_hard_target_budget() {
        assert_eq!(CONTROL_DOMAIN_SERIES, 33);
        assert_eq!(LINK_DOMAIN_SERIES, 7);
        assert!(
            observability::Service::ControlPlane.max_http_series() + CONTROL_DOMAIN_SERIES <= 256
        );
        assert!(
            observability::Service::RaftLinkProxy.max_http_series() + LINK_DOMAIN_SERIES <= 256
        );
    }
}
