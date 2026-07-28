use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::Router;
use fake_worker::Config;
use futures_util::StreamExt;
use gateway::{
    admission::AdmissionConfig,
    app, app_with_admission, app_with_config,
    resilience::ResilienceConfig,
    routing::{RoutingConfig, RoutingPolicy, WorkerPool, WorkerRegistration},
};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::time::{sleep, timeout};

#[tokio::test]
async fn proxies_real_sse_streams_and_cycles_workers() {
    let mut registered = Vec::new();
    for id in ["worker-a", "worker-b", "worker-c"] {
        let address = spawn(fake_worker::app(Config::for_test(id))).await;
        registered.push((id.to_owned(), format!("http://{address}")));
    }

    let gateway_address = spawn(app(Arc::new(
        WorkerPool::new(registered).expect("valid worker pool"),
    )))
    .await;
    let client = reqwest::Client::new();

    for expected_worker in ["worker-a", "worker-b", "worker-c", "worker-a"] {
        let response = client
            .post(format!("http://{gateway_address}/v1/chat/completions"))
            .json(&json!({
                "model": "inferlab-fake",
                "stream": true,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await
            .expect("gateway response");

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("x-inferlab-worker")
                .expect("worker header"),
            expected_worker
        );

        let mut byte_stream = response.bytes_stream();
        let mut frames = 0;
        let mut body = Vec::new();
        while let Some(frame) = byte_stream.next().await {
            frames += 1;
            body.extend_from_slice(&frame.expect("valid response frame"));
        }

        let body = String::from_utf8(body).expect("UTF-8 SSE body");
        assert!(frames > 1, "expected incremental body frames, got {frames}");
        assert!(body.contains("data: [DONE]"));
        assert!(body.contains(expected_worker));
    }
}

#[tokio::test]
async fn proxies_non_streaming_json() {
    let worker_address = spawn(fake_worker::app(Config::for_test("worker-json"))).await;
    let gateway_address = spawn(app(Arc::new(
        WorkerPool::new(vec![(
            "worker-json".to_owned(),
            format!("http://{worker_address}"),
        )])
        .expect("valid worker pool"),
    )))
    .await;

    let response = reqwest::Client::new()
        .post(format!("http://{gateway_address}/v1/chat/completions"))
        .json(&json!({
            "model": "inferlab-fake",
            "stream": false,
            "messages": [{"role": "user", "content": "json please"}]
        }))
        .send()
        .await
        .expect("gateway response");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("JSON response");
    assert_eq!(body["object"], "chat.completion");
    assert!(
        body["choices"][0]["message"]["content"]
            .as_str()
            .expect("content")
            .contains("worker-json")
    );
}

#[tokio::test]
async fn holds_the_worker_lease_for_the_stream_lifetime() {
    let mut config = Config::for_test("worker-slow");
    config.token_delay = Duration::from_millis(100);
    let worker_address = spawn(fake_worker::app(config)).await;
    let gateway_address = spawn(app(Arc::new(
        WorkerPool::new(vec![(
            "worker-slow".to_owned(),
            format!("http://{worker_address}"),
        )])
        .expect("valid worker pool"),
    )))
    .await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("http://{gateway_address}/v1/chat/completions"))
        .json(&json!({
            "stream": true,
            "messages": [{"role": "user", "content": "slow stream"}]
        }))
        .send()
        .await
        .expect("stream response");
    let mut stream = response.bytes_stream();
    stream
        .next()
        .await
        .expect("first frame")
        .expect("valid first frame");

    assert_eq!(in_flight(&client, gateway_address).await, 1);
    drop(stream);

    timeout(Duration::from_secs(1), async {
        while in_flight(&client, gateway_address).await != 0 {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("lease should release after client disconnect");
}

#[tokio::test]
async fn passes_through_a_deterministic_worker_failure() {
    let mut config = Config::for_test("worker-failing");
    config.fail_every = Some(1);
    let worker_address = spawn(fake_worker::app(config)).await;
    let gateway_address = spawn(app(Arc::new(
        WorkerPool::new(vec![(
            "worker-failing".to_owned(),
            format!("http://{worker_address}"),
        )])
        .expect("valid worker pool"),
    )))
    .await;

    let response = reqwest::Client::new()
        .post(format!("http://{gateway_address}/v1/chat/completions"))
        .json(&json!({"stream": false, "messages": []}))
        .send()
        .await
        .expect("gateway response");

    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get("x-inferlab-worker")
            .expect("worker header"),
        "worker-failing"
    );
    let error: serde_json::Value = response.json().await.expect("error JSON");
    assert_eq!(error["error"]["type"], "fake_worker_failure");
}

#[tokio::test]
async fn consistent_hash_keeps_a_prompt_prefix_on_one_worker() {
    let mut registrations = Vec::new();
    for id in ["worker-a", "worker-b", "worker-c"] {
        let address = spawn(fake_worker::app(Config::for_test(id))).await;
        registrations.push(WorkerRegistration::new(id, format!("http://{address}"), 1));
    }
    let pool = WorkerPool::from_config(
        registrations,
        RoutingConfig {
            policy: RoutingPolicy::ConsistentHash,
            ewma_alpha: 0.25,
            ewma_probe_interval: 10,
            consistent_hash_virtual_nodes: 64,
            worker_concurrency_limit: 8,
        },
    )
    .expect("valid consistent-hash pool");
    let gateway_address = spawn(app(Arc::new(pool))).await;
    let client = reqwest::Client::new();
    let mut selected_workers = Vec::new();

    for request_number in 0..6 {
        let response = client
            .post(format!("http://{gateway_address}/v1/chat/completions"))
            .header("x-inferlab-cache-key", "tenant-7/shared-prefix")
            .json(&json!({
                "model": "inferlab-fake",
                "stream": false,
                "temperature": request_number as f64 / 10.0,
                "messages": [{"role": "user", "content": "shared system prompt"}]
            }))
            .send()
            .await
            .expect("gateway response");

        selected_workers.push(
            response
                .headers()
                .get("x-inferlab-worker")
                .expect("worker header")
                .to_str()
                .expect("ASCII worker ID")
                .to_owned(),
        );
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        response.bytes().await.expect("complete response body");
    }

    assert!(
        selected_workers
            .iter()
            .all(|worker| worker == &selected_workers[0]),
        "same cache key must retain worker affinity: {selected_workers:?}"
    );
}

#[tokio::test]
async fn bounds_execution_and_queue_then_rejects_overload() {
    let mut config = Config::for_test("worker-limited");
    config.token_delay = Duration::from_millis(100);
    let worker_address = spawn(fake_worker::app(config)).await;
    let gateway_address = spawn(
        app_with_admission(
            Arc::new(limited_pool(worker_address)),
            AdmissionConfig { queue_capacity: 1 },
        )
        .expect("valid admission configuration"),
    )
    .await;
    let client = reqwest::Client::new();
    let url = format!("http://{gateway_address}/v1/chat/completions");

    let first = client
        .post(&url)
        .json(&json!({"stream": true, "messages": [{"role": "user", "content": "first"}]}))
        .send()
        .await
        .expect("first response headers");
    assert_eq!(first.status(), reqwest::StatusCode::OK);

    let queued_client = client.clone();
    let queued_url = url.clone();
    let queued = tokio::spawn(async move {
        queued_client
            .post(queued_url)
            .json(&json!({"stream": true, "messages": [{"role": "user", "content": "second"}]}))
            .send()
            .await
    });
    let saturated = wait_for_admission(&client, gateway_address, 1, 1).await;
    assert_eq!(saturated["admission"]["outstanding"], 2);

    let rejected = client
        .post(&url)
        .json(&json!({"stream": false, "messages": [{"role": "user", "content": "third"}]}))
        .send()
        .await
        .expect("overload response");
    assert_eq!(rejected.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        rejected
            .headers()
            .get("retry-after")
            .expect("Retry-After header"),
        "1"
    );
    let error: serde_json::Value = rejected.json().await.expect("overload JSON");
    assert_eq!(error["error"]["type"], "gateway_overloaded");
    assert_eq!(error["error"]["reason"], "admission_queue_full");

    drop(first);
    let second = timeout(Duration::from_secs(2), queued)
        .await
        .expect("queued request should start after capacity is released")
        .expect("queued task")
        .expect("queued response");
    assert_eq!(second.status(), reqwest::StatusCode::OK);
    second.bytes().await.expect("complete queued response");

    let drained = wait_for_admission(&client, gateway_address, 0, 0).await;
    assert_eq!(drained["admission"]["rejected_total"], 1);
    assert_eq!(drained["admission"]["max_observed_executing"], 1);
    assert_eq!(drained["admission"]["max_observed_queued"], 1);
    assert_eq!(drained["admission"]["max_observed_outstanding"], 2);
}

#[tokio::test]
async fn cancelling_a_waiter_releases_its_queue_slot() {
    let mut config = Config::for_test("worker-cancellable");
    config.token_delay = Duration::from_millis(100);
    let worker_address = spawn(fake_worker::app(config)).await;
    let gateway_address = spawn(
        app_with_admission(
            Arc::new(limited_pool(worker_address)),
            AdmissionConfig { queue_capacity: 1 },
        )
        .expect("valid admission configuration"),
    )
    .await;
    let client = reqwest::Client::new();
    let url = format!("http://{gateway_address}/v1/chat/completions");

    let first = client
        .post(&url)
        .json(&json!({"stream": true, "messages": [{"role": "user", "content": "hold"}]}))
        .send()
        .await
        .expect("first response");
    let waiting_client = client.clone();
    let waiting_url = url.clone();
    let waiting = tokio::spawn(async move {
        waiting_client
            .post(waiting_url)
            .json(&json!({"stream": true, "messages": [{"role": "user", "content": "cancel"}]}))
            .send()
            .await
    });
    wait_for_admission(&client, gateway_address, 1, 1).await;

    waiting.abort();
    let _ = waiting.await;
    let after_cancel = wait_for_admission(&client, gateway_address, 1, 0).await;
    assert_eq!(after_cancel["admission"]["rejected_total"], 0);

    drop(first);
    wait_for_admission(&client, gateway_address, 0, 0).await;
}

#[tokio::test]
async fn retries_a_transient_failure_on_an_untried_worker() {
    let mut failing_config = Config::for_test("worker-a");
    failing_config.fail_every = Some(1);
    let failing_address = spawn(fake_worker::app(failing_config)).await;
    let healthy_address = spawn(fake_worker::app(Config::for_test("worker-b"))).await;
    let pool = Arc::new(
        WorkerPool::new(vec![
            ("worker-a".to_owned(), format!("http://{failing_address}")),
            ("worker-b".to_owned(), format!("http://{healthy_address}")),
        ])
        .expect("valid worker pool"),
    );
    let gateway_address = spawn(
        app_with_config(
            pool,
            AdmissionConfig { queue_capacity: 4 },
            resilience_config(Duration::from_secs(2), 1, 100),
        )
        .expect("valid resilient app"),
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("http://{gateway_address}/v1/chat/completions"))
        .json(&json!({
            "stream": false,
            "messages": [{"role": "user", "content": "retry safely"}]
        }))
        .send()
        .await
        .expect("gateway response");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.headers()["x-inferlab-worker"], "worker-b");
    assert_eq!(response.headers()["x-inferlab-attempts"], "2");
    let body: serde_json::Value = response.json().await.expect("completion JSON");
    assert!(
        body["choices"][0]["message"]["content"]
            .as_str()
            .expect("content")
            .contains("worker-b")
    );

    let client = reqwest::Client::new();
    let status = gateway_status(&client, gateway_address).await;
    assert_eq!(status["resilience"]["original_requests"], 1);
    assert_eq!(status["resilience"]["attempts"], 2);
    assert_eq!(status["resilience"]["transient_failures"], 1);
    assert_eq!(status["resilience"]["retries_granted"], 1);

    let worker_health: serde_json::Value = client
        .get(format!("http://{healthy_address}/health"))
        .send()
        .await
        .expect("worker health")
        .json()
        .await
        .expect("worker health JSON");
    assert_eq!(worker_health["last_attempt"], 2);
    assert!(
        worker_health["last_timeout_ms"]
            .as_u64()
            .is_some_and(|timeout_ms| timeout_ms > 0 && timeout_ms <= 500)
    );
}

#[tokio::test]
async fn deadline_stops_a_stream_without_retrying_after_headers() {
    let mut config = Config::for_test("worker-stream-deadline");
    config.token_delay = Duration::from_millis(100);
    let worker_address = spawn(fake_worker::app(config)).await;
    let pool = Arc::new(
        WorkerPool::new(vec![(
            "worker-stream-deadline".to_owned(),
            format!("http://{worker_address}"),
        )])
        .expect("valid worker pool"),
    );
    let gateway_address = spawn(
        app_with_config(
            pool,
            AdmissionConfig { queue_capacity: 1 },
            resilience_config(Duration::from_millis(170), 2, 100),
        )
        .expect("valid resilient app"),
    )
    .await;
    let client = reqwest::Client::new();
    let started = Instant::now();
    let response = client
        .post(format!("http://{gateway_address}/v1/chat/completions"))
        .json(&json!({
            "stream": true,
            "messages": [{"role": "user", "content": "deadline stream"}]
        }))
        .send()
        .await
        .expect("stream response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.headers()["x-inferlab-attempts"], "1");
    let body = response
        .bytes()
        .await
        .expect("deadline closes body cleanly");
    let elapsed = started.elapsed();

    assert!(elapsed >= Duration::from_millis(140));
    assert!(elapsed < Duration::from_millis(350));
    assert!(!String::from_utf8_lossy(&body).contains("data: [DONE]"));

    let status = gateway_status(&client, gateway_address).await;
    assert_eq!(status["resilience"]["attempts"], 1);
    assert_eq!(status["resilience"]["retries_granted"], 0);
    assert_eq!(status["resilience"]["deadline_exceeded"], 1);
}

#[tokio::test]
async fn request_deadline_includes_admission_queue_wait() {
    let worker_address = spawn(fake_worker::app(Config::for_test("worker-queued"))).await;
    let pool = Arc::new(limited_pool(worker_address));
    let held_lease = pool.choose();
    let held_execution = held_lease
        .try_reserve_execution()
        .expect("hold only worker execution slot");
    let gateway_address = spawn(
        app_with_config(
            Arc::clone(&pool),
            AdmissionConfig { queue_capacity: 1 },
            resilience_config(Duration::from_millis(100), 0, 100),
        )
        .expect("valid resilient app"),
    )
    .await;
    let client = reqwest::Client::new();
    let started = Instant::now();
    let response = client
        .post(format!("http://{gateway_address}/v1/chat/completions"))
        .json(&json!({"stream": false, "messages": []}))
        .send()
        .await
        .expect("deadline response");

    assert_eq!(response.status(), reqwest::StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(response.headers()["x-inferlab-attempts"], "0");
    assert!(started.elapsed() < Duration::from_millis(250));
    let error: serde_json::Value = response.json().await.expect("deadline JSON");
    assert_eq!(error["error"]["type"], "request_deadline_exceeded");
    let status = gateway_status(&client, gateway_address).await;
    assert_eq!(status["resilience"]["attempts"], 0);
    assert_eq!(status["resilience"]["deadline_exceeded"], 1);
    assert_eq!(status["admission"]["queued"], 0);

    drop(held_execution);
    drop(held_lease);
}

fn resilience_config(
    request_deadline: Duration,
    max_retries: usize,
    retry_budget_percent: u64,
) -> ResilienceConfig {
    ResilienceConfig {
        request_deadline,
        attempt_timeout: Duration::from_millis(500),
        max_retries,
        retry_budget_percent,
        retry_base_delay: Duration::from_millis(1),
        retry_max_delay: Duration::from_millis(1),
        jitter_seed: 7,
    }
}

fn limited_pool(worker_address: SocketAddr) -> WorkerPool {
    WorkerPool::from_config(
        vec![WorkerRegistration::new(
            "worker-limited",
            format!("http://{worker_address}"),
            1,
        )],
        RoutingConfig {
            policy: RoutingPolicy::RoundRobin,
            ewma_alpha: 0.25,
            ewma_probe_interval: 10,
            consistent_hash_virtual_nodes: 128,
            worker_concurrency_limit: 1,
        },
    )
    .expect("valid limited worker pool")
}

async fn in_flight(client: &reqwest::Client, gateway_address: SocketAddr) -> u64 {
    let status = gateway_status(client, gateway_address).await;
    status["workers"][0]["in_flight"]
        .as_u64()
        .expect("numeric in-flight count")
}

async fn gateway_status(
    client: &reqwest::Client,
    gateway_address: SocketAddr,
) -> serde_json::Value {
    client
        .get(format!("http://{gateway_address}/internal/workers"))
        .send()
        .await
        .expect("worker status response")
        .json()
        .await
        .expect("worker status JSON")
}

async fn wait_for_admission(
    client: &reqwest::Client,
    gateway_address: SocketAddr,
    expected_executing: u64,
    expected_queued: u64,
) -> serde_json::Value {
    timeout(Duration::from_secs(2), async {
        loop {
            let status = gateway_status(client, gateway_address).await;
            if status["admission"]["executing"].as_u64() == Some(expected_executing)
                && status["admission"]["queued"].as_u64() == Some(expected_queued)
            {
                return status;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("admission state should converge")
}

async fn spawn(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let address = listener.local_addr().expect("test address");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("test server exited cleanly");
    });
    address
}
