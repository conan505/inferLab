use std::{
    fmt,
    str::FromStr,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use axum::body::Bytes;
use prometheus_client::metrics::counter::Counter;
use serde::Serialize;
use serde_json::Value;

use crate::public_authentication::CredentialSlot;

pub const MAX_PUBLIC_REQUEST_BYTES: usize = 64 * 1024;
pub const DEFAULT_MAX_MESSAGES: usize = 32;
pub const MAX_MAX_MESSAGES: usize = 256;
pub const DEFAULT_MAX_PROMPT_BYTES: usize = 16 * 1024;
pub const MAX_MAX_PROMPT_BYTES: usize = MAX_PUBLIC_REQUEST_BYTES;
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 256;
pub const MAX_MAX_OUTPUT_TOKENS: u64 = 4_096;
pub const DEFAULT_RATE_REQUESTS_PER_MINUTE: u64 = 60;
pub const MAX_RATE_REQUESTS_PER_MINUTE: u64 = 60_000;
pub const DEFAULT_RATE_BURST: u64 = 4;
pub const MAX_RATE_BURST: u64 = 1_000;

const NANOS_PER_SECOND: u128 = 1_000_000_000;
const NANOS_PER_MINUTE: u128 = 60 * NANOS_PER_SECOND;
const REJECTION_REASON_COUNT: usize = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PublicEdgeMode {
    Local,
    Hosted,
}

impl PublicEdgeMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Hosted => "hosted",
        }
    }
}

impl fmt::Display for PublicEdgeMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PublicEdgeMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "local" => Ok(Self::Local),
            "hosted" => Ok(Self::Hosted),
            _ => Err("INFERLAB_PUBLIC_EDGE_MODE must be local or hosted".to_owned()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublicEdgeConfig {
    pub mode: PublicEdgeMode,
    pub max_messages: usize,
    pub max_prompt_bytes: usize,
    pub max_output_tokens: u64,
    pub rate_requests_per_minute: u64,
    pub rate_burst: u64,
}

impl PublicEdgeConfig {
    pub fn hosted(
        max_messages: usize,
        max_prompt_bytes: usize,
        max_output_tokens: u64,
        rate_requests_per_minute: u64,
        rate_burst: u64,
    ) -> Result<Self, String> {
        let config = Self {
            mode: PublicEdgeMode::Hosted,
            max_messages,
            max_prompt_bytes,
            max_output_tokens,
            rate_requests_per_minute,
            rate_burst,
        };
        config.validate()?;
        Ok(config)
    }

    pub const fn local() -> Self {
        Self {
            mode: PublicEdgeMode::Local,
            max_messages: DEFAULT_MAX_MESSAGES,
            max_prompt_bytes: DEFAULT_MAX_PROMPT_BYTES,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            rate_requests_per_minute: DEFAULT_RATE_REQUESTS_PER_MINUTE,
            rate_burst: DEFAULT_RATE_BURST,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_positive_bounded(
            "INFERLAB_PUBLIC_MAX_MESSAGES",
            self.max_messages as u64,
            MAX_MAX_MESSAGES as u64,
        )?;
        validate_positive_bounded(
            "INFERLAB_PUBLIC_MAX_PROMPT_BYTES",
            self.max_prompt_bytes as u64,
            MAX_MAX_PROMPT_BYTES as u64,
        )?;
        validate_positive_bounded(
            "INFERLAB_PUBLIC_MAX_OUTPUT_TOKENS",
            self.max_output_tokens,
            MAX_MAX_OUTPUT_TOKENS,
        )?;
        validate_positive_bounded(
            "INFERLAB_PUBLIC_RATE_REQUESTS_PER_MINUTE",
            self.rate_requests_per_minute,
            MAX_RATE_REQUESTS_PER_MINUTE,
        )?;
        validate_positive_bounded(
            "INFERLAB_PUBLIC_RATE_BURST",
            self.rate_burst,
            MAX_RATE_BURST,
        )?;
        Ok(())
    }
}

impl Default for PublicEdgeConfig {
    fn default() -> Self {
        Self::local()
    }
}

fn validate_positive_bounded(name: &str, value: u64, maximum: u64) -> Result<(), String> {
    if value == 0 {
        return Err(format!("{name} must be positive"));
    }
    if value > maximum {
        return Err(format!("{name} must not exceed {maximum}"));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PublicEdgeRejectionReason {
    Authentication,
    BodyTooLarge,
    MalformedJson,
    InvalidMessages,
    TooManyMessages,
    PromptTooLarge,
    InvalidMaxTokens,
    MaxOutputTokensExceeded,
    RateLimited,
    AdmissionFull,
}

impl PublicEdgeRejectionReason {
    #[cfg(test)]
    const ALL: [Self; REJECTION_REASON_COUNT] = [
        Self::Authentication,
        Self::BodyTooLarge,
        Self::MalformedJson,
        Self::InvalidMessages,
        Self::TooManyMessages,
        Self::PromptTooLarge,
        Self::InvalidMaxTokens,
        Self::MaxOutputTokensExceeded,
        Self::RateLimited,
        Self::AdmissionFull,
    ];

    const fn index(self) -> usize {
        match self {
            Self::Authentication => 0,
            Self::BodyTooLarge => 1,
            Self::MalformedJson => 2,
            Self::InvalidMessages => 3,
            Self::TooManyMessages => 4,
            Self::PromptTooLarge => 5,
            Self::InvalidMaxTokens => 6,
            Self::MaxOutputTokensExceeded => 7,
            Self::RateLimited => 8,
            Self::AdmissionFull => 9,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::BodyTooLarge => "body_too_large",
            Self::MalformedJson => "malformed_json",
            Self::InvalidMessages => "invalid_messages",
            Self::TooManyMessages => "too_many_messages",
            Self::PromptTooLarge => "prompt_too_large",
            Self::InvalidMaxTokens => "invalid_max_tokens",
            Self::MaxOutputTokensExceeded => "max_output_tokens_exceeded",
            Self::RateLimited => "rate_limited",
            Self::AdmissionFull => "admission_full",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct PublicEdgeStatus {
    pub mode: PublicEdgeMode,
    pub enforced: bool,
    pub max_request_bytes: Option<usize>,
    pub max_messages: Option<usize>,
    pub max_prompt_bytes: Option<usize>,
    pub max_output_tokens: Option<u64>,
    pub rate_requests_per_minute: Option<u64>,
    pub rate_burst: Option<u64>,
    pub credential_count: Option<usize>,
    pub rejections: PublicEdgeRejectionStatus,
}

#[derive(Debug, Serialize)]
pub struct PublicEdgeRejectionStatus {
    pub authentication: u64,
    pub body_too_large: u64,
    pub malformed_json: u64,
    pub invalid_messages: u64,
    pub too_many_messages: u64,
    pub prompt_too_large: u64,
    pub invalid_max_tokens: u64,
    pub max_output_tokens_exceeded: u64,
    pub rate_limited: u64,
    pub admission_full: u64,
}

pub(crate) struct PublicEdgeController {
    config: PublicEdgeConfig,
    credential_slots: usize,
    started: Instant,
    buckets: Vec<Mutex<TokenBucket>>,
    rejections: [AtomicU64; REJECTION_REASON_COUNT],
    rejection_metric: Option<Counter>,
}

impl PublicEdgeController {
    pub(crate) fn new(
        config: PublicEdgeConfig,
        credential_slots: usize,
        rejection_metric: Option<Counter>,
    ) -> Result<Self, String> {
        config.validate()?;
        if config.mode == PublicEdgeMode::Hosted && credential_slots == 0 {
            return Err("hosted public edge requires at least one credential slot".to_owned());
        }
        if credential_slots > 16 {
            return Err("public edge supports at most 16 credential slots".to_owned());
        }
        Ok(Self {
            config,
            credential_slots,
            started: Instant::now(),
            buckets: (0..credential_slots)
                .map(|_| Mutex::new(TokenBucket::full(config.rate_burst)))
                .collect(),
            rejections: std::array::from_fn(|_| AtomicU64::new(0)),
            rejection_metric,
        })
    }

    pub(crate) fn validate_input(&self, body: &Bytes) -> Result<(), PublicEdgeRejectionReason> {
        let value: Value =
            serde_json::from_slice(body).map_err(|_| PublicEdgeRejectionReason::MalformedJson)?;
        let Some(request) = value.as_object() else {
            return Err(PublicEdgeRejectionReason::InvalidMessages);
        };
        let Some(messages) = request.get("messages").and_then(Value::as_array) else {
            return Err(PublicEdgeRejectionReason::InvalidMessages);
        };
        if messages.is_empty() {
            return Err(PublicEdgeRejectionReason::InvalidMessages);
        }
        if messages.len() > self.config.max_messages {
            return Err(PublicEdgeRejectionReason::TooManyMessages);
        }
        let mut prompt_bytes = 0_usize;
        for message in messages {
            let Some(message) = message.as_object() else {
                return Err(PublicEdgeRejectionReason::InvalidMessages);
            };
            if !message.get("role").is_some_and(Value::is_string) {
                return Err(PublicEdgeRejectionReason::InvalidMessages);
            }
            let Some(content) = message.get("content").and_then(Value::as_str) else {
                return Err(PublicEdgeRejectionReason::InvalidMessages);
            };
            prompt_bytes = prompt_bytes
                .checked_add(content.len())
                .ok_or(PublicEdgeRejectionReason::PromptTooLarge)?;
        }
        if prompt_bytes > self.config.max_prompt_bytes {
            return Err(PublicEdgeRejectionReason::PromptTooLarge);
        }
        let Some(max_tokens) = request.get("max_tokens").and_then(Value::as_u64) else {
            return Err(PublicEdgeRejectionReason::InvalidMaxTokens);
        };
        if max_tokens == 0 {
            return Err(PublicEdgeRejectionReason::InvalidMaxTokens);
        }
        if max_tokens > self.config.max_output_tokens {
            return Err(PublicEdgeRejectionReason::MaxOutputTokensExceeded);
        }
        Ok(())
    }

    pub(crate) const fn is_enforced(&self) -> bool {
        matches!(self.config.mode, PublicEdgeMode::Hosted)
    }

    pub(crate) fn try_consume(&self, slot: CredentialSlot) -> Result<(), u64> {
        self.try_consume_at(slot, self.started.elapsed())
    }

    fn try_consume_at(&self, slot: CredentialSlot, elapsed: Duration) -> Result<(), u64> {
        let Some(bucket) = self.buckets.get(slot.index()) else {
            return Err(60);
        };
        bucket
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .try_consume(
                elapsed.as_nanos(),
                self.config.rate_requests_per_minute,
                self.config.rate_burst,
            )
    }

    pub(crate) fn record_rejection(&self, reason: PublicEdgeRejectionReason) {
        self.rejections[reason.index()].fetch_add(1, Ordering::Relaxed);
        if let Some(metric) = self.rejection_metric.as_ref() {
            metric.inc();
        }
    }

    pub(crate) fn status(&self) -> PublicEdgeStatus {
        let count = |reason: PublicEdgeRejectionReason| {
            self.rejections[reason.index()].load(Ordering::Relaxed)
        };
        let enforced = self.config.mode == PublicEdgeMode::Hosted;
        PublicEdgeStatus {
            mode: self.config.mode,
            enforced,
            max_request_bytes: enforced.then_some(MAX_PUBLIC_REQUEST_BYTES),
            max_messages: enforced.then_some(self.config.max_messages),
            max_prompt_bytes: enforced.then_some(self.config.max_prompt_bytes),
            max_output_tokens: enforced.then_some(self.config.max_output_tokens),
            rate_requests_per_minute: enforced.then_some(self.config.rate_requests_per_minute),
            rate_burst: enforced.then_some(self.config.rate_burst),
            credential_count: enforced.then_some(self.credential_slots),
            rejections: PublicEdgeRejectionStatus {
                authentication: count(PublicEdgeRejectionReason::Authentication),
                body_too_large: count(PublicEdgeRejectionReason::BodyTooLarge),
                malformed_json: count(PublicEdgeRejectionReason::MalformedJson),
                invalid_messages: count(PublicEdgeRejectionReason::InvalidMessages),
                too_many_messages: count(PublicEdgeRejectionReason::TooManyMessages),
                prompt_too_large: count(PublicEdgeRejectionReason::PromptTooLarge),
                invalid_max_tokens: count(PublicEdgeRejectionReason::InvalidMaxTokens),
                max_output_tokens_exceeded: count(
                    PublicEdgeRejectionReason::MaxOutputTokensExceeded,
                ),
                rate_limited: count(PublicEdgeRejectionReason::RateLimited),
                admission_full: count(PublicEdgeRejectionReason::AdmissionFull),
            },
        }
    }
}

#[derive(Debug)]
struct TokenBucket {
    available_units: u128,
    last_elapsed_nanos: u128,
}

impl TokenBucket {
    fn full(burst: u64) -> Self {
        Self {
            available_units: u128::from(burst) * NANOS_PER_MINUTE,
            last_elapsed_nanos: 0,
        }
    }

    fn try_consume(
        &mut self,
        elapsed_nanos: u128,
        rate_requests_per_minute: u64,
        burst: u64,
    ) -> Result<(), u64> {
        let elapsed_nanos = elapsed_nanos.max(self.last_elapsed_nanos);
        let delta = elapsed_nanos - self.last_elapsed_nanos;
        self.last_elapsed_nanos = elapsed_nanos;
        let capacity = u128::from(burst) * NANOS_PER_MINUTE;
        let refill = delta.saturating_mul(u128::from(rate_requests_per_minute));
        self.available_units = self.available_units.saturating_add(refill).min(capacity);
        if self.available_units >= NANOS_PER_MINUTE {
            self.available_units -= NANOS_PER_MINUTE;
            return Ok(());
        }
        let missing = NANOS_PER_MINUTE - self.available_units;
        let rate = u128::from(rate_requests_per_minute);
        let wait_nanos = missing.div_ceil(rate);
        let retry_seconds = wait_nanos.div_ceil(NANOS_PER_SECOND).clamp(1, 60);
        Err(retry_seconds as u64)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Bytes;
    use prometheus_client::metrics::counter::Counter;

    use super::{
        PublicEdgeConfig, PublicEdgeController, PublicEdgeMode, PublicEdgeRejectionReason,
        PublicEdgeRejectionStatus,
    };
    use crate::public_authentication::CredentialSlot;

    fn controller(rate: u64, burst: u64, slots: usize) -> PublicEdgeController {
        PublicEdgeController::new(
            PublicEdgeConfig::hosted(2, 128, 8, rate, burst).expect("config"),
            slots,
            Some(Counter::default()),
        )
        .expect("controller")
    }

    #[test]
    fn config_bounds_and_mode_are_explicit() {
        assert_eq!(PublicEdgeConfig::local().mode, PublicEdgeMode::Local);
        assert!(PublicEdgeConfig::hosted(0, 1, 1, 1, 1).is_err());
        assert!(PublicEdgeConfig::hosted(1, 1, 1, 0, 1).is_err());
        assert!(PublicEdgeConfig::hosted(1, 1, 1, 1, 0).is_err());

        let local = PublicEdgeController::new(PublicEdgeConfig::local(), 0, None)
            .expect("local controller")
            .status();
        assert!(!local.enforced);
        assert_eq!(local.max_request_bytes, None);
        assert_eq!(local.credential_count, None);
    }

    #[test]
    fn exact_burst_refill_and_slot_isolation_use_a_deterministic_clock() {
        let controller = controller(60, 2, 2);
        let first = CredentialSlot::new(0).expect("slot");
        let second = CredentialSlot::new(1).expect("slot");

        assert_eq!(controller.try_consume_at(first, Duration::ZERO), Ok(()));
        assert_eq!(controller.try_consume_at(first, Duration::ZERO), Ok(()));
        assert_eq!(controller.try_consume_at(first, Duration::ZERO), Err(1));
        assert_eq!(controller.try_consume_at(second, Duration::ZERO), Ok(()));
        assert_eq!(
            controller.try_consume_at(first, Duration::from_millis(999)),
            Err(1)
        );
        assert_eq!(
            controller.try_consume_at(first, Duration::from_secs(1)),
            Ok(())
        );
        assert_eq!(
            controller.try_consume_at(first, Duration::from_secs(2)),
            Ok(())
        );
    }

    #[test]
    fn input_policy_distinguishes_json_messages_prompt_and_output_limits() {
        let controller = controller(60, 2, 1);
        assert_eq!(
            controller.validate_input(&Bytes::from_static(b"{")),
            Err(PublicEdgeRejectionReason::MalformedJson)
        );
        assert_eq!(
            controller.validate_input(&Bytes::from_static(br#"{"messages":[]}"#)),
            Err(PublicEdgeRejectionReason::InvalidMessages)
        );
        assert_eq!(
            controller.validate_input(&Bytes::from_static(br#"{"messages":[{}, {}, {}]}"#)),
            Err(PublicEdgeRejectionReason::TooManyMessages)
        );
        assert_eq!(
            controller.validate_input(&Bytes::from_static(
                br#"{"messages":[{"role":"user","content":{}}]}"#
            )),
            Err(PublicEdgeRejectionReason::InvalidMessages)
        );
        assert_eq!(
            controller.validate_input(&Bytes::from_static(
                br#"{"messages":[{"role":"user","content":"ok"}]}"#
            )),
            Err(PublicEdgeRejectionReason::InvalidMaxTokens)
        );
        assert_eq!(
            controller.validate_input(&Bytes::from_static(
                br#"{"messages":[{"role":"user","content":"ok"}],"max_tokens":0}"#
            )),
            Err(PublicEdgeRejectionReason::InvalidMaxTokens)
        );
        assert_eq!(
            controller.validate_input(&Bytes::from_static(
                br#"{"messages":[{"role":"user","content":"ok"}],"max_tokens":9}"#
            )),
            Err(PublicEdgeRejectionReason::MaxOutputTokensExceeded)
        );
    }

    #[test]
    fn prompt_budget_counts_only_utf8_content_bytes() {
        let controller = PublicEdgeController::new(
            PublicEdgeConfig::hosted(2, 4, 8, 60, 2).expect("config"),
            1,
            Some(Counter::default()),
        )
        .expect("controller");
        assert!(
            controller
                .validate_input(&Bytes::from_static(
                    br#"{"model":"a-long-model-name","messages":[{"role":"an-unusually-long-role","content":"1234"}],"max_tokens":1}"#
                ))
                .is_ok()
        );
        assert_eq!(
            controller.validate_input(&Bytes::from_static(
                br#"{"messages":[{"role":"user","content":"12345"}],"max_tokens":1}"#
            )),
            Err(PublicEdgeRejectionReason::PromptTooLarge)
        );
    }

    #[test]
    fn status_has_one_counter_for_every_finite_reason() {
        let controller = controller(60, 2, 1);
        for reason in PublicEdgeRejectionReason::ALL {
            controller.record_rejection(reason);
        }
        let PublicEdgeRejectionStatus {
            authentication,
            body_too_large,
            malformed_json,
            invalid_messages,
            too_many_messages,
            prompt_too_large,
            invalid_max_tokens,
            max_output_tokens_exceeded,
            rate_limited,
            admission_full,
        } = controller.status().rejections;
        assert_eq!(
            [
                authentication,
                body_too_large,
                malformed_json,
                invalid_messages,
                too_many_messages,
                prompt_too_large,
                invalid_max_tokens,
                max_output_tokens_exceeded,
                rate_limited,
                admission_full,
            ],
            [1; 10]
        );
    }
}
