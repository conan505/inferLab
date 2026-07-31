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
use tokio::time::sleep;

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
        Session::new(self.clone(), prompt, max_tokens)
    }

    pub fn generate(&self, prompt: &str, max_tokens: u32) -> Result<Generation, String> {
        let prompt_token_ids = self.tokenize(prompt)?;
        let mut session = self.session(prompt, max_tokens)?;
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
            steps,
        })
    }
}

pub struct Session {
    pointer: NonNull<c_void>,
    model: Model,
    prompt_tokens: u32,
    step_index: usize,
}

// A session owns all mutable C++ state and is moved, never concurrently shared.
unsafe impl Send for Session {}

impl Session {
    fn new(model: Model, prompt: &str, max_tokens: u32) -> Result<Self, String> {
        let encoded = CString::new(prompt).map_err(|_| "prompt contains a NUL byte".to_owned())?;
        let mut error = error_buffer();
        // SAFETY: model, encoded, and error are valid for the complete call.
        let pointer = unsafe {
            inferlab_session_create(
                model.raw.pointer.as_ptr(),
                encoded.as_ptr(),
                max_tokens,
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
    pub steps: Vec<StepTrace>,
}

#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub id: String,
    pub token_delay: Duration,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            id: "cpu-worker-a".to_owned(),
            token_delay: Duration::ZERO,
        }
    }
}

#[derive(Clone)]
struct WorkerState {
    config: WorkerConfig,
    model: Model,
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
    Router::new()
        .route("/health", get(health))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(WorkerState {
            config,
            model,
            requests: Arc::new(AtomicU64::new(0)),
        })
}

async fn health(State(state): State<WorkerState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "inferlab-cpu-worker",
        "worker_id": state.config.id,
        "requests": state.requests.load(Ordering::Relaxed),
        "model": state.model.info(),
        "model_path": state.model.path()
    }))
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
            "v0.7 supports greedy decoding only; omit temperature or set it to 0",
        );
    }
    let prompt = last_message_text(&request.messages);
    let completion_id = format!("chatcmpl-{}-{request_number}", state.config.id);
    let created = unix_timestamp();
    if request.stream {
        let session = match state.model.session(&prompt, request.max_tokens) {
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
        with_worker_header(
            streaming_response(session, completion_id, created, state.config.token_delay),
            &state.config.id,
        )
    } else {
        match state.model.generate(&prompt, request.max_tokens) {
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
                        "prompt_tokens": generation.prompt_token_ids.len().saturating_sub(1),
                        "completion_tokens": generation.completion_tokens,
                        "total_tokens": generation.prompt_token_ids.len().saturating_sub(1)
                            + generation.completion_tokens
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
    session: Session,
    stage: StreamStage,
    completion_id: String,
    created: u64,
    token_delay: Duration,
    completion_tokens: usize,
}

fn streaming_response(
    session: Session,
    completion_id: String,
    created: u64,
    token_delay: Duration,
) -> Response {
    let machine = StreamMachine {
        session,
        stage: StreamStage::Role,
        completion_id,
        created,
        token_delay,
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
            StreamStage::Generate => {
                if !machine.token_delay.is_zero() {
                    sleep(machine.token_delay).await;
                }
                match machine.session.next_token() {
                    Ok(StepOutcome::Token(step)) => {
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
                    Ok(StepOutcome::EndOfSequence(_)) => {
                        machine.stage = StreamStage::Done;
                        finish_chunk(&machine, "stop")
                    }
                    Ok(StepOutcome::Length) => {
                        machine.stage = StreamStage::Done;
                        finish_chunk(&machine, "length")
                    }
                    Err(error) => {
                        machine.stage = StreamStage::Done;
                        json!({
                            "error": {
                                "type": "inference_error",
                                "message": error
                            }
                        })
                        .to_string()
                    }
                }
            }
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

fn finish_chunk(machine: &StreamMachine, reason: &str) -> String {
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
            "prompt_tokens": machine.session.prompt_tokens(),
            "completion_tokens": machine.completion_tokens,
            "total_tokens": machine.session.prompt_tokens() as usize
                + machine.completion_tokens
        }
    })
    .to_string()
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
    use super::{Model, StepOutcome};
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
}
