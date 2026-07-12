use aionui_api_types::{
    BrainDefinition, BrainKind, DecisionActionItemInput, DecisionActionItemResponse, DecisionBrainResponse,
    DecisionBrainState, DecisionCandidateResponse, DecisionEvidenceInput, DecisionEvidenceLocator,
    DecisionEvidenceResponse, DecisionResolutionResponse, DecisionResponse, DecisionSessionResponse, DecisionStatus,
    DecisionToolDefinition, DecisionTurnResponse, RetrievalBundle, RoleDefinition,
};
use chrono::{DateTime, Utc};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};

use crate::DecisionError;

#[derive(Clone)]
pub struct DecisionRepository {
    pool: SqlitePool,
}

#[derive(Debug, FromRow)]
struct DecisionRow {
    id: String,
    question: String,
    status: String,
    conclusion: Option<String>,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug, FromRow)]
struct BrainRow {
    id: String,
    kind: String,
    provider_id: String,
    model: String,
    role_id: String,
    agent_id: Option<String>,
    tool_ids_json: String,
    state: String,
    error_code: Option<String>,
}

#[derive(Debug, FromRow)]
struct SessionRow {
    id: String,
    status: String,
    revision: i64,
    started_at: Option<i64>,
    completed_at: Option<i64>,
    updated_at: i64,
}

#[derive(Debug, FromRow)]
struct TurnRow {
    id: String,
    session_id: String,
    brain_id: Option<String>,
    kind: String,
    content: String,
    status: String,
    attempt: i64,
    error_code: Option<String>,
    provider_id: Option<String>,
    model: Option<String>,
    retrieval_bundle_id: Option<String>,
    evidence_ids_json: String,
    lineage_bundle_ids_json: String,
    input_cloud_egress_allowed: bool,
    resolved_location: String,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug, FromRow)]
struct EvidenceRow {
    id: String,
    source_id: String,
    title: String,
    snippet: String,
    score: f64,
    media_type: String,
    page: Option<i64>,
    chapter: Option<String>,
    timestamp_ms: Option<i64>,
    end_seconds: Option<f64>,
    uri: Option<String>,
    turn_id: Option<String>,
    retrieval_bundle_id: Option<String>,
    lineage_bundle_ids_json: String,
    cloud_egress_allowed: bool,
    origin_location: String,
}

#[derive(Debug, FromRow)]
struct CandidateRow {
    id: String,
    brain_id: Option<String>,
    title: String,
    content: String,
    rank: i64,
    selected: bool,
}

#[derive(Debug, FromRow)]
struct ResolutionRow {
    id: String,
    summary: String,
    status: String,
    partial: bool,
}

#[derive(Debug, FromRow)]
struct ActionItemRow {
    id: String,
    title: String,
    owner: Option<String>,
    due_at: Option<i64>,
    status: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PersistedDecisionContext {
    pub question: String,
    pub roles: Vec<RoleDefinition>,
    pub tools: Vec<DecisionToolDefinition>,
    pub brains: Vec<DecisionBrainResponse>,
    pub session_id: String,
    pub interjections: Vec<String>,
    pub evidence: Vec<PersistedEvidence>,
    pub retrieval: Option<PersistedRetrievalBundle>,
}

#[derive(Debug, Clone)]
pub(crate) struct PersistedRetrievalBundle {
    pub id: String,
    pub bundle: RetrievalBundle,
}

#[derive(Debug, Clone)]
pub(crate) struct PersistedEvidence {
    pub id: String,
    pub evidence: DecisionEvidenceInput,
    pub retrieval_bundle_id: Option<String>,
    pub lineage_bundle_ids: Vec<String>,
    pub cloud_egress_allowed: bool,
    pub origin_location: String,
}

pub(crate) struct EvidenceLineage<'a> {
    pub turn_id: Option<&'a str>,
    pub retrieval_bundle_id: Option<&'a str>,
    pub lineage_bundle_ids: &'a [String],
    pub cloud_egress_allowed: bool,
    pub origin_location: &'a str,
}

pub(crate) struct NewDecision<'a> {
    pub user_id: &'a str,
    pub question: &'a str,
    pub brain_count: usize,
    pub knowledge: &'a serde_json::Value,
    pub roles: &'a [RoleDefinition],
    pub tools: &'a [DecisionToolDefinition],
    pub brains: &'a [BrainDefinition],
    pub user_evidence: &'a [DecisionEvidenceInput],
    pub retrieval_evidence: &'a [DecisionEvidenceInput],
    pub retrieval: Option<&'a RetrievalBundle>,
    pub idempotency_key: Option<&'a str>,
    pub request_fingerprint: Option<&'a str>,
}

pub(crate) struct CompletedOpinion<'a> {
    pub decision_id: &'a str,
    pub brain: &'a DecisionBrainResponse,
    pub turn_id: &'a str,
    pub content: &'a str,
    pub evidence: &'a [DecisionEvidenceInput],
    pub retrieval_bundle_id: Option<&'a str>,
    pub lineage_bundle_ids: &'a [String],
    pub cloud_egress_allowed: bool,
    pub origin_location: &'a str,
    pub rank: i64,
    pub fallback_provider_id: Option<&'a str>,
}

pub(crate) struct EgressAudit<'a> {
    pub decision_id: &'a str,
    pub session_id: &'a str,
    pub brain_id: &'a str,
    pub turn_id: &'a str,
    pub provider_id: &'a str,
    pub model: &'a str,
    pub location: &'a str,
    pub retrieval: Option<&'a PersistedRetrievalBundle>,
    pub knowledge_egress_allowed: bool,
    pub hit_count: usize,
    pub evidence_ids: &'a [String],
    pub lineage_bundle_ids: &'a [String],
}

impl DecisionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub(crate) async fn create(&self, input: NewDecision<'_>) -> Result<DecisionResponse, DecisionError> {
        let id = aionui_common::generate_prefixed_id("decision");
        let now = aionui_common::now_ms();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO decisions (id, user_id, question, status, brain_count, knowledge_json, roles_json, \
             tools_json, created_at, updated_at) VALUES (?, ?, ?, 'draft', ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(input.user_id)
        .bind(input.question)
        .bind(input.brain_count as i64)
        .bind(serde_json::to_string(input.knowledge)?)
        .bind(serde_json::to_string(input.roles)?)
        .bind(serde_json::to_string(input.tools)?)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        for (ordinal, brain) in input.brains.iter().enumerate() {
            let brain_id = brain
                .id
                .clone()
                .unwrap_or_else(|| aionui_common::generate_prefixed_id("brain"));
            sqlx::query(
                "INSERT INTO decision_brains (id, decision_id, kind, provider_id, model, role_id, agent_id, \
                 tool_ids_json, state, ordinal, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?)",
            )
            .bind(brain_id)
            .bind(&id)
            .bind(brain_kind_to_str(brain.kind))
            .bind(&brain.provider_id)
            .bind(&brain.model)
            .bind(&brain.role_id)
            .bind(&brain.agent_id)
            .bind(serde_json::to_string(&brain.tool_ids)?)
            .bind(ordinal as i64)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        insert_evidence_tx(
            &mut tx,
            &id,
            input.user_evidence,
            EvidenceLineage {
                turn_id: None,
                retrieval_bundle_id: None,
                lineage_bundle_ids: &[],
                cloud_egress_allowed: true,
                origin_location: "user",
            },
        )
        .await?;
        if let Some(bundle) = input.retrieval {
            let bundle_id = insert_retrieval_bundle_tx(&mut tx, &id, bundle).await?;
            insert_evidence_tx(
                &mut tx,
                &id,
                input.retrieval_evidence,
                EvidenceLineage {
                    turn_id: None,
                    retrieval_bundle_id: Some(&bundle_id),
                    lineage_bundle_ids: std::slice::from_ref(&bundle_id),
                    cloud_egress_allowed: bundle.cloud_authorized,
                    origin_location: "knowledge",
                },
            )
            .await?;
        }
        if let (Some(operation_key), Some(request_fingerprint)) = (input.idempotency_key, input.request_fingerprint) {
            sqlx::query(
                "INSERT INTO idempotency_records (id, user_id, operation_scope, operation_key, \
                 request_fingerprint, resource_id, created_at) VALUES (?, ?, 'decision.create', ?, ?, ?, ?)",
            )
            .bind(aionui_common::generate_prefixed_id("idem"))
            .bind(input.user_id)
            .bind(operation_key)
            .bind(request_fingerprint)
            .bind(&id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.get(input.user_id, &id).await
    }

    pub(crate) async fn idempotent_decision(
        &self,
        user_id: &str,
        operation_key: &str,
        request_fingerprint: &str,
    ) -> Result<Option<DecisionResponse>, DecisionError> {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT request_fingerprint, resource_id FROM idempotency_records \
             WHERE user_id = ? AND operation_scope = 'decision.create' AND operation_key = ?",
        )
        .bind(user_id)
        .bind(operation_key)
        .fetch_optional(&self.pool)
        .await?;
        let Some((stored_fingerprint, resource_id)) = row else {
            return Ok(None);
        };
        if stored_fingerprint != request_fingerprint {
            return Err(DecisionError::Conflict(
                "idempotency key was already used for a different decision request".into(),
            ));
        }
        self.get(user_id, &resource_id).await.map(Some)
    }

    pub async fn list(&self, user_id: &str) -> Result<Vec<DecisionResponse>, DecisionError> {
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT id FROM decisions WHERE user_id = ? ORDER BY updated_at DESC, created_at DESC")
                .bind(user_id)
                .fetch_all(&self.pool)
                .await?;
        let mut decisions = Vec::with_capacity(ids.len());
        for id in ids {
            decisions.push(self.get(user_id, &id).await?);
        }
        Ok(decisions)
    }

    pub async fn get(&self, user_id: &str, id: &str) -> Result<DecisionResponse, DecisionError> {
        let row = sqlx::query_as::<_, DecisionRow>(
            "SELECT id, question, status, conclusion, created_at, updated_at \
             FROM decisions WHERE id = ? AND user_id = ?",
        )
        .bind(id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(DecisionError::NotFound)?;
        let brains = self.brains(id).await?;
        let session = self.latest_session(id).await?;
        let turns = self.turns(id).await?;
        let evidence = self.evidence(id).await?;
        let candidates = self.candidates(id).await?;
        let resolution = self.resolution(id).await?;
        Ok(DecisionResponse {
            id: row.id,
            question: row.question,
            status: parse_status(&row.status)?,
            brains,
            conclusion: row.conclusion,
            created_at: timestamp(row.created_at),
            updated_at: timestamp(row.updated_at),
            session,
            turns,
            evidence,
            candidates,
            resolution,
        })
    }

    pub async fn update_question(&self, user_id: &str, id: &str, question: &str) -> Result<(), DecisionError> {
        let result = sqlx::query(
            "UPDATE decisions SET question = ?, updated_at = ? WHERE id = ? AND user_id = ? AND status = 'draft'",
        )
        .bind(question)
        .bind(aionui_common::now_ms())
        .bind(id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            self.get(user_id, id).await?;
            return Err(DecisionError::Conflict(
                "question can only be changed while the decision is a draft".into(),
            ));
        }
        Ok(())
    }

    pub async fn knowledge_context(
        &self,
        user_id: &str,
        id: &str,
    ) -> Result<(String, serde_json::Value), DecisionError> {
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT question, knowledge_json FROM decisions WHERE id = ? AND user_id = ?")
                .bind(id)
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?;
        let (question, policy) = row.ok_or(DecisionError::NotFound)?;
        Ok((question, serde_json::from_str(&policy)?))
    }

    pub async fn begin_session(&self, user_id: &str, id: &str) -> Result<String, DecisionError> {
        let decision = self.get(user_id, id).await?;
        if matches!(decision.status, DecisionStatus::Running | DecisionStatus::Cancelled)
            || (decision.status == DecisionStatus::Completed
                && decision
                    .brains
                    .iter()
                    .all(|brain| brain.state == DecisionBrainState::Completed))
        {
            return Err(DecisionError::Conflict(format!(
                "cannot start decision from {:?}",
                decision.status
            )));
        }
        if decision.brains.is_empty() {
            return Err(DecisionError::Conflict("no runnable brains are configured".into()));
        }
        let now = aionui_common::now_ms();
        let session_id = aionui_common::generate_prefixed_id("decision_session");
        let mut tx = self.pool.begin().await?;
        let revision: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(revision), 0) + 1 FROM decision_sessions WHERE decision_id = ?")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        sqlx::query(
            "INSERT INTO decision_sessions (id, decision_id, status, revision, current_prompt, started_at, updated_at) \
             VALUES (?, ?, 'running', ?, ?, ?, ?)",
        )
        .bind(&session_id)
        .bind(id)
        .bind(revision)
        .bind(&decision.question)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE decisions SET status = 'running', updated_at = ? WHERE id = ? AND user_id = ?")
            .bind(now)
            .bind(id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE decision_brains SET state = 'pending', error_code = NULL, updated_at = ? \
             WHERE decision_id = ? AND state IN ('failed', 'cancelled')",
        )
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(session_id)
    }

    pub(crate) async fn load_context(
        &self,
        user_id: &str,
        id: &str,
        session_id: &str,
    ) -> Result<PersistedDecisionContext, DecisionError> {
        let decision = self.get(user_id, id).await?;
        let stored: (String, String) =
            sqlx::query_as("SELECT roles_json, tools_json FROM decisions WHERE id = ? AND user_id = ?")
                .bind(id)
                .bind(user_id)
                .fetch_one(&self.pool)
                .await?;
        let interjections: Vec<String> = sqlx::query_scalar(
            "SELECT content FROM decision_turns WHERE decision_id = ? AND kind = 'interjection' ORDER BY created_at",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        let evidence = self.persisted_evidence(id).await?;
        let retrieval = self.latest_retrieval_bundle(id).await?;
        Ok(PersistedDecisionContext {
            question: decision.question,
            roles: serde_json::from_str(&stored.0)?,
            tools: serde_json::from_str(&stored.1)?,
            brains: decision.brains,
            session_id: session_id.to_owned(),
            interjections,
            evidence,
            retrieval,
        })
    }

    pub(crate) async fn store_retrieval_evidence(
        &self,
        user_id: &str,
        decision_id: &str,
        bundle: &RetrievalBundle,
        evidence: &[DecisionEvidenceInput],
    ) -> Result<String, DecisionError> {
        // Ownership check prevents a caller from attaching a bundle to
        // another user's decision even if an id is guessed.
        self.get(user_id, decision_id).await?;
        let mut tx = self.pool.begin().await?;
        let id = insert_retrieval_bundle_tx(&mut tx, decision_id, bundle).await?;
        insert_evidence_tx(
            &mut tx,
            decision_id,
            evidence,
            EvidenceLineage {
                turn_id: None,
                retrieval_bundle_id: Some(&id),
                lineage_bundle_ids: std::slice::from_ref(&id),
                cloud_egress_allowed: bundle.cloud_authorized,
                origin_location: "knowledge",
            },
        )
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    async fn latest_retrieval_bundle(
        &self,
        decision_id: &str,
    ) -> Result<Option<PersistedRetrievalBundle>, DecisionError> {
        let row: Option<(String, String, String, i64, bool, String)> = sqlx::query_as(
            "SELECT id, query, hits_json, token_budget, cloud_authorized, space_ids_json \
             FROM decision_retrieval_bundles WHERE decision_id = ? ORDER BY created_at DESC, rowid DESC LIMIT 1",
        )
        .bind(decision_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(
            |(id, query, hits_json, token_budget, cloud_authorized, space_ids_json)| {
                let token_budget = u32::try_from(token_budget)
                    .map_err(|_| DecisionError::Internal("invalid persisted retrieval token budget".into()))?;
                Ok(PersistedRetrievalBundle {
                    id,
                    bundle: RetrievalBundle {
                        query,
                        hits: serde_json::from_str(&hits_json)?,
                        token_budget,
                        cloud_authorized,
                        space_ids: serde_json::from_str(&space_ids_json)?,
                    },
                })
            },
        )
        .transpose()
    }

    pub(crate) async fn record_egress(&self, audit: EgressAudit<'_>) -> Result<(), DecisionError> {
        let (retrieval_bundle_id, space_ids_json) = match audit.retrieval {
            Some(retrieval) => (
                Some(retrieval.id.as_str()),
                serde_json::to_string(&retrieval.bundle.space_ids)?,
            ),
            None => (None, "[]".to_owned()),
        };
        sqlx::query(
            "INSERT INTO decision_egress_audit (id, decision_id, session_id, brain_id, turn_id, provider_id, \
             model, location, retrieval_bundle_id, knowledge_egress_allowed, hit_count, evidence_ids_json, \
             lineage_bundle_ids_json, space_ids_json, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(aionui_common::generate_prefixed_id("egress"))
        .bind(audit.decision_id)
        .bind(audit.session_id)
        .bind(audit.brain_id)
        .bind(audit.turn_id)
        .bind(audit.provider_id)
        .bind(audit.model)
        .bind(audit.location)
        .bind(retrieval_bundle_id)
        .bind(audit.knowledge_egress_allowed)
        .bind(i64::try_from(audit.hit_count).unwrap_or(i64::MAX))
        .bind(serde_json::to_string(audit.evidence_ids)?)
        .bind(serde_json::to_string(audit.lineage_bundle_ids)?)
        .bind(space_ids_json)
        .bind(aionui_common::now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_brain_running(&self, brain_id: &str) -> Result<(), DecisionError> {
        sqlx::query("UPDATE decision_brains SET state = 'running', error_code = NULL, updated_at = ? WHERE id = ?")
            .bind(aionui_common::now_ms())
            .bind(brain_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn insert_turn_attempt(
        &self,
        decision_id: &str,
        session_id: &str,
        brain_id: &str,
        attempt: i64,
        provider_id: &str,
        model: &str,
        retrieval_bundle_id: Option<&str>,
        evidence_ids: &[String],
        lineage_bundle_ids: &[String],
        input_cloud_egress_allowed: bool,
        resolved_location: &str,
    ) -> Result<String, DecisionError> {
        let id = aionui_common::generate_prefixed_id("decision_turn");
        let now = aionui_common::now_ms();
        sqlx::query(
            "INSERT INTO decision_turns (id, decision_id, session_id, brain_id, kind, status, attempt, provider_id, \
             model, retrieval_bundle_id, evidence_ids_json, lineage_bundle_ids_json, input_cloud_egress_allowed, \
             resolved_location, created_at, updated_at) \
             VALUES (?, ?, ?, ?, 'opinion', 'running', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(decision_id)
        .bind(session_id)
        .bind(brain_id)
        .bind(attempt)
        .bind(provider_id)
        .bind(model)
        .bind(retrieval_bundle_id)
        .bind(serde_json::to_string(evidence_ids)?)
        .bind(serde_json::to_string(lineage_bundle_ids)?)
        .bind(input_cloud_egress_allowed)
        .bind(resolved_location)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn fail_turn(&self, turn_id: &str, code: &str) -> Result<(), DecisionError> {
        sqlx::query("UPDATE decision_turns SET status = 'failed', error_code = ?, updated_at = ? WHERE id = ?")
            .bind(code)
            .bind(aionui_common::now_ms())
            .bind(turn_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_turn_execution_context(
        &self,
        turn_id: &str,
        retrieval_bundle_id: Option<&str>,
        evidence_ids: &[String],
        lineage_bundle_ids: &[String],
        input_cloud_egress_allowed: bool,
        resolved_location: &str,
    ) -> Result<(), DecisionError> {
        sqlx::query(
            "UPDATE decision_turns SET retrieval_bundle_id = ?, evidence_ids_json = ?, \
             lineage_bundle_ids_json = ?, input_cloud_egress_allowed = ?, resolved_location = ?, updated_at = ? \
             WHERE id = ?",
        )
        .bind(retrieval_bundle_id)
        .bind(serde_json::to_string(evidence_ids)?)
        .bind(serde_json::to_string(lineage_bundle_ids)?)
        .bind(input_cloud_egress_allowed)
        .bind(resolved_location)
        .bind(aionui_common::now_ms())
        .bind(turn_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn complete_opinion(&self, opinion: CompletedOpinion<'_>) -> Result<(), DecisionError> {
        let now = aionui_common::now_ms();
        let candidate_id = aionui_common::generate_prefixed_id("candidate");
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE decision_turns SET content = ?, status = 'completed', updated_at = ? WHERE id = ?")
            .bind(opinion.content)
            .bind(now)
            .bind(opinion.turn_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE decision_brains SET state = 'completed', error_code = NULL, fallback_provider_id = ?, \
             updated_at = ? WHERE id = ?",
        )
        .bind(opinion.fallback_provider_id)
        .bind(now)
        .bind(&opinion.brain.id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO decision_candidates (id, decision_id, brain_id, title, content, rank, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(candidate_id)
        .bind(opinion.decision_id)
        .bind(&opinion.brain.id)
        .bind(format!("{} · {}", opinion.brain.role_id, opinion.brain.model))
        .bind(opinion.content)
        .bind(opinion.rank)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        insert_evidence_tx(
            &mut tx,
            opinion.decision_id,
            opinion.evidence,
            EvidenceLineage {
                turn_id: Some(opinion.turn_id),
                retrieval_bundle_id: opinion.retrieval_bundle_id,
                lineage_bundle_ids: opinion.lineage_bundle_ids,
                cloud_egress_allowed: opinion.cloud_egress_allowed,
                origin_location: opinion.origin_location,
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn mark_brain_failed(&self, brain_id: &str, code: &str) -> Result<(), DecisionError> {
        sqlx::query("UPDATE decision_brains SET state = 'failed', error_code = ?, updated_at = ? WHERE id = ?")
            .bind(code)
            .bind(aionui_common::now_ms())
            .bind(brain_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn reset_brain_pending(&self, brain_id: &str) -> Result<(), DecisionError> {
        sqlx::query("UPDATE decision_brains SET state = 'pending', error_code = NULL, updated_at = ? WHERE id = ?")
            .bind(aionui_common::now_ms())
            .bind(brain_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_brain_cancelled(&self, brain_id: &str) -> Result<(), DecisionError> {
        sqlx::query("UPDATE decision_brains SET state = 'cancelled', error_code = NULL, updated_at = ? WHERE id = ?")
            .bind(aionui_common::now_ms())
            .bind(brain_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn finish(
        &self,
        user_id: &str,
        id: &str,
        session_id: &str,
        summary: &str,
        partial: bool,
    ) -> Result<(), DecisionError> {
        let now = aionui_common::now_ms();
        let resolution_id = aionui_common::generate_prefixed_id("resolution");
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO decision_resolutions (id, decision_id, summary, status, partial, created_at, updated_at) \
             VALUES (?, ?, ?, 'completed', ?, ?, ?) \
             ON CONFLICT(decision_id) DO UPDATE SET summary = excluded.summary, status = excluded.status, \
             partial = excluded.partial, updated_at = excluded.updated_at",
        )
        .bind(&resolution_id)
        .bind(id)
        .bind(summary)
        .bind(partial)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE decisions SET status = 'completed', conclusion = ?, updated_at = ? WHERE id = ? AND user_id = ?",
        )
        .bind(summary)
        .bind(now)
        .bind(id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE decision_sessions SET status = 'completed', completed_at = ?, updated_at = ? WHERE id = ?")
            .bind(now)
            .bind(now)
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn fail_session(&self, user_id: &str, id: &str, session_id: &str) -> Result<(), DecisionError> {
        let now = aionui_common::now_ms();
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE decisions SET status = 'failed', updated_at = ? WHERE id = ? AND user_id = ?")
            .bind(now)
            .bind(id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE decision_sessions SET status = 'failed', completed_at = ?, updated_at = ? WHERE id = ?")
            .bind(now)
            .bind(now)
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_control_state(
        &self,
        user_id: &str,
        id: &str,
        status: DecisionStatus,
    ) -> Result<(), DecisionError> {
        let value = status_to_str(status);
        let now = aionui_common::now_ms();
        let result = sqlx::query("UPDATE decisions SET status = ?, updated_at = ? WHERE id = ? AND user_id = ?")
            .bind(value)
            .bind(now)
            .bind(id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DecisionError::NotFound);
        }
        sqlx::query(
            "UPDATE decision_sessions SET status = ?, completed_at = CASE WHEN ? = 'cancelled' THEN ? ELSE completed_at END, \
             updated_at = ? WHERE id = (SELECT id FROM decision_sessions WHERE decision_id = ? ORDER BY revision DESC LIMIT 1)",
        )
        .bind(value)
        .bind(value)
        .bind(now)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        match status {
            DecisionStatus::Paused => {
                sqlx::query(
                    "UPDATE decision_brains SET state = 'pending', error_code = NULL, updated_at = ? \
                     WHERE decision_id = ? AND state = 'running'",
                )
                .bind(now)
                .bind(id)
                .execute(&self.pool)
                .await?;
            }
            DecisionStatus::Cancelled => {
                sqlx::query(
                    "UPDATE decision_brains SET state = 'cancelled', error_code = NULL, updated_at = ? \
                     WHERE decision_id = ? AND state != 'completed'",
                )
                .bind(now)
                .bind(id)
                .execute(&self.pool)
                .await?;
            }
            _ => {}
        }
        Ok(())
    }

    pub async fn add_interjection(&self, user_id: &str, id: &str, content: &str) -> Result<(), DecisionError> {
        let decision = self.get(user_id, id).await?;
        if matches!(decision.status, DecisionStatus::Completed | DecisionStatus::Cancelled) {
            return Err(DecisionError::Conflict(
                "a terminal decision cannot be interjected".into(),
            ));
        }
        let session_id = decision
            .session
            .map(|session| session.id)
            .ok_or_else(|| DecisionError::Conflict("start the decision before interjecting".into()))?;
        let now = aionui_common::now_ms();
        sqlx::query(
            "INSERT INTO decision_turns (id, decision_id, session_id, kind, content, status, attempt, created_at, updated_at) \
             VALUES (?, ?, ?, 'interjection', ?, 'completed', 1, ?, ?)",
        )
        .bind(aionui_common::generate_prefixed_id("decision_turn"))
        .bind(id)
        .bind(session_id)
        .bind(content)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn add_evidence(
        &self,
        user_id: &str,
        id: &str,
        evidence: &[DecisionEvidenceInput],
    ) -> Result<Vec<DecisionEvidenceResponse>, DecisionError> {
        self.get(user_id, id).await?;
        let mut tx = self.pool.begin().await?;
        insert_evidence_tx(
            &mut tx,
            id,
            evidence,
            EvidenceLineage {
                turn_id: None,
                retrieval_bundle_id: None,
                lineage_bundle_ids: &[],
                cloud_egress_allowed: true,
                origin_location: "user",
            },
        )
        .await?;
        tx.commit().await?;
        self.evidence(id).await
    }

    pub async fn select_candidate(
        &self,
        user_id: &str,
        id: &str,
        candidate_id: &str,
        action_items: &[DecisionActionItemInput],
    ) -> Result<(), DecisionError> {
        self.get(user_id, id).await?;
        let mut tx = self.pool.begin().await?;
        let exists: bool =
            sqlx::query_scalar("SELECT COUNT(*) > 0 FROM decision_candidates WHERE id = ? AND decision_id = ?")
                .bind(candidate_id)
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        if !exists {
            return Err(DecisionError::InvalidRequest(
                "candidate does not belong to this decision".into(),
            ));
        }
        sqlx::query(
            "UPDATE decision_candidates SET selected = CASE WHEN id = ? THEN 1 ELSE 0 END WHERE decision_id = ?",
        )
        .bind(candidate_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE decisions SET selected_candidate_id = ?, updated_at = ? WHERE id = ?")
            .bind(candidate_id)
            .bind(aionui_common::now_ms())
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let resolution_id: String = sqlx::query_scalar("SELECT id FROM decision_resolutions WHERE decision_id = ?")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        for item in action_items {
            let due_at = item
                .due_at
                .as_deref()
                .map(DateTime::parse_from_rfc3339)
                .transpose()
                .map_err(|_| DecisionError::InvalidRequest("action item due_at must be RFC 3339".into()))?
                .map(|value| value.timestamp_millis());
            sqlx::query(
                "INSERT INTO decision_action_items (id, resolution_id, title, owner, due_at, status, created_at, updated_at) \
                 SELECT ?, ?, ?, ?, ?, 'open', ?, ? WHERE NOT EXISTS (SELECT 1 FROM decision_action_items \
                 WHERE resolution_id = ? AND title = ? AND COALESCE(owner, '') = COALESCE(?, '') \
                 AND COALESCE(due_at, -1) = COALESCE(?, -1))",
            )
            .bind(aionui_common::generate_prefixed_id("action_item"))
            .bind(&resolution_id)
            .bind(item.title.trim())
            .bind(item.owner.as_deref().map(str::trim).filter(|value| !value.is_empty()))
            .bind(due_at)
            .bind(aionui_common::now_ms())
            .bind(aionui_common::now_ms())
            .bind(&resolution_id)
            .bind(item.title.trim())
            .bind(item.owner.as_deref().map(str::trim).filter(|value| !value.is_empty()))
            .bind(due_at)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn recover_interrupted(&self) -> Result<u64, DecisionError> {
        let now = aionui_common::now_ms();
        let result = sqlx::query("UPDATE decisions SET status = 'paused', updated_at = ? WHERE status = 'running'")
            .bind(now)
            .execute(&self.pool)
            .await?;
        sqlx::query("UPDATE decision_sessions SET status = 'paused', updated_at = ? WHERE status = 'running'")
            .bind(now)
            .execute(&self.pool)
            .await?;
        sqlx::query("UPDATE decision_brains SET state = 'pending', updated_at = ? WHERE state = 'running'")
            .bind(now)
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "UPDATE decision_turns SET status = 'failed', error_code = 'SESSION_INTERRUPTED', updated_at = ? \
             WHERE status = 'running'",
        )
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    async fn brains(&self, id: &str) -> Result<Vec<DecisionBrainResponse>, DecisionError> {
        let rows = sqlx::query_as::<_, BrainRow>(
            "SELECT id, kind, provider_id, model, role_id, agent_id, tool_ids_json, state, error_code \
             FROM decision_brains WHERE decision_id = ? ORDER BY ordinal",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(DecisionBrainResponse {
                    id: row.id,
                    provider_id: row.provider_id,
                    model: row.model,
                    role_id: row.role_id,
                    state: parse_brain_state(&row.state)?,
                    error_code: row.error_code,
                    tool_ids: serde_json::from_str(&row.tool_ids_json)?,
                    kind: parse_brain_kind(&row.kind)?,
                    agent_id: row.agent_id,
                })
            })
            .collect()
    }

    async fn latest_session(&self, id: &str) -> Result<Option<DecisionSessionResponse>, DecisionError> {
        let row = sqlx::query_as::<_, SessionRow>(
            "SELECT id, status, revision, started_at, completed_at, updated_at FROM decision_sessions \
             WHERE decision_id = ? ORDER BY revision DESC LIMIT 1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(DecisionSessionResponse {
                id: row.id,
                status: parse_status(&row.status)?,
                revision: row.revision,
                started_at: row.started_at.map(timestamp),
                completed_at: row.completed_at.map(timestamp),
                updated_at: timestamp(row.updated_at),
            })
        })
        .transpose()
    }

    async fn turns(&self, id: &str) -> Result<Vec<DecisionTurnResponse>, DecisionError> {
        let rows = sqlx::query_as::<_, TurnRow>(
            "SELECT id, session_id, brain_id, kind, content, status, attempt, error_code, provider_id, model, \
             retrieval_bundle_id, evidence_ids_json, lineage_bundle_ids_json, input_cloud_egress_allowed, \
             resolved_location, created_at, updated_at \
             FROM decision_turns WHERE decision_id = ? ORDER BY created_at, id",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| DecisionTurnResponse {
                id: row.id,
                session_id: row.session_id,
                brain_id: row.brain_id,
                kind: row.kind,
                content: row.content,
                status: row.status,
                attempt: row.attempt,
                error_code: row.error_code,
                provider_id: row.provider_id,
                model: row.model,
                retrieval_bundle_id: row.retrieval_bundle_id,
                evidence_ids: serde_json::from_str(&row.evidence_ids_json).unwrap_or_default(),
                lineage_bundle_ids: serde_json::from_str(&row.lineage_bundle_ids_json).unwrap_or_default(),
                input_cloud_egress_allowed: row.input_cloud_egress_allowed,
                resolved_location: row.resolved_location,
                created_at: timestamp(row.created_at),
                updated_at: timestamp(row.updated_at),
            })
            .collect())
    }

    async fn evidence(&self, id: &str) -> Result<Vec<DecisionEvidenceResponse>, DecisionError> {
        let rows = sqlx::query_as::<_, EvidenceRow>(
            "SELECT id, source_id, title, snippet, score, media_type, page, chapter, timestamp_ms, end_seconds, uri, turn_id, \
             retrieval_bundle_id, lineage_bundle_ids_json, cloud_egress_allowed, origin_location \
             FROM decision_evidence WHERE decision_id = ? ORDER BY created_at, id",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| DecisionEvidenceResponse {
                id: row.id,
                source_id: row.source_id,
                title: row.title,
                snippet: row.snippet,
                score: row.score,
                media_type: row.media_type,
                page: row.page,
                chapter: row.chapter.clone(),
                timestamp_ms: row.timestamp_ms,
                turn_id: row.turn_id,
                retrieval_bundle_id: row.retrieval_bundle_id,
                lineage_bundle_ids: serde_json::from_str(&row.lineage_bundle_ids_json).unwrap_or_default(),
                cloud_egress_allowed: row.cloud_egress_allowed,
                origin_location: row.origin_location,
                locator: DecisionEvidenceLocator {
                    page: row.page,
                    chapter: row.chapter,
                    start_seconds: row.timestamp_ms.map(|value| value as f64 / 1_000.0),
                    end_seconds: row.end_seconds,
                    uri: row.uri,
                },
            })
            .collect())
    }

    async fn persisted_evidence(&self, id: &str) -> Result<Vec<PersistedEvidence>, DecisionError> {
        let rows = sqlx::query_as::<_, EvidenceRow>(
            "SELECT id, source_id, title, snippet, score, media_type, page, chapter, timestamp_ms, end_seconds, uri, turn_id, \
             retrieval_bundle_id, lineage_bundle_ids_json, cloud_egress_allowed, origin_location \
             FROM decision_evidence WHERE decision_id = ? ORDER BY created_at, id",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(PersistedEvidence {
                    id: row.id,
                    evidence: DecisionEvidenceInput {
                        source_id: row.source_id,
                        title: row.title,
                        snippet: row.snippet,
                        score: row.score,
                        media_type: row.media_type,
                        page: row.page,
                        chapter: row.chapter,
                        timestamp_ms: row.timestamp_ms,
                        end_seconds: row.end_seconds,
                        uri: row.uri,
                    },
                    retrieval_bundle_id: row.retrieval_bundle_id,
                    lineage_bundle_ids: serde_json::from_str(&row.lineage_bundle_ids_json)?,
                    cloud_egress_allowed: row.cloud_egress_allowed,
                    origin_location: row.origin_location,
                })
            })
            .collect()
    }

    async fn candidates(&self, id: &str) -> Result<Vec<DecisionCandidateResponse>, DecisionError> {
        let rows = sqlx::query_as::<_, CandidateRow>(
            "SELECT id, brain_id, title, content, rank, selected FROM decision_candidates \
             WHERE decision_id = ? ORDER BY rank, created_at, id",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| DecisionCandidateResponse {
                id: row.id,
                brain_id: row.brain_id,
                title: row.title,
                content: row.content,
                rank: row.rank,
                selected: row.selected,
            })
            .collect())
    }

    async fn resolution(&self, id: &str) -> Result<Option<DecisionResolutionResponse>, DecisionError> {
        let row = sqlx::query_as::<_, ResolutionRow>(
            "SELECT id, summary, status, partial FROM decision_resolutions WHERE decision_id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let items = sqlx::query_as::<_, ActionItemRow>(
            "SELECT id, title, owner, due_at, status FROM decision_action_items WHERE resolution_id = ? \
             ORDER BY created_at, id",
        )
        .bind(&row.id)
        .fetch_all(&self.pool)
        .await?;
        Ok(Some(DecisionResolutionResponse {
            id: row.id,
            summary: row.summary,
            status: row.status,
            partial: row.partial,
            action_items: items
                .into_iter()
                .map(|item| DecisionActionItemResponse {
                    id: item.id,
                    title: item.title,
                    owner: item.owner,
                    due_at: item.due_at.map(timestamp),
                    status: item.status,
                })
                .collect(),
        }))
    }
}

async fn insert_retrieval_bundle_tx(
    tx: &mut Transaction<'_, Sqlite>,
    decision_id: &str,
    bundle: &RetrievalBundle,
) -> Result<String, DecisionError> {
    let id = aionui_common::generate_prefixed_id("retrieval");
    sqlx::query(
        "INSERT INTO decision_retrieval_bundles (id, decision_id, query, hits_json, token_budget, \
         cloud_authorized, space_ids_json, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(decision_id)
    .bind(&bundle.query)
    .bind(serde_json::to_string(&bundle.hits)?)
    .bind(i64::from(bundle.token_budget))
    .bind(bundle.cloud_authorized)
    .bind(serde_json::to_string(&bundle.space_ids)?)
    .bind(aionui_common::now_ms())
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

async fn insert_evidence_tx(
    tx: &mut Transaction<'_, Sqlite>,
    decision_id: &str,
    evidence: &[DecisionEvidenceInput],
    lineage: EvidenceLineage<'_>,
) -> Result<(), DecisionError> {
    let now = aionui_common::now_ms();
    let lineage_bundle_ids_json = serde_json::to_string(lineage.lineage_bundle_ids)?;
    for hit in evidence {
        sqlx::query(
            "INSERT INTO decision_evidence (id, decision_id, turn_id, source_id, title, snippet, score, media_type, \
             page, chapter, timestamp_ms, end_seconds, uri, created_at, retrieval_bundle_id, lineage_bundle_ids_json, \
             cloud_egress_allowed, origin_location) \
             SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ? \
             WHERE NOT EXISTS (SELECT 1 FROM decision_evidence WHERE decision_id = ? AND source_id = ? \
             AND snippet = ? AND COALESCE(page, -1) = COALESCE(?, -1) \
             AND COALESCE(chapter, '') = COALESCE(?, '') \
             AND COALESCE(timestamp_ms, -1) = COALESCE(?, -1) \
             AND COALESCE(end_seconds, -1.0) = COALESCE(?, -1.0) \
             AND COALESCE(uri, '') = COALESCE(?, '') \
             AND COALESCE(retrieval_bundle_id, '') = COALESCE(?, '') \
             AND COALESCE(turn_id, '') = COALESCE(?, ''))",
        )
        .bind(aionui_common::generate_prefixed_id("evidence"))
        .bind(decision_id)
        .bind(lineage.turn_id)
        .bind(&hit.source_id)
        .bind(&hit.title)
        .bind(&hit.snippet)
        .bind(hit.score)
        .bind(&hit.media_type)
        .bind(hit.page)
        .bind(&hit.chapter)
        .bind(hit.timestamp_ms)
        .bind(hit.end_seconds)
        .bind(&hit.uri)
        .bind(now)
        .bind(lineage.retrieval_bundle_id)
        .bind(&lineage_bundle_ids_json)
        .bind(lineage.cloud_egress_allowed)
        .bind(lineage.origin_location)
        .bind(decision_id)
        .bind(&hit.source_id)
        .bind(&hit.snippet)
        .bind(hit.page)
        .bind(&hit.chapter)
        .bind(hit.timestamp_ms)
        .bind(hit.end_seconds)
        .bind(&hit.uri)
        .bind(lineage.retrieval_bundle_id)
        .bind(lineage.turn_id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

pub(crate) fn status_to_str(status: DecisionStatus) -> &'static str {
    match status {
        DecisionStatus::Draft => "draft",
        DecisionStatus::Running => "running",
        DecisionStatus::Paused => "paused",
        DecisionStatus::Completed => "completed",
        DecisionStatus::Cancelled => "cancelled",
        DecisionStatus::Failed => "failed",
    }
}

fn parse_status(value: &str) -> Result<DecisionStatus, DecisionError> {
    match value {
        "draft" => Ok(DecisionStatus::Draft),
        "running" => Ok(DecisionStatus::Running),
        "paused" => Ok(DecisionStatus::Paused),
        "completed" => Ok(DecisionStatus::Completed),
        "cancelled" => Ok(DecisionStatus::Cancelled),
        "failed" => Ok(DecisionStatus::Failed),
        _ => Err(DecisionError::Internal(
            "database contains an invalid decision status".into(),
        )),
    }
}

fn parse_brain_state(value: &str) -> Result<DecisionBrainState, DecisionError> {
    match value {
        "pending" => Ok(DecisionBrainState::Pending),
        "running" => Ok(DecisionBrainState::Running),
        "completed" => Ok(DecisionBrainState::Completed),
        "failed" => Ok(DecisionBrainState::Failed),
        "cancelled" => Ok(DecisionBrainState::Cancelled),
        _ => Err(DecisionError::Internal(
            "database contains an invalid brain state".into(),
        )),
    }
}

fn brain_kind_to_str(value: BrainKind) -> &'static str {
    match value {
        BrainKind::ProviderModel => "provider_model",
        BrainKind::AcpAgent => "acp_agent",
    }
}

fn parse_brain_kind(value: &str) -> Result<BrainKind, DecisionError> {
    match value {
        "provider_model" => Ok(BrainKind::ProviderModel),
        "acp_agent" => Ok(BrainKind::AcpAgent),
        _ => Err(DecisionError::Internal(
            "database contains an invalid brain kind".into(),
        )),
    }
}

fn timestamp(value: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(value)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
        .to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::init_database_memory;

    #[tokio::test]
    async fn persists_and_recovers_complete_decision_graph() {
        let db = init_database_memory().await.unwrap();
        let repo = DecisionRepository::new(db.pool().clone());
        let role = RoleDefinition {
            id: "strategy".into(),
            name: "Strategy".into(),
            instructions: "Test assumptions".into(),
        };
        let brain = BrainDefinition {
            id: Some("brain-1".into()),
            kind: BrainKind::ProviderModel,
            provider_id: "provider-1".into(),
            model: "model-1".into(),
            role_id: role.id.clone(),
            agent_id: None,
            tool_ids: vec![],
        };
        let knowledge = serde_json::json!({});
        let roles = [role];
        let brains = [brain];
        let created = repo
            .create(NewDecision {
                user_id: "system_default_user",
                question: "Should we ship?",
                brain_count: 1,
                knowledge: &knowledge,
                roles: &roles,
                tools: &[],
                brains: &brains,
                user_evidence: &[],
                retrieval_evidence: &[],
                retrieval: None,
                idempotency_key: None,
                request_fingerprint: None,
            })
            .await
            .unwrap();
        let session = repo.begin_session("system_default_user", &created.id).await.unwrap();
        let turn = repo
            .insert_turn_attempt(
                &created.id,
                &session,
                "brain-1",
                1,
                "provider-1",
                "model-1",
                None,
                &[],
                &[],
                true,
                "external",
            )
            .await
            .unwrap();
        let loaded = repo.get("system_default_user", &created.id).await.unwrap();
        repo.complete_opinion(CompletedOpinion {
            decision_id: &created.id,
            brain: &loaded.brains[0],
            turn_id: &turn,
            content: "Ship behind a flag",
            evidence: &[],
            retrieval_bundle_id: None,
            lineage_bundle_ids: &[],
            cloud_egress_allowed: true,
            origin_location: "external",
            rank: 0,
            fallback_provider_id: None,
        })
        .await
        .unwrap();
        repo.finish(
            "system_default_user",
            &created.id,
            &session,
            "Ship behind a flag",
            false,
        )
        .await
        .unwrap();

        let candidate_id = repo.get("system_default_user", &created.id).await.unwrap().candidates[0]
            .id
            .clone();
        repo.select_candidate(
            "system_default_user",
            &created.id,
            &candidate_id,
            &[DecisionActionItemInput {
                title: "Run the guarded pilot".into(),
                owner: Some("owner".into()),
                due_at: Some("2030-01-01T00:00:00Z".into()),
            }],
        )
        .await
        .unwrap();

        let restored = repo.get("system_default_user", &created.id).await.unwrap();
        assert_eq!(restored.status, DecisionStatus::Completed);
        assert_eq!(restored.brains[0].state, DecisionBrainState::Completed);
        assert_eq!(restored.turns.len(), 1);
        assert_eq!(restored.candidates.len(), 1);
        assert!(restored.resolution.is_some());
        assert_eq!(restored.resolution.unwrap().action_items.len(), 1);
    }

    #[tokio::test]
    async fn restart_turns_running_state_into_resumable_pause() {
        let db = init_database_memory().await.unwrap();
        let repo = DecisionRepository::new(db.pool().clone());
        sqlx::query(
            "INSERT INTO decisions (id, user_id, question, status, brain_count, created_at, updated_at) \
             VALUES ('d1', 'system_default_user', 'Q', 'running', 3, 1, 1)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO decision_sessions (id, decision_id, status, revision, current_prompt, started_at, updated_at) \
             VALUES ('s1', 'd1', 'running', 1, 'Q', 1, 1)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO decision_brains (id, decision_id, provider_id, model, role_id, state, ordinal, created_at, updated_at) \
             VALUES ('b1', 'd1', 'p1', 'm1', 'risk', 'running', 0, 1, 1)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO decision_turns (id, decision_id, session_id, brain_id, kind, status, created_at, updated_at) \
             VALUES ('t1', 'd1', 's1', 'b1', 'opinion', 'running', 1, 1)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        assert_eq!(repo.recover_interrupted().await.unwrap(), 1);
        let restored = repo.get("system_default_user", "d1").await.unwrap();
        assert_eq!(restored.status, DecisionStatus::Paused);
        assert_eq!(restored.brains[0].state, DecisionBrainState::Pending);
        assert_eq!(restored.turns[0].status, "failed");
        assert_eq!(restored.turns[0].error_code.as_deref(), Some("SESSION_INTERRUPTED"));
    }
}
