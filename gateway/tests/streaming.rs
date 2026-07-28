use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::Router;
use fake_worker::Config;
use futures_util::StreamExt;
use gateway::{app, routing::WorkerPool};
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

async fn in_flight(client: &reqwest::Client, gateway_address: SocketAddr) -> u64 {
    let status: serde_json::Value = client
        .get(format!("http://{gateway_address}/internal/workers"))
        .send()
        .await
        .expect("worker status response")
        .json()
        .await
        .expect("worker status JSON");
    status["workers"][0]["in_flight"]
        .as_u64()
        .expect("numeric in-flight count")
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
