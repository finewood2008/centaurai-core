use crate::{DbError, models::AgentRunRow};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AgentRuntimePolicyRow {
    pub mode: String,
    pub global_active_limit: i64,
    pub per_user_active_limit: i64,
    pub per_user_queue_limit: i64,
    pub global_queue_limit: i64,
    pub queue_timeout_ms: i64,
    pub confirmation_timeout_ms: i64,
    pub resident_task_limit: i64,
    pub resident_idle_timeout_ms: i64,
}

#[derive(Debug, Clone)]
pub struct CreateAgentRunParams<'a> {
    pub id: &'a str,
    pub turn_id: &'a str,
    pub user_id: &'a str,
    pub conversation_id: &'a str,
    pub source: &'a str,
    pub request_json: &'a str,
    pub message_id: Option<&'a str>,
    pub queued_at: i64,
}

#[async_trait::async_trait]
pub trait IAgentRunRepository: Send + Sync {
    async fn load_runtime_policy(&self) -> Result<AgentRuntimePolicyRow, DbError>;
    async fn save_runtime_policy(&self, policy: &AgentRuntimePolicyRow, updated_at: i64) -> Result<(), DbError>;
    async fn create_queued(&self, params: &CreateAgentRunParams<'_>) -> Result<AgentRunRow, DbError>;
    async fn find_by_turn_id(&self, turn_id: &str) -> Result<Option<AgentRunRow>, DbError>;
    async fn list_for_user(&self, user_id: &str) -> Result<Vec<AgentRunRow>, DbError>;
    async fn list_queued(&self) -> Result<Vec<AgentRunRow>, DbError>;
    async fn mark_dispatching(&self, id: &str, started_at: i64) -> Result<bool, DbError>;
    async fn mark_running(&self, id: &str, started_at: i64) -> Result<(), DbError>;
    async fn set_effective_model(
        &self,
        id: &str,
        model: &str,
        fallback_used: bool,
        updated_at: i64,
    ) -> Result<(), DbError>;
    async fn mark_terminal(
        &self,
        id: &str,
        status: &str,
        error_code: Option<&str>,
        finished_at: i64,
    ) -> Result<(), DbError>;
    async fn cancel_queued(&self, turn_id: &str, user_id: &str, finished_at: i64) -> Result<bool, DbError>;
    async fn fail_interrupted(&self, finished_at: i64) -> Result<u64, DbError>;
}
