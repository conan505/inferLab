mod figure_eight;
pub mod link_proxy;
mod metrics;
pub mod model;
mod raft;
pub mod service_authentication;
pub mod service_trust;
mod storage;
pub mod write_authorization;

use std::{fmt, io, sync::Arc, time::SystemTime};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;

use control_auth::{RoutingPayload, RoutingWorker, SigningIdentity};
pub use figure_eight::{FigureEightSafetyReport, figure_eight_safety_report};
pub use metrics::{ControlMetrics, LinkMetrics};
use model::{
    AppendEntriesRequest, AuthenticatedCommittedConfiguration, CommittedConfiguration,
    ConfigurationWriteRequest, RequestVoteRequest,
};
pub use raft::{NodeConfig, Peer, RaftNode};
use service_auth::{VerifiedServiceCredential, canonical_json_body};
pub use service_authentication::{ServiceAuthorizer, ServiceRequestContext};
pub use write_authorization::WriteAuthorizer;

#[derive(Debug)]
pub enum RaftError {
    Invalid(String),
    Unauthorized(String),
    Forbidden(String),
    Conflict(String),
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
            Self::Unauthorized(_) => "unauthorized",
            Self::Forbidden(_) => "forbidden",
            Self::Conflict(_) => "revision_conflict",
            Self::NotLeader { .. } => "not_leader",
            Self::Unavailable(_) => "unavailable",
            Self::Storage(_) => "storage_error",
        }
    }
}

impl fmt::Display for RaftError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message)
            | Self::Unauthorized(message)
            | Self::Forbidden(message)
            | Self::Conflict(message)
            | Self::Unavailable(message)
            | Self::Storage(message) => formatter.write_str(message),
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
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::Conflict(_) => StatusCode::CONFLICT,
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
    writer_authorizer: Arc<WriteAuthorizer>,
    service_authorizer: Arc<ServiceAuthorizer>,
}

pub fn app(node: Arc<RaftNode>) -> Router {
    app_with_signer(node, None)
}

pub fn app_with_signer(node: Arc<RaftNode>, signer: Option<Arc<SigningIdentity>>) -> Router {
    app_with_security(node, signer, Arc::new(WriteAuthorizer::disabled()))
}

pub fn app_with_security(
    node: Arc<RaftNode>,
    signer: Option<Arc<SigningIdentity>>,
    writer_authorizer: Arc<WriteAuthorizer>,
) -> Router {
    app_with_authentication(
        node,
        signer,
        writer_authorizer,
        Arc::new(ServiceAuthorizer::disabled()),
    )
}

pub fn app_with_authentication(
    node: Arc<RaftNode>,
    signer: Option<Arc<SigningIdentity>>,
    writer_authorizer: Arc<WriteAuthorizer>,
    service_authorizer: Arc<ServiceAuthorizer>,
) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/raft/request-vote", post(request_vote))
        .route("/raft/append-entries", post(append_entries))
        .route("/v1/control/status", get(status))
        .route(
            "/v1/control/config",
            get(get_configuration).put(set_configuration),
        )
        .with_state(AppState {
            node,
            signer,
            writer_authorizer,
            service_authorizer,
        })
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
    headers: HeaderMap,
    Json(request): Json<RequestVoteRequest>,
) -> Result<Json<model::RequestVoteResponse>, RaftError> {
    let body = canonical_json_body(&request)
        .map_err(|error| RaftError::Invalid(format!("canonicalize request-vote body: {error}")))?;
    let service_id =
        authenticate_service_request(&state, &headers, "POST", "/raft/request-vote", &body)?;
    authorize_peer_identity(
        &state,
        service_id
            .as_ref()
            .map(|credential| credential.service_id.as_str()),
        &request.candidate_id,
    )?;
    state.node.handle_request_vote(request).map(Json)
}

async fn append_entries(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AppendEntriesRequest>,
) -> Result<Json<model::AppendEntriesResponse>, RaftError> {
    let body = canonical_json_body(&request).map_err(|error| {
        RaftError::Invalid(format!("canonicalize append-entries body: {error}"))
    })?;
    let service_id =
        authenticate_service_request(&state, &headers, "POST", "/raft/append-entries", &body)?;
    authorize_peer_identity(
        &state,
        service_id
            .as_ref()
            .map(|credential| credential.service_id.as_str()),
        &request.leader_id,
    )?;
    state.node.handle_append_entries(request).map(Json)
}

#[derive(Serialize)]
struct ControlPlaneStatus {
    #[serde(flatten)]
    node: model::NodeStatus,
    local_service_credential_id: Option<String>,
    write_authorization: write_authorization::WriteAuthorizationStatus,
    service_authentication: service_authentication::ServiceAuthenticationStatus,
}

async fn status(State(state): State<AppState>) -> Result<Json<ControlPlaneStatus>, RaftError> {
    Ok(Json(ControlPlaneStatus {
        node: state.node.status()?,
        local_service_credential_id: state.node.service_credential_id().map(str::to_owned),
        write_authorization: state.writer_authorizer.status(),
        service_authentication: state.service_authorizer.status(),
    }))
}

async fn get_configuration(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AuthenticatedCommittedConfiguration>, RaftError> {
    let service_id =
        authenticate_service_request(&state, &headers, "GET", "/v1/control/config", &[])?;
    state
        .service_authorizer
        .authorize_gateway(
            service_id
                .as_ref()
                .map(|credential| credential.service_id.as_str()),
        )
        .map_err(|error| RaftError::Forbidden(error.message))?;
    state.service_authorizer.record_authorized_gateway_read(
        service_id
            .as_ref()
            .map(|credential| credential.service_id.as_str()),
    );
    let committed = state.node.committed_configuration()?;
    authenticate_configuration(committed, state.signer.as_deref()).map(Json)
}

fn authenticate_service_request(
    state: &AppState,
    headers: &HeaderMap,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<Option<VerifiedServiceCredential>, RaftError> {
    let authentication =
        service_authentication::authentication_from_headers(headers).map_err(|message| {
            state
                .service_authorizer
                .record_header_rejection(message.clone());
            RaftError::Unauthorized(message)
        })?;
    state
        .service_authorizer
        .authenticate(
            authentication,
            ServiceRequestContext {
                method,
                path,
                cluster_id: state.node.cluster_id(),
                audience_id: state.node.node_id(),
                body,
                now_ms: now_ms()?,
            },
        )
        .map_err(|error| RaftError::Unauthorized(error.message))
}

fn authorize_peer_identity(
    state: &AppState,
    service_id: Option<&str>,
    claimed_peer_id: &str,
) -> Result<(), RaftError> {
    if !state.service_authorizer.required_mode() {
        return Ok(());
    }
    if service_id == Some(claimed_peer_id) && state.node.is_peer_id(claimed_peer_id) {
        state
            .service_authorizer
            .record_authorized_peer_rpc(service_id);
        return Ok(());
    }
    let message = format!(
        "service identity '{}' cannot act as Raft peer '{claimed_peer_id}'",
        service_id.unwrap_or("<missing>")
    );
    state
        .service_authorizer
        .record_peer_authorization_rejection(service_id, message.clone());
    Err(RaftError::Forbidden(message))
}

async fn set_configuration(
    State(state): State<AppState>,
    Json(request): Json<ConfigurationWriteRequest>,
) -> Result<Json<AuthenticatedCommittedConfiguration>, RaftError> {
    let now_ms = now_ms()?;
    let proposal = state
        .writer_authorizer
        .authorize(request, state.node.cluster_id(), now_ms)
        .map_err(|error| RaftError::Unauthorized(error.message))?;
    let writer_id = proposal
        .writer
        .as_ref()
        .map(|writer| writer.writer_id.clone());
    let committed = match state
        .node
        .write_configuration_with_fence(
            proposal.configuration,
            proposal.expected_revision,
            proposal.writer,
        )
        .await
    {
        Ok(committed) => committed,
        Err(error @ RaftError::Conflict(_)) => {
            state
                .writer_authorizer
                .record_revision_conflict(writer_id.as_deref(), &error.to_string());
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    state
        .writer_authorizer
        .record_committed(writer_id.as_deref());
    authenticate_configuration(committed, state.signer.as_deref()).map(Json)
}

fn now_ms() -> Result<u64, RaftError> {
    Ok(SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|error| RaftError::Unavailable(format!("system clock is before epoch: {error}")))?
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX))
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
