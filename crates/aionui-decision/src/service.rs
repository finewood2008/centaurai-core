use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use aionui_api_types::{
    BrainDefinition, BrainKind, CreateDecisionRequest, DecisionBrainResponse, DecisionBrainState,
    DecisionEvidenceInput, DecisionResponse, DecisionStatus, InterjectDecisionRequest, RefreshDecisionKnowledgeRequest,
    RoleDefinition, SelectDecisionCandidateRequest, UpdateDecisionRequest, WebSocketMessage,
};
use aionui_realtime::EventBroadcaster;
use futures_util::future::join_all;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, watch};

use crate::DecisionError;
use crate::ports::{
    BrainCatalogPort, BrainExecutionFailure, BrainExecutionFailureKind, BrainExecutionPort, BrainInvocation,
    BrainLocation, BrainOpinion, DecisionKnowledgePort, DecisionKnowledgeRequest, DecisionKnowledgeResult,
    NoopDecisionKnowledge,
};
use crate::repository::{CompletedOpinion, DecisionRepository, EgressAudit, NewDecision, PersistedDecisionContext};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(45);
const MIN_BRAINS: usize = 3;
const MAX_BRAINS: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunControl {
    Running,
    Paused,
    Cancelled,
}

struct ActiveRun {
    session_id: String,
    control: watch::Sender<RunControl>,
}

enum BrainRunOutcome {
    Success,
    Failed,
    Stopped,
}

struct SelectedEvidence {
    items: Vec<DecisionEvidenceInput>,
    evidence_ids: Vec<String>,
    retrieval_bundle_id: Option<String>,
    lineage_bundle_ids: Vec<String>,
    cloud_egress_allowed: bool,
    retrieval_hit_count: usize,
}

struct AttemptSuccess {
    turn_id: String,
    opinion: BrainOpinion,
    retrieval_bundle_id: Option<String>,
    lineage_bundle_ids: Vec<String>,
    cloud_egress_allowed: bool,
    origin_location: BrainLocation,
}

pub struct DecisionService {
    repository: DecisionRepository,
    catalog: Arc<dyn BrainCatalogPort>,
    executor: Arc<dyn BrainExecutionPort>,
    knowledge: Arc<dyn DecisionKnowledgePort>,
    broadcaster: Arc<dyn EventBroadcaster>,
    timeout: Duration,
    active_runs: Mutex<HashMap<String, ActiveRun>>,
    launch_lock: Mutex<()>,
}

impl DecisionService {
    pub fn new(
        repository: DecisionRepository,
        catalog: Arc<dyn BrainCatalogPort>,
        executor: Arc<dyn BrainExecutionPort>,
        broadcaster: Arc<dyn EventBroadcaster>,
    ) -> Arc<Self> {
        Self::new_with_knowledge(
            repository,
            catalog,
            executor,
            broadcaster,
            Arc::new(NoopDecisionKnowledge),
        )
    }

    pub fn new_with_knowledge(
        repository: DecisionRepository,
        catalog: Arc<dyn BrainCatalogPort>,
        executor: Arc<dyn BrainExecutionPort>,
        broadcaster: Arc<dyn EventBroadcaster>,
        knowledge: Arc<dyn DecisionKnowledgePort>,
    ) -> Arc<Self> {
        Arc::new(Self {
            repository,
            catalog,
            executor,
            knowledge,
            broadcaster,
            timeout: DEFAULT_TIMEOUT,
            active_runs: Mutex::new(HashMap::new()),
            launch_lock: Mutex::new(()),
        })
    }

    pub fn with_timeout(
        repository: DecisionRepository,
        catalog: Arc<dyn BrainCatalogPort>,
        executor: Arc<dyn BrainExecutionPort>,
        broadcaster: Arc<dyn EventBroadcaster>,
        timeout: Duration,
    ) -> Arc<Self> {
        Self::with_timeout_and_knowledge(
            repository,
            catalog,
            executor,
            broadcaster,
            Arc::new(NoopDecisionKnowledge),
            timeout,
        )
    }

    pub fn with_timeout_and_knowledge(
        repository: DecisionRepository,
        catalog: Arc<dyn BrainCatalogPort>,
        executor: Arc<dyn BrainExecutionPort>,
        broadcaster: Arc<dyn EventBroadcaster>,
        knowledge: Arc<dyn DecisionKnowledgePort>,
        timeout: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            repository,
            catalog,
            executor,
            knowledge,
            broadcaster,
            timeout,
            active_runs: Mutex::new(HashMap::new()),
            launch_lock: Mutex::new(()),
        })
    }

    pub async fn recover_interrupted(&self) -> Result<u64, DecisionError> {
        self.repository.recover_interrupted().await
    }

    pub async fn create(
        &self,
        user_id: &str,
        request: CreateDecisionRequest,
    ) -> Result<DecisionResponse, DecisionError> {
        self.create_with_idempotency(user_id, request, None).await
    }

    pub async fn create_with_idempotency(
        &self,
        user_id: &str,
        mut request: CreateDecisionRequest,
        header_operation_id: Option<&str>,
    ) -> Result<DecisionResponse, DecisionError> {
        let operation_id = normalize_operation_id(header_operation_id, request.client_operation_id.take())?;
        request.question = validate_text(request.question, "question", 8_000)?;
        if !(MIN_BRAINS..=MAX_BRAINS).contains(&request.brain_count) {
            return Err(DecisionError::InvalidRequest(format!(
                "brain_count must be between {MIN_BRAINS} and {MAX_BRAINS}"
            )));
        }
        // The idempotency identity belongs to the client's normalized input,
        // not to dynamic server-side Brain selection. A lost-ack retry must
        // still return the original decision if the provider catalog changed.
        let request_fingerprint = request_fingerprint(&request)?;
        if let Some(operation_id) = operation_id.as_deref()
            && let Some(existing) = self
                .repository
                .idempotent_decision(user_id, operation_id, &request_fingerprint)
                .await?
        {
            return Ok(existing);
        }
        let roles = normalize_roles(request.roles)?;
        let mut brains = if request.brains.is_empty() {
            self.catalog
                .available_brains()
                .await
                .map_err(|error| DecisionError::ProviderUnavailable(error.message))?
                .into_iter()
                .take(request.brain_count)
                .collect::<Vec<_>>()
        } else {
            if request.brains.len() > MAX_BRAINS {
                return Err(DecisionError::InvalidRequest(format!(
                    "at most {MAX_BRAINS} brains are allowed"
                )));
            }
            request.brains
        };
        validate_tools(&request.tools)?;
        assign_and_validate_brains(&mut brains, &roles, &request.tools)?;
        let user_evidence = normalize_user_notes(request.evidence);
        let knowledge = self
            .retrieve_knowledge(user_id, None, &request.question, &request.knowledge)
            .await?;
        let response = match self
            .repository
            .create(NewDecision {
                user_id,
                question: &request.question,
                brain_count: request.brain_count,
                knowledge: &request.knowledge,
                roles: &roles,
                tools: &request.tools,
                brains: &brains,
                user_evidence: &user_evidence,
                retrieval_evidence: &knowledge.evidence,
                retrieval: knowledge.retrieval.as_ref(),
                idempotency_key: operation_id.as_deref(),
                request_fingerprint: operation_id.as_ref().map(|_| request_fingerprint.as_str()),
            })
            .await
        {
            Ok(response) => response,
            Err(DecisionError::Database(_)) if operation_id.is_some() => {
                if let Some(existing) = self
                    .repository
                    .idempotent_decision(
                        user_id,
                        operation_id.as_deref().expect("checked above"),
                        &request_fingerprint,
                    )
                    .await?
                {
                    return Ok(existing);
                }
                return Err(DecisionError::Internal(
                    "idempotent decision creation could not be reconciled".into(),
                ));
            }
            Err(error) => return Err(error),
        };
        self.emit_to_user(user_id, "decision.sessionChanged", &response)?;
        Ok(response)
    }

    pub async fn list(&self, user_id: &str) -> Result<Vec<DecisionResponse>, DecisionError> {
        self.repository.list(user_id).await
    }

    pub async fn get(&self, user_id: &str, id: &str) -> Result<DecisionResponse, DecisionError> {
        self.repository.get(user_id, id).await
    }

    pub async fn update(
        self: &Arc<Self>,
        user_id: &str,
        id: &str,
        request: UpdateDecisionRequest,
    ) -> Result<DecisionResponse, DecisionError> {
        if let Some(question) = request.question {
            let question = validate_text(question, "question", 8_000)?;
            self.repository.update_question(user_id, id, &question).await?;
        }
        if let Some(status) = request.status {
            if status != DecisionStatus::Paused {
                return Err(DecisionError::InvalidRequest(
                    "PATCH only supports status=paused; use the action endpoints for other transitions".into(),
                ));
            }
            self.pause(user_id, id).await?;
        }
        let response = self.repository.get(user_id, id).await?;
        self.emit_to_user(user_id, "decision.sessionChanged", &response)?;
        Ok(response)
    }

    pub async fn start(self: &Arc<Self>, user_id: &str, id: &str) -> Result<DecisionResponse, DecisionError> {
        if self.repository.get(user_id, id).await?.status != DecisionStatus::Draft {
            return Err(DecisionError::Conflict(
                "only a draft decision can be started; use continue for recovery".into(),
            ));
        }
        self.launch(user_id, id).await
    }

    pub async fn continue_decision(
        self: &Arc<Self>,
        user_id: &str,
        id: &str,
    ) -> Result<DecisionResponse, DecisionError> {
        let decision = self.repository.get(user_id, id).await?;
        let recoverable_partial = decision.status == DecisionStatus::Completed
            && decision
                .resolution
                .as_ref()
                .is_some_and(|resolution| resolution.partial)
            && decision
                .brains
                .iter()
                .any(|brain| brain.state != DecisionBrainState::Completed);
        if !matches!(decision.status, DecisionStatus::Paused | DecisionStatus::Failed) && !recoverable_partial {
            return Err(DecisionError::Conflict(
                "only a paused, failed, or partial decision can continue".into(),
            ));
        }
        self.launch(user_id, id).await
    }

    async fn launch(self: &Arc<Self>, user_id: &str, id: &str) -> Result<DecisionResponse, DecisionError> {
        let _launch_guard = self.launch_lock.lock().await;
        for _ in 0..100 {
            let state = self.active_runs.lock().await.get(id).map(|run| *run.control.borrow());
            match state {
                None => break,
                Some(RunControl::Running) => {
                    return Err(DecisionError::Conflict("decision is already running".into()));
                }
                Some(RunControl::Paused | RunControl::Cancelled) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
        if self.active_runs.lock().await.contains_key(id) {
            return Err(DecisionError::Conflict(
                "previous decision run is still stopping; retry shortly".into(),
            ));
        }
        let decision = self.repository.get(user_id, id).await?;
        let runnable = decision
            .brains
            .iter()
            .filter(|brain| brain.state != DecisionBrainState::Completed)
            .count();
        let completed = decision
            .brains
            .iter()
            .filter(|brain| brain.state == DecisionBrainState::Completed)
            .count();
        if runnable + completed < MIN_BRAINS {
            return Err(DecisionError::Conflict(format!(
                "a decision needs at least {MIN_BRAINS} configured brains"
            )));
        }
        self.refresh_from_gateway(user_id, id, false).await?;
        let session_id = self.repository.begin_session(user_id, id).await?;
        let (control, receiver) = watch::channel(RunControl::Running);
        self.active_runs.lock().await.insert(
            id.to_owned(),
            ActiveRun {
                session_id: session_id.clone(),
                control,
            },
        );
        let response = self.repository.get(user_id, id).await?;
        self.emit_to_user(user_id, "decision.sessionChanged", &response)?;

        let service = Arc::clone(self);
        let user_id = user_id.to_owned();
        let decision_id = id.to_owned();
        tokio::spawn(async move {
            if let Err(error) = service.run_session(&user_id, &decision_id, &session_id, receiver).await {
                tracing::error!(
                    decision_id = %decision_id,
                    session_id = %session_id,
                    error = %error,
                    "decision session failed"
                );
                if let Ok(current) = service.repository.get(&user_id, &decision_id).await
                    && current.status == DecisionStatus::Running
                {
                    let _ = service
                        .repository
                        .fail_session(&user_id, &decision_id, &session_id)
                        .await;
                }
                if let Ok(response) = service.repository.get(&user_id, &decision_id).await {
                    let _ = service.emit_to_user(&user_id, "decision.sessionChanged", &response);
                }
            }
            service.remove_active_run(&decision_id, &session_id).await;
        });
        Ok(response)
    }

    async fn run_session(
        self: &Arc<Self>,
        user_id: &str,
        decision_id: &str,
        session_id: &str,
        receiver: watch::Receiver<RunControl>,
    ) -> Result<(), DecisionError> {
        let context = self.repository.load_context(user_id, decision_id, session_id).await?;
        let fallbacks = self.catalog.available_brains().await.unwrap_or_default();
        let runnable = context
            .brains
            .iter()
            .enumerate()
            .filter(|(_, brain)| brain.state != DecisionBrainState::Completed)
            .map(|(rank, brain)| {
                self.run_brain(
                    user_id,
                    decision_id,
                    rank as i64,
                    brain.clone(),
                    context.clone(),
                    fallbacks.clone(),
                    receiver.clone(),
                )
            });
        let outcomes = join_all(runnable).await;
        let decision = self.repository.get(user_id, decision_id).await?;
        if matches!(decision.status, DecisionStatus::Paused | DecisionStatus::Cancelled) {
            self.emit_to_user(user_id, "decision.sessionChanged", &decision)?;
            return Ok(());
        }

        let successes = decision
            .brains
            .iter()
            .filter(|brain| brain.state == DecisionBrainState::Completed)
            .count();
        if successes == 0 {
            self.repository.fail_session(user_id, decision_id, session_id).await?;
            let response = self.repository.get(user_id, decision_id).await?;
            self.emit_to_user(user_id, "decision.sessionChanged", &response)?;
            return Ok(());
        }
        let failed = outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Ok(BrainRunOutcome::Failed) | Err(_)))
            .count();
        let response = self.repository.get(user_id, decision_id).await?;
        let summary = synthesize_resolution(&response);
        let partial = failed > 0
            || response
                .brains
                .iter()
                .any(|brain| brain.state != DecisionBrainState::Completed);
        self.repository
            .finish(user_id, decision_id, session_id, &summary, partial)
            .await?;
        let completed = self.repository.get(user_id, decision_id).await?;
        self.emit_to_user(user_id, "decision.completed", &completed)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_brain(
        &self,
        user_id: &str,
        decision_id: &str,
        rank: i64,
        brain: DecisionBrainResponse,
        context: PersistedDecisionContext,
        fallbacks: Vec<BrainDefinition>,
        receiver: watch::Receiver<RunControl>,
    ) -> Result<BrainRunOutcome, DecisionError> {
        self.repository.mark_brain_running(&brain.id).await?;
        let original = response_to_definition(&brain);
        let mut last_failure: Option<BrainExecutionFailure> = None;

        for attempt in 1..=2 {
            let result = self
                .execute_attempt(
                    decision_id,
                    &brain,
                    &context,
                    original.clone(),
                    attempt,
                    receiver.clone(),
                )
                .await?;
            match result {
                Ok(success) => {
                    self.persist_success(user_id, decision_id, rank, &brain, &original, success, None)
                        .await?;
                    return Ok(BrainRunOutcome::Success);
                }
                Err(failure) if failure.kind == BrainExecutionFailureKind::Cancelled => {
                    if failure.message.contains("cancelled") {
                        self.repository.mark_brain_cancelled(&brain.id).await?;
                    } else {
                        self.repository.reset_brain_pending(&brain.id).await?;
                    }
                    return Ok(BrainRunOutcome::Stopped);
                }
                Err(failure) => {
                    let retryable = failure.kind.retryable();
                    last_failure = Some(failure);
                    if !retryable {
                        break;
                    }
                }
            }
        }

        if last_failure.as_ref().is_some_and(|failure| failure.kind.retryable())
            && let Some(mut fallback) = select_fallback(&fallbacks, &original)
        {
            fallback.id = original.id.clone();
            fallback.role_id = original.role_id.clone();
            fallback.tool_ids = original.tool_ids.clone();
            match self
                .execute_attempt(decision_id, &brain, &context, fallback.clone(), 3, receiver)
                .await?
            {
                Ok(success) => {
                    self.persist_success(
                        user_id,
                        decision_id,
                        rank,
                        &brain,
                        &fallback,
                        success,
                        Some(&fallback.provider_id),
                    )
                    .await?;
                    return Ok(BrainRunOutcome::Success);
                }
                Err(failure) if failure.kind == BrainExecutionFailureKind::Cancelled => {
                    if failure.message.contains("cancelled") {
                        self.repository.mark_brain_cancelled(&brain.id).await?;
                    } else {
                        self.repository.reset_brain_pending(&brain.id).await?;
                    }
                    return Ok(BrainRunOutcome::Stopped);
                }
                Err(failure) => last_failure = Some(failure),
            }
        }

        let failure = last_failure.unwrap_or_else(|| {
            BrainExecutionFailure::new(BrainExecutionFailureKind::Unavailable, "brain returned no result")
        });
        self.repository
            .mark_brain_failed(&brain.id, failure.kind.code())
            .await?;
        Ok(BrainRunOutcome::Failed)
    }

    async fn execute_attempt(
        &self,
        decision_id: &str,
        brain: &DecisionBrainResponse,
        context: &PersistedDecisionContext,
        definition: BrainDefinition,
        attempt: i64,
        mut control: watch::Receiver<RunControl>,
    ) -> Result<Result<AttemptSuccess, BrainExecutionFailure>, DecisionError> {
        let role = context
            .roles
            .iter()
            .find(|role| role.id == brain.role_id)
            .cloned()
            .unwrap_or_else(|| default_roles()[0].clone());
        let tools = context
            .tools
            .iter()
            .filter(|tool| brain.tool_ids.iter().any(|id| id == &tool.id))
            .cloned()
            .collect();
        let turn_id = self
            .repository
            .insert_turn_attempt(
                decision_id,
                &context.session_id,
                &brain.id,
                attempt,
                &definition.provider_id,
                &definition.model,
                None,
                &[],
                &[],
                false,
                "unknown",
            )
            .await?;
        let deadline = tokio::time::Instant::now() + self.timeout;
        let plan = tokio::select! {
            control = wait_for_control(&mut control) => {
                let label = match control {
                    RunControl::Paused => "decision paused",
                    RunControl::Cancelled => "decision cancelled",
                    RunControl::Running => "decision run ended",
                };
                Err(BrainExecutionFailure::new(BrainExecutionFailureKind::Cancelled, label))
            }
            result = tokio::time::timeout_at(deadline, self.executor.prepare(&definition)) => {
                match result {
                    Ok(result) => result,
                    Err(_) => Err(BrainExecutionFailure::new(
                        BrainExecutionFailureKind::Timeout,
                        "brain preparation exceeded its attempt deadline",
                    )),
                }
            }
        };
        let plan = match plan {
            Ok(plan) => plan,
            Err(failure) => {
                self.repository
                    .record_egress(EgressAudit {
                        decision_id,
                        session_id: &context.session_id,
                        brain_id: &brain.id,
                        turn_id: &turn_id,
                        provider_id: &definition.provider_id,
                        model: &definition.model,
                        location: "unknown",
                        retrieval: context.retrieval.as_ref(),
                        knowledge_egress_allowed: false,
                        hit_count: 0,
                        evidence_ids: &[],
                        lineage_bundle_ids: &[],
                    })
                    .await?;
                self.repository.fail_turn(&turn_id, failure.kind.code()).await?;
                return Ok(Err(failure));
            }
        };
        let location = plan.location;
        let selected = evidence_for_brain(context, location);
        self.repository
            .set_turn_execution_context(
                &turn_id,
                selected.retrieval_bundle_id.as_deref(),
                &selected.evidence_ids,
                &selected.lineage_bundle_ids,
                selected.cloud_egress_allowed,
                location_label(location),
            )
            .await?;
        let external = location != BrainLocation::Local;
        self.repository
            .record_egress(EgressAudit {
                decision_id,
                session_id: &context.session_id,
                brain_id: &brain.id,
                turn_id: &turn_id,
                provider_id: &definition.provider_id,
                model: &definition.model,
                location: location_label(location),
                retrieval: context.retrieval.as_ref(),
                knowledge_egress_allowed: external
                    && context
                        .retrieval
                        .as_ref()
                        .is_some_and(|retrieval| retrieval.bundle.cloud_authorized),
                hit_count: selected.retrieval_hit_count,
                evidence_ids: &selected.evidence_ids,
                lineage_bundle_ids: &selected.lineage_bundle_ids,
            })
            .await?;
        let invocation = BrainInvocation {
            decision_id: decision_id.to_owned(),
            session_id: context.session_id.clone(),
            brain: plan.brain.clone(),
            question: context.question.clone(),
            role,
            tools,
            interjections: context.interjections.clone(),
            evidence: selected.items,
        };
        let result = tokio::select! {
            control = wait_for_control(&mut control) => {
                let label = match control {
                    RunControl::Paused => "decision paused",
                    RunControl::Cancelled => "decision cancelled",
                    RunControl::Running => "decision run ended",
                };
                Err(BrainExecutionFailure::new(BrainExecutionFailureKind::Cancelled, label))
            }
            result = tokio::time::timeout_at(deadline, self.executor.execute(plan, invocation)) => {
                match result {
                    Ok(result) => result,
                    Err(_) => Err(BrainExecutionFailure::new(
                        BrainExecutionFailureKind::Timeout,
                        "brain execution exceeded its deadline",
                    )),
                }
            }
        };
        if let Err(failure) = &result {
            self.repository.fail_turn(&turn_id, failure.kind.code()).await?;
        }
        Ok(result.map(|opinion| AttemptSuccess {
            turn_id,
            opinion,
            retrieval_bundle_id: selected.retrieval_bundle_id,
            lineage_bundle_ids: selected.lineage_bundle_ids,
            cloud_egress_allowed: selected.cloud_egress_allowed,
            origin_location: location,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    async fn persist_success(
        &self,
        user_id: &str,
        decision_id: &str,
        rank: i64,
        brain: &DecisionBrainResponse,
        executed: &BrainDefinition,
        success: AttemptSuccess,
        fallback_provider_id: Option<&str>,
    ) -> Result<(), DecisionError> {
        let mut evidence = success.opinion.evidence;
        if evidence.is_empty() {
            evidence.push(DecisionEvidenceInput {
                source_id: format!("provider:{}:{}", executed.provider_id, executed.model),
                title: format!("AI opinion · {}", executed.model),
                snippet: truncate(&success.opinion.content, 4_000),
                score: 1.0,
                media_type: "ai_opinion".into(),
                page: None,
                chapter: None,
                timestamp_ms: None,
                end_seconds: None,
                uri: None,
            });
        }
        self.repository
            .complete_opinion(CompletedOpinion {
                decision_id,
                brain,
                turn_id: &success.turn_id,
                content: &success.opinion.content,
                evidence: &evidence,
                retrieval_bundle_id: success.retrieval_bundle_id.as_deref(),
                lineage_bundle_ids: &success.lineage_bundle_ids,
                cloud_egress_allowed: success.cloud_egress_allowed,
                origin_location: location_label(success.origin_location),
                rank,
                fallback_provider_id,
            })
            .await?;
        self.emit_value_to_user(
            user_id,
            "decision.turnDelta",
            json!({
                "decision_id": decision_id,
                "brain_id": brain.id,
                "delta": success.opinion.content,
            }),
        );
        for hit in evidence {
            self.emit_value_to_user(
                user_id,
                "decision.evidenceAdded",
                json!({
                    "decision_id": decision_id,
                    "evidence": evidence_event_payload(&hit),
                }),
            );
        }
        Ok(())
    }

    pub async fn interject(
        self: &Arc<Self>,
        user_id: &str,
        id: &str,
        request: InterjectDecisionRequest,
    ) -> Result<DecisionResponse, DecisionError> {
        let content = validate_text(request.content, "content", 8_000)?;
        let current = self.repository.get(user_id, id).await?;
        if current.status == DecisionStatus::Running {
            self.pause(user_id, id).await?;
        }
        self.repository.add_interjection(user_id, id, &content).await?;
        let response = self.repository.get(user_id, id).await?;
        self.emit_to_user(user_id, "decision.sessionChanged", &response)?;
        Ok(response)
    }

    pub async fn cancel(self: &Arc<Self>, user_id: &str, id: &str) -> Result<DecisionResponse, DecisionError> {
        let decision = self.repository.get(user_id, id).await?;
        if decision.status == DecisionStatus::Completed {
            return Err(DecisionError::Conflict(
                "a completed decision cannot be cancelled".into(),
            ));
        }
        self.signal(id, RunControl::Cancelled).await;
        self.repository
            .set_control_state(user_id, id, DecisionStatus::Cancelled)
            .await?;
        let response = self.repository.get(user_id, id).await?;
        self.emit_to_user(user_id, "decision.sessionChanged", &response)?;
        Ok(response)
    }

    pub async fn pause(self: &Arc<Self>, user_id: &str, id: &str) -> Result<DecisionResponse, DecisionError> {
        let decision = self.repository.get(user_id, id).await?;
        if decision.status != DecisionStatus::Running {
            return Err(DecisionError::Conflict("only a running decision can be paused".into()));
        }
        self.signal(id, RunControl::Paused).await;
        self.repository
            .set_control_state(user_id, id, DecisionStatus::Paused)
            .await?;
        self.repository.get(user_id, id).await
    }

    pub async fn select(
        &self,
        user_id: &str,
        id: &str,
        request: SelectDecisionCandidateRequest,
    ) -> Result<DecisionResponse, DecisionError> {
        if request
            .action_items
            .iter()
            .any(|item| item.title.trim().is_empty() || item.title.chars().count() > 2_000)
        {
            return Err(DecisionError::InvalidRequest(
                "action item title is required and must be at most 2000 characters".into(),
            ));
        }
        self.repository
            .select_candidate(user_id, id, &request.candidate_id, &request.action_items)
            .await?;
        let response = self.repository.get(user_id, id).await?;
        self.emit_to_user(user_id, "decision.sessionChanged", &response)?;
        Ok(response)
    }

    pub async fn refresh_knowledge(
        &self,
        user_id: &str,
        id: &str,
        request: RefreshDecisionKnowledgeRequest,
    ) -> Result<DecisionResponse, DecisionError> {
        let before = self.repository.get(user_id, id).await?.evidence.len();
        let notes = normalize_user_notes(request.evidence);
        if !notes.is_empty() {
            self.repository.add_evidence(user_id, id, &notes).await?;
        }
        self.refresh_from_gateway(user_id, id, false).await?;
        let evidence = self.repository.get(user_id, id).await?.evidence;
        for hit in evidence.into_iter().skip(before) {
            self.emit_value_to_user(
                user_id,
                "decision.evidenceAdded",
                json!({"decision_id": id, "evidence": hit}),
            );
        }
        self.repository.get(user_id, id).await
    }

    async fn refresh_from_gateway(
        &self,
        user_id: &str,
        id: &str,
        emit: bool,
    ) -> Result<Vec<DecisionEvidenceInput>, DecisionError> {
        let (question, policy) = self.repository.knowledge_context(user_id, id).await?;
        let knowledge = self.retrieve_knowledge(user_id, Some(id), &question, &policy).await?;
        let before = self.repository.get(user_id, id).await?.evidence.len();
        if let Some(bundle) = knowledge.retrieval.as_ref() {
            self.repository
                .store_retrieval_evidence(user_id, id, bundle, &knowledge.evidence)
                .await?;
        }
        let evidence = knowledge.evidence;
        if evidence.is_empty() {
            return Ok(evidence);
        }
        let stored = self.repository.get(user_id, id).await?.evidence;
        if emit {
            for hit in stored.into_iter().skip(before) {
                self.emit_value_to_user(
                    user_id,
                    "decision.evidenceAdded",
                    json!({"decision_id": id, "evidence": hit}),
                );
            }
        }
        Ok(evidence)
    }

    async fn retrieve_knowledge(
        &self,
        user_id: &str,
        decision_id: Option<&str>,
        question: &str,
        policy: &serde_json::Value,
    ) -> Result<DecisionKnowledgeResult, DecisionError> {
        let mode = policy.get("mode").and_then(serde_json::Value::as_str).unwrap_or("auto");
        if !matches!(mode, "off" | "auto" | "required") {
            return Err(DecisionError::InvalidRequest(
                "knowledge.mode must be off, auto, or required".into(),
            ));
        }
        if mode == "off" {
            return Ok(DecisionKnowledgeResult::default());
        }
        let evidence = match self
            .knowledge
            .retrieve(DecisionKnowledgeRequest {
                user_id: user_id.to_owned(),
                decision_id: decision_id.map(str::to_owned),
                question: question.to_owned(),
                policy: policy.clone(),
            })
            .await
        {
            Ok(evidence) => evidence,
            Err(error) if mode == "auto" => {
                tracing::warn!(
                    decision_id = decision_id.unwrap_or("new"),
                    error = %error,
                    "optional decision knowledge retrieval unavailable"
                );
                DecisionKnowledgeResult::default()
            }
            Err(error) => return Err(DecisionError::KnowledgeUnavailable(error.message)),
        };
        validate_gateway_evidence(&evidence.evidence)?;
        if !evidence.evidence.is_empty() && evidence.retrieval.is_none() {
            return Err(DecisionError::KnowledgeUnavailable(
                "knowledge gateway returned evidence without a retrieval bundle".into(),
            ));
        }
        if mode == "required" && evidence.evidence.is_empty() {
            return Err(DecisionError::Conflict(
                "knowledge mode is required but retrieval returned no evidence".into(),
            ));
        }
        Ok(evidence)
    }

    async fn signal(&self, id: &str, control: RunControl) {
        if let Some(active) = self.active_runs.lock().await.get(id) {
            let _ = active.control.send(control);
        }
    }

    async fn remove_active_run(&self, decision_id: &str, session_id: &str) {
        let mut active = self.active_runs.lock().await;
        if active.get(decision_id).is_some_and(|run| run.session_id == session_id) {
            active.remove(decision_id);
        }
    }

    fn emit_to_user<T: serde::Serialize>(
        &self,
        user_id: &str,
        name: &'static str,
        payload: &T,
    ) -> Result<(), DecisionError> {
        let payload = serde_json::to_value(payload)?;
        self.emit_value_to_user(user_id, name, payload);
        Ok(())
    }

    fn emit_value_to_user(&self, user_id: &str, name: &'static str, payload: serde_json::Value) {
        self.broadcaster
            .broadcast_to_user(user_id, WebSocketMessage::new(name, payload));
    }
}

async fn wait_for_control(receiver: &mut watch::Receiver<RunControl>) -> RunControl {
    loop {
        let current = *receiver.borrow_and_update();
        if current != RunControl::Running {
            return current;
        }
        if receiver.changed().await.is_err() {
            return RunControl::Cancelled;
        }
    }
}

fn response_to_definition(brain: &DecisionBrainResponse) -> BrainDefinition {
    BrainDefinition {
        id: Some(brain.id.clone()),
        kind: brain.kind,
        provider_id: brain.provider_id.clone(),
        model: brain.model.clone(),
        role_id: brain.role_id.clone(),
        agent_id: brain.agent_id.clone(),
        tool_ids: brain.tool_ids.clone(),
    }
}

fn select_fallback(fallbacks: &[BrainDefinition], original: &BrainDefinition) -> Option<BrainDefinition> {
    fallbacks
        .iter()
        .find(|candidate| candidate.provider_id != original.provider_id && candidate.kind == original.kind)
        .cloned()
}

fn normalize_roles(roles: Vec<RoleDefinition>) -> Result<Vec<RoleDefinition>, DecisionError> {
    let roles = if roles.is_empty() { default_roles() } else { roles };
    let mut ids = HashSet::new();
    for role in &roles {
        if role.id.trim().is_empty() || role.name.trim().is_empty() || role.instructions.trim().is_empty() {
            return Err(DecisionError::InvalidRequest(
                "role id, name, and instructions are required".into(),
            ));
        }
        if !ids.insert(role.id.clone()) {
            return Err(DecisionError::InvalidRequest("role ids must be unique".into()));
        }
    }
    Ok(roles)
}

fn assign_and_validate_brains(
    brains: &mut [BrainDefinition],
    roles: &[RoleDefinition],
    tools: &[aionui_api_types::DecisionToolDefinition],
) -> Result<(), DecisionError> {
    let role_ids: HashSet<&str> = roles.iter().map(|role| role.id.as_str()).collect();
    let tool_ids: HashSet<&str> = tools.iter().map(|tool| tool.id.as_str()).collect();
    let mut ids = HashSet::new();
    for (index, brain) in brains.iter_mut().enumerate() {
        if brain.role_id.trim().is_empty() {
            brain.role_id = roles[index % roles.len()].id.clone();
        }
        if brain.provider_id.trim().is_empty() || brain.model.trim().is_empty() {
            return Err(DecisionError::InvalidRequest(
                "every brain requires provider_id and model".into(),
            ));
        }
        if !role_ids.contains(brain.role_id.as_str()) {
            return Err(DecisionError::InvalidRequest(format!(
                "brain references unknown role {}",
                brain.role_id
            )));
        }
        if brain.tool_ids.iter().any(|id| !tool_ids.contains(id.as_str())) {
            return Err(DecisionError::InvalidRequest(
                "brain references an unknown decision tool".into(),
            ));
        }
        if let Some(id) = &brain.id
            && !ids.insert(id.clone())
        {
            return Err(DecisionError::InvalidRequest("brain ids must be unique".into()));
        }
        if brain.kind == BrainKind::AcpAgent && brain.agent_id.as_deref().is_none_or(str::is_empty) {
            return Err(DecisionError::InvalidRequest("ACP brain requires agent_id".into()));
        }
    }
    Ok(())
}

fn validate_tools(tools: &[aionui_api_types::DecisionToolDefinition]) -> Result<(), DecisionError> {
    let mut ids = HashSet::new();
    for tool in tools {
        if tool.id.trim().is_empty() || tool.name.trim().is_empty() || tool.kind.trim().is_empty() {
            return Err(DecisionError::InvalidRequest(
                "tool id, name, and kind are required".into(),
            ));
        }
        if !ids.insert(tool.id.as_str()) {
            return Err(DecisionError::InvalidRequest("tool ids must be unique".into()));
        }
        if contains_secret_field(&tool.config) {
            return Err(DecisionError::InvalidRequest(
                "tool config must reference server-owned credentials instead of embedding secrets".into(),
            ));
        }
    }
    Ok(())
}

fn contains_secret_field(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(fields) => fields.iter().any(|(key, value)| {
            let normalized: String = key
                .chars()
                .filter(|character| !matches!(character, '-' | '_'))
                .flat_map(char::to_lowercase)
                .collect();
            matches!(
                normalized.as_str(),
                "apikey" | "token" | "accesstoken" | "refreshtoken" | "secret" | "password" | "authorization"
            ) || contains_secret_field(value)
        }),
        serde_json::Value::Array(values) => values.iter().any(contains_secret_field),
        _ => false,
    }
}

fn default_roles() -> Vec<RoleDefinition> {
    [
        (
            "strategy",
            "Strategy",
            "Test strategic fit, assumptions, options, and second-order effects.",
        ),
        (
            "risk",
            "Risk",
            "Identify failure modes, reversibility, controls, and missing evidence.",
        ),
        (
            "product",
            "Product",
            "Evaluate user value, usability, sequencing, and measurable outcomes.",
        ),
        (
            "finance",
            "Finance",
            "Evaluate costs, upside, cash impact, and sensitivity to key assumptions.",
        ),
        (
            "operations",
            "Operations",
            "Evaluate execution capacity, dependencies, and operational resilience.",
        ),
        (
            "technology",
            "Technology",
            "Evaluate architecture, security, feasibility, and long-term maintenance.",
        ),
        (
            "contrarian",
            "Contrarian",
            "Challenge consensus and present the strongest credible alternative.",
        ),
    ]
    .into_iter()
    .map(|(id, name, instructions)| RoleDefinition {
        id: id.into(),
        name: name.into(),
        instructions: instructions.into(),
    })
    .collect()
}

fn validate_text(value: String, field: &str, max: usize) -> Result<String, DecisionError> {
    let value = value.trim().to_owned();
    if value.is_empty() {
        return Err(DecisionError::InvalidRequest(format!("{field} is required")));
    }
    if value.chars().count() > max {
        return Err(DecisionError::InvalidRequest(format!(
            "{field} must be at most {max} characters"
        )));
    }
    Ok(value)
}

fn synthesize_resolution(decision: &DecisionResponse) -> String {
    let mut summary = format!("Decision: {}\n\nIndependent recommendations:\n", decision.question);
    for candidate in &decision.candidates {
        summary.push_str(&format!("\n- {}\n{}\n", candidate.title, candidate.content));
    }
    if decision
        .brains
        .iter()
        .any(|brain| brain.state != DecisionBrainState::Completed)
    {
        summary.push_str("\nThis is a partial resolution: one or more brains did not return a usable opinion.\n");
    }
    summary.push_str("\nReview the cited evidence and select a candidate before taking irreversible action.");
    summary
}

fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn normalize_operation_id(header: Option<&str>, body: Option<String>) -> Result<Option<String>, DecisionError> {
    let header = header.map(str::trim).filter(|value| !value.is_empty());
    let body = body.as_deref().map(str::trim).filter(|value| !value.is_empty());
    if let (Some(header), Some(body)) = (header, body)
        && header != body
    {
        return Err(DecisionError::Conflict(
            "Idempotency-Key and client_operation_id must match when both are provided".into(),
        ));
    }
    let value = header.or(body);
    if let Some(value) = value
        && (!(8..=128).contains(&value.len())
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')))
    {
        return Err(DecisionError::InvalidRequest(
            "operation id must be 8-128 ASCII alphanumerics or '-', '_', '.', ':'".into(),
        ));
    }
    Ok(value.map(str::to_owned))
}

fn request_fingerprint(request: &CreateDecisionRequest) -> Result<String, DecisionError> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(request)?)))
}

fn evidence_for_brain(context: &PersistedDecisionContext, location: BrainLocation) -> SelectedEvidence {
    let current = context.retrieval.as_ref();
    let selected = context
        .evidence
        .iter()
        .filter(|evidence| location == BrainLocation::Local || external_evidence_allowed(evidence, current))
        .collect::<Vec<_>>();
    let mut lineage_bundle_ids = BTreeSet::new();
    for evidence in &selected {
        lineage_bundle_ids.extend(evidence.lineage_bundle_ids.iter().cloned());
        if let Some(bundle_id) = &evidence.retrieval_bundle_id {
            lineage_bundle_ids.insert(bundle_id.clone());
        }
    }
    let lineage_bundle_ids = lineage_bundle_ids.into_iter().collect::<Vec<_>>();
    let retrieval_hit_count = current.map_or(0, |retrieval| {
        selected
            .iter()
            .filter(|evidence| evidence.retrieval_bundle_id.as_deref() == Some(retrieval.id.as_str()))
            .count()
    });
    let retrieval_bundle_id = current
        .filter(|retrieval| lineage_bundle_ids.iter().any(|id| id == &retrieval.id))
        .map(|retrieval| retrieval.id.clone());
    SelectedEvidence {
        items: selected.iter().map(|item| item.evidence.clone()).collect(),
        evidence_ids: selected.iter().map(|item| item.id.clone()).collect(),
        retrieval_bundle_id,
        lineage_bundle_ids,
        cloud_egress_allowed: selected.iter().all(|item| item.cloud_egress_allowed),
        retrieval_hit_count,
    }
}

fn external_evidence_allowed(
    evidence: &crate::repository::PersistedEvidence,
    current: Option<&crate::repository::PersistedRetrievalBundle>,
) -> bool {
    if evidence.origin_location == "user" {
        return true;
    }
    if evidence.origin_location == "knowledge" {
        return current.is_some_and(|retrieval| {
            retrieval.bundle.cloud_authorized && evidence.retrieval_bundle_id.as_deref() == Some(retrieval.id.as_str())
        });
    }
    if !evidence.cloud_egress_allowed {
        return false;
    }
    if evidence.lineage_bundle_ids.is_empty() {
        return true;
    }
    current.is_some_and(|retrieval| {
        retrieval.bundle.cloud_authorized
            && evidence
                .lineage_bundle_ids
                .iter()
                .all(|bundle_id| bundle_id == &retrieval.id)
    })
}

fn location_label(location: BrainLocation) -> &'static str {
    match location {
        BrainLocation::Local => "local",
        BrainLocation::External => "external",
        BrainLocation::Unknown => "unknown",
    }
}

fn normalize_user_notes(evidence: Vec<DecisionEvidenceInput>) -> Vec<DecisionEvidenceInput> {
    evidence
        .into_iter()
        .filter_map(|item| {
            let snippet = truncate(item.snippet.trim(), 4_000);
            if snippet.is_empty() {
                return None;
            }
            Some(DecisionEvidenceInput {
                source_id: aionui_common::generate_prefixed_id("user_note"),
                title: "User note".into(),
                snippet,
                score: 0.0,
                media_type: "user_note".into(),
                page: None,
                chapter: None,
                timestamp_ms: None,
                end_seconds: None,
                uri: None,
            })
        })
        .collect()
}

fn validate_gateway_evidence(evidence: &[DecisionEvidenceInput]) -> Result<(), DecisionError> {
    for hit in evidence {
        if hit.source_id.trim().is_empty()
            || hit.title.trim().is_empty()
            || hit.snippet.trim().is_empty()
            || hit.media_type.trim().is_empty()
            || !hit.score.is_finite()
        {
            return Err(DecisionError::KnowledgeUnavailable(
                "knowledge gateway returned an invalid evidence contract".into(),
            ));
        }
    }
    Ok(())
}

fn evidence_event_payload(hit: &DecisionEvidenceInput) -> serde_json::Value {
    json!({
        "source_id": hit.source_id,
        "title": hit.title,
        "snippet": hit.snippet,
        "score": hit.score,
        "media_type": hit.media_type,
        "locator": {
            "page": hit.page,
            "chapter": hit.chapter,
            "start_seconds": hit.timestamp_ms.map(|value| value as f64 / 1_000.0),
            "end_seconds": hit.end_seconds,
            "uri": hit.uri,
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};

    use aionui_api_types::{BrainKind, CreateDecisionRequest, DecisionStatus, RetrievalBundle};
    use aionui_db::init_database_memory;
    use aionui_realtime::EventBroadcaster;

    use super::*;

    #[derive(Clone)]
    struct MockBrainRuntime {
        catalog: Vec<BrainDefinition>,
        catalog_available: Arc<AtomicBool>,
        force_external: Arc<AtomicBool>,
        ready: Arc<AtomicBool>,
        calls: Arc<StdMutex<HashMap<String, usize>>>,
        invocations: Arc<StdMutex<Vec<BrainInvocation>>>,
        recover_failures: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl BrainCatalogPort for MockBrainRuntime {
        async fn available_brains(&self) -> Result<Vec<BrainDefinition>, BrainExecutionFailure> {
            if !self.catalog_available.load(Ordering::SeqCst) {
                return Err(BrainExecutionFailure::new(
                    BrainExecutionFailureKind::Unavailable,
                    "scripted catalog outage",
                ));
            }
            Ok(self.catalog.clone())
        }
    }

    #[async_trait::async_trait]
    impl BrainExecutionPort for MockBrainRuntime {
        async fn prepare(&self, brain: &BrainDefinition) -> Result<crate::BrainExecutionPlan, BrainExecutionFailure> {
            let location = if self.force_external.load(Ordering::SeqCst) {
                BrainLocation::External
            } else if brain.provider_id.starts_with("local") {
                BrainLocation::Local
            } else if brain.provider_id.starts_with("unknown") {
                BrainLocation::Unknown
            } else {
                BrainLocation::External
            };
            Ok(crate::BrainExecutionPlan::new(brain.clone(), location, ()))
        }

        async fn execute(
            &self,
            plan: crate::BrainExecutionPlan,
            invocation: BrainInvocation,
        ) -> Result<BrainOpinion, BrainExecutionFailure> {
            assert_eq!(plan.brain, invocation.brain);
            self.invocations.lock().unwrap().push(invocation.clone());
            *self
                .calls
                .lock()
                .unwrap()
                .entry(invocation.brain.provider_id.clone())
                .or_default() += 1;
            if self.recover_failures.load(Ordering::SeqCst) {
                return Ok(BrainOpinion {
                    content: format!("{} recovered with a bounded rollout", invocation.brain.model),
                    evidence: vec![],
                });
            }
            match invocation.brain.provider_id.as_str() {
                "timeout" => Err(BrainExecutionFailure::new(
                    BrainExecutionFailureKind::Timeout,
                    "scripted timeout",
                )),
                "rate-limit" => Err(BrainExecutionFailure::new(
                    BrainExecutionFailureKind::RateLimited,
                    "scripted 429",
                )),
                provider if provider.contains("wait") => {
                    while !self.ready.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Ok(BrainOpinion {
                        content: format!("{} recommends a reversible pilot", invocation.brain.model),
                        evidence: vec![],
                    })
                }
                _ => Ok(BrainOpinion {
                    content: format!("{} recommends a reversible pilot", invocation.brain.model),
                    evidence: vec![],
                }),
            }
        }
    }

    #[derive(Default)]
    struct RecordingBroadcaster {
        events: StdMutex<Vec<WebSocketMessage<serde_json::Value>>>,
    }

    #[derive(Default)]
    struct MockKnowledge {
        calls: StdMutex<Vec<DecisionKnowledgeRequest>>,
    }

    #[async_trait::async_trait]
    impl DecisionKnowledgePort for MockKnowledge {
        async fn retrieve(
            &self,
            request: DecisionKnowledgeRequest,
        ) -> Result<DecisionKnowledgeResult, crate::DecisionKnowledgeFailure> {
            let cloud_authorized = request
                .policy
                .get("cloud_use")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let generation = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(request);
                calls.len()
            };
            let snippet = format!("Retention improved by 12 percent · retrieval {generation}");
            let evidence = vec![DecisionEvidenceInput {
                source_id: "source-1".into(),
                title: "Pilot results".into(),
                snippet: snippet.clone(),
                score: 0.91,
                media_type: "pdf".into(),
                page: Some(3),
                chapter: None,
                timestamp_ms: None,
                end_seconds: Some(18.25),
                uri: Some("contextofme://knowledge/wiki/pilot-results".into()),
            }];
            Ok(DecisionKnowledgeResult {
                retrieval: Some(RetrievalBundle {
                    query: "Should we launch this month?".into(),
                    hits: vec![aionui_api_types::KnowledgeHit {
                        source_id: "source-1".into(),
                        title: "Pilot results".into(),
                        snippet,
                        score: 0.91,
                        media_type: "pdf".into(),
                        locator: aionui_api_types::KnowledgeLocator {
                            page: Some(3),
                            end_seconds: Some(18.25),
                            uri: Some("contextofme://knowledge/wiki/pilot-results".into()),
                            ..Default::default()
                        },
                    }],
                    token_budget: 16,
                    cloud_authorized,
                    space_ids: vec!["personal".into()],
                }),
                evidence,
            })
        }
    }

    impl EventBroadcaster for RecordingBroadcaster {
        fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn brain(provider: &str, model: &str, role: &str) -> BrainDefinition {
        BrainDefinition {
            id: None,
            kind: BrainKind::ProviderModel,
            provider_id: provider.into(),
            model: model.into(),
            role_id: role.into(),
            agent_id: None,
            tool_ids: vec![],
        }
    }

    fn request(brains: Vec<BrainDefinition>) -> CreateDecisionRequest {
        CreateDecisionRequest {
            client_operation_id: None,
            question: "Should we launch this month?".into(),
            brain_count: brains.len(),
            brains,
            roles: default_roles(),
            tools: vec![],
            knowledge: json!({}),
            evidence: vec![DecisionEvidenceInput {
                source_id: "forged-source".into(),
                title: "Pretend retrieval hit".into(),
                snippet: "Owner-provided context".into(),
                score: 1.0,
                media_type: "pdf".into(),
                page: Some(99),
                chapter: None,
                timestamp_ms: None,
                end_seconds: None,
                uri: None,
            }],
        }
    }

    async fn harness(
        catalog: Vec<BrainDefinition>,
        ready: bool,
    ) -> (Arc<DecisionService>, Arc<MockBrainRuntime>, Arc<RecordingBroadcaster>) {
        let db = init_database_memory().await.unwrap();
        let runtime = Arc::new(MockBrainRuntime {
            catalog,
            catalog_available: Arc::new(AtomicBool::new(true)),
            force_external: Arc::new(AtomicBool::new(false)),
            ready: Arc::new(AtomicBool::new(ready)),
            calls: Arc::new(StdMutex::new(HashMap::new())),
            invocations: Arc::new(StdMutex::new(Vec::new())),
            recover_failures: Arc::new(AtomicBool::new(false)),
        });
        let events = Arc::new(RecordingBroadcaster::default());
        let service = DecisionService::with_timeout_and_knowledge(
            DecisionRepository::new(db.pool().clone()),
            runtime.clone(),
            runtime.clone(),
            events.clone(),
            Arc::new(MockKnowledge::default()),
            Duration::from_secs(2),
        );
        std::mem::forget(db);
        (service, runtime, events)
    }

    async fn wait_for_terminal(service: &DecisionService, id: &str) -> DecisionResponse {
        for _ in 0..200 {
            let decision = service.get("system_default_user", id).await.unwrap();
            if matches!(
                decision.status,
                DecisionStatus::Completed | DecisionStatus::Failed | DecisionStatus::Cancelled
            ) {
                return decision;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("decision did not reach a terminal state");
    }

    #[tokio::test]
    async fn timeout_and_429_do_not_discard_a_valid_opinion_or_break_ws_order() {
        let brains = vec![
            brain("timeout", "slow-model", "strategy"),
            brain("rate-limit", "busy-model", "risk"),
            brain("healthy", "good-model", "product"),
        ];
        let (service, runtime, events) = harness(brains.clone(), true).await;
        let created = service.create("system_default_user", request(brains)).await.unwrap();
        events.events.lock().unwrap().clear();
        service.start("system_default_user", &created.id).await.unwrap();
        let completed = wait_for_terminal(&service, &created.id).await;

        assert_eq!(completed.status, DecisionStatus::Completed);
        assert!(completed.resolution.as_ref().unwrap().partial);
        assert!(completed.conclusion.as_deref().unwrap().contains("good-model"));
        assert_eq!(
            completed
                .brains
                .iter()
                .filter(|brain| brain.state == DecisionBrainState::Completed)
                .count(),
            1
        );
        assert!(completed.evidence.iter().any(|hit| hit.source_id == "source-1"));
        assert!(!completed.evidence.iter().any(|hit| hit.source_id == "forged-source"));
        assert!(completed.evidence.iter().any(|hit| hit.media_type == "user_note"));
        assert!(completed.evidence.iter().any(|hit| hit.media_type == "ai_opinion"));
        let audit_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM decision_egress_audit WHERE decision_id = ?")
            .bind(&created.id)
            .fetch_one(service.repository.pool())
            .await
            .unwrap();
        assert!(audit_rows >= 5, "each retry/fallback attempt must be audited");
        {
            let calls = runtime.calls.lock().unwrap();
            assert!(calls["timeout"] >= 2);
            assert!(calls["rate-limit"] >= 2);
        }

        let names: Vec<String> = events
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|event| event.name.clone())
            .collect();
        assert_eq!(names.first().map(String::as_str), Some("decision.sessionChanged"));
        let delta = names.iter().position(|name| name == "decision.turnDelta").unwrap();
        let evidence = names.iter().position(|name| name == "decision.evidenceAdded").unwrap();
        let completed_event = names.iter().position(|name| name == "decision.completed").unwrap();
        assert!(delta < evidence && evidence < completed_event);

        runtime.recover_failures.store(true, Ordering::SeqCst);
        service
            .continue_decision("system_default_user", &created.id)
            .await
            .unwrap();
        let recovered = wait_for_terminal(&service, &created.id).await;
        assert!(
            recovered
                .brains
                .iter()
                .all(|brain| brain.state == DecisionBrainState::Completed)
        );
        assert!(!recovered.resolution.as_ref().unwrap().partial);
        assert!(recovered.session.as_ref().unwrap().revision >= 2);
    }

    #[tokio::test]
    async fn retrieval_is_gated_per_actual_brain_location_in_local_cloud_and_mixed_runs() {
        let mixed = vec![
            brain("local-ollama", "local-one", "strategy"),
            brain("cloud-one", "cloud-one", "risk"),
            brain("local-llamacpp", "local-two", "product"),
        ];
        let (service, runtime, _events) = harness(mixed.clone(), true).await;
        let created = service.create("system_default_user", request(mixed)).await.unwrap();
        service.start("system_default_user", &created.id).await.unwrap();
        wait_for_terminal(&service, &created.id).await;

        {
            let invocations = runtime.invocations.lock().unwrap();
            for invocation in invocations.iter() {
                let has_retrieval = invocation.evidence.iter().any(|item| item.source_id == "source-1");
                let has_owner_note = invocation.evidence.iter().any(|item| item.media_type == "user_note");
                assert!(
                    has_owner_note,
                    "explicit owner notes remain available to every selected Brain"
                );
                if invocation.brain.provider_id.starts_with("local") {
                    assert!(has_retrieval, "local Brain lost locally retrieved evidence");
                } else {
                    assert!(!has_retrieval, "unauthorized cloud Brain received personal knowledge");
                }
            }
        }

        let audits: Vec<(String, bool, i64)> = sqlx::query_as(
            "SELECT location, knowledge_egress_allowed, hit_count FROM decision_egress_audit \
             WHERE decision_id = ? ORDER BY created_at",
        )
        .bind(&created.id)
        .fetch_all(service.repository.pool())
        .await
        .unwrap();
        assert!(audits.iter().any(|row| row.0 == "local" && !row.1 && row.2 == 1));
        assert!(audits.iter().any(|row| row.0 == "external" && !row.1 && row.2 == 0));

        let cloud = vec![
            brain("cloud-a", "a", "strategy"),
            brain("cloud-b", "b", "risk"),
            brain("cloud-c", "c", "product"),
        ];
        let (service, runtime, _events) = harness(cloud.clone(), true).await;
        let created = service.create("system_default_user", request(cloud)).await.unwrap();
        service.start("system_default_user", &created.id).await.unwrap();
        wait_for_terminal(&service, &created.id).await;
        assert!(
            runtime
                .invocations
                .lock()
                .unwrap()
                .iter()
                .all(|invocation| { !invocation.evidence.iter().any(|item| item.source_id == "source-1") })
        );

        let local = vec![
            brain("local-a", "a", "strategy"),
            brain("local-b", "b", "risk"),
            brain("local-c", "c", "product"),
        ];
        let (service, runtime, _events) = harness(local.clone(), true).await;
        let created = service.create("system_default_user", request(local)).await.unwrap();
        service.start("system_default_user", &created.id).await.unwrap();
        wait_for_terminal(&service, &created.id).await;
        assert!(
            runtime
                .invocations
                .lock()
                .unwrap()
                .iter()
                .all(|invocation| { invocation.evidence.iter().any(|item| item.source_id == "source-1") })
        );
    }

    #[tokio::test]
    async fn refresh_continue_and_recovery_keep_old_knowledge_and_local_opinions_off_cloud() {
        let brains = vec![
            brain("local-a", "one", "strategy"),
            brain("local-b", "two", "risk"),
            brain("local-wait", "three", "product"),
        ];
        let (service, runtime, events) = harness(brains.clone(), false).await;
        let created = service.create("system_default_user", request(brains)).await.unwrap();
        service.start("system_default_user", &created.id).await.unwrap();

        for _ in 0..200 {
            if service
                .get("system_default_user", &created.id)
                .await
                .unwrap()
                .candidates
                .len()
                >= 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            service
                .get("system_default_user", &created.id)
                .await
                .unwrap()
                .candidates
                .len(),
            2
        );
        service.pause("system_default_user", &created.id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let local_tainted_opinions: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM decision_evidence WHERE decision_id = ? AND media_type = 'ai_opinion' \
             AND origin_location = 'local' AND cloud_egress_allowed = 0",
        )
        .bind(&created.id)
        .fetch_one(service.repository.pool())
        .await
        .unwrap();
        assert_eq!(local_tainted_opinions, 2);

        // Simulate a new Core process after recovery plus a provider update
        // that changes the remaining Brain's actual execution boundary.
        runtime.force_external.store(true, Ordering::SeqCst);
        runtime.ready.store(true, Ordering::SeqCst);
        let recovered_service = DecisionService::with_timeout_and_knowledge(
            service.repository.clone(),
            runtime.clone(),
            runtime.clone(),
            events,
            Arc::new(MockKnowledge::default()),
            Duration::from_secs(2),
        );
        recovered_service
            .continue_decision("system_default_user", &created.id)
            .await
            .unwrap();
        let completed = wait_for_terminal(&recovered_service, &created.id).await;
        assert!(completed.session.as_ref().unwrap().revision >= 2);

        let recovered_invocation = runtime
            .invocations
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|invocation| invocation.brain.provider_id == "local-wait")
            .cloned()
            .unwrap();
        assert!(
            recovered_invocation
                .evidence
                .iter()
                .all(|evidence| evidence.media_type == "user_note"),
            "external recovery received old retrieval evidence or a local-derived opinion"
        );

        let (same_source_rows, distinct_lineages): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COUNT(DISTINCT retrieval_bundle_id) FROM decision_evidence \
             WHERE decision_id = ? AND source_id = 'source-1'",
        )
        .bind(&created.id)
        .fetch_one(recovered_service.repository.pool())
        .await
        .unwrap();
        assert!(same_source_rows >= 3);
        assert_eq!(same_source_rows, distinct_lineages);

        let external_audit: (bool, i64, String, String) = sqlx::query_as(
            "SELECT knowledge_egress_allowed, hit_count, evidence_ids_json, lineage_bundle_ids_json \
             FROM decision_egress_audit WHERE decision_id = ? AND provider_id = 'local-wait' \
             AND location = 'external' ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&created.id)
        .fetch_one(recovered_service.repository.pool())
        .await
        .unwrap();
        assert!(!external_audit.0);
        assert_eq!(external_audit.1, 0);
        assert_ne!(
            external_audit.2, "[]",
            "owner note lineage should identify its evidence row"
        );
        assert_eq!(external_audit.3, "[]");
    }

    #[tokio::test]
    async fn authorized_bundle_reaches_cloud_brains_and_is_persisted() {
        let brains = vec![
            brain("cloud-a", "a", "strategy"),
            brain("cloud-b", "b", "risk"),
            brain("cloud-c", "c", "product"),
        ];
        let (service, runtime, _events) = harness(brains.clone(), true).await;
        let mut create = request(brains);
        create.knowledge = json!({"mode": "auto", "cloud_use": true, "space_ids": ["personal"]});
        let created = service.create("system_default_user", create).await.unwrap();
        service.start("system_default_user", &created.id).await.unwrap();
        let completed = wait_for_terminal(&service, &created.id).await;
        assert!(
            runtime
                .invocations
                .lock()
                .unwrap()
                .iter()
                .all(|invocation| { invocation.evidence.iter().any(|item| item.source_id == "source-1") })
        );
        let bundle: (bool, String) = sqlx::query_as(
            "SELECT cloud_authorized, space_ids_json FROM decision_retrieval_bundles \
             WHERE decision_id = ? ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&created.id)
        .fetch_one(service.repository.pool())
        .await
        .unwrap();
        assert!(bundle.0);
        assert_eq!(bundle.1, r#"["personal"]"#);
        let allowed: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM decision_egress_audit WHERE decision_id = ? AND knowledge_egress_allowed = 1",
        )
        .bind(&created.id)
        .fetch_one(service.repository.pool())
        .await
        .unwrap();
        assert_eq!(allowed, 3);
        let hit = completed
            .evidence
            .iter()
            .find(|evidence| evidence.source_id == "source-1")
            .unwrap();
        assert_eq!(hit.locator.end_seconds, Some(18.25));
        assert_eq!(
            hit.locator.uri.as_deref(),
            Some("contextofme://knowledge/wiki/pilot-results")
        );
    }

    #[tokio::test]
    async fn decision_create_idempotency_survives_lost_ack_is_user_scoped_and_detects_conflicts() {
        let brains = vec![
            brain("local-a", "a", "strategy"),
            brain("local-b", "b", "risk"),
            brain("local-c", "c", "product"),
        ];
        let (service, _runtime, _events) = harness(brains.clone(), true).await;
        let mut create = request(brains.clone());
        create.client_operation_id = Some("client-op-0001".into());
        let first = service.create("system_default_user", create.clone()).await.unwrap();
        let retry = service.create("system_default_user", create.clone()).await.unwrap();
        assert_eq!(first.id, retry.id);
        assert_eq!(service.list("system_default_user").await.unwrap().len(), 1);

        let mut mismatched = create.clone();
        mismatched.client_operation_id = Some("client-op-body".into());
        assert!(matches!(
            service
                .create_with_idempotency("system_default_user", mismatched, Some("client-op-header"))
                .await,
            Err(DecisionError::Conflict(_))
        ));
        let mut malformed = request(brains);
        malformed.client_operation_id = Some("short".into());
        assert!(matches!(
            service.create("system_default_user", malformed).await,
            Err(DecisionError::InvalidRequest(_))
        ));

        let mut conflict = create.clone();
        conflict.question = "A materially different decision question".into();
        assert!(matches!(
            service.create("system_default_user", conflict).await,
            Err(DecisionError::Conflict(_))
        ));

        let now = aionui_common::now_ms();
        sqlx::query("INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES (?, ?, '', ?, ?)")
            .bind("other-user")
            .bind("other-user")
            .bind(now)
            .bind(now)
            .execute(service.repository.pool())
            .await
            .unwrap();
        let other = service.create("other-user", create).await.unwrap();
        assert_ne!(first.id, other.id);
    }

    #[tokio::test]
    async fn idempotent_auto_brain_retry_does_not_depend_on_the_current_catalog() {
        let catalog = vec![
            brain("local-a", "a", "strategy"),
            brain("local-b", "b", "risk"),
            brain("local-c", "c", "product"),
        ];
        let (service, runtime, _events) = harness(catalog, true).await;
        let mut create = request(vec![]);
        create.brain_count = 3;
        create.client_operation_id = Some("auto-brain-op-0001".into());
        let first = service.create("system_default_user", create.clone()).await.unwrap();

        runtime.catalog_available.store(false, Ordering::SeqCst);
        let retry = service.create("system_default_user", create).await.unwrap();
        assert_eq!(first.id, retry.id);
    }

    #[tokio::test]
    async fn pause_interject_and_continue_preserve_session_history() {
        let brains = vec![
            brain("wait-1", "one", "strategy"),
            brain("wait-2", "two", "risk"),
            brain("wait-3", "three", "product"),
        ];
        let (service, runtime, _events) = harness(brains.clone(), false).await;
        let created = service.create("system_default_user", request(brains)).await.unwrap();
        service.start("system_default_user", &created.id).await.unwrap();
        let paused = service
            .interject(
                "system_default_user",
                &created.id,
                InterjectDecisionRequest {
                    content: "Assume the budget is capped.".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(paused.status, DecisionStatus::Paused);
        runtime.ready.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(30)).await;
        service
            .continue_decision("system_default_user", &created.id)
            .await
            .unwrap();
        let completed = wait_for_terminal(&service, &created.id).await;
        assert_eq!(completed.status, DecisionStatus::Completed);
        assert!(
            completed
                .turns
                .iter()
                .any(|turn| turn.kind == "interjection" && turn.content.contains("budget"))
        );
        assert!(completed.session.as_ref().unwrap().revision >= 2);
    }

    #[tokio::test]
    async fn cancel_is_visible_cross_device_and_prevents_late_completion() {
        let brains = vec![
            brain("wait-1", "one", "strategy"),
            brain("wait-2", "two", "risk"),
            brain("wait-3", "three", "product"),
        ];
        let (service, runtime, _events) = harness(brains.clone(), false).await;
        let created = service.create("system_default_user", request(brains)).await.unwrap();
        service.start("system_default_user", &created.id).await.unwrap();
        let cancelled = service.cancel("system_default_user", &created.id).await.unwrap();
        assert_eq!(cancelled.status, DecisionStatus::Cancelled);
        runtime.ready.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let observed = service.get("system_default_user", &created.id).await.unwrap();
        assert_eq!(observed.status, DecisionStatus::Cancelled);
        assert!(observed.resolution.is_none());
        assert!(
            observed
                .brains
                .iter()
                .all(|brain| brain.state == DecisionBrainState::Cancelled)
        );
    }

    #[test]
    fn tool_config_rejects_nested_credentials() {
        let tools = vec![aionui_api_types::DecisionToolDefinition {
            id: "search".into(),
            name: "Search".into(),
            kind: "mcp".into(),
            config: json!({"transport": {"Authorization": "Bearer secret"}}),
        }];
        assert!(validate_tools(&tools).is_err());
    }
}
