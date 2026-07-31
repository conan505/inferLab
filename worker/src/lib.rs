pub mod scheduler;

use std::{
    convert::Infallible,
    ffi::{CStr, CString, c_char, c_void},
    path::{Path, PathBuf},
    ptr::NonNull,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response, sse::Event, sse::Sse},
    routing::{get, post},
};
use futures_util::stream;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::scheduler::{
    ContinuousBatchScheduler, ScheduledRequest, SchedulerConfig, SchedulerEvent,
};

const MODEL_NAME: &str = "inferlab-tiny";
const WORKER_HEADER: &str = "x-inferlab-worker";
const ERROR_CAPACITY: usize = 512;
const TEXT_CAPACITY: usize = 256;

unsafe extern "C" {
    fn inferlab_model_load(
        path: *const c_char,
        error: *mut c_char,
        error_capacity: usize,
    ) -> *mut c_void;
    fn inferlab_model_free(model: *mut c_void);
    fn inferlab_model_vocab_size(model: *const c_void) -> u32;
    fn inferlab_model_context_length(model: *const c_void) -> u32;
    fn inferlab_model_dimension(model: *const c_void) -> u32;
    fn inferlab_model_heads(model: *const c_void) -> u32;
    fn inferlab_model_feed_forward_dimension(model: *const c_void) -> u32;
    fn inferlab_model_token(
        model: *const c_void,
        token_id: u32,
        token: *mut c_char,
        token_capacity: usize,
        error: *mut c_char,
        error_capacity: usize,
    ) -> i32;
    fn inferlab_tokenize(
        model: *const c_void,
        prompt: *const c_char,
        token_ids: *mut u32,
        token_capacity: usize,
        error: *mut c_char,
        error_capacity: usize,
    ) -> i64;
    fn inferlab_session_create(
        model: *const c_void,
        prompt: *const c_char,
        max_tokens: u32,
        use_kv_cache: u32,
        error: *mut c_char,
        error_capacity: usize,
    ) -> *mut c_void;
    fn inferlab_session_free(session: *mut c_void);
    fn inferlab_session_prompt_tokens(session: *const c_void) -> u32;
    fn inferlab_session_next(
        session: *mut c_void,
        token_id: *mut u32,
        piece: *mut c_char,
        piece_capacity: usize,
        logits: *mut f32,
        logits_capacity: usize,
        duration_ns: *mut u64,
        error: *mut c_char,
        error_capacity: usize,
    ) -> i32;
    fn inferlab_session_query_tokens(session: *const c_void) -> u64;
    fn inferlab_session_kv_tokens(session: *const c_void) -> u64;
    fn inferlab_session_attention_score_elements(session: *const c_void) -> u64;
    fn inferlab_session_cache_bytes(session: *const c_void) -> u64;
    fn inferlab_session_peak_cache_bytes(session: *const c_void) -> u64;
    fn inferlab_session_cache_rebuilds(session: *const c_void) -> u64;
}

struct RawModel {
    pointer: NonNull<c_void>,
}

// The C++ Model is immutable after loading; concurrent sessions only read it.
unsafe impl Send for RawModel {}
unsafe impl Sync for RawModel {}

impl Drop for RawModel {
    fn drop(&mut self) {
        // SAFETY: this pointer was returned by inferlab_model_load and is owned
        // by this RawModel exactly once.
        unsafe { inferlab_model_free(self.pointer.as_ptr()) };
    }
}

#[derive(Clone)]
pub struct Model {
    raw: Arc<RawModel>,
    path: Arc<PathBuf>,
    info: ModelInfo,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelInfo {
    pub name: &'static str,
    pub format: &'static str,
    pub dtype: &'static str,
    pub vocabulary: u32,
    pub context_length: u32,
    pub dimension: u32,
    pub heads: u32,
    pub feed_forward_dimension: u32,
    pub layers: u32,
}

impl Model {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let encoded = CString::new(path.to_string_lossy().as_bytes())
            .map_err(|_| "model path contains a NUL byte".to_owned())?;
        let mut error = error_buffer();
        // SAFETY: encoded and error remain alive for the complete call.
        let pointer =
            unsafe { inferlab_model_load(encoded.as_ptr(), error.as_mut_ptr(), error.len()) };
        let pointer = NonNull::new(pointer).ok_or_else(|| read_error(&error))?;
        let raw = Arc::new(RawModel { pointer });
        let info = ModelInfo {
            name: MODEL_NAME,
            format: "inferlab-tiny-fp32-v1",
            dtype: "float32",
            // SAFETY: the model pointer remains owned by raw and these accessors
            // only read validated scalar metadata.
            vocabulary: unsafe { inferlab_model_vocab_size(pointer.as_ptr()) },
            context_length: unsafe { inferlab_model_context_length(pointer.as_ptr()) },
            dimension: unsafe { inferlab_model_dimension(pointer.as_ptr()) },
            heads: unsafe { inferlab_model_heads(pointer.as_ptr()) },
            feed_forward_dimension: unsafe {
                inferlab_model_feed_forward_dimension(pointer.as_ptr())
            },
            layers: 1,
        };
        Ok(Self {
            raw,
            path: Arc::new(path.to_owned()),
            info,
        })
    }

    pub fn info(&self) -> &ModelInfo {
        &self.info
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn token(&self, token_id: u32) -> Result<String, String> {
        let mut token = vec![0 as c_char; TEXT_CAPACITY];
        let mut error = error_buffer();
        // SAFETY: all output buffers are valid for their advertised lengths.
        let status = unsafe {
            inferlab_model_token(
                self.raw.pointer.as_ptr(),
                token_id,
                token.as_mut_ptr(),
                token.len(),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if status == 0 {
            Ok(read_text(&token))
        } else {
            Err(read_error(&error))
        }
    }

    pub fn tokenize(&self, prompt: &str) -> Result<Vec<u32>, String> {
        let prompt = CString::new(prompt).map_err(|_| "prompt contains a NUL byte".to_owned())?;
        let mut error = error_buffer();
        // SAFETY: a null output with zero capacity asks only for the length.
        let count = unsafe {
            inferlab_tokenize(
                self.raw.pointer.as_ptr(),
                prompt.as_ptr(),
                std::ptr::null_mut(),
                0,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if count < 0 {
            return Err(read_error(&error));
        }
        let count = usize::try_from(count).map_err(|_| "token count overflow".to_owned())?;
        let mut token_ids = vec![0; count];
        // SAFETY: token_ids has exactly the capacity reported by the first call.
        let written = unsafe {
            inferlab_tokenize(
                self.raw.pointer.as_ptr(),
                prompt.as_ptr(),
                token_ids.as_mut_ptr(),
                token_ids.len(),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if written < 0 {
            return Err(read_error(&error));
        }
        Ok(token_ids)
    }

    pub fn session(&self, prompt: &str, max_tokens: u32) -> Result<Session, String> {
        self.session_with_mode(prompt, max_tokens, DecoderMode::KvCache)
    }

    pub fn session_with_mode(
        &self,
        prompt: &str,
        max_tokens: u32,
        mode: DecoderMode,
    ) -> Result<Session, String> {
        Session::new(self.clone(), prompt, max_tokens, mode)
    }

    pub fn generate(&self, prompt: &str, max_tokens: u32) -> Result<Generation, String> {
        self.generate_with_mode(prompt, max_tokens, DecoderMode::KvCache)
    }

    pub fn generate_with_mode(
        &self,
        prompt: &str,
        max_tokens: u32,
        mode: DecoderMode,
    ) -> Result<Generation, String> {
        let prompt_token_ids = self.tokenize(prompt)?;
        let mut session = self.session_with_mode(prompt, max_tokens, mode)?;
        let started = Instant::now();
        let mut steps = Vec::new();
        let mut text = String::new();
        let finish_reason = loop {
            match session.next_token()? {
                StepOutcome::Token(step) => {
                    text.push_str(&step.piece);
                    steps.push(step);
                }
                StepOutcome::EndOfSequence(step) => {
                    steps.push(step);
                    break "stop";
                }
                StepOutcome::Length => break "length",
            }
        };
        let generation_us = started.elapsed().as_secs_f64() * 1_000_000.0;
        let completion_tokens = steps.iter().filter(|step| !step.eos).count();
        let tokens_per_second = if generation_us > 0.0 {
            completion_tokens as f64 / (generation_us / 1_000_000.0)
        } else {
            0.0
        };
        let metrics = session.metrics();
        Ok(Generation {
            model: self.info.clone(),
            model_path: self.path.display().to_string(),
            prompt: prompt.to_owned(),
            prompt_token_ids,
            max_tokens,
            text,
            finish_reason,
            completion_tokens,
            generation_us,
            tokens_per_second,
            metrics,
            steps,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DecoderMode {
    Recompute,
    KvCache,
}

impl DecoderMode {
    fn ffi_value(self) -> u32 {
        match self {
            Self::Recompute => 0,
            Self::KvCache => 1,
        }
    }
}

impl std::str::FromStr for DecoderMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "recompute" => Ok(Self::Recompute),
            "kv-cache" => Ok(Self::KvCache),
            _ => Err(format!(
                "unknown decoder mode '{value}'; expected recompute or kv-cache"
            )),
        }
    }
}

pub struct Session {
    pointer: NonNull<c_void>,
    model: Model,
    prompt_tokens: u32,
    step_index: usize,
    mode: DecoderMode,
}

// A session owns all mutable C++ state and is moved, never concurrently shared.
unsafe impl Send for Session {}

impl Session {
    fn new(model: Model, prompt: &str, max_tokens: u32, mode: DecoderMode) -> Result<Self, String> {
        let encoded = CString::new(prompt).map_err(|_| "prompt contains a NUL byte".to_owned())?;
        let mut error = error_buffer();
        // SAFETY: model, encoded, and error are valid for the complete call.
        let pointer = unsafe {
            inferlab_session_create(
                model.raw.pointer.as_ptr(),
                encoded.as_ptr(),
                max_tokens,
                mode.ffi_value(),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        let pointer = NonNull::new(pointer).ok_or_else(|| read_error(&error))?;
        // SAFETY: pointer is a newly created valid session.
        let prompt_tokens = unsafe { inferlab_session_prompt_tokens(pointer.as_ptr()) };
        Ok(Self {
            pointer,
            model,
            prompt_tokens,
            step_index: 0,
            mode,
        })
    }

    pub fn prompt_tokens(&self) -> u32 {
        self.prompt_tokens
    }

    pub fn next_token(&mut self) -> Result<StepOutcome, String> {
        let mut token_id = 0;
        let mut piece = vec![0 as c_char; TEXT_CAPACITY];
        let mut logits = vec![0.0; self.model.info.vocabulary as usize];
        let mut duration_ns = 0;
        let mut error = error_buffer();
        // SAFETY: this session owns pointer and every output buffer is valid.
        let status = unsafe {
            inferlab_session_next(
                self.pointer.as_ptr(),
                &mut token_id,
                piece.as_mut_ptr(),
                piece.len(),
                logits.as_mut_ptr(),
                logits.len(),
                &mut duration_ns,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if status < 0 {
            return Err(read_error(&error));
        }
        if status == 0 {
            return Ok(StepOutcome::Length);
        }
        let eos = status == 2;
        let step = StepTrace {
            index: self.step_index,
            token_id,
            token: self.model.token(token_id)?,
            piece: read_text(&piece),
            eos,
            duration_us: duration_ns as f64 / 1_000.0,
            logits,
        };
        self.step_index += 1;
        if eos {
            Ok(StepOutcome::EndOfSequence(step))
        } else {
            Ok(StepOutcome::Token(step))
        }
    }

    pub fn metrics(&self) -> GenerationMetrics {
        // SAFETY: every accessor reads counters from this live, uniquely owned session.
        unsafe {
            GenerationMetrics {
                mode: self.mode,
                query_tokens: inferlab_session_query_tokens(self.pointer.as_ptr()),
                kv_tokens: inferlab_session_kv_tokens(self.pointer.as_ptr()),
                attention_score_elements: inferlab_session_attention_score_elements(
                    self.pointer.as_ptr(),
                ),
                cache_bytes: inferlab_session_cache_bytes(self.pointer.as_ptr()),
                peak_cache_bytes: inferlab_session_peak_cache_bytes(self.pointer.as_ptr()),
                cache_rebuilds: inferlab_session_cache_rebuilds(self.pointer.as_ptr()),
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: this pointer was returned by inferlab_session_create and is
        // owned by this Session exactly once.
        unsafe { inferlab_session_free(self.pointer.as_ptr()) };
    }
}

pub enum StepOutcome {
    Token(StepTrace),
    EndOfSequence(StepTrace),
    Length,
}

#[derive(Clone, Debug, Serialize)]
pub struct StepTrace {
    pub index: usize,
    pub token_id: u32,
    pub token: String,
    pub piece: String,
    pub eos: bool,
    pub duration_us: f64,
    pub logits: Vec<f32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Generation {
    pub model: ModelInfo,
    pub model_path: String,
    pub prompt: String,
    pub prompt_token_ids: Vec<u32>,
    pub max_tokens: u32,
    pub text: String,
    pub finish_reason: &'static str,
    pub completion_tokens: usize,
    pub generation_us: f64,
    pub tokens_per_second: f64,
    pub metrics: GenerationMetrics,
    pub steps: Vec<StepTrace>,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct GenerationMetrics {
    pub mode: DecoderMode,
    pub query_tokens: u64,
    pub kv_tokens: u64,
    pub attention_score_elements: u64,
    pub cache_bytes: u64,
    pub peak_cache_bytes: u64,
    pub cache_rebuilds: u64,
}

#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub id: String,
    pub batch_tick_delay: Duration,
    pub decoder_mode: DecoderMode,
    pub max_batch_size: usize,
    pub scheduler_queue_capacity: usize,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            id: "cpu-worker-a".to_owned(),
            batch_tick_delay: Duration::ZERO,
            decoder_mode: DecoderMode::KvCache,
            max_batch_size: 4,
            scheduler_queue_capacity: 64,
        }
    }
}

#[derive(Clone)]
struct WorkerState {
    config: WorkerConfig,
    model: Model,
    scheduler: ContinuousBatchScheduler,
    requests: Arc<AtomicU64>,
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    #[serde(default = "default_model")]
    model: String,
    #[serde(default)]
    messages: Vec<ChatMessage>,
    #[serde(default)]
    stream: bool,
    #[serde(default = "default_max_tokens")]
    max_tokens: u32,
    temperature: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    #[allow(dead_code)]
    role: String,
    content: Value,
}

pub fn app(model: Model, config: WorkerConfig) -> Router {
    try_app(model, config).expect("worker scheduler configuration is valid")
}

pub fn try_app(model: Model, config: WorkerConfig) -> Result<Router, String> {
    let scheduler = ContinuousBatchScheduler::start(SchedulerConfig {
        max_batch_size: config.max_batch_size,
        queue_capacity: config.scheduler_queue_capacity,
        tick_delay: config.batch_tick_delay,
    })?;
    Ok(Router::new()
        .route("/health", get(health))
        .route("/internal/scheduler", get(scheduler_status))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(WorkerState {
            config,
            model,
            scheduler,
            requests: Arc::new(AtomicU64::new(0)),
        }))
}

async fn health(State(state): State<WorkerState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "inferlab-cpu-worker",
        "worker_id": state.config.id,
        "requests": state.requests.load(Ordering::Relaxed),
        "model": state.model.info(),
        "model_path": state.model.path(),
        "decoder_mode": state.config.decoder_mode,
        "scheduler": state.scheduler.snapshot()
    }))
}

async fn scheduler_status(State(state): State<WorkerState>) -> Json<Value> {
    Json(json!({"scheduler": state.scheduler.snapshot()}))
}

async fn chat_completions(
    State(state): State<WorkerState>,
    Json(request): Json<ChatRequest>,
) -> Response {
    let request_number = state.requests.fetch_add(1, Ordering::Relaxed) + 1;
    if request.model != MODEL_NAME {
        return worker_error(
            &state.config.id,
            StatusCode::BAD_REQUEST,
            "model_not_found",
            format!("worker serves '{MODEL_NAME}', not '{}'", request.model),
        );
    }
    if request
        .temperature
        .is_some_and(|temperature| temperature != 0.0)
    {
        return worker_error(
            &state.config.id,
            StatusCode::BAD_REQUEST,
            "unsupported_sampling",
            "v0.8 supports greedy decoding only; omit temperature or set it to 0",
        );
    }
    let prompt = last_message_text(&request.messages);
    let completion_id = format!("chatcmpl-{}-{request_number}", state.config.id);
    let created = unix_timestamp();
    let session =
        match state
            .model
            .session_with_mode(&prompt, request.max_tokens, state.config.decoder_mode)
        {
            Ok(session) => session,
            Err(error) => {
                return worker_error(
                    &state.config.id,
                    StatusCode::BAD_REQUEST,
                    "invalid_generation_request",
                    error,
                );
            }
        };
    let scheduled = match state.scheduler.submit(session) {
        Ok(scheduled) => scheduled,
        Err(error) => {
            return worker_error(
                &state.config.id,
                if error.contains("full") {
                    StatusCode::TOO_MANY_REQUESTS
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                },
                "scheduler_unavailable",
                error,
            );
        }
    };
    if request.stream {
        with_worker_header(
            streaming_response(scheduled, completion_id, created),
            &state.config.id,
        )
    } else {
        match collect_scheduled(scheduled).await {
            Ok(generation) => with_worker_header(
                Json(json!({
                    "id": completion_id,
                    "object": "chat.completion",
                    "created": created,
                    "model": MODEL_NAME,
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": generation.text
                        },
                        "finish_reason": generation.finish_reason
                    }],
                    "usage": {
                        "prompt_tokens": generation.prompt_tokens,
                        "completion_tokens": generation.completion_tokens,
                        "total_tokens": generation.prompt_tokens as usize
                            + generation.completion_tokens
                    },
                    "inferlab": {
                        "request_id": generation.request_id,
                        "generation": generation.metrics
                    }
                }))
                .into_response(),
                &state.config.id,
            ),
            Err(error) => worker_error(
                &state.config.id,
                StatusCode::BAD_REQUEST,
                "invalid_generation_request",
                error,
            ),
        }
    }
}

enum StreamStage {
    Role,
    Generate,
    Done,
    End,
}

struct StreamMachine {
    scheduled: ScheduledRequest,
    stage: StreamStage,
    completion_id: String,
    created: u64,
    completion_tokens: usize,
}

fn streaming_response(
    scheduled: ScheduledRequest,
    completion_id: String,
    created: u64,
) -> Response {
    let machine = StreamMachine {
        scheduled,
        stage: StreamStage::Role,
        completion_id,
        created,
        completion_tokens: 0,
    };
    let events = stream::unfold(machine, |mut machine| async move {
        let payload = match machine.stage {
            StreamStage::Role => {
                machine.stage = StreamStage::Generate;
                json!({
                    "id": machine.completion_id,
                    "object": "chat.completion.chunk",
                    "created": machine.created,
                    "model": MODEL_NAME,
                    "choices": [{
                        "index": 0,
                        "delta": {"role": "assistant"},
                        "finish_reason": null
                    }]
                })
                .to_string()
            }
            StreamStage::Generate => match machine.scheduled.events.recv().await {
                Some(SchedulerEvent::Token(step)) => {
                    machine.completion_tokens += 1;
                    json!({
                        "id": machine.completion_id,
                        "object": "chat.completion.chunk",
                        "created": machine.created,
                        "model": MODEL_NAME,
                        "choices": [{
                            "index": 0,
                            "delta": {"content": step.piece},
                            "finish_reason": null
                        }]
                    })
                    .to_string()
                }
                Some(SchedulerEvent::Finished {
                    finish_reason,
                    metrics,
                }) => {
                    machine.stage = StreamStage::Done;
                    finish_chunk(&machine, finish_reason, metrics)
                }
                Some(SchedulerEvent::Error(error)) => {
                    machine.stage = StreamStage::Done;
                    json!({
                        "error": {
                            "type": "inference_error",
                            "message": error
                        }
                    })
                    .to_string()
                }
                None => {
                    machine.stage = StreamStage::Done;
                    json!({
                        "error": {
                            "type": "scheduler_closed",
                            "message": "scheduler ended before this request completed"
                        }
                    })
                    .to_string()
                }
            },
            StreamStage::Done => {
                machine.stage = StreamStage::End;
                "[DONE]".to_owned()
            }
            StreamStage::End => return None,
        };
        Some((
            Ok::<Event, Infallible>(Event::default().data(payload)),
            machine,
        ))
    });
    Sse::new(events).into_response()
}

fn finish_chunk(machine: &StreamMachine, reason: &str, metrics: GenerationMetrics) -> String {
    json!({
        "id": machine.completion_id,
        "object": "chat.completion.chunk",
        "created": machine.created,
        "model": MODEL_NAME,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": reason
        }],
        "usage": {
            "prompt_tokens": machine.scheduled.prompt_tokens,
            "completion_tokens": machine.completion_tokens,
            "total_tokens": machine.scheduled.prompt_tokens as usize
                + machine.completion_tokens
        },
        "inferlab": {
            "request_id": machine.scheduled.id,
            "generation": metrics
        }
    })
    .to_string()
}

struct ScheduledCompletion {
    request_id: u64,
    prompt_tokens: u32,
    text: String,
    completion_tokens: usize,
    finish_reason: &'static str,
    metrics: GenerationMetrics,
}

async fn collect_scheduled(mut scheduled: ScheduledRequest) -> Result<ScheduledCompletion, String> {
    let mut text = String::new();
    let mut completion_tokens = 0;
    while let Some(event) = scheduled.events.recv().await {
        match event {
            SchedulerEvent::Token(step) => {
                text.push_str(&step.piece);
                completion_tokens += 1;
            }
            SchedulerEvent::Finished {
                finish_reason,
                metrics,
            } => {
                return Ok(ScheduledCompletion {
                    request_id: scheduled.id,
                    prompt_tokens: scheduled.prompt_tokens,
                    text,
                    completion_tokens,
                    finish_reason,
                    metrics,
                });
            }
            SchedulerEvent::Error(error) => return Err(error),
        }
    }
    Err("scheduler ended before this request completed".to_owned())
}

fn worker_error(
    worker_id: &str,
    status: StatusCode,
    error_type: &str,
    message: impl Into<String>,
) -> Response {
    with_worker_header(
        (
            status,
            Json(json!({
                "error": {
                    "type": error_type,
                    "message": message.into()
                }
            })),
        )
            .into_response(),
        worker_id,
    )
}

fn with_worker_header(mut response: Response, worker_id: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(worker_id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(WORKER_HEADER), value);
    }
    response
}

fn last_message_text(messages: &[ChatMessage]) -> String {
    match messages.last().map(|message| &message.content) {
        Some(Value::String(content)) => content.clone(),
        Some(content) => content.to_string(),
        None => String::new(),
    }
}

fn default_model() -> String {
    MODEL_NAME.to_owned()
}

fn default_max_tokens() -> u32 {
    8
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn error_buffer() -> Vec<c_char> {
    vec![0; ERROR_CAPACITY]
}

fn read_error(buffer: &[c_char]) -> String {
    let message = read_text(buffer);
    if message.is_empty() {
        "C++ runtime returned an unspecified error".to_owned()
    } else {
        message
    }
}

fn read_text(buffer: &[c_char]) -> String {
    // SAFETY: all FFI output buffers are zero-initialized, and the C++ side
    // guarantees NUL termination on success and failure.
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::{DecoderMode, Model, StepOutcome};
    use std::{fs, path::PathBuf};

    fn model_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../models/tiny-inferlab-v1.bin")
    }

    #[test]
    fn loads_checkpoint_and_exposes_dimensions() {
        let model = Model::load(model_path()).expect("valid model");
        assert_eq!(model.info().vocabulary, 16);
        assert_eq!(model.info().context_length, 32);
        assert_eq!(model.info().dimension, 16);
        assert_eq!(model.info().heads, 4);
    }

    #[test]
    fn tokenizes_known_words_and_unknowns() {
        let model = Model::load(model_path()).expect("valid model");
        assert_eq!(
            model.tokenize("Teach me unknown.").expect("tokenize"),
            vec![1, 12, 13, 3, 10]
        );
    }

    #[test]
    fn greedy_generation_is_deterministic_and_meaningful() {
        let model = Model::load(model_path()).expect("valid model");
        let generation = model.generate("teach me streaming", 8).expect("generate");
        assert_eq!(generation.text, "InferLab turns prompts into real tokens.");
        assert_eq!(
            generation
                .steps
                .iter()
                .map(|step| step.token_id)
                .collect::<Vec<_>>(),
            vec![4, 5, 6, 7, 8, 9, 10, 2]
        );
        assert_eq!(generation.finish_reason, "stop");
    }

    #[test]
    fn length_limit_stops_without_eos() {
        let model = Model::load(model_path()).expect("valid model");
        let generation = model.generate("hello", 3).expect("generate");
        assert_eq!(generation.text, "InferLab turns prompts");
        assert_eq!(generation.finish_reason, "length");
    }

    #[test]
    fn rejects_invalid_checkpoint() {
        let path = std::env::temp_dir().join(format!(
            "inferlab-invalid-model-{}-{}.bin",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        fs::write(&path, b"not a model").expect("write invalid fixture");
        let result = Model::load(&path);
        fs::remove_file(&path).expect("remove invalid fixture");
        assert!(result.is_err());
    }

    #[test]
    fn session_reports_eos_separately_from_visible_tokens() {
        let model = Model::load(model_path()).expect("valid model");
        let mut session = model.session("systems", 8).expect("session");
        for _ in 0..7 {
            assert!(matches!(
                session.next_token().expect("visible token"),
                StepOutcome::Token(_)
            ));
        }
        assert!(matches!(
            session.next_token().expect("eos"),
            StepOutcome::EndOfSequence(_)
        ));
    }

    #[test]
    fn kv_cache_preserves_logits_and_tokens_while_reducing_work() {
        let model = Model::load(model_path()).expect("valid model");
        let recomputed = model
            .generate_with_mode("teach me streaming", 8, DecoderMode::Recompute)
            .expect("recomputed generation");
        let cached = model
            .generate_with_mode("teach me streaming", 8, DecoderMode::KvCache)
            .expect("cached generation");
        assert_eq!(cached.text, recomputed.text);
        assert_eq!(cached.steps.len(), recomputed.steps.len());
        for (cached_step, recomputed_step) in cached.steps.iter().zip(&recomputed.steps) {
            assert_eq!(cached_step.token_id, recomputed_step.token_id);
            for (cached_logit, recomputed_logit) in
                cached_step.logits.iter().zip(&recomputed_step.logits)
            {
                assert!((cached_logit - recomputed_logit).abs() <= 1.0e-6);
            }
        }
        assert_eq!(recomputed.metrics.query_tokens, 60);
        assert_eq!(recomputed.metrics.kv_tokens, 60);
        assert_eq!(recomputed.metrics.attention_score_elements, 1_104);
        assert_eq!(recomputed.metrics.peak_cache_bytes, 0);
        assert_eq!(cached.metrics.query_tokens, 8);
        assert_eq!(cached.metrics.kv_tokens, 11);
        assert_eq!(cached.metrics.attention_score_elements, 240);
        assert_eq!(cached.metrics.cache_bytes, 1_408);
        assert_eq!(cached.metrics.peak_cache_bytes, 1_408);
        assert_eq!(cached.metrics.cache_rebuilds, 0);
    }
}
