use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use sha2::Sha256;

const USER_HEADER: &str = "x-aionui-proxy-user-id";
const USERNAME_HEADER: &str = "x-aionui-proxy-username";
const ROLE_HEADER: &str = "x-aionui-proxy-role";
const TIMESTAMP_HEADER: &str = "x-aionui-proxy-timestamp";
const SIGNATURE_HEADER: &str = "x-aionui-proxy-signature";
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
        std::env::var("AIONUI_TRUSTED_PROXY_SECRET")
            .ok()
            .and_then(|secret| Self::new(secret.into_bytes()))
    }

    pub fn has_identity_headers(headers: &HeaderMap) -> bool {
        [
            USER_HEADER,
            USERNAME_HEADER,
            ROLE_HEADER,
            TIMESTAMP_HEADER,
            SIGNATURE_HEADER,
        ]
        .into_iter()
        .any(|name| headers.contains_key(name))
    }

    pub fn verify(&self, headers: &HeaderMap) -> Result<ProxyIdentity, ProxyIdentityError> {
        let user_id = header(headers, USER_HEADER)?;
        let username = header(headers, USERNAME_HEADER)?;
        let role = header(headers, ROLE_HEADER)?;
        let timestamp_raw = header(headers, TIMESTAMP_HEADER)?;
        let signature = header(headers, SIGNATURE_HEADER)?;
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

    fn signed_headers(secret: &[u8], timestamp: i64, user_id: &str) -> HeaderMap {
        let timestamp = timestamp.to_string();
        let canonical = canonical_identity(&timestamp, user_id, "alice", "user");
        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(canonical.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());
        let mut headers = HeaderMap::new();
        for (name, value) in [
            (USER_HEADER, user_id),
            (USERNAME_HEADER, "alice"),
            (ROLE_HEADER, "user"),
            (TIMESTAMP_HEADER, &timestamp),
            (SIGNATURE_HEADER, &signature),
        ] {
            headers.insert(name, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn accepts_current_signed_identity() {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let verifier = ProxyIdentityVerifier::new(b"secret".to_vec()).unwrap();
        let identity = verifier.verify(&signed_headers(b"secret", now, "user-1")).unwrap();
        assert_eq!(identity.user_id, "user-1");
        assert_eq!(identity.role, "user");
    }

    #[test]
    fn rejects_signature_tampering_and_stale_requests() {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let verifier = ProxyIdentityVerifier::new(b"secret".to_vec()).unwrap();
        let mut tampered = signed_headers(b"secret", now, "user-1");
        tampered.insert(USER_HEADER, HeaderValue::from_static("user-2"));
        assert_eq!(verifier.verify(&tampered), Err(ProxyIdentityError::InvalidSignature));
        assert_eq!(
            verifier.verify(&signed_headers(b"secret", now - 61, "user-1")),
            Err(ProxyIdentityError::Expired)
        );
    }
}
