use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::Serialize;

#[derive(Clone, Copy, Debug)]
pub struct CircuitBreakerConfig {
    pub window_size: usize,
    pub minimum_requests: usize,
    pub failure_rate_percent: u64,
    pub open_duration: Duration,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            window_size: 10,
            minimum_requests: 5,
            failure_rate_percent: 50,
            open_duration: Duration::from_secs(5),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CircuitStateName {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug, Serialize)]
pub struct CircuitSnapshot {
    pub state: CircuitStateName,
    pub window_size: usize,
    pub minimum_requests: usize,
    pub failure_rate_threshold_percent: u64,
    pub open_duration_ms: u64,
    pub samples: usize,
    pub failures: usize,
    pub failure_rate_percent: f64,
    pub remaining_open_ms: u64,
    pub probe_in_flight: bool,
    pub successful_attempts_total: u64,
    pub failed_attempts_total: u64,
    pub opened_total: u64,
    pub rejected_total: u64,
    pub half_open_probes_total: u64,
    pub recoveries_total: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CircuitMetricsSnapshot {
    pub state: CircuitStateName,
}

#[derive(Debug)]
pub(crate) struct CircuitBreaker {
    config: CircuitBreakerConfig,
    inner: Mutex<CircuitData>,
}

#[derive(Debug)]
pub(crate) struct CircuitAttempt {
    breaker: Arc<CircuitBreaker>,
    generation: u64,
    kind: AttemptKind,
    resolved: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttemptKind {
    Closed,
    HalfOpen,
}

#[derive(Debug)]
struct CircuitData {
    state: CircuitState,
    generation: u64,
    outcomes: VecDeque<bool>,
    successful_attempts_total: u64,
    failed_attempts_total: u64,
    opened_total: u64,
    rejected_total: u64,
    half_open_probes_total: u64,
    recoveries_total: u64,
}

#[derive(Debug)]
enum CircuitState {
    Closed,
    Open { until: Instant },
    HalfOpen { probe_in_flight: bool },
}

impl CircuitBreakerConfig {
    pub(crate) fn validate(self) -> Result<(), String> {
        if self.window_size == 0 || self.window_size > 100_000 {
            return Err("circuit window size must be between 1 and 100000".to_owned());
        }
        if self.minimum_requests == 0 || self.minimum_requests > self.window_size {
            return Err(
                "circuit minimum requests must be between 1 and the window size".to_owned(),
            );
        }
        if self.failure_rate_percent == 0 || self.failure_rate_percent > 100 {
            return Err("circuit failure-rate percent must be between 1 and 100".to_owned());
        }
        if self.open_duration.is_zero() || self.open_duration > Duration::from_secs(60 * 60) {
            return Err(
                "circuit open duration must be greater than zero and at most one hour".to_owned(),
            );
        }
        Ok(())
    }
}

impl CircuitBreaker {
    pub(crate) fn new(config: CircuitBreakerConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            inner: Mutex::new(CircuitData {
                state: CircuitState::Closed,
                generation: 0,
                outcomes: VecDeque::with_capacity(config.window_size),
                successful_attempts_total: 0,
                failed_attempts_total: 0,
                opened_total: 0,
                rejected_total: 0,
                half_open_probes_total: 0,
                recoveries_total: 0,
            }),
        })
    }

    pub(crate) fn try_acquire(self: &Arc<Self>) -> Option<CircuitAttempt> {
        self.try_acquire_at(Instant::now())
    }

    fn try_acquire_at(self: &Arc<Self>, now: Instant) -> Option<CircuitAttempt> {
        let mut data = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        refresh_open_state(&mut data, now);

        let kind = match &mut data.state {
            CircuitState::Closed => AttemptKind::Closed,
            CircuitState::Open { .. } => {
                data.rejected_total += 1;
                return None;
            }
            CircuitState::HalfOpen { probe_in_flight } => {
                if *probe_in_flight {
                    data.rejected_total += 1;
                    return None;
                }
                *probe_in_flight = true;
                data.half_open_probes_total += 1;
                AttemptKind::HalfOpen
            }
        };

        Some(CircuitAttempt {
            breaker: Arc::clone(self),
            generation: data.generation,
            kind,
            resolved: false,
        })
    }

    pub(crate) fn snapshot(&self) -> CircuitSnapshot {
        self.snapshot_at(Instant::now())
    }

    /// Returns the bounded scalar view used by the Prometheus scrape path.
    ///
    /// This is intentionally observational: unlike the diagnostics snapshot,
    /// it does not advance an expired open circuit into half-open state. It
    /// reports that circuit as logically half-open without making a scrape
    /// influence the next routing decision.
    pub(crate) fn metrics_snapshot(&self) -> CircuitMetricsSnapshot {
        let now = Instant::now();
        let data = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = match &data.state {
            CircuitState::Closed => CircuitStateName::Closed,
            CircuitState::Open { until } if now < *until => CircuitStateName::Open,
            CircuitState::Open { .. } | CircuitState::HalfOpen { .. } => CircuitStateName::HalfOpen,
        };
        CircuitMetricsSnapshot { state }
    }

    fn snapshot_at(&self, now: Instant) -> CircuitSnapshot {
        let mut data = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        refresh_open_state(&mut data, now);
        let failures = data.outcomes.iter().filter(|failed| **failed).count();
        let samples = data.outcomes.len();
        let (state, remaining_open_ms, probe_in_flight) = match &data.state {
            CircuitState::Closed => (CircuitStateName::Closed, 0, false),
            CircuitState::Open { until } => (
                CircuitStateName::Open,
                duration_millis_ceil(until.saturating_duration_since(now)),
                false,
            ),
            CircuitState::HalfOpen { probe_in_flight } => {
                (CircuitStateName::HalfOpen, 0, *probe_in_flight)
            }
        };

        CircuitSnapshot {
            state,
            window_size: self.config.window_size,
            minimum_requests: self.config.minimum_requests,
            failure_rate_threshold_percent: self.config.failure_rate_percent,
            open_duration_ms: duration_millis_ceil(self.config.open_duration),
            samples,
            failures,
            failure_rate_percent: if samples == 0 {
                0.0
            } else {
                failures as f64 / samples as f64 * 100.0
            },
            remaining_open_ms,
            probe_in_flight,
            successful_attempts_total: data.successful_attempts_total,
            failed_attempts_total: data.failed_attempts_total,
            opened_total: data.opened_total,
            rejected_total: data.rejected_total,
            half_open_probes_total: data.half_open_probes_total,
            recoveries_total: data.recoveries_total,
        }
    }

    fn resolve(&self, generation: u64, kind: AttemptKind, failed: bool, now: Instant) {
        let mut data = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if data.generation != generation {
            return;
        }

        match (kind, &data.state) {
            (AttemptKind::Closed, CircuitState::Closed)
            | (AttemptKind::HalfOpen, CircuitState::HalfOpen { .. }) => {}
            _ => return,
        }

        if failed {
            data.failed_attempts_total += 1;
        } else {
            data.successful_attempts_total += 1;
        }

        match kind {
            AttemptKind::Closed => {
                data.outcomes.push_back(failed);
                if data.outcomes.len() > self.config.window_size {
                    data.outcomes.pop_front();
                }
                if should_open(&data.outcomes, self.config) {
                    open_circuit(&mut data, now, self.config.open_duration);
                }
            }
            AttemptKind::HalfOpen if failed => {
                data.outcomes.clear();
                data.outcomes.push_back(true);
                open_circuit(&mut data, now, self.config.open_duration);
            }
            AttemptKind::HalfOpen => {
                data.generation = data.generation.wrapping_add(1);
                data.state = CircuitState::Closed;
                data.outcomes.clear();
                data.recoveries_total += 1;
            }
        }
    }

    fn cancel(&self, generation: u64, kind: AttemptKind) {
        if kind != AttemptKind::HalfOpen {
            return;
        }
        let mut data = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if data.generation == generation
            && let CircuitState::HalfOpen { probe_in_flight } = &mut data.state
        {
            *probe_in_flight = false;
        }
    }
}

impl CircuitAttempt {
    pub(crate) fn success(mut self) {
        self.success_at(Instant::now());
    }

    pub(crate) fn failure(mut self) {
        self.failure_at(Instant::now());
    }

    fn success_at(&mut self, now: Instant) {
        self.breaker.resolve(self.generation, self.kind, false, now);
        self.resolved = true;
    }

    fn failure_at(&mut self, now: Instant) {
        self.breaker.resolve(self.generation, self.kind, true, now);
        self.resolved = true;
    }
}

impl Drop for CircuitAttempt {
    fn drop(&mut self) {
        if !self.resolved {
            self.breaker.cancel(self.generation, self.kind);
        }
    }
}

fn refresh_open_state(data: &mut CircuitData, now: Instant) {
    if matches!(data.state, CircuitState::Open { until } if now >= until) {
        data.state = CircuitState::HalfOpen {
            probe_in_flight: false,
        };
    }
}

fn should_open(outcomes: &VecDeque<bool>, config: CircuitBreakerConfig) -> bool {
    if outcomes.len() < config.minimum_requests {
        return false;
    }
    let failures = outcomes.iter().filter(|failed| **failed).count();
    (failures as u128) * 100 >= (outcomes.len() as u128) * u128::from(config.failure_rate_percent)
}

fn open_circuit(data: &mut CircuitData, now: Instant, duration: Duration) {
    data.generation = data.generation.wrapping_add(1);
    data.state = CircuitState::Open {
        until: now + duration,
    };
    data.opened_total += 1;
}

fn duration_millis_ceil(duration: Duration) -> u64 {
    let nanos = duration.as_nanos();
    let millis = nanos.saturating_add(999_999) / 1_000_000;
    u64::try_from(millis).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{CircuitBreaker, CircuitBreakerConfig, CircuitStateName};

    fn config() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            window_size: 4,
            minimum_requests: 4,
            failure_rate_percent: 50,
            open_duration: Duration::from_millis(100),
        }
    }

    fn record(breaker: &std::sync::Arc<CircuitBreaker>, now: Instant, failed: bool) {
        let mut attempt = breaker.try_acquire_at(now).expect("closed attempt");
        if failed {
            attempt.failure_at(now);
        } else {
            attempt.success_at(now);
        }
    }

    #[test]
    fn sliding_window_opens_at_the_configured_error_rate() {
        let breaker = CircuitBreaker::new(config());
        let now = Instant::now();
        record(&breaker, now, true);
        record(&breaker, now, false);
        record(&breaker, now, true);
        assert_eq!(breaker.snapshot_at(now).state, CircuitStateName::Closed);

        record(&breaker, now, false);
        let snapshot = breaker.snapshot_at(now);
        assert_eq!(snapshot.state, CircuitStateName::Open);
        assert_eq!(snapshot.samples, 4);
        assert_eq!(snapshot.failures, 2);
        assert_eq!(snapshot.failure_rate_percent, 50.0);
        assert_eq!(snapshot.opened_total, 1);
    }

    #[test]
    fn sliding_window_discards_the_oldest_outcome() {
        let mut breaker_config = config();
        breaker_config.failure_rate_percent = 75;
        let breaker = CircuitBreaker::new(breaker_config);
        let now = Instant::now();
        for failed in [true, true, false, false, true, true] {
            record(&breaker, now, failed);
        }
        let before_threshold = breaker.snapshot_at(now);
        assert_eq!(before_threshold.state, CircuitStateName::Closed);
        assert_eq!(before_threshold.samples, 4);
        assert_eq!(before_threshold.failures, 2);

        record(&breaker, now, true);
        let opened = breaker.snapshot_at(now);
        assert_eq!(opened.state, CircuitStateName::Open);
        assert_eq!(opened.samples, 4);
        assert_eq!(opened.failures, 3);
    }

    #[test]
    fn stale_success_cannot_close_a_newer_generation() {
        let mut breaker_config = config();
        breaker_config.window_size = 2;
        breaker_config.minimum_requests = 2;
        breaker_config.failure_rate_percent = 100;
        let breaker = CircuitBreaker::new(breaker_config);
        let now = Instant::now();
        let mut stale_success = breaker.try_acquire_at(now).expect("old attempt");
        let mut first_failure = breaker.try_acquire_at(now).expect("failure one");
        let mut second_failure = breaker.try_acquire_at(now).expect("failure two");

        first_failure.failure_at(now);
        second_failure.failure_at(now);
        assert_eq!(breaker.snapshot_at(now).state, CircuitStateName::Open);
        stale_success.success_at(now);

        let snapshot = breaker.snapshot_at(now);
        assert_eq!(snapshot.state, CircuitStateName::Open);
        assert_eq!(snapshot.successful_attempts_total, 0);
        assert_eq!(snapshot.failed_attempts_total, 2);
    }

    #[test]
    fn one_half_open_probe_recovers_a_healed_worker() {
        let breaker = CircuitBreaker::new(config());
        let opened_at = Instant::now();
        for _ in 0..4 {
            record(&breaker, opened_at, true);
        }
        assert!(
            breaker
                .try_acquire_at(opened_at + Duration::from_millis(99))
                .is_none()
        );

        let probe_at = opened_at + Duration::from_millis(100);
        let mut probe = breaker.try_acquire_at(probe_at).expect("half-open probe");
        assert_eq!(
            breaker.snapshot_at(probe_at).state,
            CircuitStateName::HalfOpen
        );
        assert!(breaker.try_acquire_at(probe_at).is_none());
        probe.success_at(probe_at);

        let snapshot = breaker.snapshot_at(probe_at);
        assert_eq!(snapshot.state, CircuitStateName::Closed);
        assert_eq!(snapshot.half_open_probes_total, 1);
        assert_eq!(snapshot.recoveries_total, 1);
        assert_eq!(snapshot.samples, 0);
    }

    #[test]
    fn failed_half_open_probe_reopens_for_a_full_cooldown() {
        let breaker = CircuitBreaker::new(config());
        let opened_at = Instant::now();
        for _ in 0..4 {
            record(&breaker, opened_at, true);
        }
        let probe_at = opened_at + Duration::from_millis(100);
        let mut probe = breaker.try_acquire_at(probe_at).expect("half-open probe");
        probe.failure_at(probe_at);

        let snapshot = breaker.snapshot_at(probe_at + Duration::from_millis(99));
        assert_eq!(snapshot.state, CircuitStateName::Open);
        assert_eq!(snapshot.opened_total, 2);
        assert!(
            breaker
                .try_acquire_at(probe_at + Duration::from_millis(99))
                .is_none()
        );
    }

    #[test]
    fn cancelled_half_open_probe_releases_the_single_probe_slot() {
        let breaker = CircuitBreaker::new(config());
        let opened_at = Instant::now();
        for _ in 0..4 {
            record(&breaker, opened_at, true);
        }
        let probe_at = opened_at + Duration::from_millis(100);
        let probe = breaker.try_acquire_at(probe_at).expect("half-open probe");
        drop(probe);

        assert!(breaker.try_acquire_at(probe_at).is_some());
    }

    #[test]
    fn rejects_invalid_configuration() {
        let mut invalid = config();
        invalid.minimum_requests = 5;
        assert!(invalid.validate().is_err());
        invalid = config();
        invalid.failure_rate_percent = 0;
        assert!(invalid.validate().is_err());
        invalid = config();
        invalid.open_duration = Duration::ZERO;
        assert!(invalid.validate().is_err());
    }
}
