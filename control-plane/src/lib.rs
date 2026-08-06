pub mod model;
mod raft;
mod storage;

use std::{fmt, io, sync::Arc};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;

use control_auth::{RoutingPayload, RoutingWorker, SigningIdentity};
use model::{
    AppendEntriesRequest, AuthenticatedCommittedConfiguration, CommittedConfiguration,
    RequestVoteRequest, RoutingConfiguration,
};
pub use raft::{NodeConfig, Peer, RaftNode};

#[derive(Debug)]
pub enum RaftError {
    Invalid(String),
    NotLeader { leader_id: Option<String> },
    Unavailable(String),
    Storage(String),
}

impl RaftError {
    pub(crate) fn storage(error: io::Error) -> Self {
        Self::Storage(error.to_string())
    }

    fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid_request",
            Self::NotLeader { .. } => "not_leader",
            Self::Unavailable(_) => "unavailable",
            Self::Storage(_) => "storage_error",
        }
    }
}

impl fmt::Display for RaftError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) | Self::Unavailable(message) | Self::Storage(message) => {
                formatter.write_str(message)
            }
            Self::NotLeader {
                leader_id: Some(leader),
            } => write!(
                formatter,
                "this node is not leader; known leader is {leader}"
            ),
            Self::NotLeader { leader_id: None } => {
                formatter.write_str("this node is not leader; no leader is currently known")
            }
        }
    }
}

impl std::error::Error for RaftError {}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: String,
    leader_id: Option<String>,
}

impl IntoResponse for RaftError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Invalid(_) => StatusCode::BAD_REQUEST,
            Self::NotLeader { .. } => StatusCode::CONFLICT,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let leader_id = match &self {
            Self::NotLeader { leader_id } => leader_id.clone(),
            _ => None,
        };
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code(),
                message: self.to_string(),
                leader_id,
            },
        };
        (status, Json(body)).into_response()
    }
}

#[derive(Clone)]
struct AppState {
    node: Arc<RaftNode>,
    signer: Option<Arc<SigningIdentity>>,
}

pub fn app(node: Arc<RaftNode>) -> Router {
    app_with_signer(node, None)
}

pub fn app_with_signer(node: Arc<RaftNode>, signer: Option<Arc<SigningIdentity>>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/raft/request-vote", post(request_vote))
        .route("/raft/append-entries", post(append_entries))
        .route("/v1/control/status", get(status))
        .route(
            "/v1/control/config",
            get(get_configuration).put(set_configuration),
        )
        .with_state(AppState { node, signer })
}

async fn health(State(state): State<AppState>) -> Result<Response, RaftError> {
    let status = state.node.status()?;
    if status.storage_healthy {
        Ok((StatusCode::OK, "ok").into_response())
    } else {
        Err(RaftError::Storage("node storage is not healthy".to_owned()))
    }
}

async fn request_vote(
    State(state): State<AppState>,
    Json(request): Json<RequestVoteRequest>,
) -> Result<Json<model::RequestVoteResponse>, RaftError> {
    state.node.handle_request_vote(request).map(Json)
}

async fn append_entries(
    State(state): State<AppState>,
    Json(request): Json<AppendEntriesRequest>,
) -> Result<Json<model::AppendEntriesResponse>, RaftError> {
    state.node.handle_append_entries(request).map(Json)
}

async fn status(State(state): State<AppState>) -> Result<Json<model::NodeStatus>, RaftError> {
    state.node.status().map(Json)
}

async fn get_configuration(
    State(state): State<AppState>,
) -> Result<Json<AuthenticatedCommittedConfiguration>, RaftError> {
    let committed = state.node.committed_configuration()?;
    authenticate_configuration(committed, state.signer.as_deref()).map(Json)
}

async fn set_configuration(
    State(state): State<AppState>,
    Json(configuration): Json<RoutingConfiguration>,
) -> Result<Json<AuthenticatedCommittedConfiguration>, RaftError> {
    let committed = state.node.write_configuration(configuration).await?;
    authenticate_configuration(committed, state.signer.as_deref()).map(Json)
}

fn authenticate_configuration(
    committed: CommittedConfiguration,
    signer: Option<&SigningIdentity>,
) -> Result<AuthenticatedCommittedConfiguration, RaftError> {
    let authentication = signer
        .map(|signer| {
            signer.sign(&routing_payload(&committed)).map_err(|error| {
                RaftError::Invalid(format!("sign committed configuration: {error}"))
            })
        })
        .transpose()?;
    Ok(AuthenticatedCommittedConfiguration {
        committed,
        authentication,
    })
}

fn routing_payload(committed: &CommittedConfiguration) -> RoutingPayload<'_> {
    RoutingPayload {
        cluster_id: &committed.cluster_id,
        revision: committed.revision,
        term: committed.term,
        routing_policy: &committed.configuration.routing_policy,
        workers: committed
            .configuration
            .workers
            .iter()
            .map(|worker| RoutingWorker {
                id: &worker.id,
                base_url: &worker.base_url,
                weight: worker.weight,
            })
            .collect(),
    }
}
