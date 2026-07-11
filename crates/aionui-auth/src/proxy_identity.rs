use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use sha2::Sha256;

#[derive(Clone, Copy)]
struct ProxyHeaderNames {
    user: &'static str,
    username: &'static str,
    role: &'static str,
    timestamp: &'static str,
    signature: &'static str,
}

const CENTAURAI_HEADERS: ProxyHeaderNames = ProxyHeaderNames {
    user: "x-centaurai-proxy-user-id",
    username: "x-centaurai-proxy-username",
    role: "x-centaurai-proxy-role",
    timestamp: "x-centaurai-proxy-timestamp",
    signature: "x-centaurai-proxy-signature",
};
const LEGACY_HEADERS: ProxyHeaderNames = ProxyHeaderNames {
    user: "x-aionui-proxy-user-id",
    username: "x-aionui-proxy-username",
    role: "x-aionui-proxy-role",
    timestamp: "x-aionui-proxy-timestamp",
    signature: "x-aionui-proxy-signature",
};
const SIGNATURE_VERSION: &str = "v1";
const MAX_CLOCK_SKEW_SECS: i64 = 60;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyIdentity {
    pub user_id: String,
    pub username: String,
    pub role: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyIdentityError {
    Incomplete,
    InvalidTimestamp,
    Expired,
    InvalidSignature,
}

#[derive(Clone)]
pub struct ProxyIdentityVerifier {
    secret: Arc<[u8]>,
}

impl ProxyIdentityVerifier {
    pub fn new(secret: impl Into<Vec<u8>>) -> Option<Self> {
        let secret = secret.into();
        (!secret.is_empty()).then(|| Self { secret: secret.into() })
    }

    pub fn from_env() -> Option<Self> {
        std::env::var("CENTAURAI_CORE_TRUSTED_PROXY_SECRET")
            .or_else(|_| std::env::var("AIONUI_TRUSTED_PROXY_SECRET"))
            .ok()
            .and_then(|secret| Self::new(secret.into_bytes()))
    }

    pub fn has_identity_headers(headers: &HeaderMap) -> bool {
        header_family_present(headers, CENTAURAI_HEADERS) || header_family_present(headers, LEGACY_HEADERS)
    }

    pub fn verify(&self, headers: &HeaderMap) -> Result<ProxyIdentity, ProxyIdentityError> {
        // A partial canonical family must fail closed instead of falling back
        // to legacy headers supplied alongside it.
        let names = if header_family_present(headers, CENTAURAI_HEADERS) {
            CENTAURAI_HEADERS
        } else {
            LEGACY_HEADERS
        };
        let user_id = header(headers, names.user)?;
        let username = header(headers, names.username)?;
        let role = header(headers, names.role)?;
        let timestamp_raw = header(headers, names.timestamp)?;
        let signature = header(headers, names.signature)?;
        let timestamp = timestamp_raw
            .parse::<i64>()
            .map_err(|_| ProxyIdentityError::InvalidTimestamp)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        if now.abs_diff(timestamp) > MAX_CLOCK_SKEW_SECS as u64 {
            return Err(ProxyIdentityError::Expired);
        }

        let canonical = canonical_identity(timestamp_raw, user_id, username, role);
        let mut mac = HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts arbitrary key lengths");
        mac.update(canonical.as_bytes());
        let supplied = hex::decode(signature).map_err(|_| ProxyIdentityError::InvalidSignature)?;
        mac.verify_slice(&supplied)
            .map_err(|_| ProxyIdentityError::InvalidSignature)?;

        if user_id.trim().is_empty() || username.trim().is_empty() || !matches!(role, "admin" | "user") {
            return Err(ProxyIdentityError::Incomplete);
        }
        Ok(ProxyIdentity {
            user_id: user_id.to_owned(),
            username: username.to_owned(),
            role: role.to_owned(),
        })
    }
}

fn header_family_present(headers: &HeaderMap, names: ProxyHeaderNames) -> bool {
    [names.user, names.username, names.role, names.timestamp, names.signature]
        .into_iter()
        .any(|name| headers.contains_key(name))
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, ProxyIdentityError> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .ok_or(ProxyIdentityError::Incomplete)
}

fn canonical_identity(timestamp: &str, user_id: &str, username: &str, role: &str) -> String {
    format!("{SIGNATURE_VERSION}\n{timestamp}\n{user_id}\n{username}\n{role}")
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::*;

    fn signed_headers(secret: &[u8], timestamp: i64, user_id: &str, names: ProxyHeaderNames) -> HeaderMap {
        let timestamp = timestamp.to_string();
        let canonical = canonical_identity(&timestamp, user_id, "alice", "user");
        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(canonical.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());
        let mut headers = HeaderMap::new();
        for (name, value) in [
            (names.user, user_id),
            (names.username, "alice"),
            (names.role, "user"),
            (names.timestamp, &timestamp),
            (names.signature, &signature),
        ] {
            headers.insert(name, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn accepts_current_signed_identity() {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let verifier = ProxyIdentityVerifier::new(b"secret".to_vec()).unwrap();
        let identity = verifier
            .verify(&signed_headers(b"secret", now, "user-1", CENTAURAI_HEADERS))
            .unwrap();
        assert_eq!(identity.user_id, "user-1");
        assert_eq!(identity.role, "user");
    }

    #[test]
    fn accepts_legacy_signed_identity_headers() {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let verifier = ProxyIdentityVerifier::new(b"secret".to_vec()).unwrap();
        let identity = verifier
            .verify(&signed_headers(b"secret", now, "legacy-user", LEGACY_HEADERS))
            .unwrap();
        assert_eq!(identity.user_id, "legacy-user");
    }

    #[test]
    fn rejects_signature_tampering_and_stale_requests() {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let verifier = ProxyIdentityVerifier::new(b"secret".to_vec()).unwrap();
        let mut tampered = signed_headers(b"secret", now, "user-1", CENTAURAI_HEADERS);
        tampered.insert(CENTAURAI_HEADERS.user, HeaderValue::from_static("user-2"));
        assert_eq!(verifier.verify(&tampered), Err(ProxyIdentityError::InvalidSignature));
        assert_eq!(
            verifier.verify(&signed_headers(b"secret", now - 61, "user-1", CENTAURAI_HEADERS)),
            Err(ProxyIdentityError::Expired)
        );
    }

    #[test]
    fn partial_canonical_headers_do_not_fall_back_to_legacy_identity() {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let verifier = ProxyIdentityVerifier::new(b"secret".to_vec()).unwrap();
        let mut headers = signed_headers(b"secret", now, "legacy-user", LEGACY_HEADERS);
        headers.insert(CENTAURAI_HEADERS.user, HeaderValue::from_static("injected-user"));

        assert!(ProxyIdentityVerifier::has_identity_headers(&headers));
        assert_eq!(verifier.verify(&headers), Err(ProxyIdentityError::Incomplete));
    }
}
