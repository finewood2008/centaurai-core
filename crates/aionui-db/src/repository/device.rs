use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::{DevicePairingSessionRow, DeviceRow};

#[derive(Debug)]
pub struct CreateDevicePairingParams<'a> {
    pub id: &'a str,
    pub user_id: &'a str,
    pub code_hash: &'a str,
    pub server_url: &'a str,
    pub expires_at: TimestampMs,
    pub created_at: TimestampMs,
}

#[derive(Debug)]
pub struct RedeemDevicePairingParams<'a> {
    pub code_hash: &'a str,
    pub device_id: &'a str,
    pub token_hash: &'a str,
    pub name: &'a str,
    pub platform: &'a str,
    pub now: TimestampMs,
}

/// Persistence boundary for user-scoped devices and one-time pairing sessions.
#[async_trait::async_trait]
pub trait IDeviceRepository: Send + Sync {
    async fn create_pairing(&self, params: CreateDevicePairingParams<'_>) -> Result<DevicePairingSessionRow, DbError>;

    /// Atomically consumes an unexpired pairing and creates its device.
    /// Returns `None` for unknown, expired, or already-consumed codes.
    async fn redeem_pairing(&self, params: RedeemDevicePairingParams<'_>) -> Result<Option<DeviceRow>, DbError>;

    async fn list_devices(&self, user_id: &str) -> Result<Vec<DeviceRow>, DbError>;

    /// Revokes a device belonging to `user_id`, returning `None` when the id
    /// is absent or belongs to another user.
    async fn revoke_device(
        &self,
        user_id: &str,
        device_id: &str,
        revoked_at: TimestampMs,
    ) -> Result<Option<DeviceRow>, DbError>;

    /// Resolves an active device token hash and updates its last-seen time.
    async fn authenticate_token(
        &self,
        token_hash: &str,
        last_seen_at: TimestampMs,
    ) -> Result<Option<DeviceRow>, DbError>;
}
