use sqlx::SqlitePool;

use crate::DbError;
use crate::models::AgentRunRow;
use crate::repository::agent_run::{AgentRuntimePolicyRow, CreateAgentRunParams, IAgentRunRepository};

#[derive(Clone, Debug)]
pub struct SqliteAgentRunRepository {
    pool: SqlitePool,
}

impl SqliteAgentRunRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IAgentRunRepository for SqliteAgentRunRepository {
    async fn load_runtime_policy(&self) -> Result<AgentRuntimePolicyRow, DbError> {
        Ok(sqlx::query_as("SELECT mode, global_active_limit, per_user_active_limit, per_user_queue_limit, global_queue_limit, queue_timeout_ms, confirmation_timeout_ms, resident_task_limit, resident_idle_timeout_ms, memory_constrained_percent, memory_pause_percent, memory_reject_percent FROM agent_runtime_policy WHERE singleton = 1")
            .fetch_one(&self.pool)
            .await?)
    }

    async fn save_runtime_policy(&self, policy: &AgentRuntimePolicyRow, updated_at: i64) -> Result<(), DbError> {
        sqlx::query("UPDATE agent_runtime_policy SET mode = ?, global_active_limit = ?, per_user_active_limit = ?, per_user_queue_limit = ?, global_queue_limit = ?, queue_timeout_ms = ?, confirmation_timeout_ms = ?, resident_task_limit = ?, resident_idle_timeout_ms = ?, memory_constrained_percent = ?, memory_pause_percent = ?, memory_reject_percent = ?, updated_at = ? WHERE singleton = 1")
            .bind(&policy.mode)
            .bind(policy.global_active_limit)
            .bind(policy.per_user_active_limit)
            .bind(policy.per_user_queue_limit)
            .bind(policy.global_queue_limit)
            .bind(policy.queue_timeout_ms)
            .bind(policy.confirmation_timeout_ms)
            .bind(policy.resident_task_limit)
            .bind(policy.resident_idle_timeout_ms)
            .bind(policy.memory_constrained_percent)
            .bind(policy.memory_pause_percent)
            .bind(policy.memory_reject_percent)
            .bind(updated_at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn create_queued(&self, params: &CreateAgentRunParams<'_>) -> Result<AgentRunRow, DbError> {
        sqlx::query(
            "INSERT INTO agent_runs (id, turn_id, user_id, conversation_id, source, status, request_json, \
             message_id, queued_at, updated_at) VALUES (?, ?, ?, ?, ?, 'queued', ?, ?, ?, ?)",
        )
        .bind(params.id)
        .bind(params.turn_id)
        .bind(params.user_id)
        .bind(params.conversation_id)
        .bind(params.source)
        .bind(params.request_json)
        .bind(params.message_id)
        .bind(params.queued_at)
        .bind(params.queued_at)
        .execute(&self.pool)
        .await?;
        self.find_by_turn_id(params.turn_id)
            .await?
            .ok_or_else(|| DbError::Init("agent run disappeared after insert".into()))
    }

    async fn find_by_turn_id(&self, turn_id: &str) -> Result<Option<AgentRunRow>, DbError> {
        Ok(sqlx::query_as("SELECT * FROM agent_runs WHERE turn_id = ?")
            .bind(turn_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn list_for_user(&self, user_id: &str) -> Result<Vec<AgentRunRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT * FROM agent_runs WHERE user_id = ? \
             AND status IN ('queued', 'dispatching', 'running') ORDER BY queued_at ASC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn list_queued(&self) -> Result<Vec<AgentRunRow>, DbError> {
        Ok(
            sqlx::query_as("SELECT * FROM agent_runs WHERE status = 'queued' ORDER BY queued_at ASC, id ASC")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    async fn mark_dispatching(&self, id: &str, started_at: i64) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE agent_runs SET status = 'dispatching', started_at = ?, updated_at = ? \
             WHERE id = ? AND status = 'queued'",
        )
        .bind(started_at)
        .bind(started_at)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn mark_running(&self, id: &str, started_at: i64) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE agent_runs SET status = 'running', started_at = COALESCE(started_at, ?), updated_at = ? WHERE id = ?",
        )
        .bind(started_at)
        .bind(started_at)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_effective_model(
        &self,
        id: &str,
        model: &str,
        fallback_used: bool,
        updated_at: i64,
    ) -> Result<(), DbError> {
        sqlx::query("UPDATE agent_runs SET effective_model = ?, fallback_used = ?, updated_at = ? WHERE id = ?")
            .bind(model)
            .bind(fallback_used)
            .bind(updated_at)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn mark_terminal(
        &self,
        id: &str,
        status: &str,
        error_code: Option<&str>,
        finished_at: i64,
    ) -> Result<(), DbError> {
        sqlx::query("UPDATE agent_runs SET status = ?, error_code = ?, finished_at = ?, updated_at = ? WHERE id = ?")
            .bind(status)
            .bind(error_code)
            .bind(finished_at)
            .bind(finished_at)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn cancel_queued(&self, turn_id: &str, user_id: &str, finished_at: i64) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE agent_runs SET status = 'cancelled', error_code = 'CANCELLED', finished_at = ?, updated_at = ? \
             WHERE turn_id = ? AND user_id = ? AND status = 'queued'",
        )
        .bind(finished_at)
        .bind(finished_at)
        .bind(turn_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn fail_interrupted(&self, finished_at: i64) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE agent_runs SET status = 'failed', error_code = 'BACKEND_RESTARTED', finished_at = ?, updated_at = ? \
             WHERE status IN ('dispatching', 'running')",
        )
        .bind(finished_at)
        .bind(finished_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
