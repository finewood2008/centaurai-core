use sqlx::SqlitePool;

use crate::DbError;
use crate::models::{ConversationModelAssignmentRow, ModelRouteMemberRow, ModelRouteRow};
use crate::repository::model_route::{
    CreateModelRouteParams, IModelRouteRepository, RecordModelRouteMetricParams, UpdateModelRouteParams,
    UpsertModelRouteMemberParams,
};

#[derive(Clone, Debug)]
pub struct SqliteModelRouteRepository {
    pool: SqlitePool,
}

impl SqliteModelRouteRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IModelRouteRepository for SqliteModelRouteRepository {
    async fn list_routes(&self) -> Result<Vec<ModelRouteRow>, DbError> {
        Ok(sqlx::query_as("SELECT * FROM model_routes ORDER BY created_at, id")
            .fetch_all(&self.pool)
            .await?)
    }

    async fn find_route(&self, id: &str) -> Result<Option<ModelRouteRow>, DbError> {
        Ok(sqlx::query_as("SELECT * FROM model_routes WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn create_route(&self, params: &CreateModelRouteParams<'_>) -> Result<ModelRouteRow, DbError> {
        let now = aionui_common::now_ms();
        sqlx::query("INSERT INTO model_routes (id, name, enabled, required_capabilities, fallback_after_ms, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(params.id).bind(params.name).bind(params.enabled).bind(params.required_capabilities)
            .bind(params.fallback_after_ms).bind(now).bind(now).execute(&self.pool).await?;
        self.find_route(params.id)
            .await?
            .ok_or_else(|| DbError::Init("model route disappeared after insert".into()))
    }

    async fn update_route(&self, id: &str, params: &UpdateModelRouteParams<'_>) -> Result<ModelRouteRow, DbError> {
        let current = self
            .find_route(id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("Model route '{id}' not found")))?;
        let now = aionui_common::now_ms();
        sqlx::query("UPDATE model_routes SET name = ?, enabled = ?, required_capabilities = ?, fallback_after_ms = ?, updated_at = ? WHERE id = ?")
            .bind(params.name.unwrap_or(&current.name)).bind(params.enabled.unwrap_or(current.enabled))
            .bind(params.required_capabilities.unwrap_or(&current.required_capabilities))
            .bind(params.fallback_after_ms.unwrap_or(current.fallback_after_ms)).bind(now).bind(id)
            .execute(&self.pool).await?;
        self.find_route(id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("Model route '{id}' not found")))
    }

    async fn delete_route(&self, id: &str) -> Result<(), DbError> {
        let result = sqlx::query("DELETE FROM model_routes WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Model route '{id}' not found")));
        }
        Ok(())
    }

    async fn list_members(&self, route_id: &str) -> Result<Vec<ModelRouteMemberRow>, DbError> {
        Ok(sqlx::query_as("SELECT * FROM model_route_members WHERE route_id = ? ORDER BY CASE tier WHEN 'primary' THEN 0 ELSE 1 END, created_at, id")
            .bind(route_id).fetch_all(&self.pool).await?)
    }

    async fn find_member(&self, id: &str) -> Result<Option<ModelRouteMemberRow>, DbError> {
        Ok(sqlx::query_as("SELECT * FROM model_route_members WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn upsert_member(&self, params: &UpsertModelRouteMemberParams<'_>) -> Result<ModelRouteMemberRow, DbError> {
        let now = aionui_common::now_ms();
        sqlx::query("INSERT INTO model_route_members (id, route_id, provider_id, model, tier, weight, max_concurrency, rpm_limit, tpm_limit, enabled, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET provider_id = excluded.provider_id, model = excluded.model, tier = excluded.tier, weight = excluded.weight, max_concurrency = excluded.max_concurrency, rpm_limit = excluded.rpm_limit, tpm_limit = excluded.tpm_limit, enabled = excluded.enabled, disabled_reason = NULL, updated_at = excluded.updated_at")
            .bind(params.id).bind(params.route_id).bind(params.provider_id).bind(params.model).bind(params.tier)
            .bind(params.weight).bind(params.max_concurrency).bind(params.rpm_limit).bind(params.tpm_limit)
            .bind(params.enabled).bind(now).bind(now).execute(&self.pool).await?;
        self.find_member(params.id)
            .await?
            .ok_or_else(|| DbError::Init("model route member disappeared after upsert".into()))
    }

    async fn delete_member(&self, id: &str) -> Result<(), DbError> {
        let result = sqlx::query("DELETE FROM model_route_members WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Model route member '{id}' not found")));
        }
        Ok(())
    }

    async fn find_assignment(&self, conversation_id: &str) -> Result<Option<ConversationModelAssignmentRow>, DbError> {
        Ok(
            sqlx::query_as("SELECT * FROM conversation_model_assignments WHERE conversation_id = ?")
                .bind(conversation_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn upsert_assignment(
        &self,
        conversation_id: &str,
        route_id: &str,
        member: &ModelRouteMemberRow,
        fallback_used: bool,
    ) -> Result<ConversationModelAssignmentRow, DbError> {
        let now = aionui_common::now_ms();
        sqlx::query("INSERT INTO conversation_model_assignments (conversation_id, route_id, member_id, provider_id, model, fallback_used, assigned_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(conversation_id) DO UPDATE SET route_id = excluded.route_id, member_id = excluded.member_id, provider_id = excluded.provider_id, model = excluded.model, fallback_used = excluded.fallback_used, updated_at = excluded.updated_at")
            .bind(conversation_id).bind(route_id).bind(&member.id).bind(&member.provider_id).bind(&member.model)
            .bind(fallback_used).bind(now).bind(now).execute(&self.pool).await?;
        self.find_assignment(conversation_id)
            .await?
            .ok_or_else(|| DbError::Init("model assignment disappeared after upsert".into()))
    }

    async fn try_acquire_member(&self, member_id: &str, now: i64) -> Result<bool, DbError> {
        let result = sqlx::query("UPDATE model_route_members SET active_count = active_count + 1, request_count = CASE WHEN request_window_started_at IS NULL OR request_window_started_at <= ? THEN 1 ELSE request_count + 1 END, token_count = CASE WHEN request_window_started_at IS NULL OR request_window_started_at <= ? THEN 0 ELSE token_count END, request_window_started_at = CASE WHEN request_window_started_at IS NULL OR request_window_started_at <= ? THEN ? ELSE request_window_started_at END, updated_at = ? WHERE id = ? AND enabled = 1 AND disabled_reason IS NULL AND (cooldown_until IS NULL OR cooldown_until <= ?) AND active_count < max_concurrency AND (rpm_limit IS NULL OR request_window_started_at IS NULL OR request_window_started_at <= ? OR request_count < rpm_limit) AND (tpm_limit IS NULL OR request_window_started_at IS NULL OR request_window_started_at <= ? OR token_count < tpm_limit)")
            .bind(now - 60_000).bind(now - 60_000).bind(now - 60_000).bind(now).bind(now).bind(member_id).bind(now).bind(now - 60_000).bind(now - 60_000)
            .execute(&self.pool).await?;
        Ok(result.rows_affected() == 1)
    }

    async fn release_member(&self, member_id: &str, tokens: i64, now: i64) -> Result<(), DbError> {
        sqlx::query("UPDATE model_route_members SET active_count = MAX(active_count - 1, 0), token_count = token_count + MAX(?, 0), consecutive_5xx = 0, consecutive_429 = 0, updated_at = ? WHERE id = ?")
            .bind(tokens).bind(now).bind(member_id).execute(&self.pool).await?;
        Ok(())
    }

    async fn record_member_failure(
        &self,
        member_id: &str,
        http_status: Option<u16>,
        retry_after_ms: Option<u64>,
        balance_exhausted: bool,
        now: i64,
    ) -> Result<ModelRouteMemberRow, DbError> {
        let member = self
            .find_member(member_id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("Model route member '{member_id}' not found")))?;
        let auth_failure = matches!(http_status, Some(401 | 403)) || balance_exhausted;
        let is_5xx = http_status.is_some_and(|status| (500..600).contains(&status));
        let next_5xx = if is_5xx { member.consecutive_5xx + 1 } else { 0 };
        let next_429 = if http_status == Some(429) {
            member.consecutive_429 + 1
        } else {
            0
        };
        let cooldown = if http_status == Some(429) {
            Some(
                now + retry_after_ms.unwrap_or_else(|| (1_u64 << next_429.saturating_sub(1).clamp(0, 6)) * 1_000)
                    as i64,
            )
        } else if next_5xx >= 3 {
            Some(now + 30_000)
        } else {
            member.cooldown_until
        };
        let disabled_reason = if auth_failure {
            Some(if balance_exhausted {
                "balance_exhausted"
            } else {
                "authentication_failed"
            })
        } else {
            None
        };
        sqlx::query("UPDATE model_route_members SET active_count = MAX(active_count - 1, 0), consecutive_5xx = ?, consecutive_429 = ?, cooldown_until = ?, disabled_reason = COALESCE(?, disabled_reason), updated_at = ? WHERE id = ?")
            .bind(next_5xx).bind(next_429).bind(cooldown).bind(disabled_reason).bind(now).bind(member_id).execute(&self.pool).await?;
        self.find_member(member_id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("Model route member '{member_id}' not found")))
    }

    async fn reset_member_breaker(&self, member_id: &str, now: i64) -> Result<ModelRouteMemberRow, DbError> {
        sqlx::query("UPDATE model_route_members SET disabled_reason = NULL, cooldown_until = NULL, consecutive_5xx = 0, consecutive_429 = 0, updated_at = ? WHERE id = ?")
            .bind(now).bind(member_id).execute(&self.pool).await?;
        self.find_member(member_id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("Model route member '{member_id}' not found")))
    }

    async fn record_metric(&self, params: &RecordModelRouteMetricParams<'_>) -> Result<(), DbError> {
        sqlx::query("INSERT INTO model_route_turn_metrics (id, run_id, user_id, route_id, member_id, output_tokens, latency_ms, success, fallback_used, error_code, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(params.id)
            .bind(params.run_id)
            .bind(params.user_id)
            .bind(params.route_id)
            .bind(params.member_id)
            .bind(params.tokens.max(0))
            .bind(params.latency_ms.max(0))
            .bind(params.success)
            .bind(params.fallback_used)
            .bind(params.error_code)
            .bind(params.created_at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
