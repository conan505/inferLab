use std::{
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use axum::Router;
use fake_worker::Config;
use futures_util::StreamExt;
use gateway::{
    RoutingSnapshot,
    admission::AdmissionConfig,
    app_with_runtime_config,
    resilience::ResilienceConfig,
    routing::WorkerPool,
    routing_lease::{RoutingLeaseExpiryAction, RoutingLeaseGuard},
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, time::sleep};

#[tokio::test]
async fn reject_new_preserves_an_existing_stream_and_recovers_after_renewal() {
    let mut worker_config = Config::for_test("worker-lease");
    worker_config.token_delay = Duration::from_millis(250);
    let worker_address = spawn(fake_worker::app(worker_config)).await;
    let worker_url = format!("http://{worker_address}");
    let (gateway_address, lease) = spawn_gateway(
        &worker_url,
        Duration::from_millis(800),
        RoutingLeaseExpiryAction::RejectNew,
    )
    .await;
    let client = reqwest::Client::new();

    let started = Instant::now();
    let response = client
        .post(format!("http://{gateway_address}/v1/chat/completions"))
        .json(&json!({
            "model": "inferlab-fake",
            "stream": true,
            "messages": [{"role": "user", "content": "lease crossing"}]
        }))
        .send()
        .await
        .expect("stream response headers");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let mut stream = response.bytes_stream();

    sleep(Duration::from_millis(850)).await;
    let readiness = client
        .get(format!("http://{gateway_address}/readyz"))
        .send()
        .await
        .expect("expired readiness");
    assert_eq!(readiness.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    let readiness_body: Value = readiness.json().await.expect("readiness JSON");
    assert_eq!(readiness_body["reason"], "routing_lease_expired");
    assert_eq!(
        readiness_body["routing_lease"]["state"],
        "expired-rejecting-new"
    );

    let before_rejection = worker_requests(&client, &worker_url).await;
    let rejected = client
        .post(format!("http://{gateway_address}/v1/chat/completions"))
        .json(&json!({
            "model": "inferlab-fake",
            "stream": false,
            "messages": [{"role": "user", "content": "must not route"}]
        }))
        .send()
        .await
        .expect("lease rejection");
    assert_eq!(rejected.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(rejected.headers()["x-inferlab-attempts"], "0");
    let rejected_body: Value = rejected.json().await.expect("rejection JSON");
    assert_eq!(rejected_body["error"]["type"], "routing_lease_expired");
    assert_eq!(
        rejected_body["error"]["reason"],
        "runtime_routing_lease_expired"
    );
    assert_eq!(
        worker_requests(&client, &worker_url).await,
        before_rejection
    );

    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk.expect("valid stream chunk"));
    }
    let body = String::from_utf8(body).expect("UTF-8 SSE body");
    assert!(body.contains("data: [DONE]"));
    assert!(started.elapsed() > Duration::from_millis(800));

    let lease_diagnostics = gateway_lease(&client, gateway_address).await;
    assert_eq!(lease_diagnostics["rejections"], 1);

    lease_guard_renew_and_assert(&client, gateway_address, &lease).await;
}

#[tokio::test]
async fn serve_stale_keeps_readiness_and_new_requests_open() {
    let worker_address = spawn(fake_worker::app(Config::for_test("worker-stale"))).await;
    let worker_url = format!("http://{worker_address}");
    let (gateway_address, _lease) = spawn_gateway(
        &worker_url,
        Duration::from_millis(50),
        RoutingLeaseExpiryAction::ServeStale,
    )
    .await;
    let client = reqwest::Client::new();

    sleep(Duration::from_millis(80)).await;
    let readiness = client
        .get(format!("http://{gateway_address}/readyz"))
        .send()
        .await
        .expect("serve-stale readiness");
    assert_eq!(readiness.status(), reqwest::StatusCode::OK);
    let readiness_body: Value = readiness.json().await.expect("readiness JSON");
    assert_eq!(readiness_body["status"], "ready");
    assert_eq!(
        readiness_body["routing_lease"]["state"],
        "expired-serving-stale"
    );

    let response = client
        .post(format!("http://{gateway_address}/v1/chat/completions"))
        .json(&json!({
            "model": "inferlab-fake",
            "stream": false,
            "messages": [{"role": "user", "content": "availability mode"}]
        }))
        .send()
        .await
        .expect("serve-stale response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.headers()["x-inferlab-worker"], "worker");
}

async fn spawn_gateway(
    worker_url: &str,
    lease_duration: Duration,
    action: RoutingLeaseExpiryAction,
) -> (SocketAddr, Arc<RoutingLeaseGuard>) {
    let pool = Arc::new(
        WorkerPool::new(vec![("worker".to_owned(), worker_url.to_owned())])
            .expect("valid worker pool"),
    );
    let routing = Arc::new(RwLock::new(RoutingSnapshot::committed(pool, 7, 2)));
    let lease = Arc::new(RoutingLeaseGuard::from_live(lease_duration, action, 1_000));
    let app = app_with_runtime_config(
        routing,
        None,
        Some(Arc::clone(&lease)),
        AdmissionConfig::default(),
        ResilienceConfig::default(),
    )
    .expect("valid runtime gateway");
    (spawn(app).await, lease)
}

async fn lease_guard_renew_and_assert(
    client: &reqwest::Client,
    gateway_address: SocketAddr,
    lease: &Arc<RoutingLeaseGuard>,
) {
    lease.renew(2_000);
    let readiness = client
        .get(format!("http://{gateway_address}/readyz"))
        .send()
        .await
        .expect("renewed readiness");
    assert_eq!(readiness.status(), reqwest::StatusCode::OK);
    let response = client
        .post(format!("http://{gateway_address}/v1/chat/completions"))
        .json(&json!({
            "model": "inferlab-fake",
            "stream": false,
            "messages": [{"role": "user", "content": "renewed"}]
        }))
        .send()
        .await
        .expect("request after renewal");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
}

async fn worker_requests(client: &reqwest::Client, worker_url: &str) -> u64 {
    client
        .get(format!("{worker_url}/health"))
        .send()
        .await
        .expect("worker health")
        .json::<Value>()
        .await
        .expect("worker health JSON")["requests"]
        .as_u64()
        .expect("request count")
}

async fn gateway_lease(client: &reqwest::Client, gateway_address: SocketAddr) -> Value {
    client
        .get(format!("http://{gateway_address}/internal/workers"))
        .send()
        .await
        .expect("gateway diagnostics")
        .json::<Value>()
        .await
        .expect("gateway diagnostics JSON")["routing_lease"]
        .clone()
}

async fn spawn(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let address = listener.local_addr().expect("listener address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("test server");
    });
    address
}
