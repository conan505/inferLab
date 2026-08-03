pub mod decoding;
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
pub use decoding::{
    DecodingConfig, DecodingKind, JsonSchemaEnvelope, ResponseFormat, SamplingConfig,
    TinyObjectSchema, TinyStringSchema, inference_summary_response_format,
};
use decoding::{TokenDfa, compile_constraint};

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
    fn inferlab_model_configure_paged_cache(
        model: *mut c_void,
        page_tokens: u32,
        page_count: u32,
        prefix_capacity: u32,
        error: *mut c_char,
        error_capacity: usize,
    ) -> i32;
    fn inferlab_model_paged_cache_stats(
        model: *const c_void,
        stats: *mut RawPagedCacheStats,
        error: *mut c_char,
        error_capacity: usize,
    ) -> i32;
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
        cache_mode: u32,
        seed: u64,
        error: *mut c_char,
        error_capacity: usize,
    ) -> *mut c_void;
    fn inferlab_session_free(session: *mut c_void);
    fn inferlab_session_prompt_tokens(session: *const c_void) -> u32;
    fn inferlab_session_next(
        session: *mut c_void,
        sampling: *const RawSamplingConfig,
        banned_token_ids: *const u32,
        banned_token_count: usize,
        allowed_token_ids: *const u32,
        allowed_token_count: usize,
        sampling_result: *mut RawSamplingResult,
        piece: *mut c_char,
        piece_capacity: usize,
        logits: *mut f32,
        logits_capacity: usize,
        duration_ns: *mut u64,
        error: *mut c_char,
        error_capacity: usize,
    ) -> i32;
    fn inferlab_sample_logits(
        logits: *const f32,
        logits_count: usize,
        history: *const u32,
        history_count: usize,
        sampling: *const RawSamplingConfig,
        banned_token_ids: *const u32,
        banned_token_count: usize,
        allowed_token_ids: *const u32,
        allowed_token_count: usize,
        random_state: *mut u64,
        sampling_result: *mut RawSamplingResult,
        error: *mut c_char,
        error_capacity: usize,
    ) -> i32;
    fn inferlab_session_query_tokens(session: *const c_void) -> u64;
    fn inferlab_session_kv_tokens(session: *const c_void) -> u64;
    fn inferlab_session_attention_score_elements(session: *const c_void) -> u64;
    fn inferlab_session_cache_bytes(session: *const c_void) -> u64;
    fn inferlab_session_peak_cache_bytes(session: *const c_void) -> u64;
    fn inferlab_session_cache_rebuilds(session: *const c_void) -> u64;
    fn inferlab_session_cache_pages(session: *const c_void) -> u64;
    fn inferlab_session_shared_cache_pages(session: *const c_void) -> u64;
    fn inferlab_session_reserved_cache_bytes(session: *const c_void) -> u64;
    fn inferlab_session_internal_fragmentation_bytes(session: *const c_void) -> u64;
    fn inferlab_session_prefix_cache_hit(session: *const c_void) -> u32;
    fn inferlab_session_prefix_tokens_reused(session: *const c_void) -> u64;
    fn inferlab_session_copy_on_write_copies(session: *const c_void) -> u64;
}

#[derive(Clone, Copy)]
#[repr(C)]
struct RawSamplingConfig {
    temperature: f32,
    top_k: u32,
    top_p: f32,
    repetition_penalty: f32,
}

impl From<&SamplingConfig> for RawSamplingConfig {
    fn from(config: &SamplingConfig) -> Self {
        Self {
            temperature: config.temperature,
            top_k: config.top_k,
            top_p: config.top_p,
            repetition_penalty: config.repetition_penalty,
        }
    }
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct RawSamplingResult {
    token_id: u32,
    candidate_count: u32,
    selected_probability: f32,
    entropy: f32,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct LogitSelection {
    pub token_id: u32,
    pub candidate_count: u32,
    pub selected_probability: f32,
    pub entropy: f32,
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct RawPagedCacheStats {
    page_tokens: u64,
    page_count: u64,
    prefix_capacity: u64,
    page_bytes: u64,
    capacity_bytes: u64,
    allocated_pages: u64,
    free_pages: u64,
    used_token_slots: u64,
    allocated_token_slots: u64,
    internal_fragmentation_bytes: u64,
    live_references: u64,
    shared_pages: u64,
    maximum_refcount: u64,
    logical_referenced_bytes: u64,
    physical_used_bytes: u64,
    bytes_saved_by_sharing: u64,
    prefix_entries: u64,
    prefix_hits: u64,
    prefix_misses: u64,
    prefix_tokens_reused: u64,
    copy_on_write_copies: u64,
    evictions: u64,
    allocation_failures: u64,
}

struct RawModel {
    pointer: NonNull<c_void>,
}

// The model tensors are immutable after loading. The shared paged pool protects
// every mutable allocator, prefix, and statistics operation with its mutex.
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
    vocabulary: Arc<Vec<String>>,
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

#[derive(Clone, Copy, Debug, Serialize)]
pub struct PagedCacheConfig {
    pub page_tokens: u32,
    pub page_count: u32,
    pub prefix_capacity: u32,
}

impl Default for PagedCacheConfig {
    fn default() -> Self {
        Self {
            page_tokens: 4,
            page_count: 64,
            prefix_capacity: 32,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct PagedCacheStats {
    pub page_tokens: u64,
    pub page_count: u64,
    pub prefix_capacity: u64,
    pub page_bytes: u64,
    pub capacity_bytes: u64,
    pub allocated_pages: u64,
    pub free_pages: u64,
    pub used_token_slots: u64,
    pub allocated_token_slots: u64,
    pub internal_fragmentation_bytes: u64,
    pub live_references: u64,
    pub shared_pages: u64,
    pub maximum_refcount: u64,
    pub logical_referenced_bytes: u64,
    pub physical_used_bytes: u64,
    pub bytes_saved_by_sharing: u64,
    pub prefix_entries: u64,
    pub prefix_hits: u64,
    pub prefix_misses: u64,
    pub prefix_tokens_reused: u64,
    pub copy_on_write_copies: u64,
    pub evictions: u64,
    pub allocation_failures: u64,
    pub allocated_page_percent: f64,
    pub page_fill_percent: f64,
    pub capacity_utilization_percent: f64,
    pub prefix_hit_rate_percent: f64,
}

impl From<RawPagedCacheStats> for PagedCacheStats {
    fn from(raw: RawPagedCacheStats) -> Self {
        Self {
            page_tokens: raw.page_tokens,
            page_count: raw.page_count,
            prefix_capacity: raw.prefix_capacity,
            page_bytes: raw.page_bytes,
            capacity_bytes: raw.capacity_bytes,
            allocated_pages: raw.allocated_pages,
            free_pages: raw.free_pages,
            used_token_slots: raw.used_token_slots,
            allocated_token_slots: raw.allocated_token_slots,
            internal_fragmentation_bytes: raw.internal_fragmentation_bytes,
            live_references: raw.live_references,
            shared_pages: raw.shared_pages,
            maximum_refcount: raw.maximum_refcount,
            logical_referenced_bytes: raw.logical_referenced_bytes,
            physical_used_bytes: raw.physical_used_bytes,
            bytes_saved_by_sharing: raw.bytes_saved_by_sharing,
            prefix_entries: raw.prefix_entries,
            prefix_hits: raw.prefix_hits,
            prefix_misses: raw.prefix_misses,
            prefix_tokens_reused: raw.prefix_tokens_reused,
            copy_on_write_copies: raw.copy_on_write_copies,
            evictions: raw.evictions,
            allocation_failures: raw.allocation_failures,
            allocated_page_percent: percent(raw.allocated_pages, raw.page_count),
            page_fill_percent: percent(raw.used_token_slots, raw.allocated_token_slots),
            capacity_utilization_percent: percent(raw.physical_used_bytes, raw.capacity_bytes),
            prefix_hit_rate_percent: percent(raw.prefix_hits, raw.prefix_hits + raw.prefix_misses),
        }
    }
}

fn percent(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64 * 100.0
    }
}

pub fn sample_logits(
    logits: &[f32],
    history: &[u32],
    config: &SamplingConfig,
    allowed_token_ids: Option<&[u32]>,
    random_state: &mut u64,
) -> Result<LogitSelection, String> {
    if logits.is_empty() {
        return Err("sampling requires at least one logit".to_owned());
    }
    config.validate(logits.len())?;
    if history.iter().any(|token| *token as usize >= logits.len()) {
        return Err("sampling history token is out of range".to_owned());
    }
    if allowed_token_ids.is_some_and(|tokens| tokens.is_empty()) {
        return Err("allowed token set must not be empty".to_owned());
    }
    if allowed_token_ids
        .into_iter()
        .flatten()
        .any(|token| *token as usize >= logits.len())
    {
        return Err("allowed token ID is outside the vocabulary".to_owned());
    }
    let raw = RawSamplingConfig::from(config);
    let mut selection = RawSamplingResult::default();
    let mut error = error_buffer();
    let allowed = allowed_token_ids.unwrap_or_default();
    // SAFETY: every slice pointer is valid for its advertised length, and both
    // mutable outputs are uniquely borrowed for the complete call.
    let status = unsafe {
        inferlab_sample_logits(
            logits.as_ptr(),
            logits.len(),
            history.as_ptr(),
            history.len(),
            &raw,
            config.banned_token_ids.as_ptr(),
            config.banned_token_ids.len(),
            allowed.as_ptr(),
            allowed.len(),
            random_state,
            &mut selection,
            error.as_mut_ptr(),
            error.len(),
        )
    };
    if status != 0 {
        return Err(read_error(&error));
    }
    Ok(LogitSelection {
        token_id: selection.token_id,
        candidate_count: selection.candidate_count,
        selected_probability: selection.selected_probability,
        entropy: selection.entropy,
    })
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
        let mut vocabulary = Vec::with_capacity(info.vocabulary as usize);
        for token_id in 0..info.vocabulary {
            let mut token = vec![0 as c_char; TEXT_CAPACITY];
            let mut token_error = error_buffer();
            // SAFETY: the loaded model and both output buffers remain valid.
            let status = unsafe {
                inferlab_model_token(
                    pointer.as_ptr(),
                    token_id,
                    token.as_mut_ptr(),
                    token.len(),
                    token_error.as_mut_ptr(),
                    token_error.len(),
                )
            };
            if status != 0 {
                return Err(read_error(&token_error));
            }
            vocabulary.push(read_text(&token));
        }
        Ok(Self {
            raw,
            path: Arc::new(path.to_owned()),
            info,
            vocabulary: Arc::new(vocabulary),
        })
    }

    pub fn info(&self) -> &ModelInfo {
        &self.info
    }

    pub fn configure_paged_cache(&mut self, config: PagedCacheConfig) -> Result<(), String> {
        if Arc::strong_count(&self.raw) != 1 {
            return Err("paged cache must be configured before the model is shared".to_owned());
        }
        let mut error = error_buffer();
        // SAFETY: this uniquely owned model and error buffer remain valid for the call.
        let status = unsafe {
            inferlab_model_configure_paged_cache(
                self.raw.pointer.as_ptr(),
                config.page_tokens,
                config.page_count,
                config.prefix_capacity,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if status < 0 {
            Err(read_error(&error))
        } else {
            Ok(())
        }
    }

    pub fn paged_cache_stats(&self) -> Result<PagedCacheStats, String> {
        let mut raw = RawPagedCacheStats::default();
        let mut error = error_buffer();
        // SAFETY: the model, stats output, and error buffer remain valid for the call.
        let status = unsafe {
            inferlab_model_paged_cache_stats(
                self.raw.pointer.as_ptr(),
                &mut raw,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if status < 0 {
            Err(read_error(&error))
        } else {
            Ok(raw.into())
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn token(&self, token_id: u32) -> Result<String, String> {
        self.vocabulary
            .get(token_id as usize)
            .cloned()
            .ok_or_else(|| "token ID is outside the model vocabulary".to_owned())
    }

    pub fn vocabulary(&self) -> &[String] {
        &self.vocabulary
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
        self.session_with_mode(prompt, max_tokens, DecoderMode::PagedKvCache)
    }

    pub fn session_with_mode(
        &self,
        prompt: &str,
        max_tokens: u32,
        mode: DecoderMode,
    ) -> Result<Session, String> {
        self.session_with_decoding(prompt, max_tokens, mode, DecodingConfig::default())
    }

    pub fn session_with_decoding(
        &self,
        prompt: &str,
        max_tokens: u32,
        mode: DecoderMode,
        decoding: DecodingConfig,
    ) -> Result<Session, String> {
        Session::new(self.clone(), prompt, max_tokens, mode, decoding)
    }

    pub fn generate(&self, prompt: &str, max_tokens: u32) -> Result<Generation, String> {
        self.generate_with_mode(prompt, max_tokens, DecoderMode::PagedKvCache)
    }

    pub fn generate_with_mode(
        &self,
        prompt: &str,
        max_tokens: u32,
        mode: DecoderMode,
    ) -> Result<Generation, String> {
        self.generate_with_decoding(prompt, max_tokens, mode, DecodingConfig::default())
    }

    pub fn generate_with_decoding(
        &self,
        prompt: &str,
        max_tokens: u32,
        mode: DecoderMode,
        decoding: DecodingConfig,
    ) -> Result<Generation, String> {
        let prompt_token_ids = self.tokenize(prompt)?;
        let mut session = self.session_with_decoding(prompt, max_tokens, mode, decoding)?;
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
    PagedKvCache,
}

impl DecoderMode {
    fn ffi_value(self) -> u32 {
        match self {
            Self::Recompute => 0,
            Self::KvCache => 1,
            Self::PagedKvCache => 2,
        }
    }
}

impl std::str::FromStr for DecoderMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "recompute" => Ok(Self::Recompute),
            "kv-cache" => Ok(Self::KvCache),
            "paged-kv-cache" => Ok(Self::PagedKvCache),
            _ => Err(format!(
                "unknown decoder mode '{value}'; expected recompute, kv-cache, or paged-kv-cache"
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
    decoding: DecodingConfig,
    constraint: Option<TokenDfa>,
    sampled_steps: u64,
    greedy_steps: u64,
    candidate_tokens_total: u64,
    masked_tokens_total: u64,
    entropy_total: f64,
}

// A session owns all mutable C++ state and is moved, never concurrently shared.
unsafe impl Send for Session {}

impl Session {
    fn new(
        model: Model,
        prompt: &str,
        max_tokens: u32,
        mode: DecoderMode,
        decoding: DecodingConfig,
    ) -> Result<Self, String> {
        decoding.sampling.validate(model.info.vocabulary as usize)?;
        let constraint =
            compile_constraint(&decoding.response_format, model.vocabulary(), max_tokens)?;
        if let Some(constraint) = &constraint {
            constraint.validate_banned_tokens(&decoding.sampling.banned_token_ids)?;
        }
        let encoded = CString::new(prompt).map_err(|_| "prompt contains a NUL byte".to_owned())?;
        let mut error = error_buffer();
        // SAFETY: model, encoded, and error are valid for the complete call.
        let pointer = unsafe {
            inferlab_session_create(
                model.raw.pointer.as_ptr(),
                encoded.as_ptr(),
                max_tokens,
                mode.ffi_value(),
                decoding.sampling.seed,
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
            decoding,
            constraint,
            sampled_steps: 0,
            greedy_steps: 0,
            candidate_tokens_total: 0,
            masked_tokens_total: 0,
            entropy_total: 0.0,
        })
    }

    pub fn prompt_tokens(&self) -> u32 {
        self.prompt_tokens
    }

    pub fn next_token(&mut self) -> Result<StepOutcome, String> {
        let raw_sampling = RawSamplingConfig::from(&self.decoding.sampling);
        let allowed_token_ids = self
            .constraint
            .as_ref()
            .map(TokenDfa::allowed_token_ids)
            .unwrap_or_default();
        let grammar_state = self.constraint.as_ref().map(TokenDfa::state);
        let mut sampling_result = RawSamplingResult::default();
        let mut piece = vec![0 as c_char; TEXT_CAPACITY];
        let mut logits = vec![0.0; self.model.info.vocabulary as usize];
        let mut duration_ns = 0;
        let mut error = error_buffer();
        // SAFETY: this session owns pointer and every output buffer is valid.
        let status = unsafe {
            inferlab_session_next(
                self.pointer.as_ptr(),
                &raw_sampling,
                self.decoding.sampling.banned_token_ids.as_ptr(),
                self.decoding.sampling.banned_token_ids.len(),
                allowed_token_ids.as_ptr(),
                allowed_token_ids.len(),
                &mut sampling_result,
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
            if self
                .constraint
                .as_ref()
                .is_some_and(|constraint| !constraint.is_accepting())
            {
                return Err("generation ended before the JSON grammar accepted".to_owned());
            }
            return Ok(StepOutcome::Length);
        }
        let eos = status == 2;
        if let Some(constraint) = &mut self.constraint {
            constraint.advance(sampling_result.token_id)?;
        }
        let token = self.model.token(sampling_result.token_id)?;
        let rendered_piece = if self.constraint.is_some() {
            if eos { String::new() } else { token.clone() }
        } else {
            read_text(&piece)
        };
        if self.decoding.sampling.temperature > 0.0 {
            self.sampled_steps += 1;
        } else {
            self.greedy_steps += 1;
        }
        self.candidate_tokens_total += u64::from(sampling_result.candidate_count);
        self.masked_tokens_total +=
            self.model
                .info
                .vocabulary
                .saturating_sub(sampling_result.candidate_count) as u64;
        self.entropy_total += f64::from(sampling_result.entropy);
        let step = StepTrace {
            index: self.step_index,
            token_id: sampling_result.token_id,
            token,
            piece: rendered_piece,
            eos,
            duration_us: duration_ns as f64 / 1_000.0,
            candidate_count: sampling_result.candidate_count,
            selected_probability: sampling_result.selected_probability,
            entropy: sampling_result.entropy,
            grammar_state,
            allowed_token_ids,
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
                cache_pages: inferlab_session_cache_pages(self.pointer.as_ptr()),
                shared_cache_pages: inferlab_session_shared_cache_pages(self.pointer.as_ptr()),
                reserved_cache_bytes: inferlab_session_reserved_cache_bytes(self.pointer.as_ptr()),
                internal_fragmentation_bytes: inferlab_session_internal_fragmentation_bytes(
                    self.pointer.as_ptr(),
                ),
                prefix_cache_hit: inferlab_session_prefix_cache_hit(self.pointer.as_ptr()) != 0,
                prefix_tokens_reused: inferlab_session_prefix_tokens_reused(self.pointer.as_ptr()),
                copy_on_write_copies: inferlab_session_copy_on_write_copies(self.pointer.as_ptr()),
                decoding: DecodingMetrics {
                    kind: if self.constraint.is_some() {
                        DecodingKind::JsonSchema
                    } else {
                        DecodingKind::Text
                    },
                    schema_name: self
                        .constraint
                        .as_ref()
                        .map(|constraint| constraint.schema_name().to_owned()),
                    temperature: self.decoding.sampling.temperature,
                    top_k: self.decoding.sampling.top_k,
                    top_p: self.decoding.sampling.top_p,
                    repetition_penalty: self.decoding.sampling.repetition_penalty,
                    seed: self.decoding.sampling.seed,
                    banned_token_count: self.decoding.sampling.banned_token_ids.len(),
                    sampled_steps: self.sampled_steps,
                    greedy_steps: self.greedy_steps,
                    grammar_constrained_steps: if self.constraint.is_some() {
                        self.sampled_steps + self.greedy_steps
                    } else {
                        0
                    },
                    candidate_tokens_total: self.candidate_tokens_total,
                    masked_tokens_total: self.masked_tokens_total,
                    mean_entropy: if self.sampled_steps + self.greedy_steps == 0 {
                        0.0
                    } else {
                        self.entropy_total / (self.sampled_steps + self.greedy_steps) as f64
                    },
                },
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

#[derive(Debug)]
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
    pub candidate_count: u32,
    pub selected_probability: f32,
    pub entropy: f32,
    pub grammar_state: Option<u32>,
    pub allowed_token_ids: Vec<u32>,
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

#[derive(Clone, Debug, Serialize)]
pub struct GenerationMetrics {
    pub mode: DecoderMode,
    pub query_tokens: u64,
    pub kv_tokens: u64,
    pub attention_score_elements: u64,
    pub cache_bytes: u64,
    pub peak_cache_bytes: u64,
    pub cache_rebuilds: u64,
    pub cache_pages: u64,
    pub shared_cache_pages: u64,
    pub reserved_cache_bytes: u64,
    pub internal_fragmentation_bytes: u64,
    pub prefix_cache_hit: bool,
    pub prefix_tokens_reused: u64,
    pub copy_on_write_copies: u64,
    pub decoding: DecodingMetrics,
}

#[derive(Clone, Debug, Serialize)]
pub struct DecodingMetrics {
    pub kind: DecodingKind,
    pub schema_name: Option<String>,
    pub temperature: f32,
    pub top_k: u32,
    pub top_p: f32,
    pub repetition_penalty: f32,
    pub seed: u64,
    pub banned_token_count: usize,
    pub sampled_steps: u64,
    pub greedy_steps: u64,
    pub grammar_constrained_steps: u64,
    pub candidate_tokens_total: u64,
    pub masked_tokens_total: u64,
    pub mean_entropy: f64,
}

#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub id: String,
    pub batch_tick_delay: Duration,
    pub decoder_mode: DecoderMode,
    pub max_batch_size: usize,
    pub scheduler_queue_capacity: usize,
    pub paged_cache: PagedCacheConfig,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            id: "cpu-worker-a".to_owned(),
            batch_tick_delay: Duration::ZERO,
            decoder_mode: DecoderMode::PagedKvCache,
            max_batch_size: 4,
            scheduler_queue_capacity: 64,
            paged_cache: PagedCacheConfig::default(),
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
    top_k: Option<u32>,
    top_p: Option<f32>,
    repetition_penalty: Option<f32>,
    seed: Option<u64>,
    #[serde(default)]
    banned_token_ids: Vec<u32>,
    #[serde(default)]
    response_format: ResponseFormat,
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

pub fn try_app(mut model: Model, config: WorkerConfig) -> Result<Router, String> {
    model.configure_paged_cache(config.paged_cache)?;
    let scheduler = ContinuousBatchScheduler::start(SchedulerConfig {
        max_batch_size: config.max_batch_size,
        queue_capacity: config.scheduler_queue_capacity,
        tick_delay: config.batch_tick_delay,
    })?;
    Ok(Router::new()
        .route("/health", get(health))
        .route("/internal/scheduler", get(scheduler_status))
        .route("/internal/cache", get(cache_status))
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
        "paged_cache_config": state.config.paged_cache,
        "paged_cache": state.model.paged_cache_stats().ok(),
        "scheduler": state.scheduler.snapshot()
    }))
}

async fn scheduler_status(State(state): State<WorkerState>) -> Json<Value> {
    Json(json!({"scheduler": state.scheduler.snapshot()}))
}

async fn cache_status(State(state): State<WorkerState>) -> Json<Value> {
    match state.model.paged_cache_stats() {
        Ok(cache) => Json(json!({"cache": cache})),
        Err(error) => Json(json!({"error": error})),
    }
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
    let prompt = last_message_text(&request.messages);
    let completion_id = format!("chatcmpl-{}-{request_number}", state.config.id);
    let created = unix_timestamp();
    let decoding = DecodingConfig {
        sampling: SamplingConfig {
            temperature: request.temperature.unwrap_or(0.0),
            top_k: request.top_k.unwrap_or(0),
            top_p: request.top_p.unwrap_or(1.0),
            repetition_penalty: request.repetition_penalty.unwrap_or(1.0),
            seed: request.seed.unwrap_or(0),
            banned_token_ids: request.banned_token_ids,
        },
        response_format: request.response_format,
    };
    let session = match state.model.session_with_decoding(
        &prompt,
        request.max_tokens,
        state.config.decoder_mode,
        decoding,
    ) {
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
    use super::{
        DecoderMode, DecodingConfig, Model, PagedCacheConfig, SamplingConfig, StepOutcome,
        inference_summary_response_format, sample_logits,
    };
    use std::{fs, path::PathBuf};

    fn model_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../models/tiny-inferlab-v1.bin")
    }

    fn model_v2_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../models/tiny-inferlab-v2.bin")
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

    #[test]
    fn paged_cache_preserves_contiguous_cache_logits() {
        let model = Model::load(model_path()).expect("valid model");
        let contiguous = model
            .generate_with_mode("teach me streaming", 8, DecoderMode::KvCache)
            .expect("contiguous generation");
        let paged = model
            .generate_with_mode("teach me streaming", 8, DecoderMode::PagedKvCache)
            .expect("paged generation");
        assert_eq!(paged.text, contiguous.text);
        for (paged_step, contiguous_step) in paged.steps.iter().zip(&contiguous.steps) {
            assert_eq!(paged_step.token_id, contiguous_step.token_id);
            assert_eq!(paged_step.logits, contiguous_step.logits);
        }
        assert_eq!(paged.metrics.query_tokens, 8);
        assert_eq!(paged.metrics.kv_tokens, 11);
        assert_eq!(paged.metrics.attention_score_elements, 240);
        assert_eq!(paged.metrics.cache_bytes, 1_408);
        assert_eq!(paged.metrics.cache_pages, 3);
        assert_eq!(paged.metrics.shared_cache_pages, 1);
        assert_eq!(paged.metrics.reserved_cache_bytes, 1_536);
        assert_eq!(paged.metrics.internal_fragmentation_bytes, 128);
        assert!(!paged.metrics.prefix_cache_hit);
        assert_eq!(paged.metrics.prefix_tokens_reused, 0);
        assert_eq!(paged.metrics.copy_on_write_copies, 0);
    }

    #[test]
    fn shared_partial_prefix_is_copied_before_append() {
        let mut model = Model::load(model_path()).expect("valid model");
        model
            .configure_paged_cache(PagedCacheConfig {
                page_tokens: 4,
                page_count: 8,
                prefix_capacity: 4,
            })
            .expect("configure cache");

        let mut cold = model
            .session_with_mode("hello systems", 1, DecoderMode::PagedKvCache)
            .expect("cold session");
        assert!(matches!(
            cold.next_token().expect("cold token"),
            StepOutcome::Token(_)
        ));
        assert!(!cold.metrics().prefix_cache_hit);
        assert_eq!(cold.metrics().kv_tokens, 3);
        drop(cold);

        let mut warm = model
            .session_with_mode("hello systems", 2, DecoderMode::PagedKvCache)
            .expect("warm session");
        assert!(warm.metrics().prefix_cache_hit);
        assert_eq!(warm.metrics().prefix_tokens_reused, 3);
        assert_eq!(warm.metrics().kv_tokens, 0);
        let shared = model.paged_cache_stats().expect("shared stats");
        assert_eq!(shared.allocated_pages, 1);
        assert_eq!(shared.shared_pages, 1);
        assert_eq!(shared.maximum_refcount, 2);
        assert_eq!(shared.bytes_saved_by_sharing, 384);

        assert!(matches!(
            warm.next_token().expect("first warm token"),
            StepOutcome::Token(_)
        ));
        assert!(matches!(
            warm.next_token().expect("second warm token"),
            StepOutcome::Token(_)
        ));
        let metrics = warm.metrics();
        assert_eq!(metrics.kv_tokens, 1);
        assert_eq!(metrics.copy_on_write_copies, 1);
        let copied = model.paged_cache_stats().expect("copied stats");
        assert_eq!(copied.copy_on_write_copies, 1);
        assert_eq!(copied.allocated_pages, 2);
        drop(warm);
        assert_eq!(
            model
                .paged_cache_stats()
                .expect("released stats")
                .allocated_pages,
            1
        );
    }

    #[test]
    fn paged_capacity_is_bounded_and_pages_return_after_drop() {
        let mut model = Model::load(model_path()).expect("valid model");
        model
            .configure_paged_cache(PagedCacheConfig {
                page_tokens: 4,
                page_count: 16,
                prefix_capacity: 0,
            })
            .expect("configure cache");
        let prompt = ["hello"; 7].join(" ");
        let mut sessions = Vec::new();
        for _ in 0..8 {
            let mut session = model
                .session_with_mode(&prompt, 1, DecoderMode::PagedKvCache)
                .expect("session");
            session.next_token().expect("capacity token");
            sessions.push(session);
        }
        let full = model.paged_cache_stats().expect("full stats");
        assert_eq!(full.allocated_pages, 16);
        assert_eq!(full.free_pages, 0);
        assert_eq!(full.used_token_slots, 64);

        let mut rejected = model
            .session_with_mode(&prompt, 1, DecoderMode::PagedKvCache)
            .expect("rejected session");
        assert!(
            rejected
                .next_token()
                .expect_err("capacity must be exhausted")
                .contains("capacity exhausted")
        );
        drop(rejected);
        drop(sessions);
        let released = model.paged_cache_stats().expect("released stats");
        assert_eq!(released.allocated_pages, 0);
        assert_eq!(released.free_pages, 16);
        assert_eq!(released.allocation_failures, 1);
    }

    #[test]
    fn least_recent_prefix_is_evicted_without_breaking_live_pages() {
        let mut model = Model::load(model_path()).expect("valid model");
        model
            .configure_paged_cache(PagedCacheConfig {
                page_tokens: 4,
                page_count: 3,
                prefix_capacity: 2,
            })
            .expect("configure cache");

        let mut live = model
            .session_with_mode("hello", 2, DecoderMode::PagedKvCache)
            .expect("live session");
        live.next_token().expect("first live token");
        for prompt in ["systems", "teach"] {
            let mut session = model
                .session_with_mode(prompt, 1, DecoderMode::PagedKvCache)
                .expect("session");
            session.next_token().expect("token");
        }
        let evicted = model.paged_cache_stats().expect("eviction stats");
        assert_eq!(evicted.prefix_entries, 2);
        assert_eq!(evicted.allocated_pages, 3);
        assert_eq!(evicted.evictions, 1);

        assert!(matches!(
            live.next_token().expect("live page remains valid"),
            StepOutcome::Token(_)
        ));
        assert_eq!(live.metrics().copy_on_write_copies, 0);

        let mut first_again = model
            .session_with_mode("hello", 1, DecoderMode::PagedKvCache)
            .expect("reloaded session");
        first_again.next_token().expect("reloaded token");
        assert!(!first_again.metrics().prefix_cache_hit);
        assert_eq!(
            model
                .paged_cache_stats()
                .expect("second eviction")
                .evictions,
            2
        );
    }

    #[test]
    fn logit_processors_have_golden_selection_behavior() {
        let logits = [1.0, 4.0, 3.0, 2.0];

        let mut state = 7;
        let banned = sample_logits(
            &logits,
            &[],
            &SamplingConfig {
                banned_token_ids: vec![1],
                ..SamplingConfig::default()
            },
            None,
            &mut state,
        )
        .expect("ban selection");
        assert_eq!(banned.token_id, 2);
        assert_eq!(banned.candidate_count, 3);

        let repeated = sample_logits(
            &logits,
            &[1],
            &SamplingConfig {
                repetition_penalty: 2.0,
                ..SamplingConfig::default()
            },
            None,
            &mut state,
        )
        .expect("repetition selection");
        assert_eq!(repeated.token_id, 2);

        let allowed = sample_logits(
            &logits,
            &[],
            &SamplingConfig::default(),
            Some(&[0, 3]),
            &mut state,
        )
        .expect("allowed selection");
        assert_eq!(allowed.token_id, 3);
        assert_eq!(allowed.candidate_count, 2);

        let nucleus = sample_logits(
            &logits,
            &[],
            &SamplingConfig {
                temperature: 1.0,
                top_p: 0.6,
                ..SamplingConfig::default()
            },
            None,
            &mut state,
        )
        .expect("nucleus selection");
        assert_eq!(nucleus.token_id, 1);
        assert_eq!(nucleus.candidate_count, 1);

        let top_k = sample_logits(
            &logits,
            &[],
            &SamplingConfig {
                temperature: 1.0,
                top_k: 2,
                ..SamplingConfig::default()
            },
            None,
            &mut state,
        )
        .expect("top-k selection");
        assert_eq!(top_k.candidate_count, 2);
    }

    #[test]
    fn sampling_replays_for_the_same_seed_and_rejects_an_empty_support() {
        let config = SamplingConfig {
            temperature: 1.0,
            ..SamplingConfig::default()
        };
        let sequence = |seed| {
            let mut state = seed;
            (0..64)
                .map(|_| {
                    sample_logits(&[0.0, 1.0, 2.0], &[], &config, None, &mut state)
                        .expect("sample")
                        .token_id
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(sequence(91), sequence(91));
        assert_ne!(sequence(91), sequence(92));

        let mut state = 0;
        let error = sample_logits(
            &[1.0, 2.0],
            &[],
            &SamplingConfig {
                banned_token_ids: vec![1],
                ..SamplingConfig::default()
            },
            Some(&[1]),
            &mut state,
        )
        .expect_err("ban and grammar must not leave an empty support");
        assert!(error.contains("all tokens were masked"));
    }

    #[test]
    fn json_schema_dfa_emits_parser_valid_output_and_replays() {
        let model = Model::load(model_v2_path()).expect("valid v2 model");
        assert_eq!(model.info().vocabulary, 22);
        let decoding = DecodingConfig {
            sampling: SamplingConfig {
                temperature: 1.0,
                seed: 505,
                ..SamplingConfig::default()
            },
            response_format: inference_summary_response_format(),
        };
        let first = model
            .generate_with_decoding(
                "teach me streaming",
                6,
                DecoderMode::PagedKvCache,
                decoding.clone(),
            )
            .expect("first structured generation");
        let replay = model
            .generate_with_decoding("teach me streaming", 6, DecoderMode::PagedKvCache, decoding)
            .expect("replayed structured generation");
        assert_eq!(first.text, replay.text);
        let parsed: serde_json::Value = serde_json::from_str(&first.text).expect("valid JSON");
        assert!(matches!(
            parsed["answer"].as_str(),
            Some("InferLab" | "systems" | "tokens")
        ));
        assert!(matches!(
            parsed["confidence"].as_str(),
            Some("high" | "medium" | "low")
        ));
        assert_eq!(first.finish_reason, "stop");
        assert_eq!(first.steps.len(), 6);
        assert_eq!(first.metrics.decoding.grammar_constrained_steps, 6);
        assert_eq!(
            first.metrics.decoding.schema_name.as_deref(),
            Some("inference_summary")
        );
    }

    #[test]
    fn json_schema_rejects_an_incompatible_vocabulary_or_short_limit() {
        let v1 = Model::load(model_path()).expect("valid v1 model");
        let incompatible = v1.session_with_decoding(
            "hello",
            6,
            DecoderMode::PagedKvCache,
            DecodingConfig {
                response_format: inference_summary_response_format(),
                ..DecodingConfig::default()
            },
        );
        let incompatible = match incompatible {
            Err(error) => error,
            Ok(_) => panic!("v1 has no JSON fragment tokens"),
        };
        assert!(incompatible.contains("not one complete model token"));

        let v2 = Model::load(model_v2_path()).expect("valid v2 model");
        let short = v2.session_with_decoding(
            "hello",
            5,
            DecoderMode::PagedKvCache,
            DecodingConfig {
                response_format: inference_summary_response_format(),
                ..DecodingConfig::default()
            },
        );
        let short = match short {
            Err(error) => error,
            Ok(_) => panic!("five tokens cannot complete the grammar"),
        };
        assert!(short.contains("at least 6 max_tokens"));

        let impossible = v2.session_with_decoding(
            "hello",
            6,
            DecoderMode::PagedKvCache,
            DecodingConfig {
                sampling: SamplingConfig {
                    banned_token_ids: vec![4, 9, 15],
                    ..SamplingConfig::default()
                },
                response_format: inference_summary_response_format(),
            },
        );
        let impossible = match impossible {
            Err(error) => error,
            Ok(_) => panic!("all answer enum tokens are banned"),
        };
        assert!(impossible.contains("grammar state 1"));
    }
}
