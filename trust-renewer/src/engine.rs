use std::{
    fmt, future,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use service_auth::{
    RenewalEffectiveClock, RenewalSchedule, RenewalTemplate, RenewalTimingConfig,
    ServiceTrustRootSigningIdentity, ServiceTrustSnapshot, TrustedServiceTrustRootKeyRing,
};
use tracing::{info, warn};

use crate::{
    CommittedRenewal, DistributorSnapshot, DistributorTransport, DurableRenewalState,
    DurableStateError, DurableStateStore, PendingRenewal, PublishOutcome, RenewerConfig,
    RenewerErrorKind, RenewerPhase, RenewerStatus, SharedRenewerStatus, StateLock, StateLockError,
    TransportError, snapshot_sha256,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StepOutcome {
    ContinueImmediately,
    Sleep(Duration),
    FailedClosed,
}

pub trait WallClock: Send + Sync {
    fn now_ms(&self) -> Result<u64, ClockError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemWallClock;

impl WallClock for SystemWallClock {
    fn now_ms(&self) -> Result<u64, ClockError> {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ClockError)?;
        u64::try_from(duration.as_millis()).map_err(|_| ClockError)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockError;

pub struct RenewalEngine<T, C> {
    transport: T,
    clock: C,
    effective_clock: RenewalEffectiveClock,
    template: RenewalTemplate,
    timing: RenewalTimingConfig,
    signer: ServiceTrustRootSigningIdentity,
    roots: TrustedServiceTrustRootKeyRing,
    store: DurableStateStore,
    state: DurableRenewalState,
    status: SharedRenewerStatus,
    retry_interval: Duration,
    _writer_lock: StateLock,
}

impl<T, C> RenewalEngine<T, C>
where
    T: DistributorTransport,
    C: WallClock,
{
    pub fn open(
        config: &RenewerConfig,
        transport: T,
        clock: C,
    ) -> Result<Self, EngineBootstrapError> {
        let timing = RenewalTimingConfig::new(
            duration_ms(config.policy_lifetime)?,
            duration_ms(config.renew_before)?,
            duration_ms(config.poll_interval)?,
            duration_ms(config.retry_interval)?,
            duration_ms(config.request_timeout)?,
        )
        .map_err(|_| EngineBootstrapError::Configuration)?;
        let signer = ServiceTrustRootSigningIdentity::from_base64_seed(
            config.root_key_id.clone(),
            config.root_private_key.expose(),
        )
        .map_err(|_| EngineBootstrapError::Configuration)?;
        let template = RenewalTemplate::load(&config.template_path, &config.cluster_id)
            .map_err(|_| EngineBootstrapError::Template)?;
        let template_fingerprint = template.fingerprint().to_owned();
        let authority_fingerprint = template
            .authority_fingerprint_for_signer(&signer)
            .map_err(|_| EngineBootstrapError::Template)?;
        let roots = TrustedServiceTrustRootKeyRing::parse(
            &format!("{}={}", signer.key_id(), signer.public_key_base64()),
            "",
        )
        .map_err(|_| EngineBootstrapError::Configuration)?;
        let writer_lock = StateLock::acquire(&config.state_path).map_err(|error| match error {
            StateLockError::AlreadyRunning => EngineBootstrapError::AlreadyRunning,
            _ => EngineBootstrapError::State,
        })?;
        let store = DurableStateStore::new(config.state_path.clone());
        let state = store
            .load_or_initialize(&authority_fingerprint, &template_fingerprint)
            .map_err(|_| EngineBootstrapError::State)?;
        validate_loaded_state(&state, &template, &signer, &roots, &timing)
            .map_err(|_| EngineBootstrapError::State)?;
        let status = SharedRenewerStatus::new(status_from_state(
            &state,
            template_fingerprint,
            authority_fingerprint,
        ));
        Ok(Self {
            transport,
            clock,
            effective_clock: RenewalEffectiveClock::new(),
            template,
            timing,
            signer,
            roots,
            store,
            state,
            status,
            retry_interval: config.retry_interval,
            _writer_lock: writer_lock,
        })
    }

    #[must_use]
    pub fn status(&self) -> SharedRenewerStatus {
        self.status.clone()
    }

    #[must_use]
    pub fn durable_state(&self) -> &DurableRenewalState {
        &self.state
    }

    pub async fn run(mut self) {
        loop {
            match self.step().await {
                StepOutcome::ContinueImmediately => tokio::task::yield_now().await,
                StepOutcome::Sleep(duration) => tokio::time::sleep(duration).await,
                StepOutcome::FailedClosed => future::pending::<()>().await,
            }
        }
    }

    pub async fn step(&mut self) -> StepOutcome {
        if self.status.snapshot().phase == RenewerPhase::FailedClosed {
            return StepOutcome::FailedClosed;
        }
        let observed_now = match self.clock.now_ms() {
            Ok(now) if now > 0 => now,
            _ => return self.fail_closed(RenewerErrorKind::Clock),
        };
        let effective_now = self.effective_clock.observe(observed_now);
        self.status.update(|status| {
            status.phase = RenewerPhase::Reconciling;
            update_time_status(status, effective_now);
        });

        let remote = match self.transport.get_snapshot().await {
            Ok(remote) => remote,
            Err(error) => return self.handle_transport_error(error),
        };
        let effective_now = match self.clock.now_ms() {
            Ok(now) if now > 0 => self.effective_clock.observe(now),
            _ => return self.fail_closed(RenewerErrorKind::Clock),
        };
        self.status
            .update(|status| update_time_status(status, effective_now));
        if let Err(kind) = self.reconcile(remote.as_ref(), effective_now) {
            return self.fail_closed(kind);
        }

        if self.state.pending.is_some() {
            return self.publish_pending().await;
        }

        let Some(committed) = self.state.committed.as_ref() else {
            if let Err(kind) = self.create_pending(None, effective_now, false) {
                return self.fail_closed(kind);
            }
            return StepOutcome::ContinueImmediately;
        };
        let schedule = match self.timing.schedule(committed.expires_at_ms, effective_now) {
            Ok(schedule) => schedule,
            Err(_) => return self.fail_closed(RenewerErrorKind::Clock),
        };
        match schedule {
            RenewalSchedule::Waiting {
                deadline_ms,
                wait_ms,
            } => {
                self.status.update(|status| {
                    status.phase = RenewerPhase::Waiting;
                    status.ready = true;
                    status.renewal_deadline_ms = Some(deadline_ms);
                    status.last_error_kind = None;
                    update_time_status(status, effective_now);
                });
                StepOutcome::Sleep(Duration::from_millis(wait_ms))
            }
            RenewalSchedule::Due { deadline_ms } => {
                self.status.update(|status| {
                    status.renewal_deadline_ms = Some(deadline_ms);
                });
                if let Err(kind) =
                    self.create_pending(Some(committed.generation), effective_now, false)
                {
                    return self.fail_closed(kind);
                }
                StepOutcome::ContinueImmediately
            }
            RenewalSchedule::Late { deadline_ms, .. } => {
                self.status.update(|status| {
                    status.renewal_deadline_ms = Some(deadline_ms);
                });
                if let Err(kind) =
                    self.create_pending(Some(committed.generation), effective_now, true)
                {
                    return self.fail_closed(kind);
                }
                StepOutcome::ContinueImmediately
            }
        }
    }

    fn reconcile(
        &mut self,
        remote: Option<&DistributorSnapshot>,
        effective_now_ms: u64,
    ) -> Result<(), RenewerErrorKind> {
        let Some(remote) = remote else {
            self.status.update(|status| {
                status.distributor_generation = None;
            });
            if self.state.committed.is_some() {
                return Err(RenewerErrorKind::DistributorRollback);
            }
            return Ok(());
        };
        self.verify_remote(&remote.snapshot, effective_now_ms)?;
        let remote_generation = remote.snapshot.policy.generation;
        let remote_expiry = remote
            .snapshot
            .policy
            .expires_at_ms
            .ok_or(RenewerErrorKind::RemoteSnapshot)?;
        let remote_hash = snapshot_sha256(&remote.exact_bytes);
        self.status.update(|status| {
            status.distributor_generation = Some(remote_generation);
            status.current_expires_at_ms = Some(remote_expiry);
        });

        if let Some(pending) = self.state.pending.as_ref() {
            match remote_generation.cmp(&pending.generation) {
                std::cmp::Ordering::Equal => {
                    if remote.exact_bytes != pending.exact_bytes() {
                        return Err(RenewerErrorKind::GenerationFork);
                    }
                    let was_late = pending.late_recovery;
                    let next = self.commit_snapshot(
                        remote_generation,
                        remote.snapshot.policy.issued_at_ms,
                        remote_expiry,
                        remote_hash,
                        true,
                        was_late,
                    );
                    self.persist_state(next)?;
                    info!(
                        generation = remote_generation,
                        "reconciled automatic trust renewal"
                    );
                }
                std::cmp::Ordering::Greater => {
                    let next = self.commit_snapshot(
                        remote_generation,
                        remote.snapshot.policy.issued_at_ms,
                        remote_expiry,
                        remote_hash,
                        false,
                        false,
                    );
                    self.persist_state(next)?;
                    info!(
                        generation = remote_generation,
                        "adopted compatible trust authority floor"
                    );
                }
                std::cmp::Ordering::Less => {
                    self.reconcile_remote_floor(
                        remote_generation,
                        remote.snapshot.policy.issued_at_ms,
                        remote_expiry,
                        &remote_hash,
                    )?;
                }
            }
        } else if let Some(committed) = self.state.committed.as_ref() {
            match remote_generation.cmp(&committed.generation) {
                std::cmp::Ordering::Less => return Err(RenewerErrorKind::DistributorRollback),
                std::cmp::Ordering::Equal if remote_hash != committed.snapshot_sha256 => {
                    return Err(RenewerErrorKind::GenerationFork);
                }
                std::cmp::Ordering::Equal
                    if remote.snapshot.policy.issued_at_ms != committed.issued_at_ms
                        || remote_expiry != committed.expires_at_ms =>
                {
                    let next = self.commit_snapshot(
                        remote_generation,
                        remote.snapshot.policy.issued_at_ms,
                        remote_expiry,
                        remote_hash,
                        false,
                        false,
                    );
                    self.persist_state(next)?;
                }
                std::cmp::Ordering::Equal => {}
                std::cmp::Ordering::Greater => {
                    let next = self.commit_snapshot(
                        remote_generation,
                        remote.snapshot.policy.issued_at_ms,
                        remote_expiry,
                        remote_hash,
                        false,
                        false,
                    );
                    self.persist_state(next)?;
                    info!(
                        generation = remote_generation,
                        "adopted compatible trust authority floor"
                    );
                }
            }
        } else {
            let next = self.commit_snapshot(
                remote_generation,
                remote.snapshot.policy.issued_at_ms,
                remote_expiry,
                remote_hash,
                false,
                false,
            );
            self.persist_state(next)?;
            info!(
                generation = remote_generation,
                "adopted existing trust authority floor"
            );
        }
        self.refresh_status_from_state();
        Ok(())
    }

    fn verify_remote(
        &self,
        snapshot: &ServiceTrustSnapshot,
        effective_now_ms: u64,
    ) -> Result<(), RenewerErrorKind> {
        if snapshot.authentication.key_id != self.signer.key_id() {
            return Err(RenewerErrorKind::AuthorityMismatch);
        }
        self.roots
            .verify(snapshot)
            .map_err(|_| RenewerErrorKind::RemoteSnapshot)?;
        self.template
            .validate_snapshot_semantics(snapshot, self.signer.key_id())
            .map_err(|_| RenewerErrorKind::TemplateMismatch)?;
        validate_snapshot_lifetime(snapshot, &self.timing)
            .map_err(|_| RenewerErrorKind::RemoteSnapshot)?;
        if snapshot.policy.issued_at_ms > effective_now_ms {
            return Err(RenewerErrorKind::RemoteSnapshot);
        }
        Ok(())
    }

    fn reconcile_remote_floor(
        &mut self,
        remote_generation: u64,
        remote_issued_at_ms: u64,
        remote_expires_at_ms: u64,
        remote_hash: &str,
    ) -> Result<(), RenewerErrorKind> {
        match self.state.committed.as_ref() {
            Some(committed) if remote_generation < committed.generation => {
                Err(RenewerErrorKind::DistributorRollback)
            }
            Some(committed)
                if remote_generation == committed.generation
                    && remote_hash != committed.snapshot_sha256 =>
            {
                Err(RenewerErrorKind::GenerationFork)
            }
            Some(committed) if remote_generation == committed.generation => {
                if remote_issued_at_ms != committed.issued_at_ms
                    || remote_expires_at_ms != committed.expires_at_ms
                {
                    let mut next = self.state.clone();
                    let committed = next
                        .committed
                        .as_mut()
                        .expect("committed floor was checked");
                    committed.issued_at_ms = remote_issued_at_ms;
                    committed.expires_at_ms = remote_expires_at_ms;
                    self.persist_state(next)?;
                }
                Ok(())
            }
            None if remote_generation == 0 => Err(RenewerErrorKind::RemoteSnapshot),
            None => Err(RenewerErrorKind::GenerationFork),
            Some(_) => Err(RenewerErrorKind::GenerationFork),
        }
    }

    fn commit_snapshot(
        &self,
        generation: u64,
        issued_at_ms: u64,
        expires_at_ms: u64,
        snapshot_sha256: String,
        automatic_success: bool,
        late_recovery: bool,
    ) -> DurableRenewalState {
        let mut next = self.state.clone();
        next.committed = Some(CommittedRenewal {
            generation,
            issued_at_ms,
            expires_at_ms,
            snapshot_sha256,
        });
        next.pending = None;
        if automatic_success {
            next.counters.successful_renewals = next.counters.successful_renewals.saturating_add(1);
            if late_recovery {
                next.counters.late_recoveries = next.counters.late_recoveries.saturating_add(1);
            }
        }
        next
    }

    fn create_pending(
        &mut self,
        previous_generation: Option<u64>,
        issued_at_ms: u64,
        late_recovery: bool,
    ) -> Result<(), RenewerErrorKind> {
        let snapshot = self
            .template
            .sign_next(
                previous_generation,
                issued_at_ms,
                &self.timing,
                &self.signer,
            )
            .map_err(|_| RenewerErrorKind::Internal)?;
        let pending = PendingRenewal::from_snapshot(&snapshot, late_recovery)
            .map_err(|_| RenewerErrorKind::State)?;
        let generation = pending.generation;
        let mut next = self.state.clone();
        next.pending = Some(pending);
        self.persist_state(next)?;
        self.status.update(|status| {
            status.phase = RenewerPhase::Publishing;
            status.pending_generation = Some(generation);
            status.last_error_kind = None;
        });
        info!(generation, "durably staged signed trust renewal");
        Ok(())
    }

    async fn publish_pending(&mut self) -> StepOutcome {
        let pending = self
            .state
            .pending
            .as_ref()
            .expect("pending publication was checked")
            .clone();
        let effective_now = match self.clock.now_ms() {
            Ok(now) if now > 0 => self.effective_clock.observe(now),
            _ => return self.fail_closed(RenewerErrorKind::Clock),
        };
        if pending.issued_at_ms > effective_now || effective_now >= pending.expires_at_ms {
            return self.fail_closed(RenewerErrorKind::PendingOutsideValidity);
        }
        let mut next = self.state.clone();
        if self
            .state
            .committed
            .as_ref()
            .is_some_and(|committed| effective_now >= committed.expires_at_ms)
        {
            next.pending
                .as_mut()
                .expect("pending publication was checked")
                .late_recovery = true;
        }
        next.counters.attempts = next.counters.attempts.saturating_add(1);
        if let Err(kind) = self.persist_state(next) {
            return self.fail_closed(kind);
        }
        self.status.update(|status| {
            status.phase = RenewerPhase::Publishing;
            status.ready = status.distributor_generation.is_some();
            status.last_error_kind = None;
        });
        match self.transport.publish_snapshot(pending.exact_bytes()).await {
            Ok(PublishOutcome::Accepted | PublishOutcome::Conflict) => {
                StepOutcome::ContinueImmediately
            }
            Ok(PublishOutcome::Rejected) => self.fail_closed(RenewerErrorKind::PublicationRejected),
            Err(error) => self.handle_transport_error(error),
        }
    }

    fn handle_transport_error(&mut self, error: TransportError) -> StepOutcome {
        if !error.is_transient() {
            return self.fail_closed(RenewerErrorKind::RemoteSnapshot);
        }
        let mut next = self.state.clone();
        next.counters.transient_failures = next.counters.transient_failures.saturating_add(1);
        if let Err(kind) = self.persist_state(next) {
            return self.fail_closed(kind);
        }
        self.status.update(|status| {
            status.phase = RenewerPhase::RetryWaiting;
            status.ready = false;
            status.last_error_kind = Some(RenewerErrorKind::Transport);
        });
        warn!(
            reason = error.kind().as_str(),
            "trust renewal transport will retry"
        );
        StepOutcome::Sleep(self.retry_interval)
    }

    fn fail_closed(&mut self, requested_kind: RenewerErrorKind) -> StepOutcome {
        if requested_kind == RenewerErrorKind::StateDurabilityUncertain {
            self.refresh_status_from_state();
            self.status.update(|status| {
                status.phase = RenewerPhase::FailedClosed;
                status.ready = false;
                status.last_error_kind = Some(RenewerErrorKind::StateDurabilityUncertain);
            });
            warn!(
                reason = RenewerErrorKind::StateDurabilityUncertain.as_str(),
                "trust renewer stopped mutation after uncertain state replacement"
            );
            return StepOutcome::FailedClosed;
        }
        let mut next = self.state.clone();
        next.counters.rejected_states = next.counters.rejected_states.saturating_add(1);
        let kind = match self.store.persist(&next) {
            Ok(()) => {
                self.state = next;
                requested_kind
            }
            Err(DurableStateError::DurabilityUncertain) => {
                self.state = next;
                RenewerErrorKind::StateDurabilityUncertain
            }
            Err(_) => RenewerErrorKind::State,
        };
        self.refresh_status_from_state();
        self.status.update(|status| {
            status.phase = RenewerPhase::FailedClosed;
            status.ready = false;
            status.last_error_kind = Some(kind);
        });
        warn!(reason = kind.as_str(), "trust renewer failed closed");
        StepOutcome::FailedClosed
    }

    fn persist_state(&mut self, next: DurableRenewalState) -> Result<(), RenewerErrorKind> {
        match self.store.persist(&next) {
            Ok(()) => {
                self.state = next;
                self.refresh_status_from_state();
                Ok(())
            }
            Err(DurableStateError::DurabilityUncertain) => {
                self.state = next;
                self.refresh_status_from_state();
                Err(RenewerErrorKind::StateDurabilityUncertain)
            }
            Err(_) => Err(RenewerErrorKind::State),
        }
    }

    fn refresh_status_from_state(&self) {
        self.status.update(|status| {
            status.committed_generation = self
                .state
                .committed
                .as_ref()
                .map(|committed| committed.generation);
            status.pending_generation = self
                .state
                .pending
                .as_ref()
                .map(|pending| pending.generation);
            if let Some(committed) = self.state.committed.as_ref() {
                status.current_expires_at_ms = Some(committed.expires_at_ms);
                status.renewal_deadline_ms = self
                    .timing
                    .renewal_deadline_ms(committed.expires_at_ms)
                    .ok();
            }
            status.set_counters(&self.state.counters);
            if let Some(now) = self.effective_clock.current() {
                update_time_status(status, now);
            }
        });
    }
}

fn status_from_state(
    state: &DurableRenewalState,
    template_fingerprint: String,
    authority_fingerprint: String,
) -> RenewerStatus {
    let mut status = RenewerStatus::starting(template_fingerprint, authority_fingerprint);
    status.committed_generation = state
        .committed
        .as_ref()
        .map(|committed| committed.generation);
    status.pending_generation = state.pending.as_ref().map(|pending| pending.generation);
    status.current_expires_at_ms = state
        .committed
        .as_ref()
        .map(|committed| committed.expires_at_ms);
    status.set_counters(&state.counters);
    status
}

fn update_time_status(status: &mut RenewerStatus, effective_now_ms: u64) {
    status.remaining_margin_ms = status
        .renewal_deadline_ms
        .map(|deadline| signed_difference(deadline, effective_now_ms));
}

fn validate_loaded_state(
    state: &DurableRenewalState,
    template: &RenewalTemplate,
    signer: &ServiceTrustRootSigningIdentity,
    roots: &TrustedServiceTrustRootKeyRing,
    timing: &RenewalTimingConfig,
) -> Result<(), ()> {
    let Some(pending) = state.pending.as_ref() else {
        return Ok(());
    };
    let snapshot = pending.snapshot().map_err(|_| ())?;
    if snapshot.authentication.key_id != signer.key_id() {
        return Err(());
    }
    roots.verify(&snapshot).map_err(|_| ())?;
    template
        .validate_snapshot_semantics(&snapshot, signer.key_id())
        .map_err(|_| ())?;
    validate_snapshot_lifetime(&snapshot, timing)
}

fn validate_snapshot_lifetime(
    snapshot: &ServiceTrustSnapshot,
    timing: &RenewalTimingConfig,
) -> Result<(), ()> {
    let lifetime = snapshot
        .policy
        .expires_at_ms
        .and_then(|expiry| expiry.checked_sub(snapshot.policy.issued_at_ms))
        .ok_or(())?;
    (lifetime == timing.policy_lifetime_ms())
        .then_some(())
        .ok_or(())
}

fn signed_difference(left: u64, right: u64) -> i64 {
    let difference = i128::from(left) - i128::from(right);
    difference.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn duration_ms(duration: Duration) -> Result<u64, EngineBootstrapError> {
    u64::try_from(duration.as_millis()).map_err(|_| EngineBootstrapError::Configuration)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineBootstrapError {
    Configuration,
    Template,
    State,
    AlreadyRunning,
}

impl EngineBootstrapError {
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Configuration => "configuration",
            Self::Template => "template",
            Self::State => "state",
            Self::AlreadyRunning => "writer_already_running",
        }
    }
}

impl fmt::Display for EngineBootstrapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.kind())
    }
}

impl std::error::Error for EngineBootstrapError {}

pub type SharedRenewalEngine<T> = RenewalEngine<Arc<T>, SystemWallClock>;

impl<T> DistributorTransport for Arc<T>
where
    T: DistributorTransport + ?Sized,
{
    fn get_snapshot(&self) -> crate::TransportFuture<'_, Option<DistributorSnapshot>> {
        (**self).get_snapshot()
    }

    fn publish_snapshot<'a>(
        &'a self,
        exact_bytes: &'a [u8],
    ) -> crate::TransportFuture<'a, PublishOutcome> {
        (**self).publish_snapshot(exact_bytes)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        os::unix::fs::{DirBuilderExt, OpenOptionsExt},
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
    };

    use service_auth::{RenewalTemplate, ServiceTrustRootSigningIdentity, ServiceTrustSnapshot};

    use super::*;
    use crate::{
        DistributorSnapshot, PublishOutcome, RawConfig, TransportErrorKind, TransportFuture,
    };

    const ROOT_SEED: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    const GATEWAY_SEED: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone, Debug)]
    struct FakeClock(Arc<AtomicU64>);

    impl FakeClock {
        fn new(now_ms: u64) -> Self {
            Self(Arc::new(AtomicU64::new(now_ms)))
        }

        fn set(&self, now_ms: u64) {
            self.0.store(now_ms, Ordering::Relaxed);
        }
    }

    impl WallClock for FakeClock {
        fn now_ms(&self) -> Result<u64, ClockError> {
            Ok(self.0.load(Ordering::Relaxed))
        }
    }

    #[derive(Clone, Debug)]
    enum GetResult {
        Empty,
        Snapshot(Box<ServiceTrustSnapshot>),
        Transient,
    }

    #[derive(Clone, Debug)]
    enum PostResult {
        Accepted,
        Conflict,
        Rejected,
        Transient,
    }

    #[derive(Clone, Debug, Default)]
    struct FakeTransport {
        gets: Arc<Mutex<VecDeque<GetResult>>>,
        posts: Arc<Mutex<VecDeque<PostResult>>>,
        published: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl FakeTransport {
        fn with_gets(self, values: impl IntoIterator<Item = GetResult>) -> Self {
            self.gets.lock().unwrap().extend(values);
            self
        }

        fn with_posts(self, values: impl IntoIterator<Item = PostResult>) -> Self {
            self.posts.lock().unwrap().extend(values);
            self
        }

        fn published(&self) -> Vec<Vec<u8>> {
            self.published.lock().unwrap().clone()
        }
    }

    impl DistributorTransport for FakeTransport {
        fn get_snapshot(&self) -> TransportFuture<'_, Option<DistributorSnapshot>> {
            let result = self
                .gets
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(GetResult::Transient);
            Box::pin(async move {
                match result {
                    GetResult::Empty => Ok(None),
                    GetResult::Snapshot(snapshot) => {
                        let snapshot = *snapshot;
                        let exact_bytes = serde_json::to_vec(&snapshot).unwrap();
                        Ok(Some(DistributorSnapshot {
                            snapshot,
                            exact_bytes,
                        }))
                    }
                    GetResult::Transient => Err(TransportError::transient(
                        TransportErrorKind::RemoteUnavailable,
                    )),
                }
            })
        }

        fn publish_snapshot<'a>(
            &'a self,
            exact_bytes: &'a [u8],
        ) -> TransportFuture<'a, PublishOutcome> {
            self.published.lock().unwrap().push(exact_bytes.to_vec());
            let result = self
                .posts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(PostResult::Transient);
            Box::pin(async move {
                match result {
                    PostResult::Accepted => Ok(PublishOutcome::Accepted),
                    PostResult::Conflict => Ok(PublishOutcome::Conflict),
                    PostResult::Rejected => Ok(PublishOutcome::Rejected),
                    PostResult::Transient => {
                        Err(TransportError::transient(TransportErrorKind::RequestFailed))
                    }
                }
            })
        }
    }

    struct Fixture {
        directory: PathBuf,
        config: RenewerConfig,
        template: RenewalTemplate,
        signer: ServiceTrustRootSigningIdentity,
        timing: RenewalTimingConfig,
    }

    impl Fixture {
        fn new() -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "inferlab-trust-renewer-engine-{}-{sequence}",
                std::process::id()
            ));
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&directory)
                .expect("test directory");
            let gateway = service_auth::ServiceSigningIdentity::from_base64_seed_with_credential(
                "gateway",
                "key-a",
                GATEWAY_SEED,
            )
            .expect("gateway");
            let template_json = serde_json::json!({
                "schema": "inferlab.service-trust-renewal-template.v1",
                "cluster_id": "inferlab-primary",
                "policy_schema": "inferlab.service-trust-policy.v2",
                "trusted_credentials": [{
                    "service_id": "gateway",
                    "credential_id": "key-a",
                    "public_key_base64": gateway.public_key_base64(),
                }],
                "revoked_service_ids": [],
                "revoked_credentials": [],
                "gateway_service_ids": ["gateway"],
            });
            let template_path = directory.join("template.json");
            write_mode_0600(
                &template_path,
                &serde_json::to_vec(&template_json).expect("template JSON"),
            );
            let config = RawConfig {
                status_bind: "127.0.0.1:0".to_owned(),
                distributor_url: "https://127.0.0.1:8090".to_owned(),
                cluster_id: "inferlab-primary".to_owned(),
                template_path: template_path.to_string_lossy().into_owned(),
                state_path: directory.join("state.json").to_string_lossy().into_owned(),
                root_key_id: "root-a".to_owned(),
                root_private_key: ROOT_SEED.to_owned(),
                tls_server_ca_path: directory.join("ca.pem").to_string_lossy().into_owned(),
                tls_client_cert_path: directory.join("cert.pem").to_string_lossy().into_owned(),
                tls_client_key_path: directory.join("key.pem").to_string_lossy().into_owned(),
                policy_lifetime_ms: "1000".to_owned(),
                renew_before_ms: "500".to_owned(),
                poll_interval_ms: "25".to_owned(),
                retry_interval_ms: "100".to_owned(),
                request_timeout_ms: "100".to_owned(),
            }
            .parse()
            .expect("configuration");
            let template =
                RenewalTemplate::load(&config.template_path, &config.cluster_id).expect("template");
            let signer = ServiceTrustRootSigningIdentity::from_base64_seed("root-a", ROOT_SEED)
                .expect("signer");
            let timing = RenewalTimingConfig::new(1_000, 500, 25, 100, 100).expect("timing");
            Self {
                directory,
                config,
                template,
                signer,
                timing,
            }
        }

        fn snapshot(&self, previous: Option<u64>, issued_at_ms: u64) -> ServiceTrustSnapshot {
            self.template
                .sign_next(previous, issued_at_ms, &self.timing, &self.signer)
                .expect("snapshot")
        }

        fn snapshot_with_lifetime(
            &self,
            previous: Option<u64>,
            issued_at_ms: u64,
            lifetime_ms: u64,
        ) -> ServiceTrustSnapshot {
            let timing =
                RenewalTimingConfig::new(lifetime_ms, 400, 25, 100, 100).expect("alternate timing");
            self.template
                .sign_next(previous, issued_at_ms, &timing, &self.signer)
                .expect("snapshot")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn write_mode_0600(path: &std::path::Path, bytes: &[u8]) {
        use std::io::Write as _;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .expect("fixture file");
        file.write_all(bytes).expect("fixture contents");
    }

    #[tokio::test]
    async fn cold_start_stages_and_publishes_generation_one() {
        let fixture = Fixture::new();
        let transport = FakeTransport::default()
            .with_gets([GetResult::Empty, GetResult::Empty])
            .with_posts([PostResult::Accepted]);
        let clock = FakeClock::new(1_000);
        let mut engine =
            RenewalEngine::open(&fixture.config, transport.clone(), clock).expect("engine");
        assert_eq!(engine.step().await, StepOutcome::ContinueImmediately);
        let pending = engine.durable_state().pending.as_ref().expect("pending");
        assert_eq!(pending.generation, 1);
        let exact = pending.exact_bytes().to_vec();
        assert_eq!(engine.step().await, StepOutcome::ContinueImmediately);
        assert_eq!(transport.published(), vec![exact]);
        assert_eq!(engine.durable_state().counters.attempts, 1);
    }

    #[tokio::test]
    async fn post_rename_directory_sync_uncertainty_retains_next_state_and_stops_without_second_mutation()
     {
        let fixture = Fixture::new();
        let transport = FakeTransport::default().with_gets([GetResult::Empty]);
        let mut engine =
            RenewalEngine::open(&fixture.config, transport.clone(), FakeClock::new(1_000))
                .expect("engine");
        let before = engine.store.persistence_observation();
        engine.store.inject_parent_sync_uncertainty_once();

        assert_eq!(engine.step().await, StepOutcome::FailedClosed);

        let after = engine.store.persistence_observation();
        assert_eq!(after.persist_calls - before.persist_calls, 1);
        assert_eq!(after.replacements - before.replacements, 1);
        let retained = engine
            .durable_state()
            .pending
            .as_ref()
            .expect("post-rename pending state retained");
        assert_eq!(retained.generation, 1);
        assert_eq!(engine.durable_state().counters.rejected_states, 0);
        assert!(transport.published().is_empty());
        let status = engine.status().snapshot();
        assert_eq!(status.phase, RenewerPhase::FailedClosed);
        assert_eq!(status.pending_generation, Some(1));
        assert_eq!(
            status.last_error_kind,
            Some(RenewerErrorKind::StateDurabilityUncertain)
        );

        let replaced_state = DurableStateStore::new(fixture.config.state_path.clone())
            .load()
            .expect("renamed state remains readable");
        assert_eq!(replaced_state, *engine.durable_state());
        assert_eq!(engine.step().await, StepOutcome::FailedClosed);
        assert_eq!(engine.store.persistence_observation(), after);
    }

    #[tokio::test]
    async fn ambiguous_retry_reuses_exact_pending_bytes() {
        let fixture = Fixture::new();
        let transport = FakeTransport::default()
            .with_gets([GetResult::Empty, GetResult::Empty, GetResult::Empty])
            .with_posts([PostResult::Transient, PostResult::Accepted]);
        let mut engine =
            RenewalEngine::open(&fixture.config, transport.clone(), FakeClock::new(1_000))
                .expect("engine");
        engine.step().await;
        assert_eq!(
            engine.step().await,
            StepOutcome::Sleep(Duration::from_millis(100))
        );
        assert_eq!(engine.step().await, StepOutcome::ContinueImmediately);
        let published = transport.published();
        assert_eq!(published.len(), 2);
        assert_eq!(published[0], published[1]);
        assert_eq!(engine.durable_state().counters.transient_failures, 1);
    }

    #[tokio::test]
    async fn restart_reconciles_exact_pending_without_republishing() {
        let fixture = Fixture::new();
        let first_transport = FakeTransport::default()
            .with_gets([GetResult::Empty, GetResult::Empty])
            .with_posts([PostResult::Transient]);
        let mut first =
            RenewalEngine::open(&fixture.config, first_transport, FakeClock::new(1_000))
                .expect("first engine");
        first.step().await;
        first.step().await;
        let pending = first
            .durable_state()
            .pending
            .as_ref()
            .expect("pending")
            .snapshot()
            .expect("snapshot");
        drop(first);

        let second_transport = FakeTransport::default()
            .with_gets([GetResult::Snapshot(Box::new(pending))])
            .with_posts([PostResult::Rejected]);
        let mut second = RenewalEngine::open(
            &fixture.config,
            second_transport.clone(),
            FakeClock::new(1_100),
        )
        .expect("restarted engine");
        assert!(matches!(second.step().await, StepOutcome::Sleep(_)));
        assert!(second.durable_state().pending.is_none());
        assert_eq!(second.durable_state().counters.successful_renewals, 1);
        assert!(second_transport.published().is_empty());
    }

    #[tokio::test]
    async fn same_generation_fork_fails_closed() {
        let fixture = Fixture::new();
        let current = fixture.snapshot(None, 1_000);
        let fork = fixture.snapshot(None, 1_001);
        let transport = FakeTransport::default().with_gets([
            GetResult::Snapshot(Box::new(current)),
            GetResult::Snapshot(Box::new(fork)),
        ]);
        let mut engine =
            RenewalEngine::open(&fixture.config, transport, FakeClock::new(1_100)).expect("engine");
        assert!(matches!(engine.step().await, StepOutcome::Sleep(_)));
        assert_eq!(engine.step().await, StepOutcome::FailedClosed);
        let status = engine.status().snapshot();
        assert_eq!(status.phase, RenewerPhase::FailedClosed);
        assert_eq!(
            status.last_error_kind,
            Some(RenewerErrorKind::GenerationFork)
        );
    }

    #[tokio::test]
    async fn retry_crossing_expiry_records_late_recovery() {
        let fixture = Fixture::new();
        let g1 = fixture.snapshot(None, 1_000);
        let transport = FakeTransport::default()
            .with_gets([
                GetResult::Snapshot(Box::new(g1.clone())),
                GetResult::Snapshot(Box::new(g1.clone())),
                GetResult::Snapshot(Box::new(g1)),
            ])
            .with_posts([PostResult::Transient, PostResult::Transient]);
        let clock = FakeClock::new(1_500);
        let mut engine =
            RenewalEngine::open(&fixture.config, transport, clock.clone()).expect("engine");
        assert_eq!(engine.step().await, StepOutcome::ContinueImmediately);
        assert!(engine.durable_state().pending.is_some());
        assert!(matches!(engine.step().await, StepOutcome::Sleep(_)));
        clock.set(2_001);
        assert!(matches!(engine.step().await, StepOutcome::Sleep(_)));

        let pending = engine
            .durable_state()
            .pending
            .as_ref()
            .expect("pending")
            .snapshot()
            .expect("snapshot");
        let final_transport = FakeTransport::default()
            .with_gets([GetResult::Snapshot(Box::new(pending))])
            .with_posts([PostResult::Conflict]);
        drop(engine);
        let mut restarted =
            RenewalEngine::open(&fixture.config, final_transport, FakeClock::new(2_001))
                .expect("restarted");
        restarted.step().await;
        assert_eq!(restarted.durable_state().counters.late_recoveries, 1);
    }

    #[tokio::test]
    async fn pre_expiry_post_is_not_late_when_reconciliation_is_delayed() {
        let fixture = Fixture::new();
        let g1 = fixture.snapshot(None, 1_000);
        let transport = FakeTransport::default()
            .with_gets([
                GetResult::Snapshot(Box::new(g1.clone())),
                GetResult::Snapshot(Box::new(g1)),
            ])
            .with_posts([PostResult::Accepted]);
        let clock = FakeClock::new(1_500);
        let mut engine =
            RenewalEngine::open(&fixture.config, transport, clock.clone()).expect("engine");
        assert_eq!(engine.step().await, StepOutcome::ContinueImmediately);
        assert_eq!(engine.step().await, StepOutcome::ContinueImmediately);
        assert!(
            !engine
                .durable_state()
                .pending
                .as_ref()
                .expect("pending")
                .late_recovery
        );
        clock.set(2_001);
        let pending = engine
            .durable_state()
            .pending
            .as_ref()
            .expect("pending")
            .snapshot()
            .expect("snapshot");
        drop(engine);
        let transport =
            FakeTransport::default().with_gets([GetResult::Snapshot(Box::new(pending))]);
        let mut restarted = RenewalEngine::open(&fixture.config, transport, FakeClock::new(2_001))
            .expect("restarted");
        restarted.step().await;
        assert_eq!(restarted.durable_state().counters.late_recoveries, 0);
    }

    #[tokio::test]
    async fn post_attempt_after_expiry_persists_late_flag() {
        let fixture = Fixture::new();
        let g1 = fixture.snapshot(None, 1_000);
        let transport = FakeTransport::default()
            .with_gets([
                GetResult::Snapshot(Box::new(g1.clone())),
                GetResult::Snapshot(Box::new(g1)),
            ])
            .with_posts([PostResult::Transient]);
        let clock = FakeClock::new(1_500);
        let mut engine =
            RenewalEngine::open(&fixture.config, transport, clock.clone()).expect("engine");
        assert_eq!(engine.step().await, StepOutcome::ContinueImmediately);
        clock.set(2_001);
        assert!(matches!(engine.step().await, StepOutcome::Sleep(_)));
        assert!(
            engine
                .durable_state()
                .pending
                .as_ref()
                .expect("pending")
                .late_recovery
        );
    }

    #[tokio::test]
    async fn backward_clock_step_does_not_postpone_due_work() {
        let fixture = Fixture::new();
        let g1 = fixture.snapshot(None, 1_000);
        let transport = FakeTransport::default().with_gets([
            GetResult::Snapshot(Box::new(g1.clone())),
            GetResult::Snapshot(Box::new(g1)),
        ]);
        let clock = FakeClock::new(1_600);
        let mut engine =
            RenewalEngine::open(&fixture.config, transport, clock.clone()).expect("engine");
        assert_eq!(engine.step().await, StepOutcome::ContinueImmediately);
        let issued = engine
            .durable_state()
            .pending
            .as_ref()
            .expect("pending")
            .issued_at_ms;
        assert_eq!(issued, 1_600);
        clock.set(1_100);
        engine.step().await;
        assert_eq!(
            engine
                .durable_state()
                .pending
                .as_ref()
                .expect("pending")
                .issued_at_ms,
            1_600
        );
    }

    #[tokio::test]
    async fn deterministic_publication_rejection_latches_failure() {
        let fixture = Fixture::new();
        let transport = FakeTransport::default()
            .with_gets([GetResult::Empty, GetResult::Empty])
            .with_posts([PostResult::Rejected]);
        let mut engine =
            RenewalEngine::open(&fixture.config, transport, FakeClock::new(1_000)).expect("engine");
        engine.step().await;
        assert_eq!(engine.step().await, StepOutcome::FailedClosed);
        assert_eq!(
            engine.status().snapshot().last_error_kind,
            Some(RenewerErrorKind::PublicationRejected)
        );
    }

    #[tokio::test]
    async fn future_issued_current_fails_closed() {
        let fixture = Fixture::new();
        let future = fixture.snapshot(None, 1_001);
        let transport = FakeTransport::default().with_gets([GetResult::Snapshot(Box::new(future))]);
        let mut engine =
            RenewalEngine::open(&fixture.config, transport, FakeClock::new(1_000)).expect("engine");
        assert_eq!(engine.step().await, StepOutcome::FailedClosed);
        assert_eq!(
            engine.status().snapshot().last_error_kind,
            Some(RenewerErrorKind::RemoteSnapshot)
        );
    }

    #[tokio::test]
    async fn overlong_current_lifetime_fails_closed() {
        let fixture = Fixture::new();
        let overlong = fixture.snapshot_with_lifetime(None, 1_000, 1_100);
        let transport =
            FakeTransport::default().with_gets([GetResult::Snapshot(Box::new(overlong))]);
        let mut engine =
            RenewalEngine::open(&fixture.config, transport, FakeClock::new(1_100)).expect("engine");
        assert_eq!(engine.step().await, StepOutcome::FailedClosed);
        assert_eq!(
            engine.status().snapshot().last_error_kind,
            Some(RenewerErrorKind::RemoteSnapshot)
        );
    }

    #[tokio::test]
    async fn underlong_current_lifetime_fails_closed() {
        let fixture = Fixture::new();
        let underlong = fixture.snapshot_with_lifetime(None, 1_000, 900);
        let transport =
            FakeTransport::default().with_gets([GetResult::Snapshot(Box::new(underlong))]);
        let mut engine =
            RenewalEngine::open(&fixture.config, transport, FakeClock::new(1_100)).expect("engine");
        assert_eq!(engine.step().await, StepOutcome::FailedClosed);
        assert_eq!(
            engine.status().snapshot().last_error_kind,
            Some(RenewerErrorKind::RemoteSnapshot)
        );
    }

    #[tokio::test]
    async fn authentic_expired_current_stages_late_successor() {
        let fixture = Fixture::new();
        let expired = fixture.snapshot(None, 1_000);
        let transport =
            FakeTransport::default().with_gets([GetResult::Snapshot(Box::new(expired))]);
        let mut engine =
            RenewalEngine::open(&fixture.config, transport, FakeClock::new(2_001)).expect("engine");
        assert_eq!(engine.step().await, StepOutcome::ContinueImmediately);
        let pending = engine.durable_state().pending.as_ref().expect("pending");
        assert_eq!(pending.generation, 2);
        assert_eq!(pending.issued_at_ms, 2_001);
        assert_eq!(pending.expires_at_ms, 3_001);
        assert!(pending.late_recovery);
    }

    #[tokio::test]
    async fn expired_pending_fails_closed_without_post() {
        let fixture = Fixture::new();
        let transport = FakeTransport::default()
            .with_gets([GetResult::Empty, GetResult::Empty])
            .with_posts([PostResult::Accepted]);
        let clock = FakeClock::new(1_000);
        let mut engine =
            RenewalEngine::open(&fixture.config, transport.clone(), clock.clone()).expect("engine");
        assert_eq!(engine.step().await, StepOutcome::ContinueImmediately);
        clock.set(2_000);
        assert_eq!(engine.step().await, StepOutcome::FailedClosed);
        assert!(transport.published().is_empty());
        assert_eq!(
            engine.status().snapshot().last_error_kind,
            Some(RenewerErrorKind::PendingOutsideValidity)
        );
    }

    #[tokio::test]
    async fn same_snapshot_repairs_corrupt_committed_timestamps() {
        let fixture = Fixture::new();
        let initial = RenewalEngine::open(
            &fixture.config,
            FakeTransport::default(),
            FakeClock::new(1_100),
        )
        .expect("initial engine");
        drop(initial);
        let snapshot = fixture.snapshot(None, 1_000);
        let exact = serde_json::to_vec(&snapshot).expect("exact snapshot");
        let store = DurableStateStore::new(fixture.config.state_path.clone());
        let mut state = store.load().expect("state");
        state.committed = Some(CommittedRenewal {
            generation: 1,
            issued_at_ms: 900,
            expires_at_ms: 1_900,
            snapshot_sha256: snapshot_sha256(&exact),
        });
        store.persist(&state).expect("corrupt fixture");

        let transport =
            FakeTransport::default().with_gets([GetResult::Snapshot(Box::new(snapshot))]);
        let mut engine =
            RenewalEngine::open(&fixture.config, transport, FakeClock::new(1_100)).expect("engine");
        assert!(matches!(engine.step().await, StepOutcome::Sleep(_)));
        let repaired = engine
            .durable_state()
            .committed
            .as_ref()
            .expect("committed");
        assert_eq!(repaired.issued_at_ms, 1_000);
        assert_eq!(repaired.expires_at_ms, 2_000);
    }

    #[tokio::test]
    async fn pending_reconciliation_repairs_corrupt_committed_timestamps() {
        let fixture = Fixture::new();
        let g1 = fixture.snapshot(None, 1_000);
        let g1_exact = serde_json::to_vec(&g1).expect("g1 bytes");
        let g2 = fixture.snapshot(Some(1), 1_500);
        let initial = RenewalEngine::open(
            &fixture.config,
            FakeTransport::default(),
            FakeClock::new(1_500),
        )
        .expect("initial");
        drop(initial);
        let store = DurableStateStore::new(fixture.config.state_path.clone());
        let mut state = store.load().expect("state");
        state.committed = Some(CommittedRenewal {
            generation: 1,
            issued_at_ms: 900,
            expires_at_ms: 1_900,
            snapshot_sha256: snapshot_sha256(&g1_exact),
        });
        state.pending = Some(PendingRenewal::from_snapshot(&g2, false).expect("pending"));
        store.persist(&state).expect("fixture state");

        let transport = FakeTransport::default()
            .with_gets([GetResult::Snapshot(Box::new(g1))])
            .with_posts([PostResult::Transient]);
        let mut engine =
            RenewalEngine::open(&fixture.config, transport, FakeClock::new(1_500)).expect("engine");
        assert!(matches!(engine.step().await, StepOutcome::Sleep(_)));
        let repaired = engine
            .durable_state()
            .committed
            .as_ref()
            .expect("committed");
        assert_eq!(repaired.issued_at_ms, 1_000);
        assert_eq!(repaired.expires_at_ms, 2_000);
        assert!(engine.durable_state().pending.is_some());
    }

    #[test]
    fn bootstrap_rejects_pending_with_invalid_signature() {
        let fixture = Fixture::new();
        let transport = FakeTransport::default();
        let first = RenewalEngine::open(&fixture.config, transport.clone(), FakeClock::new(1_000))
            .expect("first");
        drop(first);
        let store = DurableStateStore::new(fixture.config.state_path.clone());
        let mut state = store.load().expect("state");
        let mut snapshot = fixture.snapshot(None, 1_000);
        snapshot.authentication.signature = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==".to_owned();
        state.pending = Some(PendingRenewal::from_snapshot(&snapshot, false).expect("pending"));
        store.persist(&state).expect("persist");
        let error = RenewalEngine::open(&fixture.config, transport, FakeClock::new(1_000))
            .err()
            .expect("invalid pending rejected");
        assert_eq!(error, EngineBootstrapError::State);
    }

    #[test]
    fn bootstrap_rejects_pending_with_wrong_lifetime() {
        let fixture = Fixture::new();
        let transport = FakeTransport::default();
        let first = RenewalEngine::open(&fixture.config, transport.clone(), FakeClock::new(1_000))
            .expect("first");
        drop(first);
        let store = DurableStateStore::new(fixture.config.state_path.clone());
        let mut state = store.load().expect("state");
        let snapshot = fixture.snapshot_with_lifetime(None, 1_000, 1_100);
        state.pending = Some(PendingRenewal::from_snapshot(&snapshot, false).expect("pending"));
        store.persist(&state).expect("persist");
        let error = RenewalEngine::open(&fixture.config, transport, FakeClock::new(1_000))
            .err()
            .expect("wrong-lifetime pending rejected");
        assert_eq!(error, EngineBootstrapError::State);
    }
}
