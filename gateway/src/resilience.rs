use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

#[derive(Clone, Copy, Debug)]
pub struct ResilienceConfig {
    pub request_deadline: Duration,
    pub attempt_timeout: Duration,
    pub max_retries: usize,
    pub retry_budget_percent: u64,
    pub retry_base_delay: Duration,
    pub retry_max_delay: Duration,
    pub jitter_seed: u64,
}

impl Default for ResilienceConfig {
    fn default() -> Self {
        Self {
            request_deadline: Duration::from_secs(30),
            attempt_timeout: Duration::from_secs(5),
            max_retries: 2,
            retry_budget_percent: 10,
            retry_base_delay: Duration::from_millis(25),
            retry_max_delay: Duration::from_millis(500),
            jitter_seed: default_jitter_seed(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ResilienceController {
    config: ResilienceConfig,
    jitter: FullJitter,
    original_requests: AtomicU64,
    attempts: AtomicU64,
    transient_failures: AtomicU64,
    retry_slots_used: AtomicU64,
    retries_granted: AtomicU64,
    retries_denied_budget: AtomicU64,
    retry_limit_exhausted: AtomicU64,
    deadline_exceeded: AtomicU64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RequestContext {
    request_number: u64,
    started_at: Instant,
    deadline: Instant,
}

#[derive(Debug)]
pub(crate) struct RetryReservation {
    controller: Arc<ResilienceController>,
    committed: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ResilienceSnapshot {
    pub request_deadline_ms: u64,
    pub attempt_timeout_ms: u64,
    pub max_retries: usize,
    pub retry_budget_percent: u64,
    pub retry_base_delay_ms: u64,
    pub retry_max_delay_ms: u64,
    pub original_requests: u64,
    pub attempts: u64,
    pub transient_failures: u64,
    pub retries_granted: u64,
    pub retries_denied_budget: u64,
    pub retry_limit_exhausted: u64,
    pub deadline_exceeded: u64,
}

#[derive(Debug)]
pub struct FullJitter {
    seed: u64,
    sequence: AtomicU64,
}

impl ResilienceController {
    pub(crate) fn new(config: ResilienceConfig) -> Result<Arc<Self>, String> {
        validate_config(config)?;
        Ok(Arc::new(Self {
            jitter: FullJitter::with_seed(config.jitter_seed),
            config,
            original_requests: AtomicU64::new(0),
            attempts: AtomicU64::new(0),
            transient_failures: AtomicU64::new(0),
            retry_slots_used: AtomicU64::new(0),
            retries_granted: AtomicU64::new(0),
            retries_denied_budget: AtomicU64::new(0),
            retry_limit_exhausted: AtomicU64::new(0),
            deadline_exceeded: AtomicU64::new(0),
        }))
    }

    pub(crate) fn start_request(&self) -> RequestContext {
        let request_number = self.original_requests.fetch_add(1, Ordering::Relaxed) + 1;
        let started_at = Instant::now();
        RequestContext {
            request_number,
            started_at,
            deadline: started_at + self.config.request_deadline,
        }
    }

    pub(crate) fn reserve_retry(
        self: &Arc<Self>,
        retry_index: usize,
    ) -> Option<(RetryReservation, Duration)> {
        let allowed = self
            .original_requests
            .load(Ordering::Relaxed)
            .saturating_mul(self.config.retry_budget_percent)
            / 100;
        let mut used = self.retry_slots_used.load(Ordering::Relaxed);
        loop {
            if used >= allowed {
                self.retries_denied_budget.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            match self.retry_slots_used.compare_exchange_weak(
                used,
                used + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => used = observed,
            }
        }

        let upper_bound = exponential_cap(
            self.config.retry_base_delay,
            self.config.retry_max_delay,
            retry_index,
        );
        Some((
            RetryReservation {
                controller: Arc::clone(self),
                committed: false,
            },
            self.jitter.delay(upper_bound),
        ))
    }

    pub(crate) fn max_retries(&self) -> usize {
        self.config.max_retries
    }

    pub(crate) fn attempt_timeout(&self) -> Duration {
        self.config.attempt_timeout
    }

    pub(crate) fn record_attempt(&self) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_transient_failure(&self) {
        self.transient_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_retry_limit_exhausted(&self) {
        self.retry_limit_exhausted.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_deadline_exceeded(&self) {
        self.deadline_exceeded.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> ResilienceSnapshot {
        ResilienceSnapshot {
            request_deadline_ms: duration_millis(self.config.request_deadline),
            attempt_timeout_ms: duration_millis(self.config.attempt_timeout),
            max_retries: self.config.max_retries,
            retry_budget_percent: self.config.retry_budget_percent,
            retry_base_delay_ms: duration_millis(self.config.retry_base_delay),
            retry_max_delay_ms: duration_millis(self.config.retry_max_delay),
            original_requests: self.original_requests.load(Ordering::Relaxed),
            attempts: self.attempts.load(Ordering::Relaxed),
            transient_failures: self.transient_failures.load(Ordering::Relaxed),
            retries_granted: self.retries_granted.load(Ordering::Relaxed),
            retries_denied_budget: self.retries_denied_budget.load(Ordering::Relaxed),
            retry_limit_exhausted: self.retry_limit_exhausted.load(Ordering::Relaxed),
            deadline_exceeded: self.deadline_exceeded.load(Ordering::Relaxed),
        }
    }
}

impl RequestContext {
    pub(crate) fn request_number(self) -> u64 {
        self.request_number
    }

    pub(crate) fn deadline(self) -> Instant {
        self.deadline
    }

    pub(crate) fn elapsed(self) -> Duration {
        self.started_at.elapsed()
    }

    pub(crate) fn remaining(self) -> Option<Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        (!remaining.is_zero()).then_some(remaining)
    }
}

impl RetryReservation {
    pub(crate) fn commit(mut self) {
        self.committed = true;
        self.controller
            .retries_granted
            .fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for RetryReservation {
    fn drop(&mut self) {
        if !self.committed {
            self.controller
                .retry_slots_used
                .fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl FullJitter {
    pub fn with_seed(seed: u64) -> Self {
        Self {
            seed,
            sequence: AtomicU64::new(0),
        }
    }

    pub fn delay(&self, upper_bound: Duration) -> Duration {
        let upper_millis = duration_millis(upper_bound);
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let random = splitmix64(
            self.seed
                .wrapping_add(sequence.wrapping_mul(0x9e37_79b9_7f4a_7c15)),
        );
        Duration::from_millis(random % upper_millis.saturating_add(1))
    }
}

pub fn exponential_cap(base: Duration, maximum: Duration, retry_index: usize) -> Duration {
    let exponent = u32::try_from(retry_index).unwrap_or(u32::MAX);
    let multiplier = 1_u32.checked_shl(exponent).unwrap_or(u32::MAX);
    base.saturating_mul(multiplier).min(maximum)
}

pub fn default_jitter_seed() -> u64 {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    u64::try_from(timestamp).unwrap_or(u64::MAX) ^ u64::from(std::process::id())
}

fn validate_config(config: ResilienceConfig) -> Result<(), String> {
    let one_day = Duration::from_secs(24 * 60 * 60);
    if config.request_deadline.is_zero() || config.request_deadline > one_day {
        return Err("request deadline must be greater than zero and at most one day".to_owned());
    }
    if config.attempt_timeout.is_zero() || config.attempt_timeout > one_day {
        return Err("attempt timeout must be greater than zero and at most one day".to_owned());
    }
    if config.max_retries > 16 {
        return Err("maximum retries must not exceed 16".to_owned());
    }
    if config.retry_budget_percent > 100 {
        return Err("retry budget percent must not exceed 100".to_owned());
    }
    if config.retry_base_delay.is_zero() {
        return Err("retry base delay must be greater than zero".to_owned());
    }
    if config.retry_base_delay > config.retry_max_delay {
        return Err("retry base delay must not exceed retry maximum delay".to_owned());
    }
    if config.retry_max_delay > Duration::from_secs(60 * 60) {
        return Err("retry maximum delay must not exceed one hour".to_owned());
    }
    Ok(())
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{ResilienceConfig, ResilienceController, exponential_cap};

    fn config() -> ResilienceConfig {
        ResilienceConfig {
            request_deadline: Duration::from_secs(1),
            attempt_timeout: Duration::from_millis(200),
            max_retries: 2,
            retry_budget_percent: 10,
            retry_base_delay: Duration::from_millis(10),
            retry_max_delay: Duration::from_millis(25),
            jitter_seed: 7,
        }
    }

    #[test]
    fn cumulative_retry_budget_never_exceeds_ten_percent() {
        let controller = ResilienceController::new(config()).expect("valid controller");
        for _ in 0..9 {
            controller.start_request();
        }
        assert!(controller.reserve_retry(0).is_none());

        controller.start_request();
        let (reservation, _) = controller
            .reserve_retry(0)
            .expect("tenth request earns retry");
        reservation.commit();
        assert!(controller.reserve_retry(0).is_none());

        for _ in 0..10 {
            controller.start_request();
        }
        controller
            .reserve_retry(1)
            .expect("twentieth request earns second retry")
            .0
            .commit();

        let snapshot = controller.snapshot();
        assert_eq!(snapshot.original_requests, 20);
        assert_eq!(snapshot.retries_granted, 2);
    }

    #[test]
    fn uncommitted_retry_reservation_returns_its_budget_slot() {
        let controller = ResilienceController::new(config()).expect("valid controller");
        for _ in 0..10 {
            controller.start_request();
        }
        let reservation = controller.reserve_retry(0).expect("retry reservation").0;
        drop(reservation);

        assert!(controller.reserve_retry(0).is_some());
    }

    #[test]
    fn exponential_backoff_caps_at_the_configured_maximum() {
        let base = Duration::from_millis(10);
        let maximum = Duration::from_millis(25);

        assert_eq!(exponential_cap(base, maximum, 0), Duration::from_millis(10));
        assert_eq!(exponential_cap(base, maximum, 1), Duration::from_millis(20));
        assert_eq!(exponential_cap(base, maximum, 2), Duration::from_millis(25));
        assert_eq!(
            exponential_cap(base, maximum, 100),
            Duration::from_millis(25)
        );
    }

    #[test]
    fn rejects_invalid_resilience_configuration() {
        let mut invalid = config();
        invalid.retry_budget_percent = 101;
        assert!(ResilienceController::new(invalid).is_err());

        invalid = config();
        invalid.request_deadline = Duration::ZERO;
        assert!(ResilienceController::new(invalid).is_err());

        invalid = config();
        invalid.retry_base_delay = Duration::from_millis(30);
        assert!(ResilienceController::new(invalid).is_err());
    }
}
