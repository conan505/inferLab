use std::{net::SocketAddr, path::PathBuf, time::Duration};

use cpu_worker::{Model, WorkerConfig};
use tokio::{net::TcpListener, task::JoinHandle};

fn model_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../models/tiny-inferlab-v1.bin")
}

async fn spawn_worker(batch_tick_delay: Duration) -> (SocketAddr, JoinHandle<()>) {
    spawn_worker_with_config(WorkerConfig {
        id: "cpu-test".to_owned(),
        batch_tick_delay,
        ..WorkerConfig::default()
    })
    .await
}

async fn spawn_worker_with_config(config: WorkerConfig) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let model = Model::load(model_path()).expect("model");
    let app = cpu_worker::app(model, config);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (address, task)
}

#[tokio::test]
async fn returns_openai_shaped_non_streaming_completion() {
    let (address, task) = spawn_worker(Duration::ZERO).await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "inferlab-tiny",
            "stream": false,
            "temperature": 0,
            "max_tokens": 8,
            "messages": [{"role": "user", "content": "teach me streaming"}]
        }))
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.headers()["x-inferlab-worker"], "cpu-test");
    let body: serde_json::Value = response.json().await.expect("JSON");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "InferLab turns prompts into real tokens."
    );
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["usage"]["completion_tokens"], 7);
    task.abort();
}

#[tokio::test]
async fn streams_each_generated_token_and_done_sentinel() {
    let (address, task) = spawn_worker(Duration::from_millis(1)).await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "inferlab-tiny",
            "stream": true,
            "temperature": 0,
            "max_tokens": 8,
            "messages": [{"role": "user", "content": "systems"}]
        }))
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.text().await.expect("SSE body");
    assert!(body.contains("\"content\":\"InferLab\""));
    assert!(body.contains("\"content\":\" turns\""));
    assert!(body.contains("\"finish_reason\":\"stop\""));
    assert!(body.contains("data: [DONE]"));
    task.abort();
}

#[tokio::test]
async fn rejects_sampling_before_streaming_starts() {
    let (address, task) = spawn_worker(Duration::ZERO).await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "inferlab-tiny",
            "temperature": 0.7,
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json().await.expect("JSON");
    assert_eq!(body["error"]["type"], "unsupported_sampling");
    task.abort();
}

#[tokio::test]
async fn http_requests_share_slots_and_backfill_continuously() {
    let (address, task) = spawn_worker_with_config(WorkerConfig {
        id: "cpu-batch-test".to_owned(),
        batch_tick_delay: Duration::from_millis(2),
        max_batch_size: 2,
        ..WorkerConfig::default()
    })
    .await;
    let client = reqwest::Client::new();
    let request = |max_tokens| {
        client
            .post(format!("http://{address}/v1/chat/completions"))
            .json(&serde_json::json!({
                "model": "inferlab-tiny",
                "stream": false,
                "temperature": 0,
                "max_tokens": max_tokens,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
    };
    let (short, long, backfill) = tokio::join!(request(2), request(8), request(2));
    let mut bodies = Vec::new();
    for response in [short, long, backfill] {
        let response = response.expect("response");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        bodies.push(response.json::<serde_json::Value>().await.expect("JSON"));
    }

    let snapshot: serde_json::Value = client
        .get(format!("http://{address}/internal/scheduler"))
        .send()
        .await
        .expect("scheduler response")
        .json()
        .await
        .expect("scheduler JSON");
    let scheduler = &snapshot["scheduler"];
    assert_eq!(scheduler["completed"], 3);
    assert_eq!(scheduler["max_active"], 2);
    assert_eq!(scheduler["active"], 0);
    assert_eq!(scheduler["queued"], 0);
    assert!(scheduler["slot_utilization_percent"].as_f64().unwrap() > 50.0);
    assert!(bodies.iter().all(|body| {
        body["inferlab"]["generation"]["mode"] == "kv-cache"
            && body["inferlab"]["generation"]["cache_bytes"]
                .as_u64()
                .unwrap()
                > 0
    }));
    task.abort();
}
