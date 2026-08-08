use std::{
    net::SocketAddr,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use fake_worker::Config;
use futures_util::StreamExt;
use gateway::{
    RoutingSnapshot, admission::AdmissionConfig,
    app_with_runtime_config_and_public_authentication_and_observability,
    public_authentication::PublicApiAuthenticator, resilience::ResilienceConfig,
    routing::WorkerPool,
};
use observability::{MetricsRegistry, REQUEST_ID_HEADER, RequestId};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::time::{sleep, timeout};

#[derive(Clone)]
struct CapturingWorker {
    status: StatusCode,
    request_ids: Arc<Mutex<Vec<String>>>,
}

async fn captured_completion(
    State(worker): State<CapturingWorker>,
    headers: HeaderMap,
) -> Response {
    let request_id = headers
        .get(&REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("missing")
        .to_owned();
    worker
        .request_ids
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(request_id);
    (
        worker.status,
        Json(json!({
            "object": "chat.completion",
            "choices": [{"message": {"content": "captured"}}]
        })),
    )
        .into_response()
}

async fn spawn_worker(status: StatusCode) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let request_ids = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/v1/chat/completions", post(captured_completion))
        .with_state(CapturingWorker {
            status,
            request_ids: Arc::clone(&request_ids),
        });
    (spawn(app).await, request_ids)
}

fn gateway_app(
    workers: WorkerPool,
    registry: &mut MetricsRegistry,
    authentication: PublicApiAuthenticator,
) -> Router {
    app_with_runtime_config_and_public_authentication_and_observability(
        Arc::new(RwLock::new(RoutingSnapshot::static_workers(Arc::new(
            workers,
        )))),
        None,
        None,
        AdmissionConfig { queue_capacity: 4 },
        ResilienceConfig {
            request_deadline: Duration::from_secs(2),
            attempt_timeout: Duration::from_millis(500),
            max_retries: 1,
            retry_budget_percent: 100,
            retry_base_delay: Duration::from_millis(1),
            retry_max_delay: Duration::from_millis(1),
            jitter_seed: 7,
        },
        authentication,
        registry,
    )
    .expect("gateway app")
}

#[tokio::test]
async fn one_valid_request_id_is_forwarded_unchanged_across_every_retry() {
    let (failing_address, failing_ids) = spawn_worker(StatusCode::SERVICE_UNAVAILABLE).await;
    let (healthy_address, healthy_ids) = spawn_worker(StatusCode::OK).await;
    let workers = WorkerPool::new(vec![
        (
            "private-failing-worker".to_owned(),
            format!("http://{failing_address}"),
        ),
        (
            "private-healthy-worker".to_owned(),
            format!("http://{healthy_address}"),
        ),
    ])
    .expect("workers");
    let mut registry = MetricsRegistry::new();
    let gateway = gateway_app(workers, &mut registry, PublicApiAuthenticator::disabled());
    let gateway_address = spawn(gateway).await;
    let request_id = "retry-stable-request-001";

    let response = reqwest::Client::new()
        .post(format!("http://{gateway_address}/v1/chat/completions"))
        .header(&REQUEST_ID_HEADER, request_id)
        .json(&json!({
            "stream": false,
            "messages": [{"role": "user", "content": "private-retry-prompt"}]
        }))
        .send()
        .await
        .expect("gateway response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.headers()[&REQUEST_ID_HEADER], request_id);
    assert_eq!(response.headers()["x-inferlab-attempts"], "2");
    response.bytes().await.expect("response body");

    assert_eq!(
        *failing_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec![request_id.to_owned()]
    );
    assert_eq!(
        *healthy_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec![request_id.to_owned()]
    );

    let first = registry.render().expect("first scrape");
    let second = registry.render().expect("second scrape");
    for metrics in [&first, &second] {
        assert!(metrics.contains("inferlab_gateway_requests_total 1"));
        assert!(metrics.contains("inferlab_gateway_attempts_total 2"));
        assert!(metrics.contains("inferlab_gateway_transient_failures_total 1"));
        assert!(metrics.contains("inferlab_gateway_retries_total{decision=\"granted\"} 1"));
        assert!(
            metrics.contains(
                "inferlab_gateway_completion_duration_seconds_count{outcome=\"success\"} 1"
            )
        );
        assert!(!metrics.contains(request_id));
        assert!(!metrics.contains("private-failing-worker"));
        assert!(!metrics.contains("private-healthy-worker"));
        assert!(!metrics.contains("private-retry-prompt"));
        let sample_series = metrics
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .count();
        assert!(sample_series <= 255, "sample series: {sample_series}");
    }
}

#[tokio::test]
async fn invalid_ids_are_replaced_and_error_responses_echo_the_canonical_id() {
    let (worker_address, worker_ids) = spawn_worker(StatusCode::INTERNAL_SERVER_ERROR).await;
    let workers = WorkerPool::new(vec![(
        "private-error-worker".to_owned(),
        format!("http://{worker_address}"),
    )])
    .expect("workers");
    let mut registry = MetricsRegistry::new();
    let gateway = gateway_app(workers, &mut registry, PublicApiAuthenticator::disabled());
    let gateway_address = spawn(gateway).await;
    let invalid = "invalid/request/id";

    let response = reqwest::Client::new()
        .post(format!("http://{gateway_address}/v1/chat/completions"))
        .header(&REQUEST_ID_HEADER, invalid)
        .json(&json!({"stream": false, "messages": []}))
        .send()
        .await
        .expect("gateway error response");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::INTERNAL_SERVER_ERROR
    );
    let replacement = response.headers()[&REQUEST_ID_HEADER]
        .to_str()
        .expect("request ID")
        .to_owned();
    assert_ne!(replacement, invalid);
    assert!(RequestId::parse(&replacement).is_ok());
    response.bytes().await.expect("error body");
    assert_eq!(
        *worker_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec![replacement.clone()]
    );

    let metrics = registry.render().expect("metrics");
    assert!(!metrics.contains(invalid));
    assert!(!metrics.contains(&replacement));
    assert!(
        metrics.contains("inferlab_gateway_completion_duration_seconds_count{outcome=\"error\"} 1")
    );
}

#[tokio::test]
async fn authentication_rejections_echo_ids_and_metrics_stay_on_a_separate_router() {
    let (worker_address, _) = spawn_worker(StatusCode::OK).await;
    let workers = WorkerPool::new(vec![(
        "private-auth-worker".to_owned(),
        format!("http://{worker_address}"),
    )])
    .expect("workers");
    let authentication =
        PublicApiAuthenticator::from_configuration(Some("interview-observability-key-0001"))
            .expect("authentication");
    let mut registry = MetricsRegistry::new();
    let gateway = gateway_app(workers, &mut registry, authentication);
    let gateway_address = spawn(gateway).await;
    let request_id = "authentication-error-401";
    let client = reqwest::Client::new();

    let response = client
        .post(format!("http://{gateway_address}/v1/chat/completions"))
        .header(&REQUEST_ID_HEADER, request_id)
        .json(&json!({"messages": []}))
        .send()
        .await
        .expect("authentication rejection");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers()[&REQUEST_ID_HEADER], request_id);

    let public_metrics = client
        .get(format!("http://{gateway_address}/metrics"))
        .send()
        .await
        .expect("public metrics route response");
    assert_eq!(public_metrics.status(), reqwest::StatusCode::NOT_FOUND);
    assert!(registry.render().expect("isolated metrics").contains(
        "inferlab_http_requests_total{service=\"gateway\",route=\"/v1/chat/completions\",method=\"POST\",status_class=\"4xx\"} 1"
    ));
}

#[tokio::test]
async fn streamed_completion_logs_and_histograms_distinguish_success_from_cancellation() {
    let mut worker = Config::for_test("private-stream-worker");
    worker.token_delay = Duration::from_millis(25);
    let worker_address = spawn(fake_worker::app(worker)).await;
    let workers = WorkerPool::new(vec![(
        "private-stream-worker".to_owned(),
        format!("http://{worker_address}"),
    )])
    .expect("workers");
    let mut registry = MetricsRegistry::new();
    let gateway = gateway_app(workers, &mut registry, PublicApiAuthenticator::disabled());
    let gateway_address = spawn(gateway).await;
    let client = reqwest::Client::new();
    let url = format!("http://{gateway_address}/v1/chat/completions");

    let completed = client
        .post(&url)
        .header(&REQUEST_ID_HEADER, "stream-complete-001")
        .json(&json!({
            "stream": true,
            "messages": [{"role": "user", "content": "complete"}]
        }))
        .send()
        .await
        .expect("completed stream response");
    assert!(
        completed
            .text()
            .await
            .expect("completed SSE")
            .contains("[DONE]")
    );

    let cancelled = client
        .post(&url)
        .header(&REQUEST_ID_HEADER, "stream-cancelled-001")
        .json(&json!({
            "stream": true,
            "messages": [{"role": "user", "content": "cancel"}]
        }))
        .send()
        .await
        .expect("cancelled stream response");
    let mut chunks = cancelled.bytes_stream();
    assert!(chunks.next().await.is_some());
    drop(chunks);

    let metrics = timeout(Duration::from_secs(2), async {
        loop {
            let metrics = registry.render().expect("metrics");
            if metrics.contains(
                "inferlab_gateway_completion_duration_seconds_count{outcome=\"cancelled\"} 1",
            ) {
                break metrics;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("gateway must observe downstream cancellation");
    assert!(
        metrics
            .contains("inferlab_gateway_completion_duration_seconds_count{outcome=\"success\"} 1")
    );
    assert!(!metrics.contains("stream-complete-001"));
    assert!(!metrics.contains("stream-cancelled-001"));
}

async fn spawn(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture");
    let address = listener.local_addr().expect("fixture address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("fixture server");
    });
    address
}
