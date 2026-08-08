use std::{env, io, net::SocketAddr, path::PathBuf, sync::Arc};

use control_plane::{
    LinkMetrics,
    link_proxy::{LinkProxy, LinkProxyConfig, link_proxy_app},
};
use observability::{
    HttpMetrics, MetricsRegistry, MetricsServerConfig, Service, init_tracing, serve_metrics,
};
use tokio::net::TcpListener;
use tracing::info;

#[tokio::main]
async fn main() -> io::Result<()> {
    init_tracing(Service::RaftLinkProxy)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let metrics_config = MetricsServerConfig::from_env()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;

    let bind = loopback_bind(&required_env("INFERLAB_RAFT_LINK_BIND")?)?;
    let proxy = LinkProxy::open(LinkProxyConfig {
        link_id: required_env("INFERLAB_RAFT_LINK_ID")?,
        source_id: required_env("INFERLAB_RAFT_LINK_SOURCE_ID")?,
        target_id: required_env("INFERLAB_RAFT_LINK_TARGET_ID")?,
        upstream_base_url: required_env("INFERLAB_RAFT_LINK_UPSTREAM")?,
        event_path: PathBuf::from(required_env("INFERLAB_RAFT_LINK_EVENT_PATH")?),
    })?;
    let listener = TcpListener::bind(bind).await?;
    let status = proxy.status()?;
    info!(
        link_id = %status.link_id,
        source_id = %status.source_id,
        target_id = %status.target_id,
        upstream_base_url = %status.upstream_base_url,
        %bind,
        "directed Raft link proxy listening"
    );
    match metrics_config {
        None => axum::serve(listener, link_proxy_app(proxy)).await,
        Some(metrics_config) => {
            let mut registry = MetricsRegistry::new();
            let http = HttpMetrics::register(&mut registry, Service::RaftLinkProxy)
                .map_err(io::Error::other)?;
            LinkMetrics::register(&mut registry, Arc::clone(&proxy)).map_err(io::Error::other)?;
            let registry = Arc::new(registry);
            let application = http.instrument(link_proxy_app(proxy));
            let ((), ()) = tokio::try_join!(
                async { axum::serve(listener, application).await },
                serve_metrics(metrics_config, registry),
            )?;
            Ok(())
        }
    }
}

fn required_env(name: &str) -> io::Result<String> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} must be set and non-empty"),
            )
        })
}

fn loopback_bind(raw: &str) -> io::Result<SocketAddr> {
    let bind = raw.parse::<SocketAddr>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("INFERLAB_RAFT_LINK_BIND must be an explicit IP socket address: {error}"),
        )
    })?;
    if !bind.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INFERLAB_RAFT_LINK_BIND must use a loopback IP address",
        ));
    }
    Ok(bind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_proxy_bind_is_explicitly_loopback_only() {
        assert!(loopback_bind("127.0.0.1:9901").is_ok());
        assert!(loopback_bind("[::1]:9901").is_ok());
        assert!(loopback_bind("0.0.0.0:9901").is_err());
        assert!(loopback_bind("192.0.2.1:9901").is_err());
        assert!(loopback_bind("localhost:9901").is_err());
    }
}
