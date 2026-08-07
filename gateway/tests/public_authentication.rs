use std::{
    net::SocketAddr,
    process::Command,
    sync::{Arc, RwLock},
};

use axum::Router;
use fake_worker::Config;
use gateway::{
    RoutingSnapshot, admission::AdmissionConfig, app_with_runtime_config_and_public_authentication,
    public_authentication::PublicApiAuthenticator, resilience::ResilienceConfig,
    routing::WorkerPool,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;

const FIRST_KEY: &str = "interview-demo-key-0001";
const SECOND_KEY: &str = "interview-demo-key-0002";

#[test]
fn invalid_environment_configuration_fails_before_serving_without_echoing_the_secret() {
    let secret = "do-not-print";
    let output = Command::new(env!("CARGO_BIN_EXE_gateway"))
        .env("INFERLAB_PUBLIC_API_KEYS", secret)
        .output()
        .expect("run gateway");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("INFERLAB_PUBLIC_API_KEYS entry 1"));
    assert!(!stderr.contains(secret));
}

#[tokio::test]
async fn configured_keys_protect_public_work_and_diagnostics_but_not_probes_or_showcase() {
    let worker_address = spawn(fake_worker::app(Config::for_test("authenticated-worker"))).await;
    let workers = Arc::new(
        WorkerPool::new(vec![(
            "authenticated-worker".to_owned(),
            format!("http://{worker_address}"),
        )])
        .expect("worker pool"),
    );
    let authenticator =
        PublicApiAuthenticator::from_configuration(Some(&format!("{FIRST_KEY},{SECOND_KEY}")))
            .expect("public authentication configuration");
    let gateway_address = spawn(
        app_with_runtime_config_and_public_authentication(
            Arc::new(RwLock::new(RoutingSnapshot::static_workers(workers))),
            None,
            None,
            AdmissionConfig::default(),
            ResilienceConfig::default(),
            authenticator,
        )
        .expect("gateway app"),
    )
    .await;
    let client = reqwest::Client::new();

    for path in ["/", "/health", "/readyz"] {
        let response = client
            .get(format!("http://{gateway_address}{path}"))
            .send()
            .await
            .expect("probe response");
        assert_eq!(response.status(), reqwest::StatusCode::OK, "{path}");
    }

    for path in ["/showcase/status", "/internal/workers"] {
        let unauthenticated_status = client
            .get(format!("http://{gateway_address}{path}"))
            .send()
            .await
            .expect("gateway status rejection");
        assert_eq!(
            unauthenticated_status.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "{path}"
        );
    }

    for authorization in [None, Some("Bearer wrong-interview-key-0001")] {
        let mut request = client
            .post(format!("http://{gateway_address}/v1/chat/completions"))
            .json(&completion_request());
        if let Some(authorization) = authorization {
            request = request.header(reqwest::header::AUTHORIZATION, authorization);
        }
        let response = request.send().await.expect("authentication rejection");
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(
            response
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .expect("challenge"),
            "Bearer realm=\"inferlab\""
        );
        let body: Value = response.json().await.expect("authentication error JSON");
        assert_eq!(body["error"]["type"], "authentication_error");
        assert_eq!(body["error"]["code"], "invalid_api_key");
        assert_eq!(
            body["error"]["message"],
            "A valid bearer API key is required."
        );
    }

    let status_response = client
        .get(format!("http://{gateway_address}/internal/workers"))
        .bearer_auth(FIRST_KEY)
        .send()
        .await
        .expect("gateway status");
    let status_text = status_response.text().await.expect("status body");
    assert!(!status_text.contains(FIRST_KEY));
    assert!(!status_text.contains(SECOND_KEY));
    let status: Value = serde_json::from_str(&status_text).expect("status JSON");
    assert_eq!(status["public_api_authentication"]["enabled"], true);
    assert_eq!(status["public_api_authentication"]["key_count"], 2);
    assert_eq!(status["admission"]["max_observed_outstanding"], 0);
    assert_eq!(status["resilience"]["original_requests"], 0);

    let showcase_status: Value = client
        .get(format!("http://{gateway_address}/showcase/status"))
        .bearer_auth(SECOND_KEY)
        .send()
        .await
        .expect("showcase status")
        .json()
        .await
        .expect("showcase status JSON");
    assert_eq!(showcase_status["routing_policy"], "round-robin");
    assert_eq!(showcase_status["worker_count"], 1);
    assert!(showcase_status.get("workers").is_none());
    assert!(showcase_status.get("control_plane").is_none());
    assert!(showcase_status.get("admission").is_none());

    let oversized = client
        .post(format!("http://{gateway_address}/v1/chat/completions"))
        .bearer_auth(FIRST_KEY)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("x".repeat(64 * 1024 + 1))
        .send()
        .await
        .expect("oversized request rejection");
    assert_eq!(oversized.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);

    let response = client
        .post(format!("http://{gateway_address}/v1/chat/completions"))
        .bearer_auth(SECOND_KEY)
        .json(&completion_request())
        .send()
        .await
        .expect("authorized completion");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("completion JSON");
    assert_eq!(body["object"], "chat.completion");
}

fn completion_request() -> Value {
    json!({
        "model": "inferlab-fake",
        "stream": false,
        "messages": [{"role": "user", "content": "hello"}]
    })
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
