use std::collections::{HashMap, HashSet};
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
use tokio::sync::{Mutex, watch};

use crate::DecisionError;
use crate::ports::{
    BrainCatalogPort, BrainExecutionFailure, BrainExecutionFailureKind, BrainExecutionPort, BrainInvocation,
    BrainOpinion, DecisionKnowledgePort, DecisionKnowledgeRequest, NoopDecisionKnowledge,
};
use crate::repository::{CompletedOpinion, DecisionRepository, NewDecision, PersistedDecisionContext};

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
        mut request: CreateDecisionRequest,
    ) -> Result<DecisionResponse, DecisionError> {
        request.question = validate_text(request.question, "question", 8_000)?;
        if !(MIN_BRAINS..=MAX_BRAINS).contains(&request.brain_count) {
            return Err(DecisionError::InvalidRequest(format!(
                "brain_count must be between {MIN_BRAINS} and {MAX_BRAINS}"
            )));
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
        let mut evidence = normalize_user_notes(request.evidence);
        evidence.extend(
            self.retrieve_knowledge(user_id, None, &request.question, &request.knowledge)
                .await?,
        );
        let response = self
            .repository
            .create(NewDecision {
                user_id,
                question: &request.question,
                brain_count: request.brain_count,
                knowledge: &request.knowledge,
                roles: &roles,
                tools: &request.tools,
                brains: &brains,
                evidence: &evidence,
            })
            .await?;
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
                Ok((turn_id, opinion)) => {
                    self.persist_success(user_id, decision_id, rank, &brain, &original, &turn_id, opinion, None)
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
                Ok((turn_id, opinion)) => {
                    self.persist_success(
                        user_id,
                        decision_id,
                        rank,
                        &brain,
                        &fallback,
                        &turn_id,
                        opinion,
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
    ) -> Result<Result<(String, BrainOpinion), BrainExecutionFailure>, DecisionError> {
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
            )
            .await?;
        let invocation = BrainInvocation {
            decision_id: decision_id.to_owned(),
            session_id: context.session_id.clone(),
            brain: definition,
            question: context.question.clone(),
            role,
            tools,
            interjections: context.interjections.clone(),
            evidence: context.evidence.clone(),
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
            result = tokio::time::timeout(self.timeout, self.executor.execute(invocation)) => {
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
        Ok(result.map(|opinion| (turn_id, opinion)))
    }

    #[allow(clippy::too_many_arguments)]
    async fn persist_success(
        &self,
        user_id: &str,
        decision_id: &str,
        rank: i64,
        brain: &DecisionBrainResponse,
        executed: &BrainDefinition,
        turn_id: &str,
        opinion: BrainOpinion,
        fallback_provider_id: Option<&str>,
    ) -> Result<(), DecisionError> {
        let mut evidence = opinion.evidence;
        if evidence.is_empty() {
            evidence.push(DecisionEvidenceInput {
                source_id: format!("provider:{}:{}", executed.provider_id, executed.model),
                title: format!("AI opinion · {}", executed.model),
                snippet: truncate(&opinion.content, 4_000),
                score: 1.0,
                media_type: "ai_opinion".into(),
                page: None,
                chapter: None,
                timestamp_ms: None,
            });
        }
        self.repository
            .complete_opinion(CompletedOpinion {
                decision_id,
                brain,
                turn_id,
                content: &opinion.content,
                evidence: &evidence,
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
                "delta": opinion.content,
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
        let evidence = self.retrieve_knowledge(user_id, Some(id), &question, &policy).await?;
        if evidence.is_empty() {
            return Ok(evidence);
        }
        let before = self.repository.get(user_id, id).await?.evidence.len();
        let stored = self.repository.add_evidence(user_id, id, &evidence).await?;
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
    ) -> Result<Vec<DecisionEvidenceInput>, DecisionError> {
        let mode = policy.get("mode").and_then(serde_json::Value::as_str).unwrap_or("auto");
        if !matches!(mode, "off" | "auto" | "required") {
            return Err(DecisionError::InvalidRequest(
                "knowledge.mode must be off, auto, or required".into(),
            ));
        }
        if mode == "off" {
            return Ok(Vec::new());
        }
        let evidence = self
            .knowledge
            .retrieve(DecisionKnowledgeRequest {
                user_id: user_id.to_owned(),
                decision_id: decision_id.map(str::to_owned),
                question: question.to_owned(),
                policy: policy.clone(),
            })
            .await
            .map_err(|error| DecisionError::KnowledgeUnavailable(error.message))?;
        validate_gateway_evidence(&evidence)?;
        if mode == "required" && evidence.is_empty() {
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
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};

    use aionui_api_types::{BrainKind, CreateDecisionRequest, DecisionStatus};
    use aionui_db::init_database_memory;
    use aionui_realtime::EventBroadcaster;

    use super::*;

    #[derive(Clone)]
    struct MockBrainRuntime {
        catalog: Vec<BrainDefinition>,
        ready: Arc<AtomicBool>,
        calls: Arc<StdMutex<HashMap<String, usize>>>,
        recover_failures: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl BrainCatalogPort for MockBrainRuntime {
        async fn available_brains(&self) -> Result<Vec<BrainDefinition>, BrainExecutionFailure> {
            Ok(self.catalog.clone())
        }
    }

    #[async_trait::async_trait]
    impl BrainExecutionPort for MockBrainRuntime {
        async fn execute(&self, invocation: BrainInvocation) -> Result<BrainOpinion, BrainExecutionFailure> {
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
                provider if provider.starts_with("wait") => {
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
        ) -> Result<Vec<DecisionEvidenceInput>, crate::DecisionKnowledgeFailure> {
            self.calls.lock().unwrap().push(request);
            Ok(vec![DecisionEvidenceInput {
                source_id: "source-1".into(),
                title: "Pilot results".into(),
                snippet: "Retention improved by 12 percent".into(),
                score: 0.91,
                media_type: "pdf".into(),
                page: Some(3),
                chapter: None,
                timestamp_ms: None,
            }])
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
            ready: Arc::new(AtomicBool::new(ready)),
            calls: Arc::new(StdMutex::new(HashMap::new())),
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
