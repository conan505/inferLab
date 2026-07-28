use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::routing::{WorkerExecutionPermit, WorkerLease};

#[derive(Clone, Copy, Debug)]
pub struct AdmissionConfig {
    pub queue_capacity: usize,
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self { queue_capacity: 64 }
    }
}

#[derive(Debug)]
pub(crate) struct AdmissionController {
    outstanding_slots: Arc<Semaphore>,
    queue_slots: Arc<Semaphore>,
    queue_capacity: usize,
    worker_execution_capacity: usize,
    outstanding: AtomicUsize,
    executing: AtomicUsize,
    queued: AtomicUsize,
    rejected_total: AtomicUsize,
    max_observed_outstanding: AtomicUsize,
    max_observed_executing: AtomicUsize,
    max_observed_queued: AtomicUsize,
}

#[derive(Clone, Debug)]
pub(crate) struct RequestAdmissionPermit {
    _inner: Arc<RequestPermitInner>,
}

#[derive(Debug)]
struct RequestPermitInner {
    _permit: OwnedSemaphorePermit,
    controller: Arc<AdmissionController>,
}

#[derive(Debug)]
pub(crate) struct ExecutionGuard {
    _worker_permit: WorkerExecutionPermit,
    controller: Arc<AdmissionController>,
}

#[derive(Debug)]
struct QueueGuard {
    _queue_permit: OwnedSemaphorePermit,
    controller: Arc<AdmissionController>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Overloaded;

#[derive(Debug, Serialize)]
pub(crate) struct AdmissionSnapshot {
    pub queue_capacity: usize,
    pub worker_execution_capacity: usize,
    pub outstanding_capacity: usize,
    pub outstanding: usize,
    pub executing: usize,
    pub queued: usize,
    pub rejected_total: usize,
    pub max_observed_outstanding: usize,
    pub max_observed_executing: usize,
    pub max_observed_queued: usize,
}

impl AdmissionController {
    pub(crate) fn new(
        config: AdmissionConfig,
        worker_execution_capacity: usize,
    ) -> Result<Arc<Self>, String> {
        if config.queue_capacity > 1_000_000 {
            return Err("admission queue capacity must not exceed 1000000".to_owned());
        }
        if worker_execution_capacity == 0 {
            return Err("worker execution capacity must be greater than zero".to_owned());
        }
        let outstanding_capacity = worker_execution_capacity
            .checked_add(config.queue_capacity)
            .ok_or_else(|| "total admission capacity is too large".to_owned())?;

        Ok(Arc::new(Self {
            outstanding_slots: Arc::new(Semaphore::new(outstanding_capacity)),
            queue_slots: Arc::new(Semaphore::new(config.queue_capacity)),
            queue_capacity: config.queue_capacity,
            worker_execution_capacity,
            outstanding: AtomicUsize::new(0),
            executing: AtomicUsize::new(0),
            queued: AtomicUsize::new(0),
            rejected_total: AtomicUsize::new(0),
            max_observed_outstanding: AtomicUsize::new(0),
            max_observed_executing: AtomicUsize::new(0),
            max_observed_queued: AtomicUsize::new(0),
        }))
    }

    pub(crate) fn try_admit_request(
        self: &Arc<Self>,
    ) -> Result<RequestAdmissionPermit, Overloaded> {
        let permit = Arc::clone(&self.outstanding_slots)
            .try_acquire_owned()
            .map_err(|_| {
                self.record_rejection();
                Overloaded
            })?;
        increment_with_peak(&self.outstanding, &self.max_observed_outstanding);

        Ok(RequestAdmissionPermit {
            _inner: Arc::new(RequestPermitInner {
                _permit: permit,
                controller: Arc::clone(self),
            }),
        })
    }

    pub(crate) async fn admit_worker(
        self: &Arc<Self>,
        lease: &WorkerLease,
    ) -> Result<ExecutionGuard, Overloaded> {
        if let Some(worker_permit) = lease.try_reserve_execution() {
            return Ok(self.begin_execution(worker_permit));
        }

        let queue_permit = Arc::clone(&self.queue_slots)
            .try_acquire_owned()
            .map_err(|_| {
                self.record_rejection();
                Overloaded
            })?;
        increment_with_peak(&self.queued, &self.max_observed_queued);
        let queue_guard = QueueGuard {
            _queue_permit: queue_permit,
            controller: Arc::clone(self),
        };

        // Tokio semaphores wake waiters fairly. Cancellation drops QueueGuard, returning its
        // bounded waiting-room slot even if execution never begins.
        let worker_permit = lease.reserve_execution().await;
        drop(queue_guard);
        Ok(self.begin_execution(worker_permit))
    }

    pub(crate) fn snapshot(&self) -> AdmissionSnapshot {
        AdmissionSnapshot {
            queue_capacity: self.queue_capacity,
            worker_execution_capacity: self.worker_execution_capacity,
            outstanding_capacity: self.worker_execution_capacity + self.queue_capacity,
            outstanding: self.outstanding.load(Ordering::Relaxed),
            executing: self.executing.load(Ordering::Relaxed),
            queued: self.queued.load(Ordering::Relaxed),
            rejected_total: self.rejected_total.load(Ordering::Relaxed),
            max_observed_outstanding: self.max_observed_outstanding.load(Ordering::Relaxed),
            max_observed_executing: self.max_observed_executing.load(Ordering::Relaxed),
            max_observed_queued: self.max_observed_queued.load(Ordering::Relaxed),
        }
    }

    fn begin_execution(self: &Arc<Self>, worker_permit: WorkerExecutionPermit) -> ExecutionGuard {
        increment_with_peak(&self.executing, &self.max_observed_executing);
        ExecutionGuard {
            _worker_permit: worker_permit,
            controller: Arc::clone(self),
        }
    }

    fn record_rejection(&self) {
        self.rejected_total.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for RequestPermitInner {
    fn drop(&mut self) {
        self.controller.outstanding.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        self.controller.executing.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Drop for QueueGuard {
    fn drop(&mut self) {
        self.controller.queued.fetch_sub(1, Ordering::Relaxed);
    }
}

fn increment_with_peak(current: &AtomicUsize, peak: &AtomicUsize) {
    let value = current.fetch_add(1, Ordering::Relaxed) + 1;
    peak.fetch_max(value, Ordering::Relaxed);
}
