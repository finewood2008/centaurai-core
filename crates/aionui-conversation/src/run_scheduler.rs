use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, RwLock};

use aionui_api_types::{
    AgentRunResponse, AgentRunStatus, AgentRuntimeMode, AgentRuntimePolicyResponse, AgentRuntimeStatusResponse,
    UpdateAgentRuntimePolicyRequest, WebSocketMessage,
};
use aionui_common::now_ms;
use aionui_db::{AgentRunRow, AgentRuntimePolicyRow, CreateAgentRunParams, IAgentRunRepository};
use aionui_realtime::EventBroadcaster;
use tokio::sync::{Mutex, oneshot};
use tracing::{error, info, warn};

const DEFAULT_TURN_ESTIMATE_MS: u64 = 60_000;

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimePolicy {
    pub mode: AgentRuntimeMode,
    pub global_active_limit: usize,
    pub per_user_active_limit: usize,
    pub per_user_queue_limit: usize,
    pub global_queue_limit: usize,
    pub queue_timeout_ms: u64,
    pub confirmation_timeout_ms: u64,
    pub resident_task_limit: usize,
    pub resident_idle_timeout_ms: u64,
    pub memory_constrained_percent: f32,
    pub memory_pause_percent: f32,
    pub memory_reject_percent: f32,
}

impl Default for RuntimePolicy {
    fn default() -> Self {
        let mode = match std::env::var("AIONUI_AGENT_SCHEDULER_MODE").as_deref() {
            Ok("off") => AgentRuntimeMode::Off,
            Ok("shadow") => AgentRuntimeMode::Shadow,
            _ => AgentRuntimeMode::Enforce,
        };
        Self {
            mode,
            global_active_limit: 6,
            per_user_active_limit: 1,
            per_user_queue_limit: 1,
            global_queue_limit: 20,
            queue_timeout_ms: 15 * 60 * 1000,
            confirmation_timeout_ms: 5 * 60 * 1000,
            resident_task_limit: 6,
            resident_idle_timeout_ms: 3 * 60 * 1000,
            memory_constrained_percent: 75.0,
            memory_pause_percent: 85.0,
            memory_reject_percent: 90.0,
        }
    }
}

impl RuntimePolicy {
    pub fn response(&self) -> AgentRuntimePolicyResponse {
        AgentRuntimePolicyResponse {
            mode: self.mode.clone(),
            global_active_limit: self.global_active_limit,
            per_user_active_limit: self.per_user_active_limit,
            per_user_queue_limit: self.per_user_queue_limit,
            global_queue_limit: self.global_queue_limit,
            queue_timeout_ms: self.queue_timeout_ms,
            confirmation_timeout_ms: self.confirmation_timeout_ms,
            resident_task_limit: self.resident_task_limit,
            resident_idle_timeout_ms: self.resident_idle_timeout_ms,
            memory_constrained_percent: self.memory_constrained_percent,
            memory_pause_percent: self.memory_pause_percent,
            memory_reject_percent: self.memory_reject_percent,
        }
    }

    fn apply(&mut self, update: UpdateAgentRuntimePolicyRequest) {
        if let Some(value) = update.mode {
            self.mode = value;
        }
        if let Some(value) = update.global_active_limit {
            self.global_active_limit = value.clamp(1, 64);
        }
        if let Some(value) = update.per_user_active_limit {
            self.per_user_active_limit = value.clamp(1, 8);
        }
        if let Some(value) = update.per_user_queue_limit {
            self.per_user_queue_limit = value.clamp(1, 8);
        }
        if let Some(value) = update.global_queue_limit {
            self.global_queue_limit = value.clamp(1, 1_000);
        }
        if let Some(value) = update.queue_timeout_ms {
            self.queue_timeout_ms = value.clamp(1_000, 24 * 60 * 60 * 1000);
        }
        if let Some(value) = update.confirmation_timeout_ms {
            self.confirmation_timeout_ms = value.clamp(1_000, 24 * 60 * 60 * 1000);
        }
        if let Some(value) = update.resident_task_limit {
            self.resident_task_limit = value.clamp(1, 64);
        }
        if let Some(value) = update.resident_idle_timeout_ms {
            self.resident_idle_timeout_ms = value.clamp(1_000, 24 * 60 * 60 * 1000);
        }
        let constrained = update
            .memory_constrained_percent
            .unwrap_or(self.memory_constrained_percent);
        let paused = update.memory_pause_percent.unwrap_or(self.memory_pause_percent);
        let rejecting = update.memory_reject_percent.unwrap_or(self.memory_reject_percent);
        self.memory_constrained_percent = constrained.clamp(50.0, 97.0);
        self.memory_pause_percent = paused.clamp(self.memory_constrained_percent + 1.0, 98.0);
        self.memory_reject_percent = rejecting.clamp(self.memory_pause_percent + 1.0, 100.0);
    }
}

pub trait MemoryPressureSource: Send + Sync {
    fn used_percent(&self) -> Option<f32>;
}

pub struct SystemMemoryPressure;

impl MemoryPressureSource for SystemMemoryPressure {
    fn used_percent(&self) -> Option<f32> {
        if let Ok(value) = std::env::var("AIONUI_MEMORY_USED_PERCENT")
            && let Ok(percent) = value.parse::<f32>()
        {
            return Some(percent.clamp(0.0, 100.0));
        }
        #[cfg(target_os = "linux")]
        {
            let content = std::fs::read_to_string("/proc/meminfo").ok()?;
            let mut total = None;
            let mut available = None;
            for line in content.lines() {
                let mut fields = line.split_whitespace();
                match fields.next() {
                    Some("MemTotal:") => total = fields.next().and_then(|value| value.parse::<f64>().ok()),
                    Some("MemAvailable:") => available = fields.next().and_then(|value| value.parse::<f64>().ok()),
                    _ => {}
                }
            }
            let total = total?;
            let available = available?;
            return Some((((total - available) / total) * 100.0).clamp(0.0, 100.0) as f32);
        }
        #[allow(unreachable_code)]
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryState {
    Normal,
    Constrained,
    Paused,
    Rejecting,
}

impl MemoryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Constrained => "constrained",
            Self::Paused => "paused",
            Self::Rejecting => "rejecting",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunAdmissionError {
    QueueFull,
    UserLimit,
    MemoryPressure,
    QueueTimeout,
    Cancelled,
    BackendRestarted,
    Persistence,
}

impl RunAdmissionError {
    pub fn code(self) -> &'static str {
        match self {
            Self::QueueFull => "RUN_QUEUE_FULL",
            Self::UserLimit => "USER_RUN_LIMIT",
            Self::MemoryPressure => "MEMORY_PRESSURE",
            Self::QueueTimeout => "RUN_QUEUE_TIMEOUT",
            Self::Cancelled => "CANCELLED",
            Self::BackendRestarted => "BACKEND_RESTARTED",
            Self::Persistence => "INTERNAL_ERROR",
        }
    }
}

pub struct AgentRunAdmission {
    pub run_id: String,
    pub turn_id: String,
    pub queued_at: i64,
    pub permit: oneshot::Receiver<Result<(), RunAdmissionError>>,
}

#[derive(Debug, Clone)]
pub struct NewAgentRun {
    pub run_id: String,
    pub turn_id: String,
    pub user_id: String,
    pub conversation_id: String,
    pub source: String,
    pub request_json: String,
    pub message_id: Option<String>,
    pub queued_at: i64,
}

struct QueuedRun {
    row: AgentRunRow,
    permit: oneshot::Sender<Result<(), RunAdmissionError>>,
}

#[derive(Debug, Clone)]
struct ActiveRun {
    user_id: String,
}

#[derive(Default)]
struct SchedulerState {
    queue: VecDeque<QueuedRun>,
    active: HashMap<String, ActiveRun>,
    user_ring: Vec<String>,
    last_dispatched_user: Option<String>,
}

pub struct AgentRunScheduler {
    repo: Arc<dyn IAgentRunRepository>,
    broadcaster: Arc<dyn EventBroadcaster>,
    memory: Arc<dyn MemoryPressureSource>,
    policy: RwLock<RuntimePolicy>,
    state: Mutex<SchedulerState>,
}

impl AgentRunScheduler {
    pub fn new(repo: Arc<dyn IAgentRunRepository>, broadcaster: Arc<dyn EventBroadcaster>) -> Arc<Self> {
        Arc::new(Self {
            repo,
            broadcaster,
            memory: Arc::new(SystemMemoryPressure),
            policy: RwLock::new(RuntimePolicy::default()),
            state: Mutex::new(SchedulerState::default()),
        })
    }

    #[cfg(test)]
    pub fn with_memory_source(
        repo: Arc<dyn IAgentRunRepository>,
        broadcaster: Arc<dyn EventBroadcaster>,
        memory: Arc<dyn MemoryPressureSource>,
    ) -> Arc<Self> {
        Arc::new(Self {
            repo,
            broadcaster,
            memory,
            policy: RwLock::new(RuntimePolicy::default()),
            state: Mutex::new(SchedulerState::default()),
        })
    }

    pub fn policy(&self) -> RuntimePolicy {
        self.policy.read().map(|policy| policy.clone()).unwrap_or_default()
    }

    pub async fn load_policy(&self) -> Result<(), RunAdmissionError> {
        let row = self.repo.load_runtime_policy().await.map_err(|error| {
            error!(error = %error, "failed to load agent runtime policy");
            RunAdmissionError::Persistence
        })?;
        if let Ok(mut policy) = self.policy.write() {
            policy.mode = match std::env::var("AIONUI_AGENT_SCHEDULER_MODE").as_deref() {
                Ok("off") => AgentRuntimeMode::Off,
                Ok("shadow") => AgentRuntimeMode::Shadow,
                Ok("enforce") => AgentRuntimeMode::Enforce,
                _ => match row.mode.as_str() {
                    "off" => AgentRuntimeMode::Off,
                    "shadow" => AgentRuntimeMode::Shadow,
                    _ => AgentRuntimeMode::Enforce,
                },
            };
            policy.global_active_limit = row.global_active_limit.max(1) as usize;
            policy.per_user_active_limit = row.per_user_active_limit.max(1) as usize;
            policy.per_user_queue_limit = row.per_user_queue_limit.max(1) as usize;
            policy.global_queue_limit = row.global_queue_limit.max(1) as usize;
            policy.queue_timeout_ms = row.queue_timeout_ms.max(1_000) as u64;
            policy.confirmation_timeout_ms = row.confirmation_timeout_ms.max(1_000) as u64;
            policy.resident_task_limit = row.resident_task_limit.max(1) as usize;
            policy.resident_idle_timeout_ms = row.resident_idle_timeout_ms.max(1_000) as u64;
        }
        Ok(())
    }

    pub async fn update_policy(&self, update: UpdateAgentRuntimePolicyRequest) -> AgentRuntimePolicyResponse {
        let (response, policy_to_save) = if let Ok(mut policy) = self.policy.write() {
            policy.apply(update);
            (policy.response(), policy.clone())
        } else {
            let policy = RuntimePolicy::default();
            (policy.response(), policy)
        };
        let row = AgentRuntimePolicyRow {
            mode: match policy_to_save.mode {
                AgentRuntimeMode::Off => "off",
                AgentRuntimeMode::Shadow => "shadow",
                AgentRuntimeMode::Enforce => "enforce",
            }
            .into(),
            global_active_limit: policy_to_save.global_active_limit as i64,
            per_user_active_limit: policy_to_save.per_user_active_limit as i64,
            per_user_queue_limit: policy_to_save.per_user_queue_limit as i64,
            global_queue_limit: policy_to_save.global_queue_limit as i64,
            queue_timeout_ms: policy_to_save.queue_timeout_ms as i64,
            confirmation_timeout_ms: policy_to_save.confirmation_timeout_ms as i64,
            resident_task_limit: policy_to_save.resident_task_limit as i64,
            resident_idle_timeout_ms: policy_to_save.resident_idle_timeout_ms as i64,
            memory_constrained_percent: policy_to_save.memory_constrained_percent as f64,
            memory_pause_percent: policy_to_save.memory_pause_percent as f64,
            memory_reject_percent: policy_to_save.memory_reject_percent as f64,
        };
        if let Err(error) = self.repo.save_runtime_policy(&row, now_ms()).await {
            error!(error = %error, "failed to persist agent runtime policy");
        }
        self.dispatch_available().await;
        response
    }

    pub fn memory_state(&self, policy: &RuntimePolicy) -> (Option<f32>, MemoryState) {
        let percent = self.memory.used_percent();
        let state = match percent {
            Some(value) if value >= policy.memory_reject_percent => MemoryState::Rejecting,
            Some(value) if value >= policy.memory_pause_percent => MemoryState::Paused,
            Some(value) if value >= policy.memory_constrained_percent => MemoryState::Constrained,
            _ => MemoryState::Normal,
        };
        (percent, state)
    }

    fn effective_limit(&self, policy: &RuntimePolicy, memory: MemoryState) -> usize {
        if !matches!(policy.mode, AgentRuntimeMode::Enforce) {
            return usize::MAX;
        }
        match memory {
            MemoryState::Normal => policy.global_active_limit,
            MemoryState::Constrained => policy.global_active_limit.min(2),
            MemoryState::Paused | MemoryState::Rejecting => 0,
        }
    }

    pub async fn preflight(&self, user_id: &str) -> Result<(), RunAdmissionError> {
        let policy = self.policy();
        let (_, memory) = self.memory_state(&policy);
        if memory == MemoryState::Rejecting {
            return Err(RunAdmissionError::MemoryPressure);
        }
        if !matches!(policy.mode, AgentRuntimeMode::Enforce) {
            return Ok(());
        }
        let state = self.state.lock().await;
        if state.queue.len() >= policy.global_queue_limit {
            return Err(RunAdmissionError::QueueFull);
        }
        let active = state.active.values().filter(|run| run.user_id == user_id).count();
        let queued = state.queue.iter().filter(|run| run.row.user_id == user_id).count();
        if active >= policy.per_user_active_limit && queued >= policy.per_user_queue_limit {
            return Err(RunAdmissionError::UserLimit);
        }
        if queued >= policy.per_user_queue_limit {
            return Err(RunAdmissionError::UserLimit);
        }
        Ok(())
    }

    pub async fn enqueue(&self, input: NewAgentRun) -> Result<AgentRunAdmission, RunAdmissionError> {
        self.preflight(&input.user_id).await?;
        let (tx, rx) = oneshot::channel();
        let row = self
            .repo
            .create_queued(&CreateAgentRunParams {
                id: &input.run_id,
                turn_id: &input.turn_id,
                user_id: &input.user_id,
                conversation_id: &input.conversation_id,
                source: &input.source,
                request_json: &input.request_json,
                message_id: input.message_id.as_deref(),
                queued_at: input.queued_at,
            })
            .await
            .map_err(|error| {
                error!(error = %error, "failed to persist queued agent run");
                RunAdmissionError::Persistence
            })?;
        {
            let mut state = self.state.lock().await;
            if !state.user_ring.iter().any(|user_id| user_id == &row.user_id) {
                state.user_ring.push(row.user_id.clone());
            }
            state.queue.push_back(QueuedRun {
                row: row.clone(),
                permit: tx,
            });
        }
        self.broadcaster.broadcast_to_user(
            &row.user_id,
            WebSocketMessage::new(
                "conversation.runQueued",
                serde_json::json!({
                    "run_id": row.id,
                    "turn_id": row.turn_id,
                    "conversation_id": row.conversation_id,
                    "queued_at": row.queued_at,
                }),
            ),
        );
        self.dispatch_available().await;
        Ok(AgentRunAdmission {
            run_id: input.run_id,
            turn_id: input.turn_id,
            queued_at: input.queued_at,
            permit: rx,
        })
    }

    pub async fn restore(&self, row: AgentRunRow) -> AgentRunAdmission {
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.state.lock().await;
            if !state.user_ring.iter().any(|user_id| user_id == &row.user_id) {
                state.user_ring.push(row.user_id.clone());
            }
            state.queue.push_back(QueuedRun {
                row: row.clone(),
                permit: tx,
            });
        }
        self.dispatch_available().await;
        AgentRunAdmission {
            run_id: row.id,
            turn_id: row.turn_id,
            queued_at: row.queued_at,
            permit: rx,
        }
    }

    async fn dispatch_available(&self) {
        loop {
            let policy = self.policy();
            let (_, memory) = self.memory_state(&policy);
            let effective_limit = self.effective_limit(&policy, memory);
            let next = {
                let mut state = self.state.lock().await;
                if state.active.len() >= effective_limit || state.queue.is_empty() {
                    None
                } else {
                    let active_users: HashSet<&str> = state.active.values().map(|run| run.user_id.as_str()).collect();
                    let start = state
                        .last_dispatched_user
                        .as_ref()
                        .and_then(|last| state.user_ring.iter().position(|user_id| user_id == last))
                        .map(|index| (index + 1) % state.user_ring.len().max(1))
                        .unwrap_or_default();
                    let chosen = (0..state.user_ring.len()).find_map(|offset| {
                        let user_id = &state.user_ring[(start + offset) % state.user_ring.len()];
                        if matches!(policy.mode, AgentRuntimeMode::Enforce) && active_users.contains(user_id.as_str()) {
                            return None;
                        }
                        state.queue.iter().position(|run| &run.row.user_id == user_id)
                    });
                    chosen.and_then(|index| {
                        let run = state.queue.remove(index)?;
                        state.last_dispatched_user = Some(run.row.user_id.clone());
                        state.active.insert(
                            run.row.id.clone(),
                            ActiveRun {
                                user_id: run.row.user_id.clone(),
                            },
                        );
                        Some(run)
                    })
                }
            };
            let Some(run) = next else {
                self.broadcast_queue_positions().await;
                return;
            };
            let started_at = now_ms();
            match self.repo.mark_dispatching(&run.row.id, started_at).await {
                Ok(true) => {
                    info!(
                        run_id = %run.row.id,
                        turn_id = %run.row.turn_id,
                        user_id = %run.row.user_id,
                        "agent run dispatched"
                    );
                    self.broadcaster.broadcast_to_user(
                        &run.row.user_id,
                        WebSocketMessage::new(
                            "conversation.runStarted",
                            serde_json::json!({
                                "run_id": run.row.id,
                                "turn_id": run.row.turn_id,
                                "conversation_id": run.row.conversation_id,
                                "started_at": started_at,
                            }),
                        ),
                    );
                    let _ = run.permit.send(Ok(()));
                }
                Ok(false) | Err(_) => {
                    warn!(run_id = %run.row.id, "queued run could not transition to dispatching");
                    self.state.lock().await.active.remove(&run.row.id);
                    let _ = run.permit.send(Err(RunAdmissionError::Persistence));
                }
            }
        }
    }

    pub async fn mark_running(&self, run_id: &str) {
        if let Err(error) = self.repo.mark_running(run_id, now_ms()).await {
            warn!(run_id, error = %error, "failed to persist running state");
        }
    }

    pub async fn set_effective_model(&self, run_id: &str, model: &str, fallback_used: bool) {
        if let Err(error) = self
            .repo
            .set_effective_model(run_id, model, fallback_used, now_ms())
            .await
        {
            warn!(run_id, error = %error, "failed to persist effective model assignment");
        }
    }

    pub async fn finish(&self, run_id: &str, status: AgentRunStatus, error_code: Option<&str>) {
        let status_str = status_name(&status);
        if let Err(error) = self.repo.mark_terminal(run_id, status_str, error_code, now_ms()).await {
            warn!(run_id, error = %error, "failed to persist terminal agent run state");
        }
        self.state.lock().await.active.remove(run_id);
        self.dispatch_available().await;
    }

    pub async fn cancel_queued(&self, user_id: &str, turn_id: &str) -> Result<bool, RunAdmissionError> {
        let removed = {
            let mut state = self.state.lock().await;
            let index = state
                .queue
                .iter()
                .position(|run| run.row.user_id == user_id && run.row.turn_id == turn_id);
            index.and_then(|index| state.queue.remove(index))
        };
        let Some(run) = removed else {
            return Ok(false);
        };
        self.repo
            .cancel_queued(turn_id, user_id, now_ms())
            .await
            .map_err(|_| RunAdmissionError::Persistence)?;
        let _ = run.permit.send(Err(RunAdmissionError::Cancelled));
        self.broadcast_queue_positions().await;
        Ok(true)
    }

    pub async fn expire_queued(&self) {
        let timeout = self.policy().queue_timeout_ms as i64;
        let deadline = now_ms().saturating_sub(timeout);
        let expired = {
            let mut state = self.state.lock().await;
            let mut expired = Vec::new();
            let mut retained = VecDeque::new();
            while let Some(run) = state.queue.pop_front() {
                if run.row.queued_at <= deadline {
                    expired.push(run);
                } else {
                    retained.push_back(run);
                }
            }
            state.queue = retained;
            expired
        };
        for run in expired {
            let _ = self
                .repo
                .mark_terminal(&run.row.id, "timed_out", Some("RUN_QUEUE_TIMEOUT"), now_ms())
                .await;
            let _ = run.permit.send(Err(RunAdmissionError::QueueTimeout));
            self.broadcaster.broadcast_to_user(
                &run.row.user_id,
                WebSocketMessage::new(
                    "conversation.runTimedOut",
                    serde_json::json!({
                        "run_id": run.row.id,
                        "turn_id": run.row.turn_id,
                        "conversation_id": run.row.conversation_id,
                        "error_code": "RUN_QUEUE_TIMEOUT",
                    }),
                ),
            );
        }
        self.dispatch_available().await;
    }

    pub fn start_maintenance(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let scheduler = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                scheduler.expire_queued().await;
            }
        })
    }

    pub async fn recover_after_restart(&self) -> Result<Vec<AgentRunRow>, RunAdmissionError> {
        let interrupted = self
            .repo
            .fail_interrupted(now_ms())
            .await
            .map_err(|_| RunAdmissionError::Persistence)?;
        if interrupted > 0 {
            warn!(
                interrupted,
                error_code = "BACKEND_RESTARTED",
                "interrupted agent runs marked failed"
            );
        }
        self.repo
            .list_queued()
            .await
            .map_err(|_| RunAdmissionError::Persistence)
    }

    pub async fn list_for_user(&self, user_id: &str) -> Result<Vec<AgentRunResponse>, RunAdmissionError> {
        let rows = self
            .repo
            .list_for_user(user_id)
            .await
            .map_err(|_| RunAdmissionError::Persistence)?;
        let state = self.state.lock().await;
        Ok(rows.into_iter().map(|row| row_to_response(row, &state.queue)).collect())
    }

    pub async fn queue_details(&self, turn_id: &str) -> Option<(u32, u64)> {
        let state = self.state.lock().await;
        let position = state.queue.iter().position(|run| run.row.turn_id == turn_id)? + 1;
        let limit = self.policy().global_active_limit.max(1);
        let waves = position.div_ceil(limit);
        Some((position as u32, waves as u64 * DEFAULT_TURN_ESTIMATE_MS))
    }

    pub async fn status(&self, resident_task_count: usize) -> AgentRuntimeStatusResponse {
        let policy = self.policy();
        let (percent, memory) = self.memory_state(&policy);
        let state = self.state.lock().await;
        AgentRuntimeStatusResponse {
            memory_used_percent: percent,
            memory_state: memory.as_str().into(),
            configured_active_limit: policy.global_active_limit,
            effective_active_limit: self.effective_limit(&policy, memory),
            active_count: state.active.len(),
            queued_count: state.queue.len(),
            resident_task_count,
        }
    }

    async fn broadcast_queue_positions(&self) {
        let updates = {
            let state = self.state.lock().await;
            let limit = self.policy().global_active_limit.max(1);
            state
                .queue
                .iter()
                .enumerate()
                .map(|(index, run)| {
                    let position = index + 1;
                    (
                        run.row.user_id.clone(),
                        run.row.conversation_id.clone(),
                        run.row.turn_id.clone(),
                        position,
                        position.div_ceil(limit) as u64 * DEFAULT_TURN_ESTIMATE_MS,
                    )
                })
                .collect::<Vec<_>>()
        };
        for (user_id, conversation_id, turn_id, position, estimated_wait_ms) in updates {
            self.broadcaster.broadcast_to_user(
                &user_id,
                WebSocketMessage::new(
                    "conversation.queueUpdated",
                    serde_json::json!({
                        "conversation_id": conversation_id,
                        "turn_id": turn_id,
                        "queue_position": position,
                        "estimated_wait_ms": estimated_wait_ms,
                    }),
                ),
            );
        }
    }
}

fn row_to_response(row: AgentRunRow, queue: &VecDeque<QueuedRun>) -> AgentRunResponse {
    let queue_position = queue
        .iter()
        .position(|queued| queued.row.id == row.id)
        .map(|index| index as u32 + 1);
    AgentRunResponse {
        id: row.id,
        conversation_id: row.conversation_id,
        turn_id: row.turn_id,
        source: row.source,
        status: parse_status(&row.status),
        estimated_wait_ms: queue_position.map(|position| position as u64 * DEFAULT_TURN_ESTIMATE_MS),
        queue_position,
        effective_model: row.effective_model,
        fallback_used: row.fallback_used,
        error_code: row.error_code,
        queued_at: row.queued_at,
        started_at: row.started_at,
        finished_at: row.finished_at,
    }
}

fn parse_status(status: &str) -> AgentRunStatus {
    match status {
        "queued" => AgentRunStatus::Queued,
        "dispatching" => AgentRunStatus::Dispatching,
        "running" => AgentRunStatus::Running,
        "completed" => AgentRunStatus::Completed,
        "cancelled" => AgentRunStatus::Cancelled,
        "timed_out" => AgentRunStatus::TimedOut,
        _ => AgentRunStatus::Failed,
    }
}

fn status_name(status: &AgentRunStatus) -> &'static str {
    match status {
        AgentRunStatus::Queued => "queued",
        AgentRunStatus::Dispatching => "dispatching",
        AgentRunStatus::Running => "running",
        AgentRunStatus::Completed => "completed",
        AgentRunStatus::Failed => "failed",
        AgentRunStatus::Cancelled => "cancelled",
        AgentRunStatus::TimedOut => "timed_out",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::models::ConversationRow;
    use aionui_db::{
        IConversationRepository, IUserRepository, SqliteAgentRunRepository, SqliteConversationRepository,
        SqliteUserRepository, init_database_memory,
    };
    use aionui_realtime::BroadcastEventBus;

    struct FixedMemory(f32);

    impl MemoryPressureSource for FixedMemory {
        fn used_percent(&self) -> Option<f32> {
            Some(self.0)
        }
    }

    async fn scheduler_with_users(memory: f32) -> (Arc<AgentRunScheduler>, aionui_db::Database, String, String) {
        let database = init_database_memory().await.unwrap();
        let users = SqliteUserRepository::new(database.pool().clone());
        let user_1 = users.create_user("scheduler-user-1", "hash").await.unwrap();
        let user_2 = users.create_user("scheduler-user-2", "hash").await.unwrap();
        let conversations = SqliteConversationRepository::new(database.pool().clone());
        for (id, user_id) in [("conv-1", &user_1.id), ("conv-2", &user_1.id), ("conv-3", &user_2.id)] {
            conversations
                .create(&ConversationRow {
                    id: id.into(),
                    user_id: user_id.clone(),
                    name: id.into(),
                    r#type: "aionrs".into(),
                    extra: "{}".into(),
                    model: None,
                    status: Some("finished".into()),
                    source: None,
                    channel_chat_id: None,
                    pinned: false,
                    pinned_at: None,
                    created_at: now_ms(),
                    updated_at: now_ms(),
                })
                .await
                .unwrap();
        }
        let scheduler = AgentRunScheduler::with_memory_source(
            Arc::new(SqliteAgentRunRepository::new(database.pool().clone())),
            Arc::new(BroadcastEventBus::new(64)),
            Arc::new(FixedMemory(memory)),
        );
        scheduler
            .update_policy(UpdateAgentRuntimePolicyRequest {
                mode: Some(AgentRuntimeMode::Enforce),
                global_active_limit: Some(1),
                per_user_active_limit: None,
                per_user_queue_limit: None,
                global_queue_limit: None,
                queue_timeout_ms: None,
                confirmation_timeout_ms: None,
                resident_task_limit: None,
                resident_idle_timeout_ms: None,
                memory_constrained_percent: None,
                memory_pause_percent: None,
                memory_reject_percent: None,
            })
            .await;
        (scheduler, database, user_1.id, user_2.id)
    }

    fn run(id: &str, turn: &str, user_id: &str, conversation_id: &str, queued_at: i64) -> NewAgentRun {
        NewAgentRun {
            run_id: id.into(),
            turn_id: turn.into(),
            user_id: user_id.into(),
            conversation_id: conversation_id.into(),
            source: "conversation".into(),
            request_json: r#"{"content":"hello"}"#.into(),
            message_id: None,
            queued_at,
        }
    }

    #[test]
    fn default_policy_allows_six_active_turns() {
        assert_eq!(RuntimePolicy::default().global_active_limit, 6);
    }

    #[tokio::test]
    async fn round_robin_runs_waiting_user_before_same_users_second_turn() {
        let (scheduler, _database, user_1, user_2) = scheduler_with_users(20.0).await;
        let first = scheduler.enqueue(run("r1", "t1", &user_1, "conv-1", 1)).await.unwrap();
        assert_eq!(first.permit.await.unwrap(), Ok(()));
        let mut second = scheduler.enqueue(run("r2", "t2", &user_1, "conv-2", 2)).await.unwrap();
        let other = scheduler.enqueue(run("r3", "t3", &user_2, "conv-3", 3)).await.unwrap();

        scheduler.finish("r1", AgentRunStatus::Completed, None).await;
        assert_eq!(other.permit.await.unwrap(), Ok(()));
        assert!(second.permit.try_recv().is_err());
        scheduler.finish("r3", AgentRunStatus::Completed, None).await;
        assert_eq!(second.permit.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn rejects_more_than_one_queued_turn_for_a_user() {
        let (scheduler, _database, user_1, _user_2) = scheduler_with_users(20.0).await;
        let first = scheduler.enqueue(run("r1", "t1", &user_1, "conv-1", 1)).await.unwrap();
        assert_eq!(first.permit.await.unwrap(), Ok(()));
        let _queued = scheduler.enqueue(run("r2", "t2", &user_1, "conv-2", 2)).await.unwrap();
        let error = scheduler
            .enqueue(run("r3", "t3", &user_1, "conv-2", 3))
            .await
            .err()
            .unwrap();
        assert_eq!(error, RunAdmissionError::UserLimit);
    }

    #[tokio::test]
    async fn queued_turn_can_be_cancelled_by_owner_turn_id() {
        let (scheduler, _database, user_1, _user_2) = scheduler_with_users(20.0).await;
        let first = scheduler
            .enqueue(run("r1", "t1", &user_1, "conv-1", now_ms()))
            .await
            .unwrap();
        assert_eq!(first.permit.await.unwrap(), Ok(()));
        let queued = scheduler
            .enqueue(run("r2", "t2", &user_1, "conv-2", now_ms()))
            .await
            .unwrap();

        assert!(scheduler.cancel_queued(&user_1, "t2").await.unwrap());
        assert_eq!(queued.permit.await.unwrap(), Err(RunAdmissionError::Cancelled));
        let persisted = scheduler.repo.find_by_turn_id("t2").await.unwrap().unwrap();
        assert_eq!(persisted.status, "cancelled");
    }

    #[tokio::test]
    async fn queued_turn_expires_without_replaying_its_message() {
        let (scheduler, _database, user_1, _user_2) = scheduler_with_users(20.0).await;
        let first = scheduler
            .enqueue(run("r1", "t1", &user_1, "conv-1", now_ms()))
            .await
            .unwrap();
        assert_eq!(first.permit.await.unwrap(), Ok(()));
        let queued = scheduler.enqueue(run("r2", "t2", &user_1, "conv-2", 1)).await.unwrap();

        scheduler.expire_queued().await;
        assert_eq!(queued.permit.await.unwrap(), Err(RunAdmissionError::QueueTimeout));
        let persisted = scheduler.repo.find_by_turn_id("t2").await.unwrap().unwrap();
        assert_eq!(persisted.status, "timed_out");
        assert_eq!(persisted.error_code.as_deref(), Some("RUN_QUEUE_TIMEOUT"));
    }

    #[tokio::test]
    async fn memory_pressure_changes_effective_dispatch_limit_without_killing_active_runs() {
        for (memory, expected_state, expected_limit) in
            [(80.0, "constrained", 2), (87.0, "paused", 0), (91.0, "rejecting", 0)]
        {
            let (scheduler, _database, user_1, _user_2) = scheduler_with_users(memory).await;
            scheduler
                .update_policy(UpdateAgentRuntimePolicyRequest {
                    mode: Some(AgentRuntimeMode::Enforce),
                    global_active_limit: Some(4),
                    per_user_active_limit: None,
                    per_user_queue_limit: None,
                    global_queue_limit: None,
                    queue_timeout_ms: None,
                    confirmation_timeout_ms: None,
                    resident_task_limit: None,
                    resident_idle_timeout_ms: None,
                    memory_constrained_percent: None,
                    memory_pause_percent: None,
                    memory_reject_percent: None,
                })
                .await;
            let status = scheduler.status(0).await;
            assert_eq!(status.memory_state, expected_state);
            assert_eq!(status.effective_active_limit, expected_limit);
            if memory >= 90.0 {
                assert_eq!(
                    scheduler.preflight(&user_1).await,
                    Err(RunAdmissionError::MemoryPressure)
                );
            }
        }
    }

    #[tokio::test]
    async fn administrator_memory_thresholds_change_admission_state() {
        let (scheduler, _database, _user_1, _user_2) = scheduler_with_users(72.0).await;
        let policy = scheduler
            .update_policy(UpdateAgentRuntimePolicyRequest {
                mode: None,
                global_active_limit: None,
                per_user_active_limit: None,
                per_user_queue_limit: None,
                global_queue_limit: None,
                queue_timeout_ms: None,
                confirmation_timeout_ms: None,
                resident_task_limit: None,
                resident_idle_timeout_ms: None,
                memory_constrained_percent: Some(60.0),
                memory_pause_percent: Some(70.0),
                memory_reject_percent: Some(80.0),
            })
            .await;
        assert_eq!(policy.memory_pause_percent, 70.0);
        let status = scheduler.status(0).await;
        assert_eq!(status.memory_state, "paused");
        assert_eq!(status.effective_active_limit, 0);
    }

    #[tokio::test]
    async fn never_dispatches_more_than_four_real_background_turns() {
        let (scheduler, database, user_1, user_2) = scheduler_with_users(20.0).await;
        let users = SqliteUserRepository::new(database.pool().clone());
        let conversations = SqliteConversationRepository::new(database.pool().clone());
        let mut user_conversations = vec![(user_1, "conv-1".to_owned()), (user_2, "conv-3".to_owned())];
        for index in 4..=6 {
            let user = users
                .create_user(&format!("scheduler-user-{index}"), "hash")
                .await
                .unwrap();
            let conversation_id = format!("conv-{index}");
            conversations
                .create(&ConversationRow {
                    id: conversation_id.clone(),
                    user_id: user.id.clone(),
                    name: conversation_id.clone(),
                    r#type: "aionrs".into(),
                    extra: "{}".into(),
                    model: None,
                    status: Some("finished".into()),
                    source: None,
                    channel_chat_id: None,
                    pinned: false,
                    pinned_at: None,
                    created_at: now_ms(),
                    updated_at: now_ms(),
                })
                .await
                .unwrap();
            user_conversations.push((user.id, conversation_id));
        }
        scheduler
            .update_policy(UpdateAgentRuntimePolicyRequest {
                mode: Some(AgentRuntimeMode::Enforce),
                global_active_limit: Some(4),
                per_user_active_limit: Some(1),
                per_user_queue_limit: Some(1),
                global_queue_limit: Some(20),
                queue_timeout_ms: None,
                confirmation_timeout_ms: None,
                resident_task_limit: None,
                resident_idle_timeout_ms: None,
                memory_constrained_percent: None,
                memory_pause_percent: None,
                memory_reject_percent: None,
            })
            .await;
        let mut first = scheduler
            .enqueue(run("r0", "t0", &user_conversations[0].0, &user_conversations[0].1, 0))
            .await
            .unwrap();
        assert_eq!(first.permit.try_recv(), Ok(Ok(())));
        let mut repeat_user = scheduler
            .enqueue(run("r-repeat", "t-repeat", &user_conversations[0].0, "conv-2", 1))
            .await
            .unwrap();

        let mut admissions = Vec::new();
        for (index, (user_id, conversation_id)) in user_conversations.iter().enumerate().skip(1) {
            admissions.push(
                scheduler
                    .enqueue(run(
                        &format!("r{index}"),
                        &format!("t{index}"),
                        user_id,
                        conversation_id,
                        index as i64,
                    ))
                    .await
                    .unwrap(),
            );
        }
        for admission in admissions.iter_mut().take(3) {
            assert_eq!(admission.permit.try_recv(), Ok(Ok(())));
        }
        assert!(admissions[3].permit.try_recv().is_err());
        assert!(repeat_user.permit.try_recv().is_err());
        let next_new_user = admissions.pop().unwrap();
        assert_eq!(scheduler.status(0).await.active_count, 4);

        scheduler.finish("r0", AgentRunStatus::Completed, None).await;
        assert_eq!(next_new_user.permit.await.unwrap(), Ok(()));
        assert!(repeat_user.permit.try_recv().is_err());
        assert_eq!(scheduler.status(0).await.active_count, 4);

        scheduler.finish("r4", AgentRunStatus::Completed, None).await;
        assert_eq!(repeat_user.permit.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn critical_memory_rejects_without_persisting() {
        let (scheduler, _database, user_1, _user_2) = scheduler_with_users(91.0).await;
        let error = scheduler
            .enqueue(run("r1", "t1", &user_1, "conv-1", 1))
            .await
            .err()
            .unwrap();
        assert_eq!(error, RunAdmissionError::MemoryPressure);
        assert!(scheduler.list_for_user(&user_1).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn restart_fails_started_runs_and_restores_only_queued_rows() {
        let (scheduler, database, user_1, _user_2) = scheduler_with_users(20.0).await;
        let first = scheduler.enqueue(run("r1", "t1", &user_1, "conv-1", 1)).await.unwrap();
        assert_eq!(first.permit.await.unwrap(), Ok(()));
        scheduler.mark_running("r1").await;
        let _queued = scheduler.enqueue(run("r2", "t2", &user_1, "conv-2", 2)).await.unwrap();

        let recovered = AgentRunScheduler::with_memory_source(
            Arc::new(SqliteAgentRunRepository::new(database.pool().clone())),
            Arc::new(BroadcastEventBus::new(16)),
            Arc::new(FixedMemory(20.0)),
        )
        .recover_after_restart()
        .await
        .unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].id, "r2");
        let row = SqliteAgentRunRepository::new(database.pool().clone())
            .find_by_turn_id("t1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.error_code.as_deref(), Some("BACKEND_RESTARTED"));
    }
}
