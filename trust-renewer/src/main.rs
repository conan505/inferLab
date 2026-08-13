use std::{future::Future, io, sync::Arc};

use observability::{
    HttpMetrics, MetricsRegistry, MetricsServerConfig, Service, init_tracing, serve_metrics,
};
use tokio::{net::TcpListener, task::JoinHandle};
use tracing::info;
use trust_renewer::{
    MtlsDistributorTransport, RenewalEngine, RenewerConfig, SystemWallClock, TrustRenewerMetrics,
    status_app,
};

#[tokio::main]
async fn main() -> io::Result<()> {
    init_tracing(Service::TrustRenewer)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let metrics_config = MetricsServerConfig::from_env()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let config = RenewerConfig::from_env()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let transport = MtlsDistributorTransport::new(
        config.distributor_endpoint.clone(),
        &config.mtls,
        config.request_timeout,
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let engine = RenewalEngine::open(&config, transport, SystemWallClock)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let status = engine.status();

    let listener = std::net::TcpListener::bind(config.status_bind)?;
    listener.set_nonblocking(true)?;
    let bound_address = listener.local_addr()?;
    let listener = TcpListener::from_std(listener)?;

    let mut application = status_app(status.clone());
    let metrics_server = match metrics_config {
        None => None,
        Some(config) => {
            let mut registry = MetricsRegistry::new();
            let http = HttpMetrics::register(&mut registry, Service::TrustRenewer)
                .map_err(io::Error::other)?;
            TrustRenewerMetrics::register(&mut registry, status).map_err(io::Error::other)?;
            application = http.instrument(application);
            Some((config, Arc::new(registry)))
        }
    };
    info!(
        %bound_address,
        transport = "mutual-tls",
        policy_lifetime_ms = config.policy_lifetime.as_millis(),
        renew_before_ms = config.renew_before.as_millis(),
        poll_interval_ms = config.poll_interval.as_millis(),
        retry_interval_ms = config.retry_interval.as_millis(),
        request_timeout_ms = config.request_timeout.as_millis(),
        "InferLab trust renewer listening"
    );

    let status_server = async move { axum::serve(listener, application).await };
    let application = async move {
        match metrics_server {
            None => status_server.await,
            Some((config, registry)) => {
                let ((), ()) = tokio::try_join!(status_server, serve_metrics(config, registry))?;
                Ok(())
            }
        }
    };
    let renewal_loop = tokio::spawn(engine.run());
    supervise(application, renewal_loop).await
}

async fn supervise<F>(application: F, renewal_loop: JoinHandle<()>) -> io::Result<()>
where
    F: Future<Output = io::Result<()>>,
{
    tokio::pin!(application);
    let mut renewal_loop = renewal_loop;
    let result = tokio::select! {
        result = &mut application => result,
        result = &mut renewal_loop => Err(unexpected_loop_exit(result)),
    };
    renewal_loop.abort();
    result.and_then(|()| {
        Err(io::Error::other(
            "trust-renewer listener stopped unexpectedly",
        ))
    })
}

fn unexpected_loop_exit(result: Result<(), tokio::task::JoinError>) -> io::Error {
    match result {
        Ok(()) => io::Error::other("trust renewal loop stopped unexpectedly"),
        Err(error) if error.is_panic() => io::Error::other("trust renewal loop failed"),
        Err(_) => io::Error::other("trust renewal loop was cancelled unexpectedly"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clean_loop_exit_is_fatal_to_supervision() {
        let loop_handle = tokio::spawn(async {});
        let error = supervise(std::future::pending(), loop_handle)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "trust renewal loop stopped unexpectedly");
    }

    #[tokio::test]
    async fn listener_exit_is_fatal_to_supervision() {
        let loop_handle = tokio::spawn(std::future::pending());
        let error = supervise(async { Ok(()) }, loop_handle).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "trust-renewer listener stopped unexpectedly"
        );
    }
}
