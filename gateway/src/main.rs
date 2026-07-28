use std::{env, io, sync::Arc};

use gateway::{app, routing::WorkerPool};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> io::Result<()> {
    init_tracing();

    let bind = env::var("INFERLAB_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
    let workers = env::var("INFERLAB_WORKERS").unwrap_or_else(|_| {
        [
            "worker-a=http://127.0.0.1:9001",
            "worker-b=http://127.0.0.1:9002",
            "worker-c=http://127.0.0.1:9003",
        ]
        .join(",")
    });
    let pool = Arc::new(WorkerPool::new(parse_workers(&workers)?).map_err(io::Error::other)?);

    let listener = TcpListener::bind(&bind).await?;
    info!(%bind, workers = pool.snapshots().len(), "gateway listening");
    axum::serve(listener, app(pool)).await
}

fn parse_workers(raw: &str) -> io::Result<Vec<(String, String)>> {
    raw.split(',')
        .map(|entry| {
            let (id, url) = entry.split_once('=').ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid worker '{entry}'; expected id=url"),
                )
            })?;
            Ok((id.trim().to_owned(), url.trim().to_owned()))
        })
        .collect()
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::parse_workers;

    #[test]
    fn parses_worker_configuration() {
        let workers = parse_workers("a=http://a:1,b=http://b:2").expect("valid workers");
        assert_eq!(workers[0], ("a".to_owned(), "http://a:1".to_owned()));
        assert_eq!(workers[1], ("b".to_owned(), "http://b:2".to_owned()));
    }

    #[test]
    fn rejects_worker_without_separator() {
        assert!(parse_workers("http://a:1").is_err());
    }
}
