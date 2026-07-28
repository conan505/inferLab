use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use serde::Serialize;

#[derive(Debug)]
struct Worker {
    id: String,
    base_url: String,
    in_flight: AtomicUsize,
}

#[derive(Debug)]
pub struct WorkerPool {
    workers: Vec<Arc<Worker>>,
    next: AtomicUsize,
}

#[derive(Debug, Serialize)]
pub struct WorkerSnapshot {
    pub id: String,
    pub base_url: String,
    pub in_flight: usize,
}

#[derive(Debug)]
pub struct WorkerLease {
    worker: Arc<Worker>,
}

impl WorkerPool {
    pub fn new(workers: Vec<(String, String)>) -> Result<Self, String> {
        if workers.is_empty() {
            return Err("at least one worker is required".to_owned());
        }
        if workers
            .iter()
            .any(|(id, base_url)| id.trim().is_empty() || base_url.trim().is_empty())
        {
            return Err("worker IDs and URLs must not be empty".to_owned());
        }
        let unique_ids: HashSet<&str> = workers.iter().map(|(id, _)| id.trim()).collect();
        if unique_ids.len() != workers.len() {
            return Err("worker IDs must be unique".to_owned());
        }

        Ok(Self {
            workers: workers
                .into_iter()
                .map(|(id, base_url)| {
                    Arc::new(Worker {
                        id: id.trim().to_owned(),
                        base_url: base_url.trim_end_matches('/').to_owned(),
                        in_flight: AtomicUsize::new(0),
                    })
                })
                .collect(),
            next: AtomicUsize::new(0),
        })
    }

    pub fn choose_round_robin(&self) -> WorkerLease {
        let index = self.next.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        let worker = Arc::clone(&self.workers[index]);
        worker.in_flight.fetch_add(1, Ordering::Relaxed);
        WorkerLease { worker }
    }

    pub fn snapshots(&self) -> Vec<WorkerSnapshot> {
        self.workers
            .iter()
            .map(|worker| WorkerSnapshot {
                id: worker.id.clone(),
                base_url: worker.base_url.clone(),
                in_flight: worker.in_flight.load(Ordering::Relaxed),
            })
            .collect()
    }
}

impl WorkerLease {
    pub fn id(&self) -> &str {
        &self.worker.id
    }

    pub fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.worker.base_url, path)
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
    use super::WorkerPool;

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
    fn lease_tracks_in_flight_until_drop() {
        let pool =
            WorkerPool::new(vec![("a".to_owned(), "http://a".to_owned())]).expect("valid pool");
        let lease = pool.choose_round_robin();
        assert_eq!(pool.snapshots()[0].in_flight, 1);
        drop(lease);
        assert_eq!(pool.snapshots()[0].in_flight, 0);
    }
}
