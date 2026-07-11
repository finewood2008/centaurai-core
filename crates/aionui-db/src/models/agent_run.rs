use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, sqlx::FromRow)]
pub struct AgentRunRow {
    pub id: String,
    pub turn_id: String,
    pub user_id: String,
    pub conversation_id: String,
    pub source: String,
    pub status: String,
    pub request_json: String,
    pub message_id: Option<String>,
    pub error_code: Option<String>,
    pub effective_model: Option<String>,
    pub fallback_used: bool,
    pub queued_at: TimestampMs,
    pub started_at: Option<TimestampMs>,
    pub finished_at: Option<TimestampMs>,
    pub updated_at: TimestampMs,
}
