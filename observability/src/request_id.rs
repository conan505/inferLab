use std::{
    error::Error,
    fmt,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::Request,
    http::{HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};

/// The only request-correlation header understood by InferLab services.
pub const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-inferlab-request-id");

static REQUEST_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A validated, bounded request-correlation identifier.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RequestId(String);

impl RequestId {
    /// Parse a caller-provided request ID.
    ///
    /// IDs contain 1--64 ASCII characters from `[A-Za-z0-9._:-]`.
    pub fn parse(value: &str) -> Result<Self, RequestIdError> {
        if value.is_empty() {
            return Err(RequestIdError::Empty);
        }
        if value.len() > 64 {
            return Err(RequestIdError::TooLong);
        }
        if !value.bytes().all(is_allowed_request_id_byte) {
            return Err(RequestIdError::InvalidCharacter);
        }
        Ok(Self(value.to_owned()))
    }

    /// Generate a process-local unique correlation identifier.
    #[must_use]
    pub fn generate() -> Self {
        let sequence = REQUEST_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let value = format!("il-{:x}-{nanos:x}-{sequence:x}", std::process::id());
        debug_assert!(Self::parse(&value).is_ok());
        Self(value)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn header_value(&self) -> HeaderValue {
        HeaderValue::from_str(self.as_str()).expect("validated request IDs are HTTP header values")
    }

    fn from_header(value: Option<&HeaderValue>) -> Self {
        value
            .and_then(|value| value.to_str().ok())
            .and_then(|value| Self::parse(value).ok())
            .unwrap_or_else(Self::generate)
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Request-ID validation failures intentionally contain no rejected value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestIdError {
    Empty,
    TooLong,
    InvalidCharacter,
}

impl fmt::Display for RequestIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "request ID is empty",
            Self::TooLong => "request ID exceeds 64 bytes",
            Self::InvalidCharacter => "request ID contains a forbidden character",
        })
    }
}

impl Error for RequestIdError {}

/// Assign a canonical request ID before calling the inner service and echo it
/// on every response returned by that service.
pub async fn request_id_middleware(mut request: Request, next: Next) -> Response {
    let request_id = RequestId::from_header(request.headers().get(&REQUEST_ID_HEADER));
    request
        .headers_mut()
        .insert(REQUEST_ID_HEADER, request_id.header_value());
    request.extensions_mut().insert(request_id.clone());

    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(REQUEST_ID_HEADER, request_id.header_value());
    response
}

const fn is_allowed_request_id_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_the_locked_alphabet_and_length() {
        let valid = "AZaz09._:-";
        assert_eq!(RequestId::parse(valid).unwrap().as_str(), valid);
        assert_eq!(RequestId::parse(""), Err(RequestIdError::Empty));
        assert_eq!(
            RequestId::parse(&"a".repeat(65)),
            Err(RequestIdError::TooLong)
        );
        for invalid in ["has space", "line\nbreak", "slash/value", "snowman-☃"] {
            assert_eq!(
                RequestId::parse(invalid),
                Err(RequestIdError::InvalidCharacter),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn generated_ids_are_valid_bounded_and_distinct() {
        let first = RequestId::generate();
        let second = RequestId::generate();
        assert_ne!(first, second);
        assert!(first.as_str().len() <= 64);
        assert_eq!(RequestId::parse(first.as_str()).unwrap(), first);
    }
}
