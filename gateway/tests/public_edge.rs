use std::{
    convert::Infallible,
    net::SocketAddr,
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
    routing::post,
};
use futures_util::{StreamExt, stream};
use gateway::{
    RoutingSnapshot,
    admission::AdmissionConfig,
    hosted_apps_with_runtime_config_and_observability,
    public_authentication::{OperatorApiAuthenticator, PublicApiAuthenticator},
    public_edge::PublicEdgeConfig,
    resilience::ResilienceConfig,
    routing::{
        RoutingConfig, RoutingPolicy, WorkerExecutionPermit, WorkerPool, WorkerRegistration,
    },
};
use observability::MetricsRegistry;
use serde_json::{Value, json};
use tokio::{net::TcpListener, time::sleep};

const PUBLIC_KEY_A: &str = "hosted-public-key-alpha-0001";
const PUBLIC_KEY_B: &str = "hosted-public-key-bravo-0002";
const OPERATOR_KEY: &str = "hosted-operator-key-0000003";

#[derive(Clone, Default)]
struct WorkerCapture {
    requests: Arc<AtomicU64>,
    authorization_headers: Arc<Mutex<Vec<Option<String>>>>,
}

impl WorkerCapture {
    fn record(&self, headers: &HeaderMap) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.authorization_headers
            .lock()
            .expect("capture lock")
            .push(
                headers
                    .get(AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
            );
    }
}

async fn captured_completion(
    State(capture): State<WorkerCapture>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    capture.record(&headers);
    if request.get("temperature").is_some_and(Value::is_string) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"type": "worker_schema_error"}})),
        )
            .into_response();
    }
    Json(json!({
        "id": "hosted-edge-test",
        "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}}]
    }))
    .into_response()
}

async fn streaming_completion(
    State(capture): State<WorkerCapture>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    capture.record(&headers);
    let content = request["messages"][0]["content"]
        .as_str()
        .unwrap_or_default();
    let first = stream::once(async {
        Ok::<Bytes, Infallible>(Bytes::from_static(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
        ))
    });
    let body = if content == "hold" {
        Body::from_stream(first.chain(stream::pending::<Result<Bytes, Infallible>>()))
    } else {
        Body::from_stream(first.chain(stream::once(async {
            Ok::<Bytes, Infallible>(Bytes::from_static(b"data: [DONE]\n\n"))
        })))
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(body)
        .expect("stream response")
}

struct HostedFixture {
    public_address: SocketAddr,
    operator_address: SocketAddr,
    capture: WorkerCapture,
    metrics: MetricsRegistry,
    _reserved_worker_permit: Option<WorkerExecutionPermit>,
}

async fn hosted_fixture(
    worker_handler: Router<WorkerCapture>,
    config: PublicEdgeConfig,
    queue_capacity: usize,
    pre_reserve_worker: bool,
) -> HostedFixture {
    let worker_listener = TcpListener::bind("127.0.0.1:0").await.expect("worker bind");
    let worker_address = worker_listener.local_addr().expect("worker address");
    let capture = WorkerCapture::default();
    let worker_handler: Router = worker_handler.with_state(capture.clone());
    tokio::spawn(async move {
        axum::serve(worker_listener, worker_handler)
            .await
            .expect("worker server");
    });

    let mut routing_config = RoutingConfig::for_policy(RoutingPolicy::RoundRobin);
    routing_config.worker_concurrency_limit = 1;
    let workers = Arc::new(
        WorkerPool::from_config(
            vec![WorkerRegistration::new(
                "hosted-worker",
                format!("http://{worker_address}"),
                1,
            )],
            routing_config,
        )
        .expect("worker pool"),
    );
    let reserved_worker_permit = pre_reserve_worker.then(|| {
        workers
            .try_choose()
            .expect("fixture worker")
            .try_reserve_execution()
            .expect("reserve fixture worker")
    });
    let public_authentication =
        PublicApiAuthenticator::from_configuration(Some(&format!("{PUBLIC_KEY_A},{PUBLIC_KEY_B}")))
            .expect("public authentication");
    let operator_authentication =
        OperatorApiAuthenticator::from_configuration(OPERATOR_KEY).expect("operator auth");
    let mut metrics = MetricsRegistry::new();
    let apps = hosted_apps_with_runtime_config_and_observability(
        Arc::new(RwLock::new(RoutingSnapshot::static_workers(workers))),
        None,
        None,
        AdmissionConfig { queue_capacity },
        ResilienceConfig::default(),
        public_authentication,
        operator_authentication,
        config,
        &mut metrics,
    )
    .expect("hosted gateway apps");
    let public_address = spawn(apps.public).await;
    let operator_address = spawn(apps.operator).await;
    HostedFixture {
        public_address,
        operator_address,
        capture,
        metrics,
        _reserved_worker_permit: reserved_worker_permit,
    }
}

#[tokio::test]
async fn hosted_routers_isolate_capabilities_keys_status_and_metrics() {
    let fixture = hosted_fixture(
        Router::<WorkerCapture>::new().route("/v1/chat/completions", post(captured_completion)),
        PublicEdgeConfig::hosted(4, 128, 8, 60_000, 100).expect("edge config"),
        4,
        false,
    )
    .await;
    let client = reqwest::Client::new();

    for credential in [None, Some(PUBLIC_KEY_A), Some(OPERATOR_KEY)] {
        let mut request = client.get(format!(
            "http://{}/internal/workers",
            fixture.public_address
        ));
        if let Some(credential) = credential {
            request = request.bearer_auth(credential);
        }
        assert_eq!(
            request
                .send()
                .await
                .expect("public internal response")
                .status(),
            reqwest::StatusCode::NOT_FOUND
        );
    }

    for credential in [None, Some(PUBLIC_KEY_A)] {
        let mut request = client.get(format!(
            "http://{}/internal/workers",
            fixture.operator_address
        ));
        if let Some(credential) = credential {
            request = request.bearer_auth(credential);
        }
        assert_eq!(
            request.send().await.expect("operator rejection").status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
    }

    let operator_status: Value = client
        .get(format!(
            "http://{}/internal/workers",
            fixture.operator_address
        ))
        .bearer_auth(OPERATOR_KEY)
        .send()
        .await
        .expect("operator status")
        .json()
        .await
        .expect("operator JSON");
    assert_eq!(operator_status["public_edge"]["mode"], "hosted");
    assert_eq!(operator_status["public_edge"]["enforced"], true);
    assert_eq!(operator_status["public_edge"]["credential_count"], 2);
    assert_eq!(
        operator_status["operator_api_authentication"]["enabled"],
        true
    );

    let rejected = client
        .post(format!(
            "http://{}/v1/chat/completions",
            fixture.public_address
        ))
        .bearer_auth(OPERATOR_KEY)
        .json(&completion_request("hello", 1, false))
        .send()
        .await
        .expect("operator-on-public rejection");
    assert_edge_rejection(
        rejected,
        reqwest::StatusCode::UNAUTHORIZED,
        "invalid_api_key",
    )
    .await;

    let response = client
        .post(format!(
            "http://{}/v1/chat/completions",
            fixture.public_address
        ))
        .bearer_auth(PUBLIC_KEY_A)
        .json(&completion_request("hello", 1, false))
        .send()
        .await
        .expect("public completion");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let _: Value = response.json().await.expect("completion JSON");
    assert_eq!(fixture.capture.requests.load(Ordering::Relaxed), 1);
    assert_eq!(
        *fixture
            .capture
            .authorization_headers
            .lock()
            .expect("headers"),
        vec![None]
    );

    let showcase: Value = client
        .get(format!("http://{}/showcase/status", fixture.public_address))
        .bearer_auth(PUBLIC_KEY_B)
        .send()
        .await
        .expect("showcase status")
        .json()
        .await
        .expect("showcase JSON");
    assert_eq!(showcase["public_edge"]["mode"], "hosted");
    assert!(showcase["public_edge"].get("rejections").is_none());
    assert_eq!(showcase["release"]["version"], env!("CARGO_PKG_VERSION"));

    let metrics = fixture.metrics.render().expect("metrics");
    assert!(metrics.contains("inferlab_gateway_public_edge_rejections_total 1"));
    assert!(!metrics.contains(PUBLIC_KEY_A));
    assert!(!metrics.contains(OPERATOR_KEY));
}

#[tokio::test]
async fn every_hosted_edge_error_is_zero_attempt_and_chunked_size_is_bounded() {
    let fixture = hosted_fixture(
        Router::<WorkerCapture>::new().route("/v1/chat/completions", post(captured_completion)),
        PublicEdgeConfig::hosted(2, 12, 2, 1, 1).expect("edge config"),
        4,
        false,
    )
    .await;
    let client = reqwest::Client::new();
    let endpoint = format!("http://{}/v1/chat/completions", fixture.public_address);

    let missing_auth = client
        .post(&endpoint)
        .json(&completion_request("ok", 1, false))
        .send()
        .await
        .expect("missing auth");
    assert_edge_rejection(
        missing_auth,
        reqwest::StatusCode::UNAUTHORIZED,
        "invalid_api_key",
    )
    .await;

    for (body, status, code) in [
        (
            "{".to_owned(),
            reqwest::StatusCode::BAD_REQUEST,
            "malformed_json",
        ),
        (
            json!({"messages": [], "max_tokens": 1}).to_string(),
            reqwest::StatusCode::BAD_REQUEST,
            "invalid_messages",
        ),
        (
            json!({"messages": [message("a"), message("b"), message("c")], "max_tokens": 1})
                .to_string(),
            reqwest::StatusCode::BAD_REQUEST,
            "too_many_messages",
        ),
        (
            json!({"messages": [message("1234567890123")], "max_tokens": 1}).to_string(),
            reqwest::StatusCode::PAYLOAD_TOO_LARGE,
            "prompt_too_large",
        ),
        (
            json!({"messages": [message("ok")]}).to_string(),
            reqwest::StatusCode::BAD_REQUEST,
            "invalid_max_tokens",
        ),
        (
            json!({"messages": [message("ok")], "max_tokens": 3}).to_string(),
            reqwest::StatusCode::BAD_REQUEST,
            "max_output_tokens_exceeded",
        ),
    ] {
        let response = client
            .post(&endpoint)
            .bearer_auth(PUBLIC_KEY_A)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .expect("policy rejection");
        assert_edge_rejection(response, status, code).await;
    }

    let chunks =
        stream::iter((0..65).map(|_| Ok::<Bytes, std::io::Error>(Bytes::from(vec![b'x'; 1_024]))));
    let chunked = client
        .post(&endpoint)
        .bearer_auth(PUBLIC_KEY_A)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(reqwest::Body::wrap_stream(chunks))
        .send()
        .await
        .expect("chunked rejection");
    assert_edge_rejection(
        chunked,
        reqwest::StatusCode::PAYLOAD_TOO_LARGE,
        "body_too_large",
    )
    .await;

    let accepted = client
        .post(&endpoint)
        .bearer_auth(PUBLIC_KEY_A)
        .json(&completion_request("ok", 1, false))
        .send()
        .await
        .expect("accepted request");
    assert_eq!(accepted.status(), reqwest::StatusCode::OK);
    let _: Value = accepted.json().await.expect("accepted JSON");

    let rate_limited = client
        .post(&endpoint)
        .bearer_auth(PUBLIC_KEY_A)
        .json(&completion_request("ok", 1, false))
        .send()
        .await
        .expect("rate rejection");
    assert_eq!(
        rate_limited
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .expect("retry after"),
        "60"
    );
    assert_edge_rejection(
        rate_limited,
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
    )
    .await;

    let spoofed_slot = client
        .post(&endpoint)
        .bearer_auth(PUBLIC_KEY_A)
        .header("x-inferlab-credential-slot", "1")
        .json(&completion_request("ok", 1, false))
        .send()
        .await
        .expect("spoofed slot rejection");
    assert_edge_rejection(
        spoofed_slot,
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
    )
    .await;

    let second_slot = client
        .post(&endpoint)
        .bearer_auth(PUBLIC_KEY_B)
        .json(&completion_request("ok", 1, false))
        .send()
        .await
        .expect("isolated slot");
    assert_eq!(second_slot.status(), reqwest::StatusCode::OK);
    let _: Value = second_slot.json().await.expect("second slot JSON");
    assert_eq!(fixture.capture.requests.load(Ordering::Relaxed), 2);

    let status = operator_status(&client, fixture.operator_address).await;
    assert_eq!(status["resilience"]["original_requests"], 2);
    assert_eq!(status["resilience"]["attempts"], 2);
    assert_eq!(status["public_edge"]["rejections"]["authentication"], 1);
    assert_eq!(status["public_edge"]["rejections"]["body_too_large"], 1);
    assert_eq!(status["public_edge"]["rejections"]["rate_limited"], 2);
}

#[tokio::test]
async fn hosted_sse_holds_shared_permits_until_eof_or_downstream_drop() {
    let fixture = hosted_fixture(
        Router::<WorkerCapture>::new().route("/v1/chat/completions", post(streaming_completion)),
        PublicEdgeConfig::hosted(4, 128, 8, 60_000, 100).expect("edge config"),
        0,
        false,
    )
    .await;
    let client = reqwest::Client::new();
    let endpoint = format!("http://{}/v1/chat/completions", fixture.public_address);

    let held = client
        .post(&endpoint)
        .bearer_auth(PUBLIC_KEY_A)
        .json(&completion_request("hold", 1, true))
        .send()
        .await
        .expect("held SSE");
    assert_eq!(held.status(), reqwest::StatusCode::OK);
    wait_for_admission(&client, fixture.operator_address, 1).await;

    let overloaded = client
        .post(&endpoint)
        .bearer_auth(PUBLIC_KEY_B)
        .json(&completion_request("finite", 1, true))
        .send()
        .await
        .expect("admission rejection");
    assert_eq!(
        overloaded.status(),
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        "headers: {:?}",
        overloaded.headers()
    );
    assert_eq!(
        overloaded
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .expect("retry after"),
        "1"
    );
    let overloaded_body: Value = assert_edge_rejection(
        overloaded,
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        "admission_queue_full",
    )
    .await;
    assert_eq!(overloaded_body["error"]["type"], "gateway_overloaded");

    drop(held);
    wait_for_admission(&client, fixture.operator_address, 0).await;

    let finite = client
        .post(&endpoint)
        .bearer_auth(PUBLIC_KEY_B)
        .json(&completion_request("finite", 1, true))
        .send()
        .await
        .expect("finite SSE");
    assert_eq!(finite.status(), reqwest::StatusCode::OK);
    let body = finite.text().await.expect("finite body");
    assert!(body.contains("[DONE]"));
    wait_for_admission(&client, fixture.operator_address, 0).await;

    let status = operator_status(&client, fixture.operator_address).await;
    assert_eq!(status["public_edge"]["rejections"]["admission_full"], 1);
    assert_eq!(status["admission"]["outstanding"], 0);
    assert_eq!(status["admission"]["executing"], 0);
    assert_eq!(status["workers"][0]["in_flight"], 0);
}

#[tokio::test]
async fn worker_execution_admission_rejection_is_counted_as_zero_attempt_public_edge_work() {
    let fixture = hosted_fixture(
        Router::<WorkerCapture>::new().route("/v1/chat/completions", post(captured_completion)),
        PublicEdgeConfig::hosted(4, 128, 8, 60_000, 10).expect("edge config"),
        0,
        true,
    )
    .await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "http://{}/v1/chat/completions",
            fixture.public_address
        ))
        .bearer_auth(PUBLIC_KEY_A)
        .json(&completion_request("hello", 1, false))
        .send()
        .await
        .expect("worker-admission response");
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .expect("retry after"),
        "1"
    );
    let body = assert_edge_rejection(
        response,
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        "admission_queue_full",
    )
    .await;
    assert_eq!(body["error"]["type"], "gateway_overloaded");
    assert_eq!(fixture.capture.requests.load(Ordering::Relaxed), 0);

    let status = operator_status(&client, fixture.operator_address).await;
    assert_eq!(status["resilience"]["attempts"], 0);
    assert_eq!(status["public_edge"]["rejections"]["admission_full"], 1);
    assert!(
        fixture
            .metrics
            .render()
            .expect("metrics")
            .contains("inferlab_gateway_public_edge_rejections_total 1")
    );
}

#[tokio::test]
async fn non_edge_worker_schema_fields_remain_downstream_responsibility() {
    let fixture = hosted_fixture(
        Router::<WorkerCapture>::new().route("/v1/chat/completions", post(captured_completion)),
        PublicEdgeConfig::hosted(4, 128, 8, 60_000, 10).expect("edge config"),
        1,
        false,
    )
    .await;
    let response = reqwest::Client::new()
        .post(format!(
            "http://{}/v1/chat/completions",
            fixture.public_address
        ))
        .bearer_auth(PUBLIC_KEY_A)
        .json(&json!({
            "messages": [message("hello")],
            "max_tokens": 1,
            "temperature": "worker-rejects-this-type"
        }))
        .send()
        .await
        .expect("worker schema response");
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .headers()
            .get("x-inferlab-attempts")
            .expect("attempt count"),
        "1"
    );
    assert_eq!(fixture.capture.requests.load(Ordering::Relaxed), 1);
}

#[test]
fn hosted_configuration_fails_closed_before_listening_without_secret_echo() {
    let mut command = gateway_command();
    command.env_remove("INFERLAB_PUBLIC_API_KEYS");
    let missing_public = run_expected_failure(command);
    assert_failed_without_secret(missing_public, "requires explicit nonempty", OPERATOR_KEY);

    let mut command = gateway_command();
    command.env_remove("INFERLAB_OPERATOR_API_KEY");
    let missing_operator_key = run_expected_failure(command);
    assert_failed_without_secret(
        missing_operator_key,
        "INFERLAB_OPERATOR_API_KEY must be explicitly configured",
        PUBLIC_KEY_A,
    );

    let mut command = gateway_command();
    command.env_remove("INFERLAB_OPERATOR_BIND");
    let missing_operator_bind = run_expected_failure(command);
    assert_failed_without_secret(
        missing_operator_bind,
        "INFERLAB_OPERATOR_BIND must be explicitly configured",
        OPERATOR_KEY,
    );

    let mut command = gateway_command();
    command.env("INFERLAB_OPERATOR_API_KEY", "short");
    let short_operator = run_expected_failure(command);
    assert_failed_without_secret(
        short_operator,
        "INFERLAB_OPERATOR_API_KEY must be at least 16 bytes",
        "short",
    );

    let mut command = gateway_command();
    command.env("INFERLAB_PUBLIC_API_KEYS", "short");
    let short_public = run_expected_failure(command);
    assert_failed_without_secret(short_public, "entry 1 must be at least 16 bytes", "short");

    let mut command = gateway_command();
    command.env("INFERLAB_PUBLIC_API_KEYS", OPERATOR_KEY);
    let overlap = run_expected_failure(command);
    assert_failed_without_secret(overlap, "must not match", OPERATOR_KEY);

    let mut command = gateway_command();
    command.env("INFERLAB_OPERATOR_BIND", "127.0.0.1:18080");
    let collision = run_expected_failure(command);
    assert_failed_without_secret(collision, "must not overlap", OPERATOR_KEY);

    let mut command = gateway_command();
    command.env("INFERLAB_PUBLIC_MAX_MESSAGES", "257");
    let invalid_bound = run_expected_failure(command);
    assert_failed_without_secret(invalid_bound, "must not exceed 256", OPERATOR_KEY);

    let mut command = Command::new(env!("CARGO_BIN_EXE_gateway"));
    command
        .env_clear()
        .env("INFERLAB_PUBLIC_EDGE_MODE", "local")
        .env("INFERLAB_OPERATOR_BIND", "127.0.0.1:18081");
    let local_ambiguity = run_expected_failure(command);
    assert_failed_without_secret(
        local_ambiguity,
        "requires INFERLAB_PUBLIC_EDGE_MODE=hosted",
        "",
    );
}

#[tokio::test]
async fn hosted_binary_serves_the_public_and_operator_listeners_together() {
    let (public_address, operator_address) = unused_loopback_addresses();
    let mut command = gateway_command();
    command
        .env("INFERLAB_BIND", public_address.to_string())
        .env("INFERLAB_OPERATOR_BIND", operator_address.to_string())
        .env("INFERLAB_WORKERS", "unused=http://127.0.0.1:9")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut gateway = ChildGuard(command.spawn().expect("spawn hosted gateway"));
    let client = reqwest::Client::new();

    let mut ready = false;
    for _ in 0..200 {
        if let Some(status) = gateway.0.try_wait().expect("poll hosted gateway") {
            panic!("hosted gateway exited before serving: {status}");
        }
        if client
            .get(format!("http://{public_address}/health"))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            ready = true;
            break;
        }
        sleep(Duration::from_millis(5)).await;
    }
    assert!(ready, "hosted public listener did not become ready");
    assert_eq!(
        client
            .get(format!("http://{public_address}/internal/workers"))
            .bearer_auth(OPERATOR_KEY)
            .send()
            .await
            .expect("public isolation response")
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    assert_eq!(
        client
            .get(format!("http://{operator_address}/internal/workers"))
            .bearer_auth(OPERATOR_KEY)
            .send()
            .await
            .expect("operator status response")
            .status(),
        reqwest::StatusCode::OK
    );
}

#[cfg(unix)]
#[test]
fn hosted_non_unicode_security_configuration_fails_closed() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let mut command = Command::new(env!("CARGO_BIN_EXE_gateway"));
    command.env_clear().env(
        "INFERLAB_PUBLIC_EDGE_MODE",
        OsString::from_vec(vec![0xff, 0xfe]),
    );
    let mode = run_expected_failure(command);
    let stderr = String::from_utf8(mode.stderr).expect("UTF-8 error");
    assert!(!mode.status.success());
    assert!(stderr.contains("INFERLAB_PUBLIC_EDGE_MODE must be valid UTF-8"));

    let mut command = gateway_command();
    command.env(
        "INFERLAB_PUBLIC_RATE_BURST",
        OsString::from_vec(vec![0xff, 0xfe]),
    );
    let rate = run_expected_failure(command);
    let stderr = String::from_utf8(rate.stderr).expect("UTF-8 error");
    assert!(!rate.status.success());
    assert!(stderr.contains("INFERLAB_PUBLIC_RATE_BURST must be valid UTF-8"));

    let mut command = gateway_command();
    command.env("INFERLAB_BIND", OsString::from_vec(vec![0xff, 0xfe]));
    let bind = run_expected_failure(command);
    let stderr = String::from_utf8(bind.stderr).expect("UTF-8 error");
    assert!(!bind.status.success());
    assert!(stderr.contains("INFERLAB_BIND must be valid UTF-8 in hosted mode"));
}

fn gateway_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gateway"));
    command
        .env_clear()
        .env("INFERLAB_PUBLIC_EDGE_MODE", "hosted")
        .env("INFERLAB_BIND", "127.0.0.1:18080")
        .env("INFERLAB_OPERATOR_BIND", "127.0.0.1:18081")
        .env("INFERLAB_PUBLIC_API_KEYS", PUBLIC_KEY_A)
        .env("INFERLAB_OPERATOR_API_KEY", OPERATOR_KEY);
    command
}

fn run_expected_failure(mut command: Command) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn gateway failure probe");
    for _ in 0..200 {
        if child.try_wait().expect("poll gateway probe").is_some() {
            return child.wait_with_output().expect("collect gateway probe");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    child.kill().expect("kill unexpectedly serving gateway");
    let output = child.wait_with_output().expect("collect killed gateway");
    panic!(
        "gateway did not fail closed within one second; stdout={:?}, stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn unused_loopback_addresses() -> (SocketAddr, SocketAddr) {
    let public = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve public test port");
    let operator = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve operator test port");
    (
        public.local_addr().expect("public test port"),
        operator.local_addr().expect("operator test port"),
    )
}

fn assert_failed_without_secret(output: std::process::Output, expected: &str, secret: &str) {
    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains(expected), "stderr: {stderr}");
    if !secret.is_empty() {
        assert!(!stderr.contains(secret));
        assert!(!stdout.contains(secret));
    }
}

fn completion_request(content: &str, max_tokens: u64, stream: bool) -> Value {
    json!({
        "model": "inferlab-test",
        "stream": stream,
        "max_tokens": max_tokens,
        "messages": [message(content)]
    })
}

fn message(content: &str) -> Value {
    json!({"role": "user", "content": content})
}

async fn assert_edge_rejection(
    response: reqwest::Response,
    expected_status: reqwest::StatusCode,
    expected_code_or_reason: &str,
) -> Value {
    assert_eq!(response.status(), expected_status);
    assert_eq!(
        response
            .headers()
            .get("x-inferlab-attempts")
            .expect("zero-attempt header"),
        "0"
    );
    let body: Value = response.json().await.expect("error JSON");
    let observed = body["error"]
        .get("code")
        .or_else(|| body["error"].get("reason"))
        .and_then(Value::as_str);
    assert_eq!(observed, Some(expected_code_or_reason));
    body
}

async fn operator_status(client: &reqwest::Client, address: SocketAddr) -> Value {
    client
        .get(format!("http://{address}/internal/workers"))
        .bearer_auth(OPERATOR_KEY)
        .send()
        .await
        .expect("operator status")
        .json()
        .await
        .expect("operator JSON")
}

async fn wait_for_admission(client: &reqwest::Client, address: SocketAddr, expected: u64) {
    for _ in 0..100 {
        let status = operator_status(client, address).await;
        if status["admission"]["outstanding"].as_u64() == Some(expected)
            && status["admission"]["executing"].as_u64() == Some(expected)
        {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("admission did not reach {expected}");
}

async fn spawn(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fixture bind");
    let address = listener.local_addr().expect("fixture address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("fixture server");
    });
    address
}
