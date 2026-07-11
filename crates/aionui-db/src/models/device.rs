use aionui_common::TimestampMs;

/// Persisted device credential metadata. Secret material is represented only
/// by its SHA-256 hash.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct DeviceRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub platform: String,
    pub token_hash: String,
    pub last_seen_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub revoked_at: Option<TimestampMs>,
}

/// Persisted one-time pairing session. The plaintext code is never stored.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct DevicePairingSessionRow {
    pub id: String,
    pub user_id: String,
    pub code_hash: String,
    pub server_url: String,
    pub expires_at: TimestampMs,
    pub consumed_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
}
