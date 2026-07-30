use std::{env, io, path::PathBuf, time::Duration};

use control_plane::{NodeConfig, Peer, RaftNode, app};
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
        %bind,
        data_directory = %data_directory.display(),
        election_timeout_min_ms = election_timeout_min.as_millis(),
        election_timeout_max_ms = election_timeout_max.as_millis(),
        heartbeat_interval_ms = heartbeat_interval.as_millis(),
        "InferLab Raft control-plane node listening"
    );
    axum::serve(listener, app(node)).await
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
