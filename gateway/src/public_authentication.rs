use std::sync::Arc;

use axum::http::{HeaderMap, header::AUTHORIZATION};
use serde::Serialize;

const MAXIMUM_API_KEY_COUNT: usize = 16;
const MINIMUM_API_KEY_BYTES: usize = 16;
const MAXIMUM_API_KEY_BYTES: usize = 256;
const MAXIMUM_CONFIGURATION_BYTES: usize = 4_096;

#[derive(Clone)]
pub struct PublicApiAuthenticator {
    // Keep key material private and deliberately omit `Debug` so routine diagnostics cannot
    // accidentally print configured credentials.
    keys: Arc<[Box<[u8]>]>,
}

#[derive(Clone)]
pub struct OperatorApiAuthenticator {
    inner: PublicApiAuthenticator,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct CredentialSlot(u8);

impl CredentialSlot {
    pub(crate) fn new(index: usize) -> Option<Self> {
        u8::try_from(index).ok().map(Self)
    }

    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct AuthenticatedPublicCredential {
    slot: Option<CredentialSlot>,
}

impl AuthenticatedPublicCredential {
    pub(crate) const fn slot(self) -> Option<CredentialSlot> {
        self.slot
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct PublicApiAuthenticationStatus {
    pub enabled: bool,
    pub key_count: usize,
}

impl PublicApiAuthenticator {
    pub fn disabled() -> Self {
        Self {
            keys: Arc::from([]),
        }
    }

    pub fn from_configuration(configuration: Option<&str>) -> Result<Self, String> {
        let Some(configuration) = configuration else {
            return Ok(Self::disabled());
        };
        if configuration.len() > MAXIMUM_CONFIGURATION_BYTES {
            return Err(format!(
                "INFERLAB_PUBLIC_API_KEYS exceeds the maximum configuration size of {MAXIMUM_CONFIGURATION_BYTES} bytes"
            ));
        }
        if configuration.is_empty() {
            return Err(
                "INFERLAB_PUBLIC_API_KEYS must contain at least one API key when configured"
                    .to_owned(),
            );
        }

        let mut keys = Vec::<Box<[u8]>>::new();
        for (index, configured_key) in configuration.split(',').enumerate() {
            if index >= MAXIMUM_API_KEY_COUNT {
                return Err(format!(
                    "INFERLAB_PUBLIC_API_KEYS supports at most {MAXIMUM_API_KEY_COUNT} keys"
                ));
            }
            let configured_key = configured_key.trim();
            let position = index + 1;
            if configured_key.is_empty() {
                return Err(format!(
                    "INFERLAB_PUBLIC_API_KEYS entry {position} must not be empty"
                ));
            }
            if configured_key.len() < MINIMUM_API_KEY_BYTES {
                return Err(format!(
                    "INFERLAB_PUBLIC_API_KEYS entry {position} must be at least {MINIMUM_API_KEY_BYTES} bytes"
                ));
            }
            if configured_key.len() > MAXIMUM_API_KEY_BYTES {
                return Err(format!(
                    "INFERLAB_PUBLIC_API_KEYS entry {position} exceeds the maximum size of {MAXIMUM_API_KEY_BYTES} bytes"
                ));
            }
            if !configured_key.bytes().all(is_visible_non_whitespace_ascii) {
                return Err(format!(
                    "INFERLAB_PUBLIC_API_KEYS entry {position} must contain only visible non-whitespace ASCII characters"
                ));
            }
            if keys
                .iter()
                .any(|existing| existing.as_ref() == configured_key.as_bytes())
            {
                return Err(format!(
                    "INFERLAB_PUBLIC_API_KEYS entry {position} duplicates an earlier key"
                ));
            }
            keys.push(configured_key.as_bytes().into());
        }

        Ok(Self {
            keys: Arc::from(keys),
        })
    }

    pub fn key_count(&self) -> usize {
        self.keys.len()
    }

    pub fn status(&self) -> PublicApiAuthenticationStatus {
        PublicApiAuthenticationStatus {
            enabled: !self.keys.is_empty(),
            key_count: self.keys.len(),
        }
    }

    pub(crate) fn authenticate(
        &self,
        headers: &HeaderMap,
    ) -> Option<AuthenticatedPublicCredential> {
        if self.keys.is_empty() {
            return Some(AuthenticatedPublicCredential { slot: None });
        }
        if headers.get_all(AUTHORIZATION).iter().count() != 1 {
            return None;
        }
        let candidate = headers
            .get(AUTHORIZATION)
            .and_then(|header| header.to_str().ok())
            .and_then(bearer_token)?;
        if candidate.len() > MAXIMUM_API_KEY_BYTES {
            return None;
        }

        // Evaluate every configured key and use a fixed-width comparison so request timing does
        // not reveal which key matched or how many prefix bytes were correct.
        let mut matched_slot = None;
        for (index, configured) in self.keys.iter().enumerate() {
            let matches = constant_time_equal(configured, candidate.as_bytes());
            if matches {
                matched_slot = CredentialSlot::new(index);
            }
        }
        matched_slot.map(|slot| AuthenticatedPublicCredential { slot: Some(slot) })
    }

    pub(crate) fn authorizes(&self, headers: &HeaderMap) -> bool {
        self.authenticate(headers).is_some()
    }

    pub fn overlaps_operator(&self, operator: &OperatorApiAuthenticator) -> bool {
        self.keys.iter().fold(false, |overlaps, configured| {
            constant_time_equal(configured, operator.inner.keys[0].as_ref()) | overlaps
        })
    }
}

impl OperatorApiAuthenticator {
    pub fn from_configuration(configuration: &str) -> Result<Self, String> {
        if configuration.contains(',') {
            return Err("INFERLAB_OPERATOR_API_KEY must contain exactly one API key".to_owned());
        }
        let inner =
            PublicApiAuthenticator::from_configuration(Some(configuration)).map_err(|error| {
                error
                    .replace(
                        "INFERLAB_PUBLIC_API_KEYS entry 1",
                        "INFERLAB_OPERATOR_API_KEY",
                    )
                    .replace("INFERLAB_PUBLIC_API_KEYS", "INFERLAB_OPERATOR_API_KEY")
            })?;
        if inner.keys.len() != 1 {
            return Err("INFERLAB_OPERATOR_API_KEY must contain exactly one API key".to_owned());
        }
        Ok(Self { inner })
    }

    pub(crate) fn authorizes(&self, headers: &HeaderMap) -> bool {
        self.inner.authenticate(headers).is_some()
    }
}

impl Default for PublicApiAuthenticator {
    fn default() -> Self {
        Self::disabled()
    }
}

fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer")
        && !token.is_empty()
        && token.bytes().all(is_visible_non_whitespace_ascii))
    .then_some(token)
}

fn is_visible_non_whitespace_ascii(byte: u8) -> bool {
    byte.is_ascii_graphic() && byte != b','
}

fn constant_time_equal(configured: &[u8], candidate: &[u8]) -> bool {
    let mut difference = configured.len() ^ candidate.len();
    for index in 0..MAXIMUM_API_KEY_BYTES {
        let configured_byte = configured.get(index).copied().unwrap_or_default();
        let candidate_byte = candidate.get(index).copied().unwrap_or_default();
        difference |= usize::from(configured_byte ^ candidate_byte);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, header::AUTHORIZATION};

    use super::{OperatorApiAuthenticator, PublicApiAuthenticator};

    const FIRST_KEY: &str = "interview-demo-key-0001";
    const SECOND_KEY: &str = "interview-demo-key-0002";

    #[test]
    fn absent_configuration_preserves_unauthenticated_behavior() {
        let authenticator = PublicApiAuthenticator::from_configuration(None).expect("disabled");

        assert!(authenticator.authorizes(&HeaderMap::new()));
        assert!(!authenticator.status().enabled);
        assert_eq!(authenticator.status().key_count, 0);
    }

    #[test]
    fn valid_bearer_key_is_accepted_without_exposing_key_material_in_status() {
        let authenticator =
            PublicApiAuthenticator::from_configuration(Some(&format!("{FIRST_KEY},{SECOND_KEY}")))
                .expect("valid keys");
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {SECOND_KEY}")).expect("header"),
        );

        assert!(authenticator.authorizes(&headers));
        assert!(authenticator.status().enabled);
        assert_eq!(authenticator.status().key_count, 2);
        assert_eq!(
            authenticator
                .authenticate(&headers)
                .expect("authenticated")
                .slot()
                .expect("configured key has a slot")
                .index(),
            1
        );
    }

    #[test]
    fn missing_malformed_wrong_and_duplicate_headers_are_rejected() {
        let authenticator =
            PublicApiAuthenticator::from_configuration(Some(FIRST_KEY)).expect("valid key");
        assert!(!authenticator.authorizes(&HeaderMap::new()));

        for authorization in [
            FIRST_KEY,
            "Basic interview-demo-key-0001",
            "Bearer wrong-interview-key-0001",
            "Bearer  interview-demo-key-0001",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(authorization).expect("header"),
            );
            assert!(!authenticator.authorizes(&headers), "{authorization}");
        }

        let mut duplicate_headers = HeaderMap::new();
        duplicate_headers.append(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer interview-demo-key-0001"),
        );
        duplicate_headers.append(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer interview-demo-key-0001"),
        );
        assert!(!authenticator.authorizes(&duplicate_headers));
    }

    #[test]
    fn invalid_configuration_fails_fast_without_echoing_key_material() {
        let too_short = PublicApiAuthenticator::from_configuration(Some("short"))
            .err()
            .expect("short key rejected");
        assert!(too_short.contains("entry 1"));
        assert!(!too_short.contains("short"));

        let empty = PublicApiAuthenticator::from_configuration(Some(""))
            .err()
            .expect("empty configuration rejected");
        assert!(empty.contains("at least one"));

        let duplicate =
            PublicApiAuthenticator::from_configuration(Some(&format!("{FIRST_KEY},{FIRST_KEY}")))
                .err()
                .expect("duplicate configuration rejected");
        assert!(duplicate.contains("duplicates"));
        assert!(!duplicate.contains(FIRST_KEY));
    }

    #[test]
    fn configuration_size_key_count_and_key_size_are_bounded() {
        let oversized_configuration = "x".repeat(super::MAXIMUM_CONFIGURATION_BYTES + 1);
        assert!(
            PublicApiAuthenticator::from_configuration(Some(&oversized_configuration))
                .err()
                .expect("oversized configuration rejected")
                .contains("maximum configuration size")
        );

        let oversized_key = "x".repeat(super::MAXIMUM_API_KEY_BYTES + 1);
        assert!(
            PublicApiAuthenticator::from_configuration(Some(&oversized_key))
                .err()
                .expect("oversized key rejected")
                .contains("maximum size")
        );

        let too_many_keys = (0..=super::MAXIMUM_API_KEY_COUNT)
            .map(|index| format!("interview-demo-key-{index:04}"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(
            PublicApiAuthenticator::from_configuration(Some(&too_many_keys))
                .err()
                .expect("excess key count rejected")
                .contains("at most")
        );
    }

    #[test]
    fn operator_key_is_single_bounded_and_overlap_is_detected_without_echoing_it() {
        let public = PublicApiAuthenticator::from_configuration(Some(FIRST_KEY)).expect("public");
        let distinct = OperatorApiAuthenticator::from_configuration(SECOND_KEY).expect("operator");
        assert!(!public.overlaps_operator(&distinct));

        let overlapping =
            OperatorApiAuthenticator::from_configuration(FIRST_KEY).expect("operator");
        assert!(public.overlaps_operator(&overlapping));

        let error = OperatorApiAuthenticator::from_configuration("short")
            .err()
            .expect("short key rejected");
        assert!(error.contains("INFERLAB_OPERATOR_API_KEY"));
        assert!(!error.contains("short"));

        assert!(
            OperatorApiAuthenticator::from_configuration(&format!("{FIRST_KEY},{SECOND_KEY}"))
                .err()
                .expect("multiple keys rejected")
                .contains("exactly one")
        );
    }
}
