use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::State,
    http::{HeaderMap, HeaderName, Method, Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, put},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tracing::warn;

const STATUS_SCHEMA: &str = "inferlab.raft-link-status.v0.25";
const EVENT_SCHEMA: &str = "inferlab.raft-link-event.v0.25";
const MAX_FORWARDED_BODY_BYTES: usize = 2 * 1024 * 1024;
const UPSTREAM_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);
const UPSTREAM_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct LinkProxyConfig {
    pub link_id: String,
    pub source_id: String,
    pub target_id: String,
    pub upstream_base_url: String,
    pub event_path: PathBuf,
}

impl LinkProxyConfig {
    fn validate(&self) -> io::Result<()> {
        for (name, value) in [
            ("link_id", &self.link_id),
            ("source_id", &self.source_id),
            ("target_id", &self.target_id),
        ] {
            if value.trim().is_empty() || value.len() > 128 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{name} must contain 1 to 128 bytes"),
                ));
            }
        }
        if self.source_id == self.target_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source_id and target_id must differ",
            ));
        }
        let upstream = reqwest::Url::parse(&self.upstream_base_url).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("upstream_base_url is invalid: {error}"),
            )
        })?;
        if !matches!(upstream.scheme(), "http" | "https") || upstream.host().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "upstream_base_url must be an absolute HTTP or HTTPS URL",
            ));
        }
        if !upstream.username().is_empty() || upstream.password().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "upstream_base_url must not contain username or password userinfo",
            ));
        }
        if upstream.path() != "/" || upstream.query().is_some() || upstream.fragment().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "upstream_base_url must use the root path without query or fragment",
            ));
        }
        let upstream_ip = upstream.host_str().and_then(|host| {
            host.trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .ok()
        });
        if !upstream_ip.is_some_and(|address| address.is_loopback()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "upstream_base_url host must be an explicit loopback IP address",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkMode {
    Allow,
    Drop,
}

#[derive(Debug)]
struct ModeState {
    mode: LinkMode,
    last_transition_at_ms: u64,
    last_reason: String,
}

#[derive(Debug)]
struct EventJournal(Mutex<JournalState>);

#[derive(Debug)]
struct JournalState {
    file: File,
    next_sequence: u64,
}

impl EventJournal {
    fn open(path: &PathBuf) -> io::Result<Self> {
        // A sequence starts at one for exactly one proxy process. Reusing an
        // existing path would either duplicate sequence numbers or require a
        // crash-recovery protocol, so v0.25 fails closed and requires a fresh
        // proof-owned journal path on every start.
        let file = OpenOptions::new().create_new(true).write(true).open(path)?;
        Ok(Self(Mutex::new(JournalState {
            file,
            next_sequence: 1,
        })))
    }

    fn record(&self, event: &LinkEvent<'_>) -> io::Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| io::Error::other("link event journal mutex was poisoned"))?;
        let sequence = state.next_sequence;
        let mut encoded = serde_json::to_vec(&SequencedLinkEvent { sequence, event })
            .map_err(io::Error::other)?;
        encoded.push(b'\n');
        state.file.write_all(&encoded)?;
        state.file.flush()?;
        state.next_sequence = sequence.saturating_add(1);
        Ok(())
    }
}

#[derive(Debug)]
pub struct LinkProxy {
    config: LinkProxyConfig,
    client: reqwest::Client,
    started_at_ms: u64,
    mode: RwLock<ModeState>,
    mode_changes: AtomicU64,
    forwarded_requests: AtomicU64,
    dropped_requests: AtomicU64,
    upstream_failures: AtomicU64,
    journal: EventJournal,
}

impl LinkProxy {
    pub fn open(mut config: LinkProxyConfig) -> io::Result<Arc<Self>> {
        config.upstream_base_url = config.upstream_base_url.trim_end_matches('/').to_owned();
        config.validate()?;
        let started_at_ms = now_ms();
        let journal = EventJournal::open(&config.event_path)?;
        journal.record(&LinkEvent {
            schema: EVENT_SCHEMA,
            at_ms: started_at_ms,
            link_id: &config.link_id,
            source_id: &config.source_id,
            target_id: &config.target_id,
            event: "started",
            mode: LinkMode::Allow,
            method: None,
            path_and_query: None,
            reason: Some("proxy started in allow mode"),
            detail: None,
        })?;
        Ok(Arc::new(Self {
            config,
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(UPSTREAM_CONNECT_TIMEOUT)
                .timeout(UPSTREAM_REQUEST_TIMEOUT)
                .build()
                .map_err(io::Error::other)?,
            started_at_ms,
            mode: RwLock::new(ModeState {
                mode: LinkMode::Allow,
                last_transition_at_ms: started_at_ms,
                last_reason: "proxy started in allow mode".to_owned(),
            }),
            mode_changes: AtomicU64::new(0),
            forwarded_requests: AtomicU64::new(0),
            dropped_requests: AtomicU64::new(0),
            upstream_failures: AtomicU64::new(0),
            journal,
        }))
    }

    pub fn status(&self) -> io::Result<LinkStatus> {
        let mode = self
            .mode
            .read()
            .map_err(|_| io::Error::other("link mode lock was poisoned"))?;
        Ok(LinkStatus {
            schema: STATUS_SCHEMA.to_owned(),
            link_id: self.config.link_id.clone(),
            source_id: self.config.source_id.clone(),
            target_id: self.config.target_id.clone(),
            mode: mode.mode,
            upstream_base_url: self.config.upstream_base_url.clone(),
            started_at_ms: self.started_at_ms,
            last_transition_at_ms: mode.last_transition_at_ms,
            last_reason: mode.last_reason.clone(),
            mode_changes: self.mode_changes.load(Ordering::Relaxed),
            forwarded_requests: self.forwarded_requests.load(Ordering::Relaxed),
            dropped_requests: self.dropped_requests.load(Ordering::Relaxed),
            upstream_failures: self.upstream_failures.load(Ordering::Relaxed),
        })
    }

    fn mode(&self) -> io::Result<LinkMode> {
        self.mode
            .read()
            .map(|state| state.mode)
            .map_err(|_| io::Error::other("link mode lock was poisoned"))
    }

    fn set_mode(&self, request: ModeChangeRequest) -> io::Result<LinkStatus> {
        let reason = request.reason.trim();
        if reason.is_empty() || reason.len() > 256 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "reason must contain 1 to 256 bytes",
            ));
        }
        let at_ms = now_ms();
        let mut state = self
            .mode
            .write()
            .map_err(|_| io::Error::other("link mode lock was poisoned"))?;
        if state.mode != request.mode {
            self.journal.record(&LinkEvent {
                schema: EVENT_SCHEMA,
                at_ms,
                link_id: &self.config.link_id,
                source_id: &self.config.source_id,
                target_id: &self.config.target_id,
                event: "mode_changed",
                mode: request.mode,
                method: None,
                path_and_query: None,
                reason: Some(reason),
                detail: None,
            })?;
            state.mode = request.mode;
            state.last_transition_at_ms = at_ms;
            state.last_reason = reason.to_owned();
            self.mode_changes.fetch_add(1, Ordering::Relaxed);
        }
        drop(state);
        self.status()
    }

    fn record_request_event(
        &self,
        event: &'static str,
        mode: LinkMode,
        method: &str,
        path_and_query: &str,
        detail: Option<&str>,
    ) {
        let record = LinkEvent {
            schema: EVENT_SCHEMA,
            at_ms: now_ms(),
            link_id: &self.config.link_id,
            source_id: &self.config.source_id,
            target_id: &self.config.target_id,
            event,
            mode,
            method: Some(method),
            path_and_query: Some(path_and_query),
            reason: None,
            detail,
        };
        if let Err(error) = self.journal.record(&record) {
            warn!(link_id = %self.config.link_id, %error, "could not record Raft link event");
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LinkStatus {
    pub schema: String,
    pub link_id: String,
    pub source_id: String,
    pub target_id: String,
    pub mode: LinkMode,
    pub upstream_base_url: String,
    pub started_at_ms: u64,
    pub last_transition_at_ms: u64,
    pub last_reason: String,
    pub mode_changes: u64,
    pub forwarded_requests: u64,
    pub dropped_requests: u64,
    pub upstream_failures: u64,
}

#[derive(Debug, Deserialize)]
pub struct ModeChangeRequest {
    pub mode: LinkMode,
    pub reason: String,
}

#[derive(Serialize)]
struct LinkEvent<'a> {
    schema: &'static str,
    at_ms: u64,
    link_id: &'a str,
    source_id: &'a str,
    target_id: &'a str,
    event: &'static str,
    mode: LinkMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path_and_query: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
}

#[derive(Serialize)]
struct SequencedLinkEvent<'event, 'value> {
    sequence: u64,
    #[serde(flatten)]
    event: &'event LinkEvent<'value>,
}

#[derive(Serialize)]
struct LinkErrorBody<'a> {
    error: LinkErrorDetail<'a>,
}

#[derive(Serialize)]
struct LinkErrorDetail<'a> {
    code: &'static str,
    message: &'a str,
    link_id: &'a str,
    source_id: &'a str,
    target_id: &'a str,
}

pub fn link_proxy_app(proxy: Arc<LinkProxy>) -> Router {
    Router::new()
        .route("/healthz", get(link_health))
        .route("/v1/link/status", get(link_status))
        .route("/v1/link/mode", put(change_link_mode))
        .fallback(forward)
        .with_state(proxy)
}

async fn link_health() -> &'static str {
    "ok"
}

async fn link_status(State(proxy): State<Arc<LinkProxy>>) -> Response {
    match proxy.status() {
        Ok(status) => Json(status).into_response(),
        Err(error) => internal_error(&proxy, &error.to_string()),
    }
}

async fn change_link_mode(
    State(proxy): State<Arc<LinkProxy>>,
    Json(request): Json<ModeChangeRequest>,
) -> Response {
    match proxy.set_mode(request) {
        Ok(status) => Json(status).into_response(),
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => link_error(
            &proxy,
            StatusCode::BAD_REQUEST,
            "invalid_request",
            &error.to_string(),
        ),
        Err(error) => internal_error(&proxy, &error.to_string()),
    }
}

async fn forward(State(proxy): State<Arc<LinkProxy>>, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let method = parts.method.to_string();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map_or_else(|| parts.uri.path().to_owned(), ToString::to_string);
    let supported_path = matches!(
        parts.uri.path(),
        "/raft/request-vote" | "/raft/append-entries"
    );
    if parts.method != Method::POST || !supported_path || parts.uri.query().is_some() {
        return link_error(
            &proxy,
            StatusCode::NOT_FOUND,
            "unsupported_route",
            "link proxy forwards only exact query-free Raft POST routes",
        );
    }
    let mode = match proxy.mode() {
        Ok(mode) => mode,
        Err(error) => return internal_error(&proxy, &error.to_string()),
    };
    if mode == LinkMode::Drop {
        proxy.dropped_requests.fetch_add(1, Ordering::Relaxed);
        proxy.record_request_event(
            "request_dropped",
            mode,
            &method,
            &path_and_query,
            Some("link is in drop mode"),
        );
        return link_error(
            &proxy,
            StatusCode::SERVICE_UNAVAILABLE,
            "link_dropped",
            "directed Raft link is in drop mode",
        );
    }

    let body = match to_bytes(body, MAX_FORWARDED_BODY_BYTES).await {
        Ok(body) => body,
        Err(error) => {
            return link_error(
                &proxy,
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                &format!("request body exceeds {MAX_FORWARDED_BODY_BYTES} bytes: {error}"),
            );
        }
    };
    let url = format!("{}{}", proxy.config.upstream_base_url, path_and_query);
    let mut headers = parts.headers;
    remove_hop_by_hop_headers(&mut headers);
    let upstream = proxy
        .client
        .request(parts.method, url)
        .headers(headers)
        .body(body)
        .send()
        .await;
    let upstream = match upstream {
        Ok(response) => response,
        Err(error) => {
            proxy.upstream_failures.fetch_add(1, Ordering::Relaxed);
            let category = reqwest_error_category(&error);
            proxy.record_request_event(
                "upstream_failure",
                mode,
                &method,
                &path_and_query,
                Some(category),
            );
            return link_error(
                &proxy,
                StatusCode::BAD_GATEWAY,
                "upstream_failure",
                "directed Raft link could not reach its upstream",
            );
        }
    };
    let status = upstream.status();
    let mut headers = upstream.headers().clone();
    remove_hop_by_hop_headers(&mut headers);
    let body = match bounded_response_body(upstream).await {
        Ok(body) => body,
        Err(error) => {
            proxy.upstream_failures.fetch_add(1, Ordering::Relaxed);
            let category = upstream_body_error_category(&error);
            proxy.record_request_event(
                "upstream_failure",
                mode,
                &method,
                &path_and_query,
                Some(category),
            );
            return link_error(
                &proxy,
                StatusCode::BAD_GATEWAY,
                "upstream_failure",
                "directed Raft link could not read its upstream response",
            );
        }
    };
    proxy.forwarded_requests.fetch_add(1, Ordering::Relaxed);
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

fn remove_hop_by_hop_headers(headers: &mut HeaderMap) {
    let connection_headers = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect::<Vec<_>>();
    for name in connection_headers {
        headers.remove(name);
    }
    headers.remove(header::HOST);
    headers.remove(header::CONNECTION);
    headers.remove(header::PROXY_AUTHENTICATE);
    headers.remove(header::PROXY_AUTHORIZATION);
    headers.remove(header::TE);
    headers.remove(header::TRAILER);
    headers.remove(header::TRANSFER_ENCODING);
    headers.remove(header::UPGRADE);
    headers.remove("keep-alive");
    headers.remove("proxy-connection");
}

#[derive(Debug)]
enum UpstreamBodyError {
    TooLarge,
    Request(reqwest::Error),
}

async fn bounded_response_body(response: reqwest::Response) -> Result<Bytes, UpstreamBodyError> {
    // Raft responses are tiny JSON objects. Check both the declared length and
    // the actual stream so a misleading upstream cannot make buffering grow
    // without bound.
    if response
        .content_length()
        .is_some_and(|length| length > MAX_FORWARDED_BODY_BYTES as u64)
    {
        return Err(UpstreamBodyError::TooLarge);
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(UpstreamBodyError::Request)?;
        if body.len().saturating_add(chunk.len()) > MAX_FORWARDED_BODY_BYTES {
            return Err(UpstreamBodyError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body))
}

fn upstream_body_error_category(error: &UpstreamBodyError) -> &'static str {
    match error {
        UpstreamBodyError::TooLarge => "response_too_large",
        UpstreamBodyError::Request(error) => reqwest_error_category(error),
    }
}

fn reqwest_error_category(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else if error.is_request() {
        "request"
    } else {
        "other"
    }
}

fn internal_error(proxy: &LinkProxy, message: &str) -> Response {
    link_error(
        proxy,
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        message,
    )
}

fn link_error(
    proxy: &LinkProxy,
    status: StatusCode,
    code: &'static str,
    message: &str,
) -> Response {
    (
        status,
        Json(LinkErrorBody {
            error: LinkErrorDetail {
                code,
                message,
                link_id: &proxy.config.link_id,
                source_id: &proxy.config.source_id,
                target_id: &proxy.config.target_id,
            },
        }),
    )
        .into_response()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        fs,
        sync::{Mutex, atomic::AtomicUsize},
    };

    use axum::{
        body::Bytes,
        extract::State,
        http::{HeaderMap, Uri},
        routing::post,
    };
    use tokio::net::TcpListener;

    use super::*;

    static NEXT_TEST: AtomicUsize = AtomicUsize::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "inferlab-link-proxy-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create proxy test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Default)]
    struct CapturedRequests(Mutex<Vec<(HeaderMap, Bytes)>>);

    async fn capture_upstream(
        State(captured): State<Arc<CapturedRequests>>,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        captured
            .0
            .lock()
            .expect("capture lock")
            .push((headers, body.clone()));
        if body == "declared-oversized-response" {
            return Response::builder()
                .status(StatusCode::OK)
                .header(
                    header::CONTENT_LENGTH,
                    (MAX_FORWARDED_BODY_BYTES + 1).to_string(),
                )
                .body(Body::from(vec![b'x'; MAX_FORWARDED_BODY_BYTES + 1]))
                .expect("declared oversized response");
        }
        if body == "streamed-oversized-response" {
            let stream = futures_util::stream::iter([
                Ok::<_, Infallible>(Bytes::from(vec![b'x'; MAX_FORWARDED_BODY_BYTES])),
                Ok::<_, Infallible>(Bytes::from_static(b"x")),
            ]);
            return Response::builder()
                .status(StatusCode::OK)
                .body(Body::from_stream(stream))
                .expect("streamed oversized response");
        }
        if uri.path() == "/raft/append-entries" {
            return Response::builder()
                .status(StatusCode::TEMPORARY_REDIRECT)
                .header(header::LOCATION, "http://192.0.2.1/should-not-follow")
                .body(Body::empty())
                .expect("redirect response");
        }
        Response::builder()
            .status(StatusCode::ACCEPTED)
            .header("x-upstream-safe", "kept")
            .header(header::CONNECTION, "x-response-hop")
            .header("x-response-hop", "removed")
            .body(Body::from(body))
            .expect("capture response")
    }

    fn config(upstream_base_url: &str, event_path: PathBuf) -> LinkProxyConfig {
        LinkProxyConfig {
            link_id: "node-a-to-node-b".to_owned(),
            source_id: "node-a".to_owned(),
            target_id: "node-b".to_owned(),
            upstream_base_url: upstream_base_url.to_owned(),
            event_path,
        }
    }

    #[test]
    fn config_rejects_non_loopback_and_ambiguous_upstreams() {
        let event_path = PathBuf::from("unused.jsonl");
        assert!(
            config("http://127.0.0.1:9901", event_path.clone())
                .validate()
                .is_ok()
        );
        assert!(
            config("http://[::1]:9901", event_path.clone())
                .validate()
                .is_ok()
        );
        assert!(
            config("http://192.0.2.1:9901", event_path.clone())
                .validate()
                .is_err()
        );
        assert!(
            config("http://localhost:9901", event_path.clone())
                .validate()
                .is_err()
        );
        assert!(
            config("http://user:password@127.0.0.1:9901", event_path.clone())
                .validate()
                .is_err()
        );
        assert!(
            config("http://127.0.0.1:9901/base", event_path.clone())
                .validate()
                .is_err()
        );
        assert!(
            config("http://127.0.0.1:9901?secret=yes", event_path.clone())
                .validate()
                .is_err()
        );
        assert!(
            config("http://127.0.0.1:9901/#fragment", event_path)
                .validate()
                .is_err()
        );
    }

    #[test]
    fn hop_by_hop_and_connection_named_headers_are_removed() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONNECTION, "x-private-hop".parse().expect("header"));
        headers.insert("x-private-hop", "remove".parse().expect("header"));
        headers.insert("keep-alive", "timeout=5".parse().expect("header"));
        headers.insert("x-end-to-end", "preserve".parse().expect("header"));
        remove_hop_by_hop_headers(&mut headers);
        assert!(!headers.contains_key(header::CONNECTION));
        assert!(!headers.contains_key("x-private-hop"));
        assert!(!headers.contains_key("keep-alive"));
        assert_eq!(headers["x-end-to-end"], "preserve");
    }

    #[test]
    fn concurrent_journal_records_are_contiguous_in_file_order() {
        let directory = TestDirectory::new();
        let event_path = directory.0.join("ordered-events.jsonl");
        let proxy = LinkProxy::open(config("http://127.0.0.1:9901", event_path.clone()))
            .expect("open ordered proxy");
        proxy
            .set_mode(ModeChangeRequest {
                mode: LinkMode::Drop,
                reason: "exercise shared mode-event allocator".to_owned(),
            })
            .expect("record mode change");

        let threads = (0..64)
            .map(|index| {
                let proxy = Arc::clone(&proxy);
                std::thread::spawn(move || {
                    let event = if index % 2 == 0 {
                        "request_dropped"
                    } else {
                        "upstream_failure"
                    };
                    proxy.record_request_event(
                        event,
                        LinkMode::Drop,
                        "POST",
                        "/raft/append-entries",
                        Some("stable-category"),
                    );
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().expect("journal writer thread");
        }

        let sequences = fs::read_to_string(event_path)
            .expect("read ordered journal")
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .expect("parse ordered event")["sequence"]
                    .as_u64()
                    .expect("event sequence")
            })
            .collect::<Vec<_>>();
        assert_eq!(sequences, (1_u64..=66).collect::<Vec<_>>());
    }

    #[test]
    fn existing_journal_path_is_rejected_without_changing_evidence() {
        let directory = TestDirectory::new();
        let event_path = directory.0.join("single-owner-events.jsonl");
        let proxy = LinkProxy::open(config("http://127.0.0.1:9901", event_path.clone()))
            .expect("first proxy owns fresh journal");
        let before = fs::read(&event_path).expect("read initial journal");

        let error = LinkProxy::open(config("http://127.0.0.1:9901", event_path.clone()))
            .expect_err("reject reused journal path");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(event_path).expect("read rejected journal"), before);
        assert_eq!(
            proxy.status().expect("original proxy status").mode,
            LinkMode::Allow
        );
    }

    #[tokio::test]
    async fn forwards_only_bounded_exact_raft_posts_and_drop_is_observable() {
        let directory = TestDirectory::new();
        let captured = Arc::new(CapturedRequests::default());
        let upstream_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream");
        let upstream_url = format!(
            "http://{}",
            upstream_listener.local_addr().expect("upstream address")
        );
        let upstream = Router::new()
            .route("/raft/request-vote", post(capture_upstream))
            .route("/raft/append-entries", post(capture_upstream))
            .with_state(Arc::clone(&captured));
        let upstream_task = tokio::spawn(async move {
            axum::serve(upstream_listener, upstream)
                .await
                .expect("serve capture upstream");
        });

        let event_path = directory.0.join("events.jsonl");
        let proxy = LinkProxy::open(config(&upstream_url, event_path.clone())).expect("open proxy");
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
        let proxy_url = format!(
            "http://{}",
            proxy_listener.local_addr().expect("proxy address")
        );
        let proxy_task = tokio::spawn(async move {
            axum::serve(proxy_listener, link_proxy_app(proxy))
                .await
                .expect("serve proxy");
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("test client");

        let signed_body = br#"{"term":1,"candidate_id":"node-a"}"#;
        let forwarded = client
            .post(format!("{proxy_url}/raft/request-vote"))
            .header("x-inferlab-service-signature", "signature-secret")
            .header(header::CONNECTION, "x-private-hop")
            .header("x-private-hop", "hop-secret")
            .body(signed_body.as_slice())
            .send()
            .await
            .expect("forward exact request");
        assert_eq!(forwarded.status(), reqwest::StatusCode::ACCEPTED);
        assert_eq!(forwarded.headers()["x-upstream-safe"], "kept");
        assert!(!forwarded.headers().contains_key("x-response-hop"));
        assert_eq!(
            forwarded.bytes().await.expect("forwarded body"),
            signed_body.as_slice()
        );
        {
            let requests = captured.0.lock().expect("captured requests");
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0].0["x-inferlab-service-signature"],
                "signature-secret"
            );
            assert!(!requests[0].0.contains_key("x-private-hop"));
            assert_eq!(requests[0].1, signed_body.as_slice());
        }

        let queried = client
            .post(format!("{proxy_url}/raft/request-vote?not=raft"))
            .body("query-secret")
            .send()
            .await
            .expect("reject query");
        assert_eq!(queried.status(), reqwest::StatusCode::NOT_FOUND);
        let wrong_method = client
            .get(format!("{proxy_url}/raft/request-vote"))
            .send()
            .await
            .expect("reject method");
        assert_eq!(wrong_method.status(), reqwest::StatusCode::NOT_FOUND);
        let oversized = client
            .post(format!("{proxy_url}/raft/request-vote"))
            .body(vec![b'x'; MAX_FORWARDED_BODY_BYTES + 1])
            .send()
            .await
            .expect("reject oversized body");
        assert_eq!(oversized.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(captured.0.lock().expect("captured count").len(), 1);

        let declared_response = client
            .post(format!("{proxy_url}/raft/request-vote"))
            .body("declared-oversized-response")
            .send()
            .await
            .expect("bound declared oversized response");
        assert_eq!(declared_response.status(), reqwest::StatusCode::BAD_GATEWAY);
        let streamed_response = client
            .post(format!("{proxy_url}/raft/request-vote"))
            .body("streamed-oversized-response")
            .send()
            .await
            .expect("bound streamed oversized response");
        assert_eq!(streamed_response.status(), reqwest::StatusCode::BAD_GATEWAY);
        assert_eq!(captured.0.lock().expect("captured responses").len(), 3);

        let redirect = client
            .post(format!("{proxy_url}/raft/append-entries"))
            .body("append")
            .send()
            .await
            .expect("return upstream redirect");
        assert_eq!(redirect.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(captured.0.lock().expect("captured redirect").len(), 4);

        let mode = client
            .put(format!("{proxy_url}/v1/link/mode"))
            .json(&serde_json::json!({
                "mode": "drop",
                "reason": "test directed partition"
            }))
            .send()
            .await
            .expect("enable drop mode");
        assert_eq!(mode.status(), reqwest::StatusCode::OK);
        let mode: LinkStatus = mode.json().await.expect("mode status");
        assert_eq!(mode.mode, LinkMode::Drop);

        let dropped = client
            .post(format!("{proxy_url}/raft/request-vote"))
            .header("x-inferlab-service-signature", "new-signature-secret")
            .body("dropped-body-secret")
            .send()
            .await
            .expect("drop request");
        assert_eq!(dropped.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        let dropped: serde_json::Value = dropped.json().await.expect("drop error");
        assert_eq!(dropped["error"]["code"], "link_dropped");
        assert_eq!(captured.0.lock().expect("captured after drop").len(), 4);

        let status: LinkStatus = client
            .get(format!("{proxy_url}/v1/link/status"))
            .send()
            .await
            .expect("get link status")
            .json()
            .await
            .expect("decode link status");
        assert_eq!(status.schema, STATUS_SCHEMA);
        assert_eq!(status.mode_changes, 1);
        assert_eq!(status.forwarded_requests, 2);
        assert_eq!(status.dropped_requests, 1);
        assert_eq!(status.upstream_failures, 2);

        let journal = fs::read_to_string(event_path).expect("read event journal");
        assert!(journal.contains("mode_changed"));
        assert!(journal.contains("request_dropped"));
        assert!(!journal.contains("signature-secret"));
        assert!(!journal.contains("dropped-body-secret"));
        assert!(!journal.contains("query-secret"));

        proxy_task.abort();
        upstream_task.abort();
    }
}
