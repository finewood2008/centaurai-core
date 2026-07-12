use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use aionui_api_types::{
    CreateDevicePairingRequest, DevicePairingSessionResponse, DeviceResponse, PairedDeviceCredentialResponse,
    RedeemDevicePairingRequest,
};
use aionui_common::{generate_prefixed_id, now_ms};
use aionui_db::{CreateDevicePairingParams, DbError, DeviceRow, IDeviceRepository, RedeemDevicePairingParams};
use base64::Engine as _;
use chrono::{DateTime, SecondsFormat, Utc};
use sha2::{Digest, Sha256};
use url::{Host, Url};

pub const DEVICE_TOKEN_PREFIX: &str = "cai_dev_v1_";
const PAIRING_CODE_PREFIX: &str = "cai_pair_v1_";
const PAIRING_TTL_MS: i64 = 5 * 60 * 1000;
const RANDOM_SECRET_BYTES: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("Invalid private server URL")]
    InvalidServerUrl,
    #[error("Invalid device name")]
    InvalidName,
    #[error("Invalid device platform")]
    InvalidPlatform,
    #[error("Invalid or expired pairing code")]
    InvalidPairing,
    #[error("Device not found")]
    NotFound,
    #[error("Device persistence failed")]
    Persistence(#[source] DbError),
    #[error("Stored device timestamp is invalid")]
    InvalidTimestamp,
}

impl From<DbError> for DeviceError {
    fn from(value: DbError) -> Self {
        Self::Persistence(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicePrincipal {
    pub device_id: String,
    pub user_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokedDevice {
    pub device: DeviceResponse,
    pub token_hash: String,
}

#[derive(Clone)]
pub struct DeviceService {
    repo: Arc<dyn IDeviceRepository>,
}

impl DeviceService {
    pub fn new(repo: Arc<dyn IDeviceRepository>) -> Self {
        Self { repo }
    }

    pub async fn create_pairing(
        &self,
        user_id: &str,
        req: CreateDevicePairingRequest,
    ) -> Result<DevicePairingSessionResponse, DeviceError> {
        let server_url = normalize_private_server_origin(
            req.server_url.trim(),
            cfg!(debug_assertions) || loopback_pairing_enabled(|name| std::env::var(name).ok()),
        )?;

        let now = now_ms();
        let expires_at = now + PAIRING_TTL_MS;
        let pairing_id = generate_prefixed_id("pair");
        let code = generate_secret(PAIRING_CODE_PREFIX);
        let code_hash = hash_credential(&code);
        let row = self
            .repo
            .create_pairing(CreateDevicePairingParams {
                id: &pairing_id,
                user_id,
                code_hash: &code_hash,
                server_url: &server_url,
                expires_at,
                created_at: now,
            })
            .await?;

        let mut pairing_uri = Url::parse("contextofme://pair").expect("static pairing URL must parse");
        pairing_uri
            .query_pairs_mut()
            .append_pair("server", &server_url)
            .append_pair("code", &code);

        tracing::info!(
            pairing_id = %row.id,
            user_id,
            expires_at = row.expires_at,
            "device pairing session created"
        );
        Ok(DevicePairingSessionResponse {
            id: row.id,
            pairing_uri: pairing_uri.into(),
            expires_at: timestamp_to_iso(row.expires_at)?,
        })
    }

    pub async fn redeem_pairing(
        &self,
        req: RedeemDevicePairingRequest,
    ) -> Result<PairedDeviceCredentialResponse, DeviceError> {
        let code = req.code.trim();
        if !valid_generated_secret(code, PAIRING_CODE_PREFIX) {
            return Err(DeviceError::InvalidPairing);
        }
        let name = validate_device_name(&req.name)?;
        let platform = validate_device_platform(&req.platform)?;

        let now = now_ms();
        let device_id = generate_prefixed_id("device");
        let device_token = generate_secret(DEVICE_TOKEN_PREFIX);
        let code_hash = hash_credential(code);
        let token_hash = hash_credential(&device_token);
        let row = self
            .repo
            .redeem_pairing(RedeemDevicePairingParams {
                code_hash: &code_hash,
                device_id: &device_id,
                token_hash: &token_hash,
                name,
                platform,
                now,
            })
            .await?
            .ok_or(DeviceError::InvalidPairing)?;

        tracing::info!(device_id = %row.id, user_id = %row.user_id, "device pairing redeemed");
        Ok(PairedDeviceCredentialResponse {
            device: device_to_response(row)?,
            device_token,
        })
    }

    pub async fn list_devices(&self, user_id: &str) -> Result<Vec<DeviceResponse>, DeviceError> {
        self.repo
            .list_devices(user_id)
            .await?
            .into_iter()
            .map(device_to_response)
            .collect()
    }

    pub async fn revoke_device(&self, user_id: &str, device_id: &str) -> Result<RevokedDevice, DeviceError> {
        let row = self
            .repo
            .revoke_device(user_id, device_id, now_ms())
            .await?
            .ok_or(DeviceError::NotFound)?;
        let token_hash = row.token_hash.clone();
        tracing::info!(device_id = %row.id, user_id = %row.user_id, "device credential revoked");
        Ok(RevokedDevice {
            device: device_to_response(row)?,
            token_hash,
        })
    }

    pub async fn authenticate_token(&self, token: &str) -> Result<Option<DevicePrincipal>, DeviceError> {
        if !Self::is_device_token(token) {
            return Ok(None);
        }
        let token_hash = hash_credential(token);
        let row = self.repo.authenticate_token(&token_hash, now_ms()).await?;
        Ok(row.map(|device| DevicePrincipal {
            device_id: device.id,
            user_id: device.user_id,
        }))
    }

    pub fn is_device_token(token: &str) -> bool {
        valid_generated_secret(token, DEVICE_TOKEN_PREFIX)
    }
}

/// Hash a bearer credential before lookup or comparison. The plaintext value
/// must never be persisted or logged.
pub fn hash_device_credential(value: &str) -> String {
    hash_credential(value)
}

/// Constant-time comparison between a plaintext in-memory credential and a
/// persisted hexadecimal SHA-256 hash.
pub fn device_credential_matches_hash(value: &str, expected_hash: &str) -> bool {
    let actual = Sha256::digest(value.as_bytes());
    let Ok(expected) = hex::decode(expected_hash) else {
        return false;
    };
    if expected.len() != actual.len() {
        return false;
    }
    actual
        .iter()
        .zip(expected)
        .fold(0u8, |difference, (left, right)| difference | (*left ^ right))
        == 0
}

fn generate_secret(prefix: &str) -> String {
    let mut bytes = [0u8; RANDOM_SECRET_BYTES];
    getrandom::getrandom(&mut bytes).expect("OS entropy source unavailable");
    format!(
        "{prefix}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

fn valid_generated_secret(value: &str, prefix: &str) -> bool {
    let Some(secret) = value.strip_prefix(prefix) else {
        return false;
    };
    secret.len() == 43
        && secret
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn hash_credential(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn validate_device_name(value: &str) -> Result<&str, DeviceError> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 100 || value.chars().any(char::is_control) {
        return Err(DeviceError::InvalidName);
    }
    Ok(value)
}

fn validate_device_platform(value: &str) -> Result<&str, DeviceError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 50
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(DeviceError::InvalidPlatform);
    }
    Ok(value)
}

fn normalize_private_server_origin(value: &str, allow_loopback: bool) -> Result<String, DeviceError> {
    if value.is_empty() || value.len() > 2048 {
        return Err(DeviceError::InvalidServerUrl);
    }
    let url = Url::parse(value).map_err(|_| DeviceError::InvalidServerUrl)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(DeviceError::InvalidServerUrl);
    }

    let allowed = match url.host() {
        Some(Host::Ipv4(ip)) => allowed_ipv4(ip, allow_loopback),
        Some(Host::Ipv6(ip)) => allowed_ipv6(ip, allow_loopback),
        Some(Host::Domain(domain)) => {
            let domain = domain.trim_end_matches('.').to_ascii_lowercase();
            ((allow_loopback && domain == "localhost") || domain.ends_with(".local") || domain.ends_with(".ts.net"))
                && (!domain.ends_with(".ts.net") || url.scheme() == "https")
        }
        None => false,
    };
    if !allowed {
        return Err(DeviceError::InvalidServerUrl);
    }
    Ok(url.origin().ascii_serialization())
}

fn allowed_ipv4(ip: Ipv4Addr, allow_loopback: bool) -> bool {
    let octets = ip.octets();
    ip.is_private() || (allow_loopback && ip.is_loopback()) || (octets[0] == 100 && (64..=127).contains(&octets[1]))
}

fn allowed_ipv6(ip: Ipv6Addr, allow_loopback: bool) -> bool {
    (allow_loopback && ip.is_loopback()) || ip.is_unique_local()
}

fn loopback_pairing_enabled(mut value: impl FnMut(&str) -> Option<String>) -> bool {
    value("CENTAURAI_CORE_ALLOW_LOOPBACK_PAIRING")
        .or_else(|| value("AIONUI_ALLOW_LOOPBACK_PAIRING"))
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
}

fn timestamp_to_iso(timestamp: i64) -> Result<String, DeviceError> {
    DateTime::<Utc>::from_timestamp_millis(timestamp)
        .map(|value| value.to_rfc3339_opts(SecondsFormat::Millis, true))
        .ok_or(DeviceError::InvalidTimestamp)
}

fn device_to_response(row: DeviceRow) -> Result<DeviceResponse, DeviceError> {
    Ok(DeviceResponse {
        id: row.id,
        name: row.name,
        platform: row.platform,
        last_seen_at: row.last_seen_at.map(timestamp_to_iso).transpose()?,
        created_at: timestamp_to_iso(row.created_at)?,
        revoked_at: row.revoked_at.map(timestamp_to_iso).transpose()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_server_url_policy_accepts_lan_mdns_and_tailscale() {
        for (value, normalized) in [
            ("http://192.168.1.5:25808", "http://192.168.1.5:25808"),
            ("http://10.0.0.2", "http://10.0.0.2"),
            ("http://[fd00::1]:25808", "http://[fd00::1]:25808"),
            ("https://CONTEXT.home.local:443", "https://context.home.local"),
            ("https://machine.tail123.ts.net", "https://machine.tail123.ts.net"),
            ("http://100.100.100.100:25808", "http://100.100.100.100:25808"),
        ] {
            assert_eq!(normalize_private_server_origin(value, false).unwrap(), normalized);
        }
    }

    #[test]
    fn loopback_is_an_explicit_development_policy() {
        for value in [
            "http://192.168.1.5:25808",
            "http://127.0.0.1:25808",
            "http://[::1]:25808",
            "http://localhost:25808",
        ] {
            let expected = !value.contains("127.0.0.1") && !value.contains("::1") && !value.contains("localhost");
            assert_eq!(
                normalize_private_server_origin(value, false).is_ok(),
                expected,
                "{value}"
            );
            assert!(normalize_private_server_origin(value, true).is_ok(), "{value}");
        }
    }

    #[test]
    fn private_server_url_policy_rejects_public_or_unsafe_urls() {
        for value in [
            "https://example.com",
            "https://8.8.8.8",
            "file:///tmp/core",
            "javascript:alert(1)",
            "https://user:pass@context.home.local",
            "https://context.home.local/#fragment",
            "https://context.home.local/api",
            "https://context.home.local?token=secret",
            "http://machine.tail123.ts.net",
            "http://centaur-server:25808",
            "https://printer",
            "http://169.254.1.1",
        ] {
            assert!(normalize_private_server_origin(value, false).is_err(), "{value}");
        }
    }

    #[test]
    fn canonical_loopback_environment_precedes_alias() {
        assert!(!loopback_pairing_enabled(|name| match name {
            "CENTAURAI_CORE_ALLOW_LOOPBACK_PAIRING" => Some("false".into()),
            "AIONUI_ALLOW_LOOPBACK_PAIRING" => Some("true".into()),
            _ => None,
        }));
    }

    #[test]
    fn generated_credentials_have_explicit_prefix_and_stable_hashes() {
        let token = generate_secret(DEVICE_TOKEN_PREFIX);
        assert!(DeviceService::is_device_token(&token));
        assert_eq!(hash_credential(&token), hash_credential(&token));
        assert_eq!(hash_credential(&token).len(), 64);
        assert_ne!(token, hash_credential(&token));
        assert!(device_credential_matches_hash(&token, &hash_credential(&token)));
        assert!(!device_credential_matches_hash("different", &hash_credential(&token)));
    }
}
