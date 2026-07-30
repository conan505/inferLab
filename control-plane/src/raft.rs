use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::future::join_all;
use reqwest::Client;
use tokio::{
    sync::Mutex as AsyncMutex,
    time::{Instant, sleep, timeout},
};
use tracing::{info, warn};

use crate::{
    RaftError,
    model::{
        AppendEntriesRequest, AppendEntriesResponse, Command, CommittedConfiguration, LogEntry,
        NodeStatus, PersistentState, RequestVoteRequest, RequestVoteResponse, Role,
        RoutingConfiguration, TraceEvent,
    },
    storage::{EventJournal, StableStorage},
};

#[derive(Clone, Debug)]
pub struct Peer {
    pub id: String,
    pub base_url: String,
}

#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub node_id: String,
    pub peers: Vec<Peer>,
    pub state_path: PathBuf,
    pub event_path: PathBuf,
    pub election_timeout_min: Duration,
    pub election_timeout_max: Duration,
    pub heartbeat_interval: Duration,
    pub rpc_timeout: Duration,
    pub commit_timeout: Duration,
}

impl NodeConfig {
    fn validate(&self) -> Result<(), RaftError> {
        if self.node_id.trim().is_empty() {
            return Err(RaftError::Invalid("node_id must not be empty".to_owned()));
        }
        if self.peers.len() != 2 {
            return Err(RaftError::Invalid(
                "v0.6 requires exactly two peers for a three-node cluster".to_owned(),
            ));
        }
        let mut identities = std::collections::HashSet::new();
        identities.insert(self.node_id.as_str());
        for peer in &self.peers {
            if peer.id.trim().is_empty() || peer.base_url.trim().is_empty() {
                return Err(RaftError::Invalid(
                    "peer IDs and base URLs must not be empty".to_owned(),
                ));
            }
            if !identities.insert(peer.id.as_str()) {
                return Err(RaftError::Invalid(format!(
                    "node or peer ID {} is duplicated",
                    peer.id
                )));
            }
        }
        if self.election_timeout_min >= self.election_timeout_max {
            return Err(RaftError::Invalid(
                "election timeout minimum must be less than maximum".to_owned(),
            ));
        }
        if self.heartbeat_interval >= self.election_timeout_min {
            return Err(RaftError::Invalid(
                "heartbeat interval must be shorter than the election timeout".to_owned(),
            ));
        }
        if self.rpc_timeout.is_zero() || self.commit_timeout.is_zero() {
            return Err(RaftError::Invalid(
                "RPC and commit timeouts must be positive".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
struct VolatileState {
    persistent: PersistentState,
    role: Role,
    leader_id: Option<String>,
    last_applied: u64,
    committed_configuration: Option<CommittedConfiguration>,
    election_deadline: Instant,
    heartbeat_deadline: Instant,
    timeout_resets: u64,
    next_index: HashMap<String, u64>,
    match_index: HashMap<String, u64>,
    elections_started: u64,
    leadership_terms: u64,
    votes_granted: u64,
    append_entries_accepted: u64,
    append_entries_rejected: u64,
    replication_successes: u64,
    replication_failures: u64,
    recovering_from_disk: bool,
    storage_error: Option<String>,
}

#[derive(Debug)]
pub struct RaftNode {
    config: NodeConfig,
    state: Mutex<VolatileState>,
    storage: StableStorage,
    journal: EventJournal,
    client: Client,
    campaign_lock: AsyncMutex<()>,
    replication_lock: AsyncMutex<()>,
    proposal_lock: AsyncMutex<()>,
}

impl RaftNode {
    pub fn open(config: NodeConfig) -> Result<Arc<Self>, RaftError> {
        config.validate()?;
        let storage = StableStorage::new(&config.state_path)?;
        let persistent = storage.load()?;
        let (last_applied, committed_configuration) = apply_committed(&persistent)?;
        let recovering_from_disk = !persistent.log.is_empty();
        let journal = EventJournal::open(&config.event_path)?;
        let now = Instant::now();
        let mut state = VolatileState {
            persistent,
            role: Role::Follower,
            leader_id: None,
            last_applied,
            committed_configuration,
            election_deadline: now,
            heartbeat_deadline: now,
            timeout_resets: 0,
            next_index: HashMap::new(),
            match_index: HashMap::new(),
            elections_started: 0,
            leadership_terms: 0,
            votes_granted: 0,
            append_entries_accepted: 0,
            append_entries_rejected: 0,
            replication_successes: 0,
            replication_failures: 0,
            recovering_from_disk,
            storage_error: None,
        };
        reset_election_deadline(&config, &mut state);
        let node = Arc::new(Self {
            config,
            state: Mutex::new(state),
            storage,
            journal,
            client: Client::new(),
            campaign_lock: AsyncMutex::new(()),
            replication_lock: AsyncMutex::new(()),
            proposal_lock: AsyncMutex::new(()),
        });
        {
            let state = node.lock_state()?;
            node.trace_locked(
                &state,
                "node_started",
                None,
                format!(
                    "replayed {} log entries through commit index {}",
                    state.persistent.log.len(),
                    state.persistent.commit_index
                ),
            )?;
        }
        Ok(node)
    }

    pub fn spawn_background(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let node = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_millis(10)).await;
                let action = match node.lock_state() {
                    Ok(state) if state.storage_error.is_some() => BackgroundAction::None,
                    Ok(state)
                        if state.role == Role::Leader
                            && Instant::now() >= state.heartbeat_deadline =>
                    {
                        BackgroundAction::Replicate
                    }
                    Ok(state)
                        if state.role != Role::Leader
                            && Instant::now() >= state.election_deadline =>
                    {
                        BackgroundAction::Campaign
                    }
                    Ok(_) => BackgroundAction::None,
                    Err(error) => {
                        warn!(%error, "Raft background loop cannot read state");
                        BackgroundAction::None
                    }
                };
                match action {
                    BackgroundAction::Campaign => {
                        if let Err(error) = node.campaign().await {
                            warn!(node = %node.config.node_id, %error, "election attempt failed");
                        }
                    }
                    BackgroundAction::Replicate => {
                        if let Err(error) = node.replicate_round().await {
                            warn!(node = %node.config.node_id, %error, "replication round failed");
                        }
                    }
                    BackgroundAction::None => {}
                }
            }
        })
    }

    pub fn status(&self) -> Result<NodeStatus, RaftError> {
        let state = self.lock_state()?;
        let (last_log_index, last_log_term) = last_log_position(&state.persistent);
        Ok(NodeStatus {
            node_id: self.config.node_id.clone(),
            role: state.role.clone(),
            term: state.persistent.current_term,
            leader_id: state.leader_id.clone(),
            voted_for: state.persistent.voted_for.clone(),
            commit_index: state.persistent.commit_index,
            last_applied: state.last_applied,
            last_log_index,
            last_log_term,
            committed_configuration: state.committed_configuration.clone(),
            elections_started: state.elections_started,
            leadership_terms: state.leadership_terms,
            votes_granted: state.votes_granted,
            append_entries_accepted: state.append_entries_accepted,
            append_entries_rejected: state.append_entries_rejected,
            replication_successes: state.replication_successes,
            replication_failures: state.replication_failures,
            storage_healthy: state.storage_error.is_none(),
            peers: self
                .config
                .peers
                .iter()
                .map(|peer| peer.id.clone())
                .collect(),
        })
    }

    pub fn committed_configuration(&self) -> Result<CommittedConfiguration, RaftError> {
        self.lock_state()?
            .committed_configuration
            .clone()
            .ok_or_else(|| {
                RaftError::Unavailable("no routing configuration has been committed".to_owned())
            })
    }

    pub fn handle_request_vote(
        &self,
        request: RequestVoteRequest,
    ) -> Result<RequestVoteResponse, RaftError> {
        let mut state = self.lock_state()?;
        self.require_storage(&state)?;
        let mut persistent_changed = false;
        if request.term > state.persistent.current_term {
            become_follower(&self.config, &mut state, request.term, None);
            persistent_changed = true;
        }
        if request.term < state.persistent.current_term {
            return Ok(RequestVoteResponse {
                term: state.persistent.current_term,
                vote_granted: false,
            });
        }

        let (last_index, last_term) = last_log_position(&state.persistent);
        let candidate_is_current = request.last_log_term > last_term
            || (request.last_log_term == last_term && request.last_log_index >= last_index);
        let can_vote = state.persistent.voted_for.is_none()
            || state.persistent.voted_for.as_deref() == Some(&request.candidate_id);
        let vote_granted = can_vote && candidate_is_current;
        if vote_granted {
            state.persistent.voted_for = Some(request.candidate_id.clone());
            state.votes_granted = state.votes_granted.saturating_add(1);
            reset_election_deadline(&self.config, &mut state);
            persistent_changed = true;
        }
        if persistent_changed {
            self.persist_locked(&mut state)?;
        }
        if vote_granted {
            self.trace_locked(
                &state,
                "vote_granted",
                Some(request.last_log_index),
                format!("voted for {}", request.candidate_id),
            )?;
        }
        Ok(RequestVoteResponse {
            term: state.persistent.current_term,
            vote_granted,
        })
    }

    pub fn handle_append_entries(
        &self,
        request: AppendEntriesRequest,
    ) -> Result<AppendEntriesResponse, RaftError> {
        let mut state = self.lock_state()?;
        self.require_storage(&state)?;
        if request.term < state.persistent.current_term {
            state.append_entries_rejected = state.append_entries_rejected.saturating_add(1);
            return Ok(AppendEntriesResponse {
                term: state.persistent.current_term,
                success: false,
                match_index: last_log_position(&state.persistent).0,
            });
        }

        let previous_role = state.role.clone();
        let previous_term = state.persistent.current_term;
        let mut persistent_changed = false;
        if request.term > state.persistent.current_term {
            become_follower(
                &self.config,
                &mut state,
                request.term,
                Some(request.leader_id.clone()),
            );
            persistent_changed = true;
        } else {
            if state.role != Role::Follower {
                state.role = Role::Follower;
            }
            state.leader_id = Some(request.leader_id.clone());
            reset_election_deadline(&self.config, &mut state);
        }
        // A higher term must reach stable storage before any later validation
        // can reject the RPC. Otherwise a crash after the rejection could let
        // this node vote again in an obsolete term.
        if persistent_changed {
            self.persist_locked(&mut state)?;
            persistent_changed = false;
        }

        if !previous_matches(
            &state.persistent,
            request.prev_log_index,
            request.prev_log_term,
        ) {
            state.append_entries_rejected = state.append_entries_rejected.saturating_add(1);
            return Ok(AppendEntriesResponse {
                term: state.persistent.current_term,
                success: false,
                match_index: last_log_position(&state.persistent).0,
            });
        }
        validate_incoming_entries(request.prev_log_index, &request.entries)?;

        let mut log_changed = false;
        for (offset, incoming) in request.entries.iter().enumerate() {
            let index = request
                .prev_log_index
                .saturating_add(u64::try_from(offset).unwrap_or(u64::MAX))
                .saturating_add(1);
            let position = usize::try_from(index.saturating_sub(1)).map_err(|_| {
                RaftError::Invalid("incoming log index does not fit in memory".to_owned())
            })?;
            if let Some(existing) = state.persistent.log.get(position) {
                if existing != incoming {
                    if index <= state.persistent.commit_index {
                        return Err(RaftError::Storage(format!(
                            "leader attempted to overwrite committed log index {index}"
                        )));
                    }
                    state.persistent.log.truncate(position);
                    state
                        .persistent
                        .log
                        .extend_from_slice(&request.entries[offset..]);
                    persistent_changed = true;
                    log_changed = true;
                    break;
                }
            } else {
                state
                    .persistent
                    .log
                    .extend_from_slice(&request.entries[offset..]);
                persistent_changed = true;
                log_changed = true;
                break;
            }
        }

        let matched_index = request
            .prev_log_index
            .saturating_add(u64::try_from(request.entries.len()).unwrap_or(u64::MAX));
        let new_commit = request
            .leader_commit
            .min(u64::try_from(state.persistent.log.len()).unwrap_or(u64::MAX));
        if new_commit > state.persistent.commit_index {
            state.persistent.commit_index = new_commit;
            apply_committed_locked(&mut state)?;
            persistent_changed = true;
        }
        if persistent_changed {
            self.persist_locked(&mut state)?;
        }
        let repaired = state.recovering_from_disk && log_changed;
        if repaired {
            state.recovering_from_disk = false;
        }
        state.append_entries_accepted = state.append_entries_accepted.saturating_add(1);
        if previous_term < state.persistent.current_term || previous_role != Role::Follower {
            self.trace_locked(
                &state,
                "leader_observed",
                Some(matched_index),
                format!("following {}", request.leader_id),
            )?;
        }
        if repaired {
            self.trace_locked(
                &state,
                "log_repaired",
                Some(matched_index),
                format!("accepted authoritative suffix from {}", request.leader_id),
            )?;
        }
        Ok(AppendEntriesResponse {
            term: state.persistent.current_term,
            success: true,
            match_index: matched_index,
        })
    }

    pub async fn write_configuration(
        self: &Arc<Self>,
        configuration: RoutingConfiguration,
    ) -> Result<CommittedConfiguration, RaftError> {
        // Serialize client proposals so each successful response names the
        // configuration appended by that request, even when clients race.
        let _proposal = self.proposal_lock.lock().await;
        configuration.validate()?;
        let (entry_index, entry_term) = {
            let mut state = self.lock_state()?;
            self.require_storage(&state)?;
            if state.role != Role::Leader {
                return Err(RaftError::NotLeader {
                    leader_id: state.leader_id.clone(),
                });
            }
            let index = u64::try_from(state.persistent.log.len())
                .unwrap_or(u64::MAX)
                .saturating_add(1);
            let term = state.persistent.current_term;
            state.persistent.log.push(LogEntry {
                index,
                term,
                command: Command::SetRoutingConfiguration { configuration },
            });
            state.match_index.insert(self.config.node_id.clone(), index);
            self.persist_locked(&mut state)?;
            self.trace_locked(
                &state,
                "entry_appended",
                Some(index),
                "leader accepted routing configuration".to_owned(),
            )?;
            (index, term)
        };

        let deadline = Instant::now() + self.config.commit_timeout;
        loop {
            self.replicate_round().await?;
            {
                let state = self.lock_state()?;
                self.require_storage(&state)?;
                if state.persistent.commit_index >= entry_index {
                    return state.committed_configuration.clone().ok_or_else(|| {
                        RaftError::Storage(format!(
                            "configuration entry {entry_index} committed but was not applied"
                        ))
                    });
                }
                if state.role != Role::Leader || state.persistent.current_term != entry_term {
                    return Err(RaftError::NotLeader {
                        leader_id: state.leader_id.clone(),
                    });
                }
            }
            if Instant::now() >= deadline {
                return Err(RaftError::Unavailable(format!(
                    "configuration at log index {entry_index} was not committed before timeout"
                )));
            }
            sleep(Duration::from_millis(15)).await;
        }
    }

    async fn campaign(self: &Arc<Self>) -> Result<(), RaftError> {
        let _campaign = self.campaign_lock.lock().await;
        {
            let state = self.lock_state()?;
            if state.role == Role::Leader || Instant::now() < state.election_deadline {
                return Ok(());
            }
        }

        let request = {
            let mut state = self.lock_state()?;
            self.require_storage(&state)?;
            state.role = Role::Candidate;
            state.leader_id = None;
            state.persistent.current_term = state.persistent.current_term.saturating_add(1);
            state.persistent.voted_for = Some(self.config.node_id.clone());
            state.elections_started = state.elections_started.saturating_add(1);
            reset_election_deadline(&self.config, &mut state);
            self.persist_locked(&mut state)?;
            let (last_log_index, last_log_term) = last_log_position(&state.persistent);
            self.trace_locked(
                &state,
                "election_started",
                Some(last_log_index),
                format!("campaigning in term {}", state.persistent.current_term),
            )?;
            RequestVoteRequest {
                term: state.persistent.current_term,
                candidate_id: self.config.node_id.clone(),
                last_log_index,
                last_log_term,
            }
        };

        let requests = self.config.peers.iter().cloned().map(|peer| {
            let client = self.client.clone();
            let request = request.clone();
            let timeout_duration = self.config.rpc_timeout;
            async move {
                let response = timeout(
                    timeout_duration,
                    client
                        .post(format!("{}/raft/request-vote", peer.base_url))
                        .json(&request)
                        .send(),
                )
                .await;
                let parsed = match response {
                    Ok(Ok(response)) if response.status().is_success() => {
                        response.json::<RequestVoteResponse>().await.ok()
                    }
                    _ => None,
                };
                (peer.id, parsed)
            }
        });
        let responses = join_all(requests).await;
        let mut votes = 1_usize;
        let mut highest_term = request.term;
        for (_, response) in responses {
            if let Some(response) = response {
                highest_term = highest_term.max(response.term);
                if response.term == request.term && response.vote_granted {
                    votes += 1;
                }
            }
        }

        let became_leader = {
            let mut state = self.lock_state()?;
            if highest_term > state.persistent.current_term {
                become_follower(&self.config, &mut state, highest_term, None);
                self.persist_locked(&mut state)?;
                self.trace_locked(
                    &state,
                    "stepped_down",
                    None,
                    format!("observed higher term {highest_term} while campaigning"),
                )?;
                false
            } else if state.role == Role::Candidate
                && state.persistent.current_term == request.term
                && votes >= self.majority()
            {
                state.role = Role::Leader;
                state.leader_id = Some(self.config.node_id.clone());
                state.leadership_terms = state.leadership_terms.saturating_add(1);
                let noop_index = u64::try_from(state.persistent.log.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(1);
                let term = state.persistent.current_term;
                state.persistent.log.push(LogEntry {
                    index: noop_index,
                    term,
                    command: Command::Noop,
                });
                let next = noop_index.saturating_add(1);
                state.next_index = self
                    .config
                    .peers
                    .iter()
                    .map(|peer| (peer.id.clone(), next))
                    .collect();
                state.match_index = self
                    .config
                    .peers
                    .iter()
                    .map(|peer| (peer.id.clone(), 0))
                    .collect();
                state
                    .match_index
                    .insert(self.config.node_id.clone(), noop_index);
                state.heartbeat_deadline = Instant::now();
                self.persist_locked(&mut state)?;
                self.trace_locked(
                    &state,
                    "leader_elected",
                    Some(noop_index),
                    format!("won {votes} of {} votes", self.cluster_size()),
                )?;
                info!(
                    node = %self.config.node_id,
                    term = request.term,
                    votes,
                    "Raft leader elected"
                );
                true
            } else {
                false
            }
        };
        if became_leader {
            self.replicate_round().await?;
        }
        Ok(())
    }

    async fn replicate_round(self: &Arc<Self>) -> Result<(), RaftError> {
        let _replication = self.replication_lock.lock().await;
        let (term, requests) = {
            let mut state = self.lock_state()?;
            self.require_storage(&state)?;
            if state.role != Role::Leader {
                return Ok(());
            }
            state.heartbeat_deadline = Instant::now() + self.config.heartbeat_interval;
            let term = state.persistent.current_term;
            let requests = self
                .config
                .peers
                .iter()
                .map(|peer| {
                    let next_index = state.next_index.get(&peer.id).copied().unwrap_or_else(|| {
                        u64::try_from(state.persistent.log.len())
                            .unwrap_or(u64::MAX)
                            .saturating_add(1)
                    });
                    let prev_log_index = next_index.saturating_sub(1);
                    let prev_log_term = log_term(&state.persistent, prev_log_index).unwrap_or(0);
                    let start = usize::try_from(next_index.saturating_sub(1))
                        .unwrap_or(usize::MAX)
                        .min(state.persistent.log.len());
                    (
                        peer.clone(),
                        AppendEntriesRequest {
                            term,
                            leader_id: self.config.node_id.clone(),
                            prev_log_index,
                            prev_log_term,
                            entries: state.persistent.log[start..].to_vec(),
                            leader_commit: state.persistent.commit_index,
                        },
                    )
                })
                .collect::<Vec<_>>();
            (term, requests)
        };

        let sends = requests.into_iter().map(|(peer, request)| {
            let client = self.client.clone();
            let timeout_duration = self.config.rpc_timeout;
            async move {
                let result = timeout(
                    timeout_duration,
                    client
                        .post(format!("{}/raft/append-entries", peer.base_url))
                        .json(&request)
                        .send(),
                )
                .await;
                let parsed = match result {
                    Ok(Ok(response)) if response.status().is_success() => {
                        response.json::<AppendEntriesResponse>().await.ok()
                    }
                    _ => None,
                };
                (peer.id, parsed)
            }
        });
        let responses = join_all(sends).await;

        let mut state = self.lock_state()?;
        if state.role != Role::Leader || state.persistent.current_term != term {
            return Ok(());
        }
        for (peer_id, response) in responses {
            match response {
                Some(response) if response.term > state.persistent.current_term => {
                    become_follower(&self.config, &mut state, response.term, None);
                    self.persist_locked(&mut state)?;
                    self.trace_locked(
                        &state,
                        "stepped_down",
                        None,
                        format!("{} reported higher term {}", peer_id, response.term),
                    )?;
                    return Ok(());
                }
                Some(response) if response.success => {
                    state
                        .match_index
                        .insert(peer_id.clone(), response.match_index);
                    state
                        .next_index
                        .insert(peer_id, response.match_index.saturating_add(1));
                    state.replication_successes = state.replication_successes.saturating_add(1);
                }
                Some(_) => {
                    let next = state.next_index.get(&peer_id).copied().unwrap_or(1);
                    state
                        .next_index
                        .insert(peer_id, next.saturating_sub(1).max(1));
                    state.replication_failures = state.replication_failures.saturating_add(1);
                }
                None => {
                    state.replication_failures = state.replication_failures.saturating_add(1);
                }
            }
        }
        let old_commit = state.persistent.commit_index;
        let match_indexes =
            std::iter::once(u64::try_from(state.persistent.log.len()).unwrap_or(u64::MAX))
                .chain(
                    self.config
                        .peers
                        .iter()
                        .map(|peer| state.match_index.get(&peer.id).copied().unwrap_or(0)),
                )
                .collect::<Vec<_>>();
        if let Some(candidate) = highest_committable_index(
            &state.persistent,
            state.persistent.current_term,
            &match_indexes,
            self.majority(),
        ) && candidate > old_commit
        {
            let replicated = match_indexes
                .iter()
                .filter(|matched| **matched >= candidate)
                .count();
            state.persistent.commit_index = candidate;
            apply_committed_locked(&mut state)?;
            self.persist_locked(&mut state)?;
            self.trace_locked(
                &state,
                "entry_committed",
                Some(candidate),
                format!(
                    "replicated on {replicated} of {} nodes",
                    self.cluster_size()
                ),
            )?;
        }
        Ok(())
    }

    fn persist_locked(&self, state: &mut VolatileState) -> Result<(), RaftError> {
        if let Err(error) = self.storage.save(&state.persistent) {
            state.storage_error = Some(error.to_string());
            return Err(error);
        }
        Ok(())
    }

    fn trace_locked(
        &self,
        state: &VolatileState,
        event: &str,
        log_index: Option<u64>,
        detail: String,
    ) -> Result<(), RaftError> {
        self.journal.record(&TraceEvent {
            at_ms: now_ms(),
            node_id: self.config.node_id.clone(),
            event: event.to_owned(),
            term: state.persistent.current_term,
            role: state.role.clone(),
            leader_id: state.leader_id.clone(),
            log_index,
            detail,
        })
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, VolatileState>, RaftError> {
        self.state
            .lock()
            .map_err(|_| RaftError::Storage("Raft state mutex was poisoned".to_owned()))
    }

    fn require_storage(&self, state: &VolatileState) -> Result<(), RaftError> {
        match &state.storage_error {
            Some(error) => Err(RaftError::Storage(format!(
                "node is unavailable after storage failure: {error}"
            ))),
            None => Ok(()),
        }
    }

    fn cluster_size(&self) -> usize {
        self.config.peers.len() + 1
    }

    fn majority(&self) -> usize {
        self.cluster_size() / 2 + 1
    }
}

#[derive(Clone, Copy)]
enum BackgroundAction {
    None,
    Campaign,
    Replicate,
}

fn become_follower(
    config: &NodeConfig,
    state: &mut VolatileState,
    term: u64,
    leader_id: Option<String>,
) {
    if term > state.persistent.current_term {
        state.persistent.current_term = term;
        state.persistent.voted_for = None;
    }
    state.role = Role::Follower;
    state.leader_id = leader_id;
    state.next_index.clear();
    state.match_index.clear();
    reset_election_deadline(config, state);
}

fn reset_election_deadline(config: &NodeConfig, state: &mut VolatileState) {
    state.timeout_resets = state.timeout_resets.saturating_add(1);
    let minimum = u64::try_from(config.election_timeout_min.as_millis()).unwrap_or(u64::MAX);
    let maximum = u64::try_from(config.election_timeout_max.as_millis()).unwrap_or(u64::MAX);
    let width = maximum.saturating_sub(minimum);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    config.node_id.hash(&mut hasher);
    state.persistent.current_term.hash(&mut hasher);
    state.timeout_resets.hash(&mut hasher);
    now_ms().hash(&mut hasher);
    let jitter = if width == 0 {
        0
    } else {
        hasher.finish() % width
    };
    state.election_deadline = Instant::now() + Duration::from_millis(minimum + jitter);
}

fn previous_matches(state: &PersistentState, index: u64, term: u64) -> bool {
    index == 0 && term == 0 || log_term(state, index) == Some(term)
}

fn validate_incoming_entries(previous_index: u64, entries: &[LogEntry]) -> Result<(), RaftError> {
    for (offset, entry) in entries.iter().enumerate() {
        let expected = previous_index
            .saturating_add(u64::try_from(offset).unwrap_or(u64::MAX))
            .saturating_add(1);
        if entry.index != expected || entry.term == 0 {
            return Err(RaftError::Invalid(format!(
                "incoming entry index {} term {} does not match expected index {expected}",
                entry.index, entry.term
            )));
        }
    }
    Ok(())
}

fn last_log_position(state: &PersistentState) -> (u64, u64) {
    state
        .log
        .last()
        .map(|entry| (entry.index, entry.term))
        .unwrap_or((0, 0))
}

fn log_term(state: &PersistentState, index: u64) -> Option<u64> {
    if index == 0 {
        return Some(0);
    }
    usize::try_from(index.saturating_sub(1))
        .ok()
        .and_then(|position| state.log.get(position))
        .map(|entry| entry.term)
}

fn highest_committable_index(
    state: &PersistentState,
    current_term: u64,
    match_indexes: &[u64],
    majority: usize,
) -> Option<u64> {
    let last_index = u64::try_from(state.log.len()).unwrap_or(u64::MAX);
    (state.commit_index.saturating_add(1)..=last_index)
        .rev()
        .find(|candidate| {
            log_term(state, *candidate) == Some(current_term)
                && match_indexes
                    .iter()
                    .filter(|matched| **matched >= *candidate)
                    .count()
                    >= majority
        })
}

fn apply_committed(
    persistent: &PersistentState,
) -> Result<(u64, Option<CommittedConfiguration>), RaftError> {
    let mut committed = None;
    for entry in persistent
        .log
        .iter()
        .take(usize::try_from(persistent.commit_index).unwrap_or(usize::MAX))
    {
        if let Command::SetRoutingConfiguration { configuration } = &entry.command {
            configuration.validate().map_err(|error| {
                RaftError::Storage(format!(
                    "committed configuration at index {} is invalid: {error}",
                    entry.index
                ))
            })?;
            committed = Some(CommittedConfiguration {
                revision: entry.index,
                term: entry.term,
                configuration: configuration.clone(),
            });
        }
    }
    Ok((persistent.commit_index, committed))
}

fn apply_committed_locked(state: &mut VolatileState) -> Result<(), RaftError> {
    let (last_applied, committed) = apply_committed(&state.persistent)?;
    state.last_applied = last_applied;
    state.committed_configuration = committed;
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::model::WorkerConfiguration;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "inferlab-raft-{}-{name}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_config(directory: &TestDirectory) -> NodeConfig {
        NodeConfig {
            node_id: "node-a".to_owned(),
            peers: vec![
                Peer {
                    id: "node-b".to_owned(),
                    base_url: "http://127.0.0.1:1".to_owned(),
                },
                Peer {
                    id: "node-c".to_owned(),
                    base_url: "http://127.0.0.1:2".to_owned(),
                },
            ],
            state_path: directory.0.join("state.json"),
            event_path: directory.0.join("events.jsonl"),
            election_timeout_min: Duration::from_millis(100),
            election_timeout_max: Duration::from_millis(200),
            heartbeat_interval: Duration::from_millis(25),
            rpc_timeout: Duration::from_millis(20),
            commit_timeout: Duration::from_millis(200),
        }
    }

    fn routing(policy: &str) -> RoutingConfiguration {
        RoutingConfiguration {
            routing_policy: policy.to_owned(),
            workers: vec![WorkerConfiguration {
                id: "worker-a".to_owned(),
                base_url: "http://127.0.0.1:9001".to_owned(),
                weight: 1,
            }],
        }
    }

    #[test]
    fn node_votes_once_per_term_and_rejects_stale_candidate_logs() {
        let directory = TestDirectory::new("votes");
        let node = RaftNode::open(test_config(&directory)).expect("open node");
        let granted = node
            .handle_request_vote(RequestVoteRequest {
                term: 1,
                candidate_id: "node-b".to_owned(),
                last_log_index: 0,
                last_log_term: 0,
            })
            .expect("request vote");
        assert!(granted.vote_granted);
        let duplicate_term = node
            .handle_request_vote(RequestVoteRequest {
                term: 1,
                candidate_id: "node-c".to_owned(),
                last_log_index: 0,
                last_log_term: 0,
            })
            .expect("request second vote");
        assert!(!duplicate_term.vote_granted);

        node.handle_append_entries(AppendEntriesRequest {
            term: 2,
            leader_id: "node-b".to_owned(),
            prev_log_index: 0,
            prev_log_term: 0,
            entries: vec![LogEntry {
                index: 1,
                term: 2,
                command: Command::Noop,
            }],
            leader_commit: 0,
        })
        .expect("append leader entry");
        let stale = node
            .handle_request_vote(RequestVoteRequest {
                term: 3,
                candidate_id: "node-c".to_owned(),
                last_log_index: 0,
                last_log_term: 0,
            })
            .expect("request stale vote");
        assert!(!stale.vote_granted);
        assert_eq!(stale.term, 3);
    }

    #[test]
    fn append_entries_checks_the_prefix_and_repairs_uncommitted_conflicts() {
        let directory = TestDirectory::new("repair");
        let node = RaftNode::open(test_config(&directory)).expect("open node");
        let accepted = node
            .handle_append_entries(AppendEntriesRequest {
                term: 1,
                leader_id: "node-b".to_owned(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries: vec![LogEntry {
                    index: 1,
                    term: 1,
                    command: Command::Noop,
                }],
                leader_commit: 0,
            })
            .expect("append first entry");
        assert!(accepted.success);
        let rejected = node
            .handle_append_entries(AppendEntriesRequest {
                term: 1,
                leader_id: "node-b".to_owned(),
                prev_log_index: 1,
                prev_log_term: 99,
                entries: Vec::new(),
                leader_commit: 0,
            })
            .expect("reject inconsistent prefix");
        assert!(!rejected.success);

        let repaired = node
            .handle_append_entries(AppendEntriesRequest {
                term: 2,
                leader_id: "node-c".to_owned(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries: vec![LogEntry {
                    index: 1,
                    term: 2,
                    command: Command::SetRoutingConfiguration {
                        configuration: routing("least-in-flight"),
                    },
                }],
                leader_commit: 1,
            })
            .expect("repair suffix");
        assert!(repaired.success);
        let status = node.status().expect("status");
        assert_eq!(status.last_log_index, 1);
        assert_eq!(status.last_log_term, 2);
        assert_eq!(status.commit_index, 1);
        assert_eq!(
            status
                .committed_configuration
                .expect("configuration")
                .configuration
                .routing_policy,
            "least-in-flight"
        );
    }

    #[test]
    fn committed_configuration_survives_process_reopen() {
        let directory = TestDirectory::new("persistence");
        {
            let node = RaftNode::open(test_config(&directory)).expect("open node");
            node.handle_append_entries(AppendEntriesRequest {
                term: 4,
                leader_id: "node-b".to_owned(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries: vec![LogEntry {
                    index: 1,
                    term: 4,
                    command: Command::SetRoutingConfiguration {
                        configuration: routing("consistent-hash"),
                    },
                }],
                leader_commit: 1,
            })
            .expect("commit configuration");
        }
        let reopened = RaftNode::open(test_config(&directory)).expect("reopen node");
        let status = reopened.status().expect("status");
        assert_eq!(status.term, 4);
        assert_eq!(status.commit_index, 1);
        assert_eq!(status.last_applied, 1);
        assert_eq!(
            reopened
                .committed_configuration()
                .expect("committed configuration")
                .configuration
                .routing_policy,
            "consistent-hash"
        );
    }

    #[test]
    fn committed_log_prefix_cannot_be_overwritten() {
        let directory = TestDirectory::new("committed-prefix");
        let node = RaftNode::open(test_config(&directory)).expect("open node");
        node.handle_append_entries(AppendEntriesRequest {
            term: 1,
            leader_id: "node-b".to_owned(),
            prev_log_index: 0,
            prev_log_term: 0,
            entries: vec![LogEntry {
                index: 1,
                term: 1,
                command: Command::Noop,
            }],
            leader_commit: 1,
        })
        .expect("commit first entry");
        let overwrite = node.handle_append_entries(AppendEntriesRequest {
            term: 2,
            leader_id: "node-c".to_owned(),
            prev_log_index: 0,
            prev_log_term: 0,
            entries: vec![LogEntry {
                index: 1,
                term: 2,
                command: Command::Noop,
            }],
            leader_commit: 1,
        });
        assert!(matches!(overwrite, Err(RaftError::Storage(_))));
    }

    #[test]
    fn higher_term_is_persisted_even_when_append_prefix_is_rejected() {
        let directory = TestDirectory::new("rejected-higher-term");
        {
            let node = RaftNode::open(test_config(&directory)).expect("open node");
            let response = node
                .handle_append_entries(AppendEntriesRequest {
                    term: 7,
                    leader_id: "node-b".to_owned(),
                    prev_log_index: 9,
                    prev_log_term: 4,
                    entries: Vec::new(),
                    leader_commit: 0,
                })
                .expect("reject prefix without losing higher term");
            assert!(!response.success);
            assert_eq!(response.term, 7);
        }
        let reopened = RaftNode::open(test_config(&directory)).expect("reopen node");
        assert_eq!(reopened.status().expect("status").term, 7);
    }

    #[test]
    fn routing_configuration_validation_is_strict() {
        assert!(routing("round-robin").validate().is_ok());
        assert!(routing("random").validate().is_err());
        let mut duplicate = routing("round-robin");
        duplicate.workers.push(duplicate.workers[0].clone());
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn leader_commits_only_an_entry_from_its_current_term() {
        let mut state = PersistentState {
            current_term: 2,
            voted_for: Some("node-a".to_owned()),
            log: vec![LogEntry {
                index: 1,
                term: 1,
                command: Command::Noop,
            }],
            commit_index: 0,
        };
        assert_eq!(highest_committable_index(&state, 2, &[1, 1, 0], 2), None);
        state.log.push(LogEntry {
            index: 2,
            term: 2,
            command: Command::Noop,
        });
        assert_eq!(highest_committable_index(&state, 2, &[2, 0, 0], 2), None);
        assert_eq!(highest_committable_index(&state, 2, &[2, 2, 0], 2), Some(2));
    }
}
