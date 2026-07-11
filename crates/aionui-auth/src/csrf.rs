#![allow(clippy::disallowed_types)]

use std::fmt::Write as _;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, header};
use axum::middleware::Next;
use axum::response::Response;

use aionui_common::ApiError;
use aionui_common::constants::{CSRF_COOKIE_NAME, CSRF_HEADER_NAME, LEGACY_CSRF_COOKIE_NAME};

use crate::cookie::CookieConfig;
use crate::extract::{extract_bearer_token, extract_cookie_value, extract_csrf_cookie, extract_session_token};

/// CSRF protection middleware using the Double Submit Cookie pattern.
///
/// Behavior:
/// - Safe methods (GET, HEAD, OPTIONS) bypass validation.
/// - Exempt paths (`/login`, `/api/auth/qr-login`) bypass validation.
/// - State-changing requests authenticated by a session cookie must include
///   an `x-csrf-token` header matching the canonical (or legacy fallback)
///   CSRF cookie.
/// - Authorization Bearer clients bypass CSRF and remain subject to the auth
///   middleware on protected routes.
/// - Sets canonical and transition-only legacy CSRF cookies on responses.
pub async fn csrf_middleware(
    State(cookie_config): State<Arc<CookieConfig>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();

    let bearer_token = extract_bearer_token(request.headers());
    let session_cookie = extract_session_token(request.headers());
    let primary_csrf_cookie = extract_cookie_value(request.headers(), CSRF_COOKIE_NAME);
    let legacy_csrf_cookie = extract_cookie_value(request.headers(), LEGACY_CSRF_COOKIE_NAME);
    let csrf_cookie = extract_csrf_cookie(request.headers());

    // Validate CSRF for state-changing requests
    let needs_validation = matches!(method, Method::POST | Method::PUT | Method::DELETE | Method::PATCH);
    let is_exempt = path == "/login"
        || path == "/api/auth/qr-login"
        || path == "/api/auth/refresh"
        || path == "/api/devices/pairing/redeem";
    let uses_cookie_auth = session_cookie.is_some() && bearer_token.is_none();

    if needs_validation && !is_exempt && uses_cookie_auth {
        let header_token = request
            .headers()
            .get(CSRF_HEADER_NAME)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_owned());

        match (&csrf_cookie, header_token) {
            (Some(cookie), Some(ref hdr)) if !cookie.is_empty() && cookie == hdr => {
                // Valid: cookie and header match
            }
            _ => {
                return Err(ApiError::CsrfInvalid("CSRF token validation failed".into()));
            }
        }
    }

    let mut response = next.run(request).await;

    if path != "/logout" {
        let token = csrf_cookie.unwrap_or_else(generate_csrf_token);
        let cookies = cookie_config.build_csrf_cookies(&token);
        if primary_csrf_cookie.is_none()
            && let Ok(value) = HeaderValue::from_str(&cookies[0])
        {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
        if legacy_csrf_cookie.is_none()
            && let Ok(value) = HeaderValue::from_str(&cookies[1])
        {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }

    Ok(response)
}

/// Generate a cryptographically random 32-byte CSRF token as a hex string.
fn generate_csrf_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).expect("OS entropy source unavailable");
    let mut hex = String::with_capacity(64);
    for byte in buf {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csrf_token_is_64_hex_chars() {
        let token = generate_csrf_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn csrf_tokens_are_unique() {
        let t1 = generate_csrf_token();
        let t2 = generate_csrf_token();
        assert_ne!(t1, t2);
    }
}
