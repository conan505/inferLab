use axum::{
    http::{HeaderMap, HeaderValue, header},
    response::{Html, IntoResponse, Response},
};

const SHOWCASE_PAGE: &str = include_str!("../showcase/index.html");
const SOCIAL_CARD: &[u8] = include_bytes!("../showcase/assets/og-inferlab.png");
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; base-uri 'none'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; img-src 'self' data:; object-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'";

pub async fn page() -> Response {
    let mut response = Html(SHOWCASE_PAGE).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), geolocation=(), microphone=()"),
    );
    response
}

pub async fn social_card() -> Response {
    let mut response = SOCIAL_CARD.to_vec().into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400, immutable"),
    );
    apply_public_security_headers(response.headers_mut());
    response
}

pub fn apply_public_security_headers(headers: &mut HeaderMap) {
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;

    use super::*;

    #[tokio::test]
    async fn showcase_page_is_embedded_and_hardened() {
        let response = page().await;
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        assert_eq!(
            response.headers()[header::CONTENT_SECURITY_POLICY],
            CONTENT_SECURITY_POLICY
        );
        assert_eq!(response.headers()["x-frame-options"], "DENY");
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("showcase body");
        let body = String::from_utf8(bytes.to_vec()).expect("UTF-8 showcase");
        assert!(body.contains("Distributed inference, made inspectable"));
        assert!(body.contains("/v1/chat/completions"));
        assert!(body.contains("/showcase/status"));
        assert!(body.contains("/assets/og-inferlab.png"));
    }

    #[tokio::test]
    async fn social_card_is_embedded_with_cache_headers() {
        let response = social_card().await;
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "public, max-age=86400, immutable"
        );
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("social card body");
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn security_headers_do_not_require_a_showcase_response() {
        let mut headers = HeaderMap::new();
        apply_public_security_headers(&mut headers);
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
    }
}
