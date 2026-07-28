use std::{
    collections::HashSet,
    fmt,
    str::FromStr,
    sync::{
        Arc, Mutex,
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoutingPolicy {
    #[default]
    RoundRobin,
    LeastInFlight,
}

#[derive(Debug)]
pub struct WorkerPool {
    workers: Vec<Arc<Worker>>,
    next: AtomicUsize,
    policy: RoutingPolicy,
    selection_lock: Mutex<()>,
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
        Self::with_policy(workers, RoutingPolicy::RoundRobin)
    }

    pub fn with_policy(
        workers: Vec<(String, String)>,
        policy: RoutingPolicy,
    ) -> Result<Self, String> {
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
            policy,
            selection_lock: Mutex::new(()),
        })
    }

    pub fn choose(&self) -> WorkerLease {
        match self.policy {
            RoutingPolicy::RoundRobin => self.choose_round_robin(),
            RoutingPolicy::LeastInFlight => self.choose_least_in_flight(),
        }
    }

    pub fn choose_round_robin(&self) -> WorkerLease {
        let index = self.next.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        self.lease(index)
    }

    pub fn choose_least_in_flight(&self) -> WorkerLease {
        // Selection and reservation are one logical operation. Without this short lock, concurrent
        // requests could all observe the same minimum before any of them increments it.
        let _selection = self
            .selection_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let start = self.next.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        let mut selected = start;
        let mut minimum = self.workers[start].in_flight.load(Ordering::Relaxed);

        // Begin at a rotating index so equal workers take turns instead of always favoring worker 0.
        for offset in 1..self.workers.len() {
            let index = (start + offset) % self.workers.len();
            let in_flight = self.workers[index].in_flight.load(Ordering::Relaxed);
            if in_flight < minimum {
                selected = index;
                minimum = in_flight;
            }
        }

        self.lease(selected)
    }

    pub fn policy(&self) -> RoutingPolicy {
        self.policy
    }

    fn lease(&self, index: usize) -> WorkerLease {
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

impl FromStr for RoutingPolicy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "round-robin" | "round_robin" | "rr" => Ok(Self::RoundRobin),
            "least-in-flight" | "least_in_flight" | "lif" => Ok(Self::LeastInFlight),
            _ => Err(format!(
                "unknown routing policy '{value}'; expected round-robin or least-in-flight"
            )),
        }
    }
}

impl fmt::Display for RoutingPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RoundRobin => formatter.write_str("round-robin"),
            Self::LeastInFlight => formatter.write_str("least-in-flight"),
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
    use std::str::FromStr;

    use super::{RoutingPolicy, WorkerPool};

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
    fn parses_human_friendly_policy_names() {
        assert_eq!(
            RoutingPolicy::from_str("round-robin"),
            Ok(RoutingPolicy::RoundRobin)
        );
        assert_eq!(
            RoutingPolicy::from_str("lif"),
            Ok(RoutingPolicy::LeastInFlight)
        );
        assert!(RoutingPolicy::from_str("random").is_err());
    }
}
