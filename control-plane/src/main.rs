use std::{env, io, path::PathBuf, sync::Arc, time::Duration};

use control_auth::{SigningIdentity, TrustedWriterKeyRing};
use control_plane::{
    NodeConfig, Peer, RaftNode, WriteAuthorizer, app_with_security, model::DEFAULT_CLUSTER_ID,
};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let node_id = required_env("INFERLAB_RAFT_NODE_ID")?;
    let cluster_id =
        env::var("INFERLAB_RAFT_CLUSTER_ID").unwrap_or_else(|_| DEFAULT_CLUSTER_ID.to_owned());
    let signer = control_signer()?;
    let writer_authorizer = Arc::new(control_writer_authorizer()?);
    let writer_status = writer_authorizer.status();
    let bind = required_env("INFERLAB_RAFT_BIND")?;
    let peers = parse_peers(&required_env("INFERLAB_RAFT_PEERS")?)?;
    let data_directory = env::var("INFERLAB_RAFT_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./data/raft").join(&node_id));
    let election_timeout_min =
        Duration::from_millis(parse_env("INFERLAB_RAFT_ELECTION_MIN_MS", 300_u64)?);
    let election_timeout_max =
        Duration::from_millis(parse_env("INFERLAB_RAFT_ELECTION_MAX_MS", 600_u64)?);
    let heartbeat_interval =
        Duration::from_millis(parse_env("INFERLAB_RAFT_HEARTBEAT_MS", 100_u64)?);
    let rpc_timeout = Duration::from_millis(parse_env("INFERLAB_RAFT_RPC_TIMEOUT_MS", 150_u64)?);
    let commit_timeout =
        Duration::from_millis(parse_env("INFERLAB_RAFT_COMMIT_TIMEOUT_MS", 2_000_u64)?);
    let node = RaftNode::open(NodeConfig {
        node_id: node_id.clone(),
        cluster_id: cluster_id.clone(),
        peers,
        state_path: data_directory.join("state.json"),
        event_path: data_directory.join("events.jsonl"),
        election_timeout_min,
        election_timeout_max,
        heartbeat_interval,
        rpc_timeout,
        commit_timeout,
    })
    .map_err(io::Error::other)?;
    let _background = node.spawn_background();
    let listener = TcpListener::bind(&bind).await?;
    info!(
        %node_id,
        %cluster_id,
        signing_key_id = signer.as_ref().map(|signer| signer.key_id()),
        writer_authorization_required = writer_status.required,
        trusted_writer_ids = ?writer_status.trusted_writer_ids,
        revoked_writer_ids = ?writer_status.revoked_writer_ids,
        write_max_age_ms = writer_status.max_age_ms,
        write_max_future_skew_ms = writer_status.max_future_skew_ms,
        %bind,
        data_directory = %data_directory.display(),
        election_timeout_min_ms = election_timeout_min.as_millis(),
        election_timeout_max_ms = election_timeout_max.as_millis(),
        heartbeat_interval_ms = heartbeat_interval.as_millis(),
        "InferLab Raft control-plane node listening"
    );
    axum::serve(listener, app_with_security(node, signer, writer_authorizer)).await
}

fn control_signer() -> io::Result<Option<Arc<SigningIdentity>>> {
    let key_id = env::var("INFERLAB_CONTROL_SIGNING_KEY_ID").ok();
    let private_key = env::var("INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64").ok();
    match (key_id, private_key) {
        (None, None) => Ok(None),
        (Some(key_id), Some(private_key)) => {
            SigningIdentity::from_base64_seed(key_id, &private_key)
                .map(Arc::new)
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_CONTROL_SIGNING_KEY_ID and INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64 must be configured together",
        )),
    }
}

fn control_writer_authorizer() -> io::Result<WriteAuthorizer> {
    let encoded_keys = env::var("INFERLAB_CONTROL_WRITER_KEYS").unwrap_or_default();
    let revoked_writer_ids = env::var("INFERLAB_CONTROL_REVOKED_WRITER_IDS").unwrap_or_default();
    if encoded_keys.trim().is_empty() {
        if !revoked_writer_ids.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "INFERLAB_CONTROL_REVOKED_WRITER_IDS requires INFERLAB_CONTROL_WRITER_KEYS",
            ));
        }
        return Ok(WriteAuthorizer::disabled());
    }
    let keys = TrustedWriterKeyRing::parse(&encoded_keys, &revoked_writer_ids)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let max_age_ms = parse_env("INFERLAB_CONTROL_WRITE_MAX_AGE_MS", 30_000_u64)?;
    let max_future_skew_ms = parse_env("INFERLAB_CONTROL_WRITE_MAX_FUTURE_SKEW_MS", 5_000_u64)?;
    if max_age_ms == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_CONTROL_WRITE_MAX_AGE_MS must be positive",
        ));
    }
    Ok(WriteAuthorizer::required(
        keys,
        max_age_ms,
        max_future_skew_ms,
    ))
}

fn required_env(name: &str) -> io::Result<String> {
    env::var(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{name} is required")))
}

fn parse_env<T>(name: &str, default: T) -> io::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match env::var(name) {
        Ok(value) => value.parse::<T>().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} has an invalid value: {error}"),
            )
        }),
        Err(_) => Ok(default),
    }
}

fn parse_peers(raw: &str) -> io::Result<Vec<Peer>> {
    raw.split(',')
        .map(|entry| {
            let (id, base_url) = entry.split_once('=').ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid peer '{entry}'; expected id=http://host:port"),
                )
            })?;
            Ok(Peer {
                id: id.trim().to_owned(),
                base_url: base_url.trim().trim_end_matches('/').to_owned(),
            })
        })
        .collect()
}
