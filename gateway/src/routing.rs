use std::{
    collections::HashSet,
    fmt,
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::circuit_breaker::{
    CircuitAttempt, CircuitBreaker, CircuitBreakerConfig, CircuitSnapshot, CircuitStateName,
};

#[derive(Debug)]
struct Worker {
    id: String,
    base_url: String,
    weight: u32,
    smooth_current: AtomicI64,
    latency: Mutex<LatencyEstimate>,
    in_flight: AtomicUsize,
    execution_slots: Arc<Semaphore>,
    concurrency_limit: usize,
    circuit_breaker: Arc<CircuitBreaker>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoutingPolicy {
    #[default]
    RoundRobin,
    LeastInFlight,
    WeightedRoundRobin,
    EwmaLatency,
    ConsistentHash,
}

#[derive(Debug)]
pub struct WorkerPool {
    workers: Vec<Arc<Worker>>,
    next: AtomicUsize,
    policy: RoutingPolicy,
    selection_lock: Mutex<()>,
    total_weight: i64,
    ewma_alpha: f64,
    ewma_probe_interval: usize,
    ewma_decisions: AtomicUsize,
    hash_ring: ConsistentHashRing,
    total_execution_capacity: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct RoutingConfig {
    pub policy: RoutingPolicy,
    pub ewma_alpha: f64,
    pub ewma_probe_interval: usize,
    pub consistent_hash_virtual_nodes: usize,
    pub worker_concurrency_limit: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerRegistration {
    pub id: String,
    pub base_url: String,
    pub weight: u32,
}

#[derive(Debug, Serialize)]
pub struct WorkerSnapshot {
    pub id: String,
    pub base_url: String,
    pub weight: u32,
    pub in_flight: usize,
    pub executing: usize,
    pub concurrency_limit: usize,
    pub ewma_ttft_ms: Option<f64>,
    pub ewma_observations: u64,
    pub circuit: CircuitSnapshot,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WorkerPoolMetricsSnapshot {
    pub workers: usize,
    pub in_flight: usize,
    pub circuits_closed: usize,
    pub circuits_open: usize,
    pub circuits_half_open: usize,
}

#[derive(Debug)]
pub struct WorkerLease {
    worker: Arc<Worker>,
    started_at: Instant,
    ewma_alpha: f64,
    latency_observed: bool,
    circuit_attempt: Option<CircuitAttempt>,
}

#[derive(Debug)]
pub struct WorkerExecutionPermit {
    _permit: OwnedSemaphorePermit,
}

#[derive(Debug, Default)]
struct LatencyEstimate {
    ewma_ms: Option<f64>,
    observations: u64,
}

#[derive(Clone, Debug)]
struct RingPoint {
    position: u64,
    worker_index: usize,
}

#[derive(Clone, Debug)]
pub struct ConsistentHashRing {
    worker_ids: Vec<String>,
    points: Vec<RingPoint>,
}

impl WorkerPool {
    pub fn new(workers: Vec<(String, String)>) -> Result<Self, String> {
        Self::with_policy(workers, RoutingPolicy::RoundRobin)
    }

    pub fn with_policy(
        workers: Vec<(String, String)>,
        policy: RoutingPolicy,
    ) -> Result<Self, String> {
        let registrations = workers
            .into_iter()
            .map(|(id, base_url)| WorkerRegistration::new(id, base_url, 1))
            .collect();
        Self::from_registrations(registrations, policy)
    }

    pub fn from_registrations(
        workers: Vec<WorkerRegistration>,
        policy: RoutingPolicy,
    ) -> Result<Self, String> {
        Self::from_config(workers, RoutingConfig::for_policy(policy))
    }

    pub fn from_config(
        workers: Vec<WorkerRegistration>,
        config: RoutingConfig,
    ) -> Result<Self, String> {
        Self::from_config_with_circuit_breaker(workers, config, CircuitBreakerConfig::default())
    }

    pub fn from_config_with_circuit_breaker(
        workers: Vec<WorkerRegistration>,
        config: RoutingConfig,
        circuit_config: CircuitBreakerConfig,
    ) -> Result<Self, String> {
        if workers.is_empty() {
            return Err("at least one worker is required".to_owned());
        }
        if workers
            .iter()
            .any(|worker| worker.id.trim().is_empty() || worker.base_url.trim().is_empty())
        {
            return Err("worker IDs and URLs must not be empty".to_owned());
        }
        if workers.iter().any(|worker| worker.weight == 0) {
            return Err("worker weights must be greater than zero".to_owned());
        }
        let unique_ids: HashSet<&str> = workers.iter().map(|worker| worker.id.trim()).collect();
        if unique_ids.len() != workers.len() {
            return Err("worker IDs must be unique".to_owned());
        }
        let total_weight = workers.iter().try_fold(0_i64, |total, worker| {
            total
                .checked_add(i64::from(worker.weight))
                .ok_or_else(|| "total worker weight is too large".to_owned())
        })?;
        if !config.ewma_alpha.is_finite() || config.ewma_alpha <= 0.0 || config.ewma_alpha > 1.0 {
            return Err("EWMA alpha must be greater than 0 and at most 1".to_owned());
        }
        if config.ewma_probe_interval == 0 {
            return Err("EWMA probe interval must be greater than zero".to_owned());
        }
        if config.consistent_hash_virtual_nodes == 0 {
            return Err("consistent-hash virtual-node count must be greater than zero".to_owned());
        }
        if config.consistent_hash_virtual_nodes > 100_000 {
            return Err("consistent-hash virtual-node count must not exceed 100000".to_owned());
        }
        if config.worker_concurrency_limit == 0 {
            return Err("worker concurrency limit must be greater than zero".to_owned());
        }
        if config.worker_concurrency_limit > 100_000 {
            return Err("worker concurrency limit must not exceed 100000".to_owned());
        }
        circuit_config.validate()?;
        let total_execution_capacity =
            workers
                .len()
                .checked_mul(config.worker_concurrency_limit)
                .ok_or_else(|| "total worker concurrency is too large".to_owned())?;
        let hash_ring = ConsistentHashRing::new(
            workers
                .iter()
                .map(|worker| worker.id.trim().to_owned())
                .collect(),
            config.consistent_hash_virtual_nodes,
        )?;

        Ok(Self {
            workers: workers
                .into_iter()
                .map(|worker| {
                    Arc::new(Worker {
                        id: worker.id.trim().to_owned(),
                        base_url: worker.base_url.trim_end_matches('/').to_owned(),
                        weight: worker.weight,
                        smooth_current: AtomicI64::new(0),
                        latency: Mutex::new(LatencyEstimate::default()),
                        in_flight: AtomicUsize::new(0),
                        execution_slots: Arc::new(Semaphore::new(config.worker_concurrency_limit)),
                        concurrency_limit: config.worker_concurrency_limit,
                        circuit_breaker: CircuitBreaker::new(circuit_config),
                    })
                })
                .collect(),
            next: AtomicUsize::new(0),
            policy: config.policy,
            selection_lock: Mutex::new(()),
            total_weight,
            ewma_alpha: config.ewma_alpha,
            ewma_probe_interval: config.ewma_probe_interval,
            ewma_decisions: AtomicUsize::new(0),
            hash_ring,
            total_execution_capacity,
        })
    }

    pub fn choose(&self) -> WorkerLease {
        self.try_choose()
            .expect("at least one worker circuit must accept an attempt")
    }

    pub fn try_choose(&self) -> Option<WorkerLease> {
        match self.policy {
            RoutingPolicy::RoundRobin => self.try_choose_round_robin(),
            RoutingPolicy::LeastInFlight => self.try_choose_least_in_flight(),
            RoutingPolicy::WeightedRoundRobin => self.try_choose_weighted_round_robin(),
            RoutingPolicy::EwmaLatency => self.try_choose_ewma_latency(),
            RoutingPolicy::ConsistentHash => self.try_choose_consistent_hash(b""),
        }
    }

    pub fn choose_for_key(&self, key: &[u8]) -> WorkerLease {
        self.try_choose_for_key(key)
            .expect("at least one worker circuit must accept an attempt")
    }

    pub fn try_choose_for_key(&self, key: &[u8]) -> Option<WorkerLease> {
        match self.policy {
            RoutingPolicy::ConsistentHash => self.try_choose_consistent_hash(key),
            _ => self.try_choose(),
        }
    }

    pub fn choose_retry(&self, attempted_workers: &HashSet<String>) -> WorkerLease {
        self.try_choose_retry(attempted_workers)
            .expect("at least one worker circuit must accept a retry")
    }

    pub fn try_choose_retry(&self, attempted_workers: &HashSet<String>) -> Option<WorkerLease> {
        let start = self.next.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        let mut candidates = Vec::with_capacity(self.workers.len());
        for offset in 0..self.workers.len() {
            let index = (start + offset) % self.workers.len();
            if !attempted_workers.contains(&self.workers[index].id) {
                candidates.push(index);
            }
        }

        // Every worker was already attempted. Reusing the rotating start preserves progress when
        // max_retries exceeds worker count, while the retry budget still bounds amplification.
        for offset in 0..self.workers.len() {
            let index = (start + offset) % self.workers.len();
            if attempted_workers.contains(&self.workers[index].id) {
                candidates.push(index);
            }
        }
        self.try_lease_indices(candidates).map(|(_, lease)| lease)
    }

    pub fn choose_round_robin(&self) -> WorkerLease {
        self.try_choose_round_robin()
            .expect("at least one worker circuit must accept an attempt")
    }

    fn try_choose_round_robin(&self) -> Option<WorkerLease> {
        let start = self.next.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        self.try_lease_indices(self.rotated_indices(start))
            .map(|(_, lease)| lease)
    }

    pub fn choose_least_in_flight(&self) -> WorkerLease {
        self.try_choose_least_in_flight()
            .expect("at least one worker circuit must accept an attempt")
    }

    fn try_choose_least_in_flight(&self) -> Option<WorkerLease> {
        // Selection and reservation are one logical operation. Without this short lock, concurrent
        // requests could all observe the same minimum before any of them increments it.
        let _selection = self
            .selection_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let start = self.next.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        let mut candidates = self.rotated_indices(start);
        // Stable sorting preserves the rotating order for equal in-flight counts.
        candidates.sort_by_key(|index| self.workers[*index].in_flight.load(Ordering::Relaxed));
        self.try_lease_indices(candidates).map(|(_, lease)| lease)
    }

    pub fn choose_weighted_round_robin(&self) -> WorkerLease {
        self.try_choose_weighted_round_robin()
            .expect("at least one worker circuit must accept an attempt")
    }

    fn try_choose_weighted_round_robin(&self) -> Option<WorkerLease> {
        let _selection = self
            .selection_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let start = self.next.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        let mut candidates = Vec::with_capacity(self.workers.len());

        // Smooth weighted round-robin accumulates entitlement over time. Subtracting the total
        // from the winner pays for this turn, while workers not selected keep their accumulated
        // score and therefore cannot be starved.
        for offset in 0..self.workers.len() {
            let index = (start + offset) % self.workers.len();
            let worker = &self.workers[index];
            let current = worker
                .smooth_current
                .fetch_add(i64::from(worker.weight), Ordering::Relaxed)
                + i64::from(worker.weight);
            candidates.push((index, current));
        }
        // Stable sorting preserves rotating order when current weights are equal.
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.1));
        let (selected, lease) =
            self.try_lease_indices(candidates.into_iter().map(|(index, _)| index))?;
        self.workers[selected]
            .smooth_current
            .fetch_sub(self.total_weight, Ordering::Relaxed);

        Some(lease)
    }

    pub fn choose_ewma_latency(&self) -> WorkerLease {
        self.try_choose_ewma_latency()
            .expect("at least one worker circuit must accept an attempt")
    }

    fn try_choose_ewma_latency(&self) -> Option<WorkerLease> {
        let _selection = self
            .selection_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let start = self.next.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        let decision = self.ewma_decisions.fetch_add(1, Ordering::Relaxed) + 1;

        // Deterministic probes prevent an avoided worker from remaining stale forever.
        if decision.is_multiple_of(self.ewma_probe_interval) {
            return self
                .try_lease_indices(self.rotated_indices(start))
                .map(|(_, lease)| lease);
        }

        let mut selected = start;
        let mut lowest_ewma = f64::INFINITY;
        for offset in 0..self.workers.len() {
            let index = (start + offset) % self.workers.len();
            match self.workers[index].latency_estimate_ms() {
                // Bootstrap every unknown worker before comparing estimates.
                None => {
                    selected = index;
                    break;
                }
                Some(ewma) if ewma < lowest_ewma => {
                    selected = index;
                    lowest_ewma = ewma;
                }
                Some(_) => {}
            }
        }

        let mut candidates = vec![selected];
        candidates.extend(
            self.rotated_indices(start)
                .into_iter()
                .filter(|index| *index != selected),
        );
        self.try_lease_indices(candidates).map(|(_, lease)| lease)
    }

    pub fn choose_consistent_hash(&self, key: &[u8]) -> WorkerLease {
        self.try_choose_consistent_hash(key)
            .expect("at least one worker circuit must accept an attempt")
    }

    fn try_choose_consistent_hash(&self, key: &[u8]) -> Option<WorkerLease> {
        self.try_lease_indices(self.hash_ring.owner_indices(key))
            .map(|(_, lease)| lease)
    }

    pub fn policy(&self) -> RoutingPolicy {
        self.policy
    }

    pub fn total_execution_capacity(&self) -> usize {
        self.total_execution_capacity
    }

    fn rotated_indices(&self, start: usize) -> Vec<usize> {
        (0..self.workers.len())
            .map(|offset| (start + offset) % self.workers.len())
            .collect()
    }

    fn try_lease_indices(
        &self,
        indices: impl IntoIterator<Item = usize>,
    ) -> Option<(usize, WorkerLease)> {
        for index in indices {
            let Some(circuit_attempt) = self.workers[index].circuit_breaker.try_acquire() else {
                continue;
            };
            return Some((index, self.lease(index, circuit_attempt)));
        }
        None
    }

    fn lease(&self, index: usize, circuit_attempt: CircuitAttempt) -> WorkerLease {
        let worker = Arc::clone(&self.workers[index]);
        worker.in_flight.fetch_add(1, Ordering::Relaxed);
        WorkerLease {
            worker,
            started_at: Instant::now(),
            ewma_alpha: self.ewma_alpha,
            latency_observed: false,
            circuit_attempt: Some(circuit_attempt),
        }
    }

    pub fn snapshots(&self) -> Vec<WorkerSnapshot> {
        self.workers
            .iter()
            .map(|worker| {
                let latency = worker
                    .latency
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                WorkerSnapshot {
                    id: worker.id.clone(),
                    base_url: worker.base_url.clone(),
                    weight: worker.weight,
                    in_flight: worker.in_flight.load(Ordering::Relaxed),
                    executing: worker.concurrency_limit
                        - worker.execution_slots.available_permits(),
                    concurrency_limit: worker.concurrency_limit,
                    ewma_ttft_ms: latency.ewma_ms,
                    ewma_observations: latency.observations,
                    circuit: worker.circuit_breaker.snapshot(),
                }
            })
            .collect()
    }

    /// Aggregates routing state for a bounded-cardinality scrape without
    /// cloning worker identities or URLs into the metrics path.
    pub(crate) fn metrics_snapshot(&self) -> WorkerPoolMetricsSnapshot {
        let mut snapshot = WorkerPoolMetricsSnapshot {
            workers: self.workers.len(),
            in_flight: 0,
            circuits_closed: 0,
            circuits_open: 0,
            circuits_half_open: 0,
        };
        for worker in &self.workers {
            snapshot.in_flight = snapshot
                .in_flight
                .saturating_add(worker.in_flight.load(Ordering::Relaxed));
            let circuit = worker.circuit_breaker.metrics_snapshot();
            match circuit.state {
                CircuitStateName::Closed => {
                    snapshot.circuits_closed = snapshot.circuits_closed.saturating_add(1);
                }
                CircuitStateName::Open => {
                    snapshot.circuits_open = snapshot.circuits_open.saturating_add(1);
                }
                CircuitStateName::HalfOpen => {
                    snapshot.circuits_half_open = snapshot.circuits_half_open.saturating_add(1);
                }
            }
        }
        snapshot
    }
}

impl RoutingConfig {
    pub fn for_policy(policy: RoutingPolicy) -> Self {
        Self {
            policy,
            ewma_alpha: 0.25,
            ewma_probe_interval: 10,
            consistent_hash_virtual_nodes: 128,
            worker_concurrency_limit: 8,
        }
    }
}

impl ConsistentHashRing {
    pub fn new(worker_ids: Vec<String>, virtual_nodes: usize) -> Result<Self, String> {
        let worker_ids: Vec<String> = worker_ids
            .into_iter()
            .map(|worker_id| worker_id.trim().to_owned())
            .collect();
        if worker_ids.is_empty() {
            return Err("consistent-hash ring requires at least one worker".to_owned());
        }
        if virtual_nodes == 0 {
            return Err("consistent-hash ring requires at least one virtual node".to_owned());
        }
        if worker_ids
            .iter()
            .any(|worker_id| worker_id.trim().is_empty())
        {
            return Err("consistent-hash worker IDs must not be empty".to_owned());
        }
        let unique_ids: HashSet<&str> = worker_ids.iter().map(String::as_str).collect();
        if unique_ids.len() != worker_ids.len() {
            return Err("consistent-hash worker IDs must be unique".to_owned());
        }
        let mut points = Vec::with_capacity(
            worker_ids
                .len()
                .checked_mul(virtual_nodes)
                .ok_or_else(|| "consistent-hash ring is too large".to_owned())?,
        );
        for (worker_index, worker_id) in worker_ids.iter().enumerate() {
            for virtual_node in 0..virtual_nodes {
                let label = format!("{worker_id}#vnode-{virtual_node}");
                points.push(RingPoint {
                    position: stable_hash(label.as_bytes()),
                    worker_index,
                });
            }
        }
        points.sort_unstable_by_key(|point| (point.position, point.worker_index));
        if points
            .windows(2)
            .any(|pair| pair[0].position == pair[1].position)
        {
            return Err("consistent-hash virtual-node collision".to_owned());
        }

        Ok(Self { worker_ids, points })
    }

    pub fn owner(&self, key: &[u8]) -> &str {
        &self.worker_ids[self.owner_index(key)]
    }

    pub fn virtual_point_count(&self) -> usize {
        self.points.len()
    }

    fn owner_index(&self, key: &[u8]) -> usize {
        let position = stable_hash(key);
        let point_index = self
            .points
            .partition_point(|point| point.position < position);
        let wrapped_index = if point_index == self.points.len() {
            0
        } else {
            point_index
        };
        self.points[wrapped_index].worker_index
    }

    fn owner_indices(&self, key: &[u8]) -> Vec<usize> {
        let position = stable_hash(key);
        let start = self
            .points
            .partition_point(|point| point.position < position);
        let mut seen = vec![false; self.worker_ids.len()];
        let mut owners = Vec::with_capacity(self.worker_ids.len());

        for offset in 0..self.points.len() {
            let point = &self.points[(start + offset) % self.points.len()];
            if !seen[point.worker_index] {
                seen[point.worker_index] = true;
                owners.push(point.worker_index);
                if owners.len() == self.worker_ids.len() {
                    break;
                }
            }
        }
        owners
    }
}

pub fn stable_hash(bytes: &[u8]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 14_695_981_039_346_656_037;
    const FNV_PRIME: u64 = 1_099_511_628_211;

    let mut hash = bytes.iter().fold(FNV_OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    });

    // FNV-1a is deliberately specified here instead of Rust's process-seeded DefaultHasher.
    // The avalanche step spreads correlated labels (for example vnode-1, vnode-2) around the ring.
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^ (hash >> 33)
}

impl Worker {
    fn latency_estimate_ms(&self) -> Option<f64> {
        self.latency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .ewma_ms
    }

    fn observe_latency(&self, latency: Duration, alpha: f64) {
        let sample_ms = latency.as_secs_f64() * 1_000.0;
        let mut estimate = self
            .latency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        estimate.ewma_ms = Some(match estimate.ewma_ms {
            Some(previous) => alpha * sample_ms + (1.0 - alpha) * previous,
            None => sample_ms,
        });
        estimate.observations += 1;
    }
}

impl WorkerRegistration {
    pub fn new(id: impl Into<String>, base_url: impl Into<String>, weight: u32) -> Self {
        Self {
            id: id.into(),
            base_url: base_url.into(),
            weight,
        }
    }
}

impl FromStr for RoutingPolicy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "round-robin" | "round_robin" | "rr" => Ok(Self::RoundRobin),
            "least-in-flight" | "least_in_flight" | "lif" => Ok(Self::LeastInFlight),
            "weighted" | "weighted-round-robin" | "weighted_round_robin" | "wrr" => {
                Ok(Self::WeightedRoundRobin)
            }
            "ewma" | "ewma-latency" | "ewma_latency" => Ok(Self::EwmaLatency),
            "consistent-hash" | "consistent_hash" | "hash" => Ok(Self::ConsistentHash),
            _ => Err(format!(
                "unknown routing policy '{value}'; expected round-robin, least-in-flight, weighted, ewma, or consistent-hash"
            )),
        }
    }
}

impl fmt::Display for RoutingPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RoundRobin => formatter.write_str("round-robin"),
            Self::LeastInFlight => formatter.write_str("least-in-flight"),
            Self::WeightedRoundRobin => formatter.write_str("weighted"),
            Self::EwmaLatency => formatter.write_str("ewma-latency"),
            Self::ConsistentHash => formatter.write_str("consistent-hash"),
        }
    }
}

impl WorkerLease {
    pub fn id(&self) -> &str {
        &self.worker.id
    }

    pub fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.worker.base_url, path)
    }

    pub fn try_reserve_execution(&self) -> Option<WorkerExecutionPermit> {
        Arc::clone(&self.worker.execution_slots)
            .try_acquire_owned()
            .ok()
            .map(|permit| WorkerExecutionPermit { _permit: permit })
    }

    pub async fn reserve_execution(&self) -> WorkerExecutionPermit {
        let permit = Arc::clone(&self.worker.execution_slots)
            .acquire_owned()
            .await
            .expect("worker execution semaphore is never closed");
        WorkerExecutionPermit { _permit: permit }
    }

    pub(crate) fn record_circuit_success(&mut self) {
        if let Some(attempt) = self.circuit_attempt.take() {
            attempt.success();
        }
    }

    pub(crate) fn record_circuit_failure(&mut self) {
        if let Some(attempt) = self.circuit_attempt.take() {
            attempt.failure();
        }
    }

    pub fn observe_latency(&mut self) {
        self.observe_duration(self.started_at.elapsed());
    }

    fn observe_duration(&mut self, latency: Duration) {
        if !self.latency_observed {
            self.worker.observe_latency(latency, self.ewma_alpha);
            self.latency_observed = true;
        }
    }
}

impl Drop for WorkerLease {
    fn drop(&mut self) {
        // The lease is deliberately owned by the response body stream. This decrement therefore
        // happens on normal completion, upstream failure, or downstream client disconnect.
        self.worker.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, str::FromStr, time::Duration};

    use super::{
        ConsistentHashRing, RoutingConfig, RoutingPolicy, WorkerPool, WorkerRegistration,
        stable_hash,
    };

    #[test]
    fn round_robin_cycles_in_registration_order() {
        let pool = WorkerPool::new(vec![
            ("a".to_owned(), "http://a".to_owned()),
            ("b".to_owned(), "http://b".to_owned()),
            ("c".to_owned(), "http://c".to_owned()),
        ])
        .expect("valid pool");

        let selected: Vec<String> = (0..7)
            .map(|_| pool.choose_round_robin().id().to_owned())
            .collect();

        assert_eq!(selected, ["a", "b", "c", "a", "b", "c", "a"]);
        assert!(pool.snapshots().iter().all(|worker| worker.in_flight == 0));
    }

    #[test]
    fn rejects_an_empty_pool() {
        assert!(WorkerPool::new(vec![]).is_err());
    }

    #[test]
    fn rejects_duplicate_worker_ids() {
        assert!(
            WorkerPool::new(vec![
                ("a".to_owned(), "http://one".to_owned()),
                (" a ".to_owned(), "http://two".to_owned()),
            ])
            .is_err()
        );
    }

    #[test]
    fn rejects_a_zero_weight() {
        assert!(
            WorkerPool::from_registrations(
                vec![WorkerRegistration::new("a", "http://a", 0)],
                RoutingPolicy::WeightedRoundRobin,
            )
            .is_err()
        );
    }

    #[test]
    fn stable_hash_is_deterministic_and_versioned_by_a_golden_value() {
        assert_eq!(stable_hash(b"inferlab"), stable_hash(b"inferlab"));
        assert_eq!(stable_hash(b"inferlab"), 15_458_312_247_603_435_045);
    }

    #[test]
    fn consistent_hash_keeps_a_key_on_one_worker() {
        let pool = WorkerPool::from_registrations(
            vec![
                WorkerRegistration::new("a", "http://a", 1),
                WorkerRegistration::new("b", "http://b", 1),
                WorkerRegistration::new("c", "http://c", 1),
            ],
            RoutingPolicy::ConsistentHash,
        )
        .expect("valid consistent-hash pool");

        let selected: Vec<String> = (0..10)
            .map(|_| {
                pool.choose_for_key(b"tenant-7/shared-prefix")
                    .id()
                    .to_owned()
            })
            .collect();

        assert!(selected.iter().all(|worker| worker == &selected[0]));
    }

    #[test]
    fn removing_a_worker_only_moves_keys_that_it_owned() {
        let before = ConsistentHashRing::new(["a", "b", "c", "d"].map(str::to_owned).to_vec(), 128)
            .expect("four-worker ring");
        let after = ConsistentHashRing::new(["a", "b", "c"].map(str::to_owned).to_vec(), 128)
            .expect("three-worker ring");
        let mut moved = 0;

        for key_number in 0..10_000 {
            let key = format!("prompt-prefix-{key_number}");
            let old_owner = before.owner(key.as_bytes());
            let new_owner = after.owner(key.as_bytes());
            if old_owner != new_owner {
                moved += 1;
                assert_eq!(old_owner, "d");
            }
        }

        assert!(moved > 0);
        assert_eq!(before.virtual_point_count(), 4 * 128);
    }

    #[test]
    fn rejects_an_invalid_consistent_hash_ring() {
        assert!(ConsistentHashRing::new(vec![], 128).is_err());
        assert!(ConsistentHashRing::new(vec!["a".to_owned()], 0).is_err());
        assert!(ConsistentHashRing::new(vec!["a".to_owned(), " a ".to_owned()], 128).is_err());
    }

    #[test]
    fn lease_tracks_in_flight_until_drop() {
        let pool =
            WorkerPool::new(vec![("a".to_owned(), "http://a".to_owned())]).expect("valid pool");
        let lease = pool.choose_round_robin();
        assert_eq!(pool.snapshots()[0].in_flight, 1);
        drop(lease);
        assert_eq!(pool.snapshots()[0].in_flight, 0);
    }

    #[test]
    fn least_in_flight_selects_the_smallest_active_count() {
        let pool = WorkerPool::with_policy(
            vec![
                ("a".to_owned(), "http://a".to_owned()),
                ("b".to_owned(), "http://b".to_owned()),
                ("c".to_owned(), "http://c".to_owned()),
            ],
            RoutingPolicy::LeastInFlight,
        )
        .expect("valid pool");

        let _a = pool.choose();
        let _b = pool.choose();
        let _c = pool.choose();
        let _second_a = pool.choose();

        assert_eq!(pool.snapshots()[0].in_flight, 2);
        assert_eq!(pool.choose().id(), "b");
    }

    #[test]
    fn least_in_flight_rotates_equal_count_ties() {
        let pool = WorkerPool::with_policy(
            vec![
                ("a".to_owned(), "http://a".to_owned()),
                ("b".to_owned(), "http://b".to_owned()),
                ("c".to_owned(), "http://c".to_owned()),
            ],
            RoutingPolicy::LeastInFlight,
        )
        .expect("valid pool");

        let selected: Vec<String> = (0..6).map(|_| pool.choose().id().to_owned()).collect();

        assert_eq!(selected, ["a", "b", "c", "a", "b", "c"]);
    }

    #[test]
    fn smooth_weighted_round_robin_honors_a_three_to_one_ratio() {
        let pool = WorkerPool::from_registrations(
            vec![
                WorkerRegistration::new("a", "http://a", 3),
                WorkerRegistration::new("b", "http://b", 1),
            ],
            RoutingPolicy::WeightedRoundRobin,
        )
        .expect("valid weighted pool");

        let selected: Vec<String> = (0..8).map(|_| pool.choose().id().to_owned()).collect();

        assert_eq!(selected, ["a", "b", "a", "a", "a", "b", "a", "a"]);
        assert_eq!(
            selected
                .iter()
                .filter(|worker| worker.as_str() == "a")
                .count(),
            6
        );
        assert_eq!(
            selected
                .iter()
                .filter(|worker| worker.as_str() == "b")
                .count(),
            2
        );
    }

    #[test]
    fn equal_weights_are_distributed_evenly() {
        let pool = WorkerPool::from_registrations(
            vec![
                WorkerRegistration::new("a", "http://a", 1),
                WorkerRegistration::new("b", "http://b", 1),
                WorkerRegistration::new("c", "http://c", 1),
            ],
            RoutingPolicy::WeightedRoundRobin,
        )
        .expect("valid weighted pool");

        let selected: Vec<String> = (0..6).map(|_| pool.choose().id().to_owned()).collect();

        assert_eq!(selected, ["a", "b", "c", "a", "b", "c"]);
    }

    #[test]
    fn ewma_uses_the_configured_alpha() {
        let pool = WorkerPool::from_config(
            vec![WorkerRegistration::new("a", "http://a", 1)],
            RoutingConfig {
                policy: RoutingPolicy::EwmaLatency,
                ewma_alpha: 0.25,
                ewma_probe_interval: 10,
                consistent_hash_virtual_nodes: 128,
                worker_concurrency_limit: 8,
            },
        )
        .expect("valid EWMA pool");

        let mut first = pool.choose();
        first.observe_duration(Duration::from_millis(100));
        drop(first);
        let mut second = pool.choose();
        second.observe_duration(Duration::from_millis(300));
        drop(second);

        let snapshots = pool.snapshots();
        assert_eq!(snapshots[0].ewma_ttft_ms, Some(150.0));
        assert_eq!(snapshots[0].ewma_observations, 2);
    }

    #[test]
    fn ewma_bootstraps_unknown_workers_then_selects_the_fastest() {
        let pool = WorkerPool::from_config(
            vec![
                WorkerRegistration::new("a", "http://a", 1),
                WorkerRegistration::new("b", "http://b", 1),
            ],
            RoutingConfig {
                policy: RoutingPolicy::EwmaLatency,
                ewma_alpha: 0.5,
                ewma_probe_interval: 100,
                consistent_hash_virtual_nodes: 128,
                worker_concurrency_limit: 8,
            },
        )
        .expect("valid EWMA pool");

        let mut a = pool.choose();
        assert_eq!(a.id(), "a");
        a.observe_duration(Duration::from_millis(100));
        drop(a);

        let mut b = pool.choose();
        assert_eq!(b.id(), "b");
        b.observe_duration(Duration::from_millis(300));
        drop(b);

        assert_eq!(pool.choose().id(), "a");
    }

    #[test]
    fn ewma_probe_periodically_explores_another_worker() {
        let pool = WorkerPool::from_config(
            vec![
                WorkerRegistration::new("a", "http://a", 1),
                WorkerRegistration::new("b", "http://b", 1),
            ],
            RoutingConfig {
                policy: RoutingPolicy::EwmaLatency,
                ewma_alpha: 0.5,
                ewma_probe_interval: 4,
                consistent_hash_virtual_nodes: 128,
                worker_concurrency_limit: 8,
            },
        )
        .expect("valid EWMA pool");

        let mut a = pool.choose();
        a.observe_duration(Duration::from_millis(100));
        let mut b = pool.choose();
        b.observe_duration(Duration::from_millis(300));

        assert_eq!(pool.choose().id(), "a");
        assert_eq!(pool.choose().id(), "b");
    }

    #[test]
    fn rejects_invalid_ewma_configuration() {
        let worker = || vec![WorkerRegistration::new("a", "http://a", 1)];
        assert!(
            WorkerPool::from_config(
                worker(),
                RoutingConfig {
                    policy: RoutingPolicy::EwmaLatency,
                    ewma_alpha: 0.0,
                    ewma_probe_interval: 10,
                    consistent_hash_virtual_nodes: 128,
                    worker_concurrency_limit: 8,
                },
            )
            .is_err()
        );
        assert!(
            WorkerPool::from_config(
                worker(),
                RoutingConfig {
                    policy: RoutingPolicy::EwmaLatency,
                    ewma_alpha: 0.5,
                    ewma_probe_interval: 0,
                    consistent_hash_virtual_nodes: 128,
                    worker_concurrency_limit: 8,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn worker_execution_permits_enforce_the_configured_limit() {
        let pool = WorkerPool::from_config(
            vec![WorkerRegistration::new("a", "http://a", 1)],
            RoutingConfig {
                policy: RoutingPolicy::RoundRobin,
                ewma_alpha: 0.25,
                ewma_probe_interval: 10,
                consistent_hash_virtual_nodes: 128,
                worker_concurrency_limit: 1,
            },
        )
        .expect("valid limited pool");
        let first_lease = pool.choose();
        let second_lease = pool.choose();
        let first_permit = first_lease
            .try_reserve_execution()
            .expect("first execution slot");

        assert!(second_lease.try_reserve_execution().is_none());
        assert_eq!(pool.snapshots()[0].executing, 1);

        drop(first_permit);
        assert!(second_lease.try_reserve_execution().is_some());
    }

    #[test]
    fn retry_selection_prefers_an_untried_worker() {
        let pool = WorkerPool::new(vec![
            ("a".to_owned(), "http://a".to_owned()),
            ("b".to_owned(), "http://b".to_owned()),
        ])
        .expect("valid pool");
        let first = pool.choose();
        let mut attempted = HashSet::new();
        attempted.insert(first.id().to_owned());

        assert_ne!(pool.choose_retry(&attempted).id(), first.id());
    }

    #[test]
    fn rejects_invalid_worker_concurrency() {
        assert!(
            WorkerPool::from_config(
                vec![WorkerRegistration::new("a", "http://a", 1)],
                RoutingConfig {
                    policy: RoutingPolicy::RoundRobin,
                    ewma_alpha: 0.25,
                    ewma_probe_interval: 10,
                    consistent_hash_virtual_nodes: 128,
                    worker_concurrency_limit: 0,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn parses_human_friendly_policy_names() {
        assert_eq!(
            RoutingPolicy::from_str("round-robin"),
            Ok(RoutingPolicy::RoundRobin)
        );
        assert_eq!(
            RoutingPolicy::from_str("lif"),
            Ok(RoutingPolicy::LeastInFlight)
        );
        assert_eq!(
            RoutingPolicy::from_str("weighted"),
            Ok(RoutingPolicy::WeightedRoundRobin)
        );
        assert_eq!(
            RoutingPolicy::from_str("ewma"),
            Ok(RoutingPolicy::EwmaLatency)
        );
        assert_eq!(
            RoutingPolicy::from_str("hash"),
            Ok(RoutingPolicy::ConsistentHash)
        );
        assert!(RoutingPolicy::from_str("random").is_err());
    }
}
