use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{DevicePairingSessionRow, DeviceRow};
use crate::repository::device::{CreateDevicePairingParams, IDeviceRepository, RedeemDevicePairingParams};

#[derive(Clone, Debug)]
pub struct SqliteDeviceRepository {
    pool: SqlitePool,
}

impl SqliteDeviceRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IDeviceRepository for SqliteDeviceRepository {
    async fn create_pairing(&self, params: CreateDevicePairingParams<'_>) -> Result<DevicePairingSessionRow, DbError> {
        let row = sqlx::query_as::<_, DevicePairingSessionRow>(
            "INSERT INTO device_pairing_sessions \
                (id, user_id, code_hash, server_url, expires_at, consumed_at, created_at) \
             VALUES (?, ?, ?, ?, ?, NULL, ?) \
             RETURNING *",
        )
        .bind(params.id)
        .bind(params.user_id)
        .bind(params.code_hash)
        .bind(params.server_url)
        .bind(params.expires_at)
        .bind(params.created_at)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn redeem_pairing(&self, params: RedeemDevicePairingParams<'_>) -> Result<Option<DeviceRow>, DbError> {
        let mut tx = self.pool.begin().await?;
        let claimed: Option<(String,)> = sqlx::query_as(
            "UPDATE device_pairing_sessions \
             SET consumed_at = ? \
             WHERE code_hash = ? AND consumed_at IS NULL AND expires_at > ? \
             RETURNING user_id",
        )
        .bind(params.now)
        .bind(params.code_hash)
        .bind(params.now)
        .fetch_optional(&mut *tx)
        .await?;

        let Some((user_id,)) = claimed else {
            tx.rollback().await?;
            return Ok(None);
        };

        let device = sqlx::query_as::<_, DeviceRow>(
            "INSERT INTO devices \
                (id, user_id, name, platform, token_hash, last_seen_at, created_at, revoked_at) \
             VALUES (?, ?, ?, ?, ?, NULL, ?, NULL) \
             RETURNING *",
        )
        .bind(params.device_id)
        .bind(user_id)
        .bind(params.name)
        .bind(params.platform)
        .bind(params.token_hash)
        .bind(params.now)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(Some(device))
    }

    async fn list_devices(&self, user_id: &str) -> Result<Vec<DeviceRow>, DbError> {
        let rows =
            sqlx::query_as::<_, DeviceRow>("SELECT * FROM devices WHERE user_id = ? ORDER BY created_at DESC, id ASC")
                .bind(user_id)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }

    async fn revoke_device(
        &self,
        user_id: &str,
        device_id: &str,
        revoked_at: aionui_common::TimestampMs,
    ) -> Result<Option<DeviceRow>, DbError> {
        let row = sqlx::query_as::<_, DeviceRow>(
            "UPDATE devices \
             SET revoked_at = COALESCE(revoked_at, ?) \
             WHERE id = ? AND user_id = ? \
             RETURNING *",
        )
        .bind(revoked_at)
        .bind(device_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn authenticate_token(
        &self,
        token_hash: &str,
        last_seen_at: aionui_common::TimestampMs,
    ) -> Result<Option<DeviceRow>, DbError> {
        let row = sqlx::query_as::<_, DeviceRow>(
            "UPDATE devices \
             SET last_seen_at = ? \
             WHERE token_hash = ? AND revoked_at IS NULL \
             RETURNING *",
        )
        .bind(last_seen_at)
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }
}
