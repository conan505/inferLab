use std::{net::SocketAddr, path::PathBuf, time::Duration};

use cpu_worker::{Model, WorkerConfig};
use tokio::{net::TcpListener, task::JoinHandle};

fn model_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../models/tiny-inferlab-v1.bin")
}

fn model_v2_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../models/tiny-inferlab-v2.bin")
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
    spawn_worker_with_model(config, model_path()).await
}

async fn spawn_worker_with_model(
    config: WorkerConfig,
    path: PathBuf,
) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let model = Model::load(path).expect("model");
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
async fn sampling_with_the_same_seed_replays_exactly() {
    let (address, task) = spawn_worker(Duration::ZERO).await;
    let client = reqwest::Client::new();
    let request = || {
        client
            .post(format!("http://{address}/v1/chat/completions"))
            .json(&serde_json::json!({
                "model": "inferlab-tiny",
                "temperature": 0.8,
                "top_k": 4,
                "top_p": 0.9,
                "seed": 42,
                "max_tokens": 4,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
    };
    let first: serde_json::Value = request()
        .await
        .expect("first response")
        .json()
        .await
        .expect("first JSON");
    let replay: serde_json::Value = request()
        .await
        .expect("replay response")
        .json()
        .await
        .expect("replay JSON");
    assert_eq!(first["choices"], replay["choices"]);
    assert_eq!(
        first["inferlab"]["generation"]["decoding"]["sampled_steps"],
        4
    );
    assert_eq!(first["inferlab"]["generation"]["decoding"]["seed"], 42);
    task.abort();
}

#[tokio::test]
async fn json_schema_masks_every_streamed_token_into_valid_json() {
    let (address, task) = spawn_worker_with_model(
        WorkerConfig {
            id: "cpu-json-test".to_owned(),
            ..WorkerConfig::default()
        },
        model_v2_path(),
    )
    .await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "inferlab-tiny",
            "stream": false,
            "temperature": 1.0,
            "seed": 99,
            "max_tokens": 6,
            "messages": [{"role": "user", "content": "teach me streaming"}],
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "inference_summary",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "properties": {
                            "answer": {"type": "string", "enum": ["InferLab", "systems", "tokens"]},
                            "confidence": {"type": "string", "enum": ["high", "medium", "low"]}
                        },
                        "required": ["answer", "confidence"],
                        "additionalProperties": false
                    }
                }
            }
        }))
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("response JSON");
    let content = body["choices"][0]["message"]["content"]
        .as_str()
        .expect("content");
    let parsed: serde_json::Value = serde_json::from_str(content).expect("valid JSON content");
    assert!(parsed["answer"].is_string());
    assert!(parsed["confidence"].is_string());
    assert_eq!(
        body["inferlab"]["generation"]["decoding"]["grammar_constrained_steps"],
        6
    );
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
        body["inferlab"]["generation"]["mode"] == "paged-kv-cache"
            && body["inferlab"]["generation"]["cache_bytes"]
                .as_u64()
                .unwrap()
                > 0
            && body["inferlab"]["generation"]["cache_pages"]
                .as_u64()
                .unwrap()
                > 0
    }));
    task.abort();
}

#[tokio::test]
async fn repeated_prompt_reuses_paged_prefix_and_exposes_cache_stats() {
    let (address, task) = spawn_worker(Duration::ZERO).await;
    let client = reqwest::Client::new();
    let request = || {
        client
            .post(format!("http://{address}/v1/chat/completions"))
            .json(&serde_json::json!({
                "model": "inferlab-tiny",
                "stream": false,
                "temperature": 0,
                "max_tokens": 2,
                "messages": [{"role": "user", "content": "hello systems"}]
            }))
            .send()
    };
    let cold: serde_json::Value = request()
        .await
        .expect("cold response")
        .json()
        .await
        .expect("cold JSON");
    let warm: serde_json::Value = request()
        .await
        .expect("warm response")
        .json()
        .await
        .expect("warm JSON");
    assert_eq!(cold["inferlab"]["generation"]["prefix_cache_hit"], false);
    assert_eq!(warm["inferlab"]["generation"]["prefix_cache_hit"], true);
    assert_eq!(warm["inferlab"]["generation"]["prefix_tokens_reused"], 3);
    assert_eq!(cold["inferlab"]["generation"]["kv_tokens"], 4);
    assert_eq!(warm["inferlab"]["generation"]["kv_tokens"], 1);

    let cache: serde_json::Value = client
        .get(format!("http://{address}/internal/cache"))
        .send()
        .await
        .expect("cache response")
        .json()
        .await
        .expect("cache JSON");
    assert_eq!(cache["cache"]["prefix_entries"], 1);
    assert_eq!(cache["cache"]["prefix_hits"], 1);
    assert_eq!(cache["cache"]["prefix_misses"], 1);
    assert_eq!(cache["cache"]["copy_on_write_copies"], 2);
    task.abort();
}
