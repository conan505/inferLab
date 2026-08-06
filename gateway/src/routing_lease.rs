use std::{
    fmt,
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::Serialize;

pub type SharedRoutingLease = Arc<RoutingLeaseGuard>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoutingLeaseExpiryAction {
    RejectNew,
    ServeStale,
}

impl fmt::Display for RoutingLeaseExpiryAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RejectNew => formatter.write_str("reject-new"),
            Self::ServeStale => formatter.write_str("serve-stale"),
        }
    }
}

impl FromStr for RoutingLeaseExpiryAction {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "reject-new" => Ok(Self::RejectNew),
            "serve-stale" => Ok(Self::ServeStale),
            _ => Err(format!(
                "unsupported routing lease expiry action '{value}'; expected reject-new or serve-stale"
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoutingLeaseAdmission {
    Fresh,
    Stale,
    Rejected,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RoutingLeaseSnapshot {
    pub enabled: bool,
    pub duration_ms: u64,
    pub expiry_action: RoutingLeaseExpiryAction,
    pub state: &'static str,
    pub accepting_new_requests: bool,
    pub last_verified_ms: u64,
    pub expires_at_ms: u64,
    pub remaining_ms: u64,
    pub renewals: u64,
    pub rejections: u64,
}

#[derive(Debug)]
struct RoutingLeaseState {
    deadline: Instant,
    last_verified_ms: u64,
    expires_at_ms: u64,
    renewals: u64,
    rejections: u64,
}

#[derive(Debug)]
pub struct RoutingLeaseGuard {
    duration: Duration,
    duration_ms: u64,
    expiry_action: RoutingLeaseExpiryAction,
    state: Mutex<RoutingLeaseState>,
}

impl RoutingLeaseGuard {
    pub fn from_live(
        duration: Duration,
        expiry_action: RoutingLeaseExpiryAction,
        verified_at_ms: u64,
    ) -> Self {
        Self::from_verified_age_at(
            duration,
            expiry_action,
            verified_at_ms,
            Duration::ZERO,
            Instant::now(),
        )
    }

    pub fn from_disk(
        duration: Duration,
        expiry_action: RoutingLeaseExpiryAction,
        persisted_at_ms: u64,
        observed_age: Duration,
    ) -> Self {
        Self::from_verified_age_at(
            duration,
            expiry_action,
            persisted_at_ms,
            observed_age,
            Instant::now(),
        )
    }

    fn from_verified_age_at(
        duration: Duration,
        expiry_action: RoutingLeaseExpiryAction,
        last_verified_ms: u64,
        observed_age: Duration,
        now: Instant,
    ) -> Self {
        assert!(
            !duration.is_zero(),
            "routing lease duration must be positive"
        );
        let duration_ms = duration_millis(duration);
        let remaining = duration.saturating_sub(observed_age);
        Self {
            duration,
            duration_ms,
            expiry_action,
            state: Mutex::new(RoutingLeaseState {
                deadline: now + remaining,
                last_verified_ms,
                expires_at_ms: last_verified_ms.saturating_add(duration_ms),
                renewals: 0,
                rejections: 0,
            }),
        }
    }

    pub fn renew(&self, verified_at_ms: u64) {
        self.renew_at(verified_at_ms, Instant::now());
    }

    fn renew_at(&self, verified_at_ms: u64, now: Instant) {
        let mut state = self.lock_state();
        state.deadline = now + self.duration;
        state.last_verified_ms = verified_at_ms;
        state.expires_at_ms = verified_at_ms.saturating_add(self.duration_ms);
        state.renewals = state.renewals.saturating_add(1);
    }

    pub fn admit_new(&self) -> RoutingLeaseAdmission {
        self.admit_new_at(Instant::now())
    }

    fn admit_new_at(&self, now: Instant) -> RoutingLeaseAdmission {
        let mut state = self.lock_state();
        if now < state.deadline {
            return RoutingLeaseAdmission::Fresh;
        }
        match self.expiry_action {
            RoutingLeaseExpiryAction::ServeStale => RoutingLeaseAdmission::Stale,
            RoutingLeaseExpiryAction::RejectNew => {
                state.rejections = state.rejections.saturating_add(1);
                RoutingLeaseAdmission::Rejected
            }
        }
    }

    pub fn accepting_new_requests(&self) -> bool {
        self.accepting_new_requests_at(Instant::now())
    }

    fn accepting_new_requests_at(&self, now: Instant) -> bool {
        let state = self.lock_state();
        now < state.deadline || self.expiry_action == RoutingLeaseExpiryAction::ServeStale
    }

    pub fn snapshot(&self) -> RoutingLeaseSnapshot {
        self.snapshot_at(Instant::now())
    }

    fn snapshot_at(&self, now: Instant) -> RoutingLeaseSnapshot {
        let state = self.lock_state();
        let fresh = now < state.deadline;
        let accepting_new_requests =
            fresh || self.expiry_action == RoutingLeaseExpiryAction::ServeStale;
        let lease_state = if fresh {
            "fresh"
        } else if accepting_new_requests {
            "expired-serving-stale"
        } else {
            "expired-rejecting-new"
        };
        RoutingLeaseSnapshot {
            enabled: true,
            duration_ms: self.duration_ms,
            expiry_action: self.expiry_action,
            state: lease_state,
            accepting_new_requests,
            last_verified_ms: state.last_verified_ms,
            expires_at_ms: state.expires_at_ms,
            remaining_ms: state
                .deadline
                .checked_duration_since(now)
                .map(duration_millis)
                .unwrap_or(0),
            renewals: state.renewals,
            rejections: state.rejections,
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, RoutingLeaseState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{RoutingLeaseAdmission, RoutingLeaseExpiryAction, RoutingLeaseGuard};
    use std::time::{Duration, Instant};

    #[test]
    fn reject_new_expires_without_interrupting_an_already_admitted_request() {
        let start = Instant::now();
        let guard = RoutingLeaseGuard::from_verified_age_at(
            Duration::from_secs(10),
            RoutingLeaseExpiryAction::RejectNew,
            1_000,
            Duration::ZERO,
            start,
        );

        assert_eq!(
            guard.admit_new_at(start + Duration::from_secs(9)),
            RoutingLeaseAdmission::Fresh
        );
        assert_eq!(
            guard.admit_new_at(start + Duration::from_secs(10)),
            RoutingLeaseAdmission::Rejected
        );
        assert_eq!(
            guard
                .snapshot_at(start + Duration::from_secs(10))
                .rejections,
            1
        );
    }

    #[test]
    fn disk_bootstrap_spends_age_from_the_runtime_lease() {
        let start = Instant::now();
        let guard = RoutingLeaseGuard::from_verified_age_at(
            Duration::from_secs(10),
            RoutingLeaseExpiryAction::RejectNew,
            5_000,
            Duration::from_secs(8),
            start,
        );

        assert_eq!(
            guard.admit_new_at(start + Duration::from_secs(1)),
            RoutingLeaseAdmission::Fresh
        );
        assert_eq!(
            guard.admit_new_at(start + Duration::from_secs(2)),
            RoutingLeaseAdmission::Rejected
        );
        assert_eq!(guard.snapshot_at(start).expires_at_ms, 15_000);
    }

    #[test]
    fn valid_live_observation_renews_an_expired_lease() {
        let start = Instant::now();
        let guard = RoutingLeaseGuard::from_verified_age_at(
            Duration::from_secs(10),
            RoutingLeaseExpiryAction::RejectNew,
            1_000,
            Duration::ZERO,
            start,
        );
        assert!(!guard.accepting_new_requests_at(start + Duration::from_secs(10)));

        guard.renew_at(11_000, start + Duration::from_secs(11));

        assert!(guard.accepting_new_requests_at(start + Duration::from_secs(20)));
        let snapshot = guard.snapshot_at(start + Duration::from_secs(20));
        assert_eq!(snapshot.state, "fresh");
        assert_eq!(snapshot.renewals, 1);
        assert_eq!(snapshot.expires_at_ms, 21_000);
    }

    #[test]
    fn serve_stale_keeps_readiness_and_admission_open_after_expiry() {
        let start = Instant::now();
        let guard = RoutingLeaseGuard::from_verified_age_at(
            Duration::from_secs(10),
            RoutingLeaseExpiryAction::ServeStale,
            1_000,
            Duration::ZERO,
            start,
        );

        assert!(guard.accepting_new_requests_at(start + Duration::from_secs(10)));
        assert_eq!(
            guard.admit_new_at(start + Duration::from_secs(10)),
            RoutingLeaseAdmission::Stale
        );
        let snapshot = guard.snapshot_at(start + Duration::from_secs(10));
        assert_eq!(snapshot.state, "expired-serving-stale");
        assert_eq!(snapshot.rejections, 0);
    }
}
