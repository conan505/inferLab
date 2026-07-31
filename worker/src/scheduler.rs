use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use serde::Serialize;
use tokio::{sync::mpsc, time::sleep};

use crate::{GenerationMetrics, Session, StepOutcome, StepTrace};

const TRACE_CAPACITY: usize = 1_024;

#[derive(Clone, Copy, Debug)]
pub struct SchedulerConfig {
    pub max_batch_size: usize,
    pub queue_capacity: usize,
    pub tick_delay: Duration,
}

impl SchedulerConfig {
    fn validate(self) -> Result<Self, String> {
        if self.max_batch_size == 0 {
            return Err("scheduler max batch size must be positive".to_owned());
        }
        if self.queue_capacity == 0 {
            return Err("scheduler queue capacity must be positive".to_owned());
        }
        Ok(self)
    }
}

#[derive(Clone)]
pub struct ContinuousBatchScheduler {
    sender: mpsc::Sender<Submission>,
    state: Arc<SchedulerState>,
}

pub struct ScheduledRequest {
    pub id: u64,
    pub prompt_tokens: u32,
    pub events: mpsc::UnboundedReceiver<SchedulerEvent>,
}

pub enum SchedulerEvent {
    Token(StepTrace),
    Finished {
        finish_reason: &'static str,
        metrics: GenerationMetrics,
    },
    Error(String),
}

struct Submission {
    id: u64,
    session: Session,
    events: mpsc::UnboundedSender<SchedulerEvent>,
}

struct ActiveSequence {
    submission: Submission,
    emitted_tokens: u64,
}

struct SchedulerState {
    config: SchedulerConfig,
    started: Instant,
    next_request_id: AtomicU64,
    queued: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    admitted: AtomicU64,
    completed: AtomicU64,
    cancelled: AtomicU64,
    failed: AtomicU64,
    batches: AtomicU64,
    token_steps: AtomicU64,
    slots_used: AtomicU64,
    slots_available: AtomicU64,
    trace_sequence: AtomicU64,
    trace: Mutex<VecDeque<SchedulerTraceEvent>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SchedulerTraceEvent {
    pub sequence: u64,
    pub at_us: u64,
    pub batch: u64,
    pub request_id: u64,
    pub event: &'static str,
    pub token_index: u64,
    pub active: usize,
    pub queued: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct SchedulerSnapshot {
    pub max_batch_size: usize,
    pub queue_capacity: usize,
    pub tick_delay_ms: u64,
    pub queued: usize,
    pub active: usize,
    pub max_active: usize,
    pub admitted: u64,
    pub completed: u64,
    pub cancelled: u64,
    pub failed: u64,
    pub batches: u64,
    pub token_steps: u64,
    pub slots_used: u64,
    pub slots_available: u64,
    pub slot_utilization_percent: f64,
    pub trace: Vec<SchedulerTraceEvent>,
}

impl ContinuousBatchScheduler {
    pub fn start(config: SchedulerConfig) -> Result<Self, String> {
        let config = config.validate()?;
        let (sender, receiver) = mpsc::channel(config.queue_capacity);
        let state = Arc::new(SchedulerState {
            config,
            started: Instant::now(),
            next_request_id: AtomicU64::new(0),
            queued: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            admitted: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            cancelled: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            batches: AtomicU64::new(0),
            token_steps: AtomicU64::new(0),
            slots_used: AtomicU64::new(0),
            slots_available: AtomicU64::new(0),
            trace_sequence: AtomicU64::new(0),
            trace: Mutex::new(VecDeque::with_capacity(TRACE_CAPACITY)),
        });
        tokio::spawn(run_scheduler(receiver, Arc::clone(&state)));
        Ok(Self { sender, state })
    }

    pub fn submit(&self, session: Session) -> Result<ScheduledRequest, String> {
        let id = self.state.next_request_id.fetch_add(1, Ordering::Relaxed) + 1;
        let prompt_tokens = session.prompt_tokens();
        let (events, receiver) = mpsc::unbounded_channel();
        self.state.queued.fetch_add(1, Ordering::Relaxed);
        if let Err(error) = self.sender.try_send(Submission {
            id,
            session,
            events,
        }) {
            self.state.queued.fetch_sub(1, Ordering::Relaxed);
            return Err(match error {
                mpsc::error::TrySendError::Full(_) => {
                    "continuous batch scheduler queue is full".to_owned()
                }
                mpsc::error::TrySendError::Closed(_) => {
                    "continuous batch scheduler is unavailable".to_owned()
                }
            });
        }
        Ok(ScheduledRequest {
            id,
            prompt_tokens,
            events: receiver,
        })
    }

    pub fn snapshot(&self) -> SchedulerSnapshot {
        let slots_used = self.state.slots_used.load(Ordering::Relaxed);
        let slots_available = self.state.slots_available.load(Ordering::Relaxed);
        let utilization = if slots_available == 0 {
            0.0
        } else {
            slots_used as f64 / slots_available as f64 * 100.0
        };
        let trace = self
            .state
            .trace
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect();
        SchedulerSnapshot {
            max_batch_size: self.state.config.max_batch_size,
            queue_capacity: self.state.config.queue_capacity,
            tick_delay_ms: u64::try_from(self.state.config.tick_delay.as_millis())
                .unwrap_or(u64::MAX),
            queued: self.state.queued.load(Ordering::Relaxed),
            active: self.state.active.load(Ordering::Relaxed),
            max_active: self.state.max_active.load(Ordering::Relaxed),
            admitted: self.state.admitted.load(Ordering::Relaxed),
            completed: self.state.completed.load(Ordering::Relaxed),
            cancelled: self.state.cancelled.load(Ordering::Relaxed),
            failed: self.state.failed.load(Ordering::Relaxed),
            batches: self.state.batches.load(Ordering::Relaxed),
            token_steps: self.state.token_steps.load(Ordering::Relaxed),
            slots_used,
            slots_available,
            slot_utilization_percent: utilization,
            trace,
        }
    }
}

async fn run_scheduler(mut receiver: mpsc::Receiver<Submission>, state: Arc<SchedulerState>) {
    let mut active = Vec::<ActiveSequence>::new();
    let mut previous_batch = 0;
    loop {
        if active.is_empty() {
            let Some(submission) = receiver.recv().await else {
                return;
            };
            admit(submission, &mut active, &state, previous_batch);
        }
        fill_available_slots(&mut receiver, &mut active, &state, previous_batch);
        if !state.config.tick_delay.is_zero() {
            sleep(state.config.tick_delay).await;
        }

        let batch = state.batches.fetch_add(1, Ordering::Relaxed) + 1;
        previous_batch = batch;
        state
            .slots_used
            .fetch_add(active.len() as u64, Ordering::Relaxed);
        state
            .slots_available
            .fetch_add(state.config.max_batch_size as u64, Ordering::Relaxed);

        let mut index = 0;
        while index < active.len() {
            let outcome = active[index].submission.session.next_token();
            state.token_steps.fetch_add(1, Ordering::Relaxed);
            match outcome {
                Ok(StepOutcome::Token(step)) => {
                    active[index].emitted_tokens += 1;
                    let token_index = active[index].emitted_tokens;
                    let request_id = active[index].submission.id;
                    if active[index]
                        .submission
                        .events
                        .send(SchedulerEvent::Token(step))
                        .is_err()
                    {
                        state.cancelled.fetch_add(1, Ordering::Relaxed);
                        active.swap_remove(index);
                        state.active.store(active.len(), Ordering::Relaxed);
                        record(
                            &state,
                            batch,
                            request_id,
                            "cancelled",
                            token_index,
                            active.len(),
                        );
                    } else {
                        record(
                            &state,
                            batch,
                            request_id,
                            "token",
                            token_index,
                            active.len(),
                        );
                        index += 1;
                    }
                }
                Ok(StepOutcome::EndOfSequence(_)) => {
                    finish(&mut active, index, &state, batch, "stop");
                }
                Ok(StepOutcome::Length) => {
                    finish(&mut active, index, &state, batch, "length");
                }
                Err(error) => {
                    let request_id = active[index].submission.id;
                    let token_index = active[index].emitted_tokens;
                    let _ = active[index]
                        .submission
                        .events
                        .send(SchedulerEvent::Error(error));
                    state.failed.fetch_add(1, Ordering::Relaxed);
                    active.swap_remove(index);
                    state.active.store(active.len(), Ordering::Relaxed);
                    record(
                        &state,
                        batch,
                        request_id,
                        "failed",
                        token_index,
                        active.len(),
                    );
                }
            }
        }
        state.active.store(active.len(), Ordering::Relaxed);
        fill_available_slots(&mut receiver, &mut active, &state, batch);
    }
}

fn fill_available_slots(
    receiver: &mut mpsc::Receiver<Submission>,
    active: &mut Vec<ActiveSequence>,
    state: &SchedulerState,
    batch: u64,
) {
    while active.len() < state.config.max_batch_size {
        let Ok(submission) = receiver.try_recv() else {
            break;
        };
        admit(submission, active, state, batch);
    }
}

fn admit(
    submission: Submission,
    active: &mut Vec<ActiveSequence>,
    state: &SchedulerState,
    batch: u64,
) {
    state.queued.fetch_sub(1, Ordering::Relaxed);
    state.admitted.fetch_add(1, Ordering::Relaxed);
    active.push(ActiveSequence {
        submission,
        emitted_tokens: 0,
    });
    state.active.store(active.len(), Ordering::Relaxed);
    state.max_active.fetch_max(active.len(), Ordering::Relaxed);
    let request_id = active.last().expect("just pushed").submission.id;
    record(state, batch, request_id, "admitted", 0, active.len());
}

fn finish(
    active: &mut Vec<ActiveSequence>,
    index: usize,
    state: &SchedulerState,
    batch: u64,
    finish_reason: &'static str,
) {
    let sequence = active.swap_remove(index);
    let request_id = sequence.submission.id;
    let token_index = sequence.emitted_tokens;
    let metrics = sequence.submission.session.metrics();
    state.active.store(active.len(), Ordering::Relaxed);
    let delivered = sequence
        .submission
        .events
        .send(SchedulerEvent::Finished {
            finish_reason,
            metrics,
        })
        .is_ok();
    if delivered {
        state.completed.fetch_add(1, Ordering::Relaxed);
        record(
            state,
            batch,
            request_id,
            "completed",
            token_index,
            active.len(),
        );
    } else {
        state.cancelled.fetch_add(1, Ordering::Relaxed);
        record(
            state,
            batch,
            request_id,
            "cancelled",
            token_index,
            active.len(),
        );
    }
}

fn record(
    state: &SchedulerState,
    batch: u64,
    request_id: u64,
    event: &'static str,
    token_index: u64,
    active: usize,
) {
    let sequence = state.trace_sequence.fetch_add(1, Ordering::Relaxed) + 1;
    let at_us = u64::try_from(state.started.elapsed().as_micros()).unwrap_or(u64::MAX);
    let queued = state.queued.load(Ordering::Relaxed);
    let mut trace = state
        .trace
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if trace.len() == TRACE_CAPACITY {
        trace.pop_front();
    }
    trace.push_back(SchedulerTraceEvent {
        sequence,
        at_us,
        batch,
        request_id,
        event,
        token_index,
        active,
        queued,
    });
}

#[cfg(test)]
mod tests {
    use super::{ContinuousBatchScheduler, SchedulerConfig, SchedulerEvent};
    use crate::{DecoderMode, Model};
    use std::{path::PathBuf, time::Duration};

    fn model() -> Model {
        Model::load(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../models/tiny-inferlab-v1.bin"),
        )
        .expect("model")
    }

    #[tokio::test]
    async fn continuously_backfills_a_finished_slot() {
        let scheduler = ContinuousBatchScheduler::start(SchedulerConfig {
            max_batch_size: 2,
            queue_capacity: 4,
            tick_delay: Duration::ZERO,
        })
        .expect("scheduler");
        let model = model();
        let mut short = scheduler
            .submit(
                model
                    .session_with_mode("hello", 2, DecoderMode::KvCache)
                    .expect("short session"),
            )
            .expect("short request");
        let mut long = scheduler
            .submit(
                model
                    .session_with_mode("hello", 8, DecoderMode::KvCache)
                    .expect("long session"),
            )
            .expect("long request");
        let mut backfill = scheduler
            .submit(
                model
                    .session_with_mode("hello", 2, DecoderMode::KvCache)
                    .expect("backfill session"),
            )
            .expect("backfill request");

        for receiver in [&mut short.events, &mut long.events, &mut backfill.events] {
            while let Some(event) = receiver.recv().await {
                if matches!(event, SchedulerEvent::Finished { .. }) {
                    break;
                }
            }
        }
        let snapshot = scheduler.snapshot();
        assert_eq!(snapshot.completed, 3);
        assert_eq!(snapshot.max_active, 2);
        let short_done = snapshot
            .trace
            .iter()
            .find(|event| event.request_id == short.id && event.event == "completed")
            .expect("short completion");
        let backfill_admitted = snapshot
            .trace
            .iter()
            .find(|event| event.request_id == backfill.id && event.event == "admitted")
            .expect("backfill admission");
        let long_done = snapshot
            .trace
            .iter()
            .find(|event| event.request_id == long.id && event.event == "completed")
            .expect("long completion");
        assert!(backfill_admitted.sequence > short_done.sequence);
        assert!(backfill_admitted.sequence < long_done.sequence);
    }

    #[test]
    fn rejects_invalid_scheduler_bounds() {
        assert!(
            ContinuousBatchScheduler::start(SchedulerConfig {
                max_batch_size: 0,
                queue_capacity: 1,
                tick_delay: Duration::ZERO,
            })
            .is_err()
        );
    }
}
