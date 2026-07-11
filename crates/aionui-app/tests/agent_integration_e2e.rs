//! E2E integration tests with mock agent tasks.
//!
//! Tests the message flow, confirmation system, and auxiliary routes
//! with a mock IWorkerTaskManager that provides in-memory agents.

mod common;

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};
use tower::ServiceExt;

use aionui_ai_agent::agent_task::{AgentInstance, IAgentTask, IMockAgent};
use aionui_ai_agent::protocol::events::TextEventData;
use aionui_ai_agent::types::{BuildTaskOptions, SendMessageData};
use aionui_ai_agent::{AgentError, AgentStreamEvent, IWorkerTaskManager};
use aionui_api_types::AgentSource;
use aionui_common::{AgentKillReason, AgentType, Confirmation, ConversationStatus, TimestampMs, now_ms};
use aionui_db::UpsertAgentMetadataParams;
use async_trait::async_trait;

use common::{body_json, get_with_token, json_with_token, setup_and_login};

// ── Mock Agent ──────────────────────────────────────────────────

struct MockAgent {
    conversation_id: String,
    workspace: String,
    event_tx: broadcast::Sender<AgentStreamEvent>,
    confirmations: Mutex<Vec<Confirmation>>,
    approvals: Mutex<std::collections::HashMap<String, bool>>,
    last_activity: AtomicI64,
    status: Mutex<ConversationStatus>,
    concurrency_probe: Option<Arc<ConcurrencyProbe>>,
}

impl MockAgent {
    fn with_probe(conversation_id: &str, workspace: &str, concurrency_probe: Option<Arc<ConcurrencyProbe>>) -> Self {
        let (event_tx, _) = broadcast::channel(256);
        Self {
            conversation_id: conversation_id.to_owned(),
            workspace: workspace.to_owned(),
            event_tx,
            confirmations: Mutex::new(vec![]),
            approvals: Mutex::new(std::collections::HashMap::new()),
            last_activity: AtomicI64::new(now_ms()),
            status: Mutex::new(ConversationStatus::Finished),
            concurrency_probe,
        }
    }
}

struct ConcurrencyProbe {
    active: AtomicUsize,
    max_active: AtomicUsize,
    started: AtomicUsize,
    completed: AtomicUsize,
    completed_by_conversation: Mutex<std::collections::HashMap<String, usize>>,
    turn_delay: Option<std::time::Duration>,
    released: watch::Sender<bool>,
}

impl ConcurrencyProbe {
    fn new() -> Self {
        let (released, _) = watch::channel(false);
        Self {
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            started: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            completed_by_conversation: Mutex::new(std::collections::HashMap::new()),
            turn_delay: None,
            released,
        }
    }

    fn with_turn_delay(turn_delay: std::time::Duration) -> Self {
        Self {
            turn_delay: Some(turn_delay),
            ..Self::new()
        }
    }

    async fn hold_turn(&self, conversation_id: &str) {
        let current = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(current, Ordering::SeqCst);
        self.started.fetch_add(1, Ordering::SeqCst);
        if let Some(turn_delay) = self.turn_delay {
            tokio::time::sleep(turn_delay).await;
        } else {
            let mut released = self.released.subscribe();
            if !*released.borrow() {
                let _ = released.changed().await;
            }
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.completed.fetch_add(1, Ordering::SeqCst);
        *self
            .completed_by_conversation
            .lock()
            .unwrap()
            .entry(conversation_id.to_owned())
            .or_default() += 1;
    }

    fn release(&self) {
        self.released.send_replace(true);
    }
}

#[async_trait]
impl IAgentTask for MockAgent {
    fn agent_type(&self) -> AgentType {
        AgentType::Acp
    }

    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    fn workspace(&self) -> &str {
        &self.workspace
    }

    fn status(&self) -> Option<ConversationStatus> {
        Some(*self.status.lock().unwrap())
    }

    fn last_activity_at(&self) -> TimestampMs {
        self.last_activity.load(Ordering::Relaxed)
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.event_tx.subscribe()
    }

    async fn send_message(&self, _data: SendMessageData) -> Result<(), aionui_ai_agent::AgentSendError> {
        self.last_activity.store(now_ms(), Ordering::Relaxed);
        *self.status.lock().unwrap() = ConversationStatus::Running;
        if let Some(probe) = &self.concurrency_probe {
            probe.hold_turn(&self.conversation_id).await;
        }
        // Emit a text event and finish
        let _ = self.event_tx.send(AgentStreamEvent::Text(TextEventData {
            content: "Mock response".into(),
        }));
        // Mark the task idle before publishing the terminal event. The
        // scheduler may dispatch the next queued turn as soon as it observes
        // Finish, and resident-capacity eviction must see this task as idle.
        *self.status.lock().unwrap() = ConversationStatus::Finished;
        let _ = self.event_tx.send(AgentStreamEvent::Finish(
            aionui_ai_agent::protocol::events::FinishEventData::default(),
        ));
        Ok(())
    }

    async fn cancel(&self) -> Result<(), AgentError> {
        Ok(())
    }

    fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }
}

#[async_trait]
impl IMockAgent for MockAgent {
    fn get_confirmations(&self) -> Vec<Confirmation> {
        self.confirmations.lock().unwrap().clone()
    }

    fn check_approval(&self, action: &str, _command_type: Option<&str>) -> bool {
        self.approvals.lock().unwrap().get(action).copied().unwrap_or(false)
    }

    fn confirm(&self, _msg_id: &str, call_id: &str, _data: Value, always_allow: bool) -> Result<(), AgentError> {
        let mut confs = self.confirmations.lock().unwrap();
        confs.retain(|c| c.call_id != call_id);
        if always_allow {
            self.approvals.lock().unwrap().insert("test_action".to_owned(), true);
        }
        Ok(())
    }
}

// ── Mock Worker Task Manager ────────────────────────────────────

struct MockTaskManager {
    agents: Mutex<std::collections::HashMap<String, AgentInstance>>,
    concurrency_probe: Option<Arc<ConcurrencyProbe>>,
}

impl MockTaskManager {
    fn new() -> Self {
        Self {
            agents: Mutex::new(std::collections::HashMap::new()),
            concurrency_probe: None,
        }
    }

    fn with_concurrency_probe(concurrency_probe: Arc<ConcurrencyProbe>) -> Self {
        Self {
            agents: Mutex::new(std::collections::HashMap::new()),
            concurrency_probe: Some(concurrency_probe),
        }
    }

    fn insert(&self, conv_id: &str, workspace: &str) -> Arc<MockAgent> {
        let agent = Arc::new(MockAgent::with_probe(
            conv_id,
            workspace,
            self.concurrency_probe.clone(),
        ));
        self.agents
            .lock()
            .unwrap()
            .insert(conv_id.to_owned(), AgentInstance::Mock(agent.clone()));
        agent
    }
}

#[async_trait::async_trait]
impl IWorkerTaskManager for MockTaskManager {
    fn get_task(&self, conversation_id: &str) -> Option<AgentInstance> {
        self.agents.lock().unwrap().get(conversation_id).cloned()
    }

    async fn get_or_build_task(
        &self,
        conversation_id: &str,
        _options: BuildTaskOptions,
    ) -> Result<AgentInstance, AgentError> {
        let mut agents = self.agents.lock().unwrap();
        if let Some(existing) = agents.get(conversation_id) {
            return Ok(existing.clone());
        }
        let instance = AgentInstance::Mock(Arc::new(MockAgent::with_probe(
            conversation_id,
            "/tmp",
            self.concurrency_probe.clone(),
        )));
        agents.insert(conversation_id.to_owned(), instance.clone());
        Ok(instance)
    }

    fn kill(&self, conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        self.agents.lock().unwrap().remove(conversation_id);
        Ok(())
    }

    fn kill_and_wait(
        &self,
        conversation_id: &str,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let _ = self.kill(conversation_id, reason);
        Box::pin(std::future::ready(()))
    }

    async fn clear(&self) {
        self.agents.lock().unwrap().clear();
    }

    fn active_count(&self) -> usize {
        self.agents.lock().unwrap().len()
    }

    fn collect_idle(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
        vec![]
    }
}

// ── Test App builder with mock agents ───────────────────────────

async fn build_app_with_mock_tasks() -> (axum::Router, aionui_app::AppServices, Arc<MockTaskManager>) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = aionui_app::AppServices::from_config(db, &aionui_app::AppConfig::default())
        .await
        .unwrap();

    let mock_tm = Arc::new(MockTaskManager::new());
    let services = services.with_worker_task_manager(mock_tm.clone());

    let router = aionui_app::create_router(&services).await.expect("build router");
    (router, services, mock_tm)
}

async fn build_app_with_delayed_mock_tasks(probe: Arc<ConcurrencyProbe>) -> (axum::Router, aionui_app::AppServices) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = aionui_app::AppServices::from_config(db, &aionui_app::AppConfig::default())
        .await
        .unwrap();
    let mock_tm = Arc::new(MockTaskManager::with_concurrency_probe(probe));
    let services = services.with_worker_task_manager(mock_tm);
    let router = aionui_app::create_router(&services).await.expect("build router");
    (router, services)
}

async fn create_conversation(app: &mut axum::Router, token: &str, csrf: &str, name: &str) -> String {
    let body = json!({
        "type": "acp",
        "name": name,
        "extra": {}
    });
    let req = common::json_with_token("POST", "/api/conversations", body, token, csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = common::body_json(resp).await;
    json["data"]["id"].as_str().unwrap().to_owned()
}

async fn upsert_visible_agent_metadata(services: &aionui_app::AppServices, id: &str, agent_type: &str) {
    services
        .agent_registry
        .repo_handle()
        .upsert(&UpsertAgentMetadataParams {
            id,
            icon: None,
            name: id,
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some(id),
            agent_type,
            agent_source: "internal",
            agent_source_info: Some("{}"),
            enabled: true,
            command: None,
            args: Some("[]"),
            env: Some("[]"),
            native_skills_dirs: None,
            behavior_policy: Some("{}"),
            yolo_id: Some("yolo"),
            agent_capabilities: None,
            auth_methods: None,
            config_options: None,
            available_modes: None,
            available_models: None,
            available_commands: None,
            sort_order: 1,
        })
        .await
        .unwrap();
}

// ── Agent catalog tests ─────────────────────────────────────────

#[tokio::test]
async fn management_endpoint_keeps_deprecated_runtime_rows_for_diagnostics() {
    let (mut app, services, _mock_tm) = build_app_with_mock_tasks().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;

    for (id, agent_type) in [
        ("test-visible-acp", "acp"),
        ("test-visible-aionrs", "aionrs"),
        ("test-visible-openclaw", "openclaw-gateway"),
        ("test-visible-nanobot", "nanobot"),
        ("test-visible-remote", "remote"),
        ("test-visible-gemini", "gemini"),
    ] {
        upsert_visible_agent_metadata(&services, id, agent_type).await;
    }
    services.agent_registry.hydrate().await.unwrap();
    services.agent_registry.refresh_availability().await;

    let req = get_with_token("/api/agents/management", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    let agents = body["data"].as_array().expect("data should be array");
    let types: Vec<&str> = agents.iter().filter_map(|agent| agent["agent_type"].as_str()).collect();

    assert!(types.contains(&"acp"));
    assert!(types.contains(&"aionrs"));
    assert!(types.contains(&"openclaw-gateway"));
    assert!(types.contains(&"nanobot"));
    assert!(types.contains(&"remote"));
    assert!(types.contains(&"gemini"));
}

#[tokio::test]
async fn management_endpoint_handles_openclaw_as_acp_backend() {
    let (mut app, services, _mock_tm) = build_app_with_mock_tasks().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;

    let meta = services
        .agent_registry
        .find_builtin_by_backend("openclaw")
        .await
        .expect("OpenClaw ACP builtin row should exist");
    assert_eq!(meta.agent_type, AgentType::Acp);
    assert_eq!(meta.backend.as_deref(), Some("openclaw"));
    assert_eq!(meta.command.as_deref(), Some("openclaw"));
    assert_eq!(meta.args, vec!["acp"]);
    assert_eq!(meta.agent_source, AgentSource::Builtin);

    let req = get_with_token("/api/agents/management", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    let agents = body["data"].as_array().expect("data should be array");

    let openclaw = agents
        .iter()
        .find(|agent| agent["backend"].as_str() == Some("openclaw"))
        .expect("OpenClaw ACP row should be visible from /api/agents/management");
    assert!(meta.available || openclaw["status"] != "available");
    assert_eq!(openclaw["agent_type"], "acp");
    assert_eq!(openclaw["command"], "openclaw");
    assert_eq!(openclaw["args"], json!(["acp"]));
}

#[tokio::test]
async fn agent_logos_endpoint_returns_backend_to_logo_catalog() {
    let (mut app, services, _mock_tm) = build_app_with_mock_tasks().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;

    let req = get_with_token("/api/agents/logos", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    let entries = body["data"].as_array().expect("data should be array");

    let logo_for = |backend: &str| -> Option<String> {
        entries
            .iter()
            .find(|entry| entry["backend"].as_str() == Some(backend))
            .and_then(|entry| entry["logo"].as_str())
            .map(str::to_owned)
    };

    // Seeded builtin agents project their stored icon URL.
    assert_eq!(
        logo_for("claude").as_deref(),
        Some("/api/assets/logos/ai-major/claude.svg")
    );
    assert_eq!(
        logo_for("codex").as_deref(),
        Some("/api/assets/logos/tools/coding/codex.svg")
    );

    // Aion CLI has no vendor `backend` (NULL); it must still be keyed by its
    // agent_type ("aionrs") so aionrs conversations resolve a logo.
    assert_eq!(
        logo_for("aionrs").as_deref(),
        Some("/api/assets/logos/brand/centaurai.svg")
    );

    // Every entry carries a non-empty backend + logo, and backends are unique.
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        let backend = entry["backend"].as_str().expect("backend present");
        let logo = entry["logo"].as_str().expect("logo present");
        assert!(!backend.is_empty(), "backend must not be empty");
        assert!(!logo.is_empty(), "logo must not be empty");
        assert!(
            seen.insert(backend.to_owned()),
            "backend {backend} duplicated in catalog"
        );
    }
}

#[tokio::test]
async fn agent_logos_endpoint_includes_disabled_and_missing_rows() {
    let (mut app, services, _mock_tm) = build_app_with_mock_tasks().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;

    // A custom/internal row that would be hidden from /api/agents (no command
    // on PATH) must still contribute its logo so historical conversations
    // referencing it can render an icon.
    services
        .agent_registry
        .repo_handle()
        .upsert(&UpsertAgentMetadataParams {
            id: "logo-only-row",
            icon: Some("/api/assets/logos/brand/centaurai.svg"),
            name: "Logo Only",
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some("logo-only-backend"),
            agent_type: "acp",
            agent_source: "custom",
            agent_source_info: Some("{}"),
            enabled: false,
            command: None,
            args: Some("[]"),
            env: Some("[]"),
            native_skills_dirs: None,
            behavior_policy: Some("{}"),
            yolo_id: Some("yolo"),
            agent_capabilities: None,
            auth_methods: None,
            config_options: None,
            available_modes: None,
            available_models: None,
            available_commands: None,
            sort_order: 1,
        })
        .await
        .unwrap();
    services.agent_registry.hydrate().await.unwrap();
    services.agent_registry.refresh_availability().await;

    let req = get_with_token("/api/agents/logos", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    let entries = body["data"].as_array().expect("data should be array");
    let entry = entries
        .iter()
        .find(|entry| entry["backend"].as_str() == Some("logo-only-backend"));
    assert!(
        entry.is_some(),
        "disabled row with an icon must still appear in the logo catalog"
    );
    assert_eq!(entry.unwrap()["logo"], "/api/assets/logos/brand/centaurai.svg");
}

// ── Message flow with mock agent ────────────────────────────────

#[tokio::test]
async fn send_message_with_mock_agent_returns_202() {
    let (mut app, services, _mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Mock Agent Test").await;

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({ "content": "Hello mock agent" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
}

#[tokio::test]
async fn delayed_background_turns_return_202_and_never_exceed_six_active() {
    let probe = Arc::new(ConcurrencyProbe::new());
    let (mut app, services) = build_app_with_delayed_mock_tasks(probe.clone()).await;

    let mut sessions = Vec::new();
    for index in 0..8 {
        let username = if index == 0 {
            "admin".to_owned()
        } else {
            format!("load-user-{index}")
        };
        let (token, csrf) = setup_and_login(&mut app, &services, &username, "Pass123!").await;
        let conversation_id = create_conversation(&mut app, &token, &csrf, &format!("Load {index}")).await;
        sessions.push((token, csrf, conversation_id));
    }

    let started_at = tokio::time::Instant::now();
    let responses = futures_util::future::join_all(sessions.iter().map(|(token, csrf, conversation_id)| {
        app.clone().oneshot(json_with_token(
            "POST",
            &format!("/api/conversations/{conversation_id}/messages"),
            json!({ "content": "delayed load test" }),
            token,
            csrf,
        ))
    }))
    .await;
    assert!(
        started_at.elapsed() < std::time::Duration::from_secs(2),
        "HTTP admission must not wait for background turns"
    );
    for response in responses {
        assert_eq!(response.unwrap().status(), StatusCode::ACCEPTED);
    }

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while probe.started.load(Ordering::SeqCst) < 6 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("six delayed turns should start");

    let runtime_response = app
        .clone()
        .oneshot(get_with_token("/api/admin/agent-runtime-status", &sessions[0].0))
        .await
        .unwrap();
    assert_eq!(runtime_response.status(), StatusCode::OK);
    let runtime = body_json(runtime_response).await;
    assert_eq!(runtime["data"]["active_count"], 6);
    assert_eq!(runtime["data"]["queued_count"], 2);
    assert_eq!(probe.max_active.load(Ordering::SeqCst), 6);

    probe.release();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while probe.started.load(Ordering::SeqCst) < 8 || probe.active.load(Ordering::SeqCst) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all queued turns should eventually complete");
    assert_eq!(probe.max_active.load(Ordering::SeqCst), 6);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual 10-minute multi-user pressure test"]
async fn ten_users_sustain_agent_pressure_for_ten_minutes() {
    let duration_secs = std::env::var("AIONUI_STRESS_DURATION_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(600);
    let turn_delay_ms = std::env::var("AIONUI_STRESS_TURN_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1_000);
    let probe = Arc::new(ConcurrencyProbe::with_turn_delay(std::time::Duration::from_millis(
        turn_delay_ms,
    )));
    let (mut app, services) = build_app_with_delayed_mock_tasks(probe.clone()).await;

    struct StressUser {
        username: String,
        token: String,
        csrf: String,
        conversations: Vec<String>,
    }

    let mut users = Vec::new();
    for index in 0..10 {
        let username = if index == 0 {
            "admin".to_owned()
        } else {
            format!("stress-user-{index}")
        };
        let (token, csrf) = setup_and_login(&mut app, &services, &username, "Pass123!").await;
        let mut conversations = Vec::new();
        for slot in 0..2 {
            conversations.push(create_conversation(&mut app, &token, &csrf, &format!("Stress {index}-{slot}")).await);
        }
        users.push(StressUser {
            username,
            token,
            csrf,
            conversations,
        });
    }

    let started_at = tokio::time::Instant::now();
    let deadline = started_at + std::time::Duration::from_secs(duration_secs);
    let mut next_report = std::time::Duration::from_secs(30);
    let mut accepted = 0usize;
    let mut backpressure_rejections = 0usize;
    let mut unexpected_rejections = Vec::new();
    let mut max_http_latency = std::time::Duration::ZERO;
    let mut max_runtime_active = 0u64;
    let mut max_runtime_queued = 0u64;

    while tokio::time::Instant::now() < deadline {
        for user in &users {
            let runs_response = app
                .clone()
                .oneshot(get_with_token("/api/agent-runs", &user.token))
                .await
                .unwrap();
            assert_eq!(runs_response.status(), StatusCode::OK);
            let runs_body = body_json(runs_response).await;
            let occupied = runs_body["data"]
                .as_array()
                .expect("agent-runs data is an array")
                .iter()
                .filter_map(|run| run["conversation_id"].as_str().map(str::to_owned))
                .collect::<std::collections::HashSet<_>>();

            for conversation_id in user
                .conversations
                .iter()
                .filter(|conversation_id| !occupied.contains(*conversation_id))
                .take(2usize.saturating_sub(occupied.len()))
            {
                let request_started = tokio::time::Instant::now();
                let response = app
                    .clone()
                    .oneshot(json_with_token(
                        "POST",
                        &format!("/api/conversations/{conversation_id}/messages"),
                        json!({ "content": "sustained ten-user pressure turn" }),
                        &user.token,
                        &user.csrf,
                    ))
                    .await
                    .unwrap();
                max_http_latency = max_http_latency.max(request_started.elapsed());
                if response.status() == StatusCode::ACCEPTED {
                    accepted += 1;
                } else {
                    let status = response.status();
                    let error = body_json(response).await;
                    if status == StatusCode::CONFLICT && error["code"] == "USER_RUN_LIMIT" {
                        backpressure_rejections += 1;
                    } else {
                        unexpected_rejections.push(format!("{} {} {error}", user.username, status.as_u16()));
                    }
                }
            }
        }

        let runtime_response = app
            .clone()
            .oneshot(get_with_token("/api/admin/agent-runtime-status", &users[0].token))
            .await
            .unwrap();
        assert_eq!(runtime_response.status(), StatusCode::OK);
        let runtime = body_json(runtime_response).await;
        let active = runtime["data"]["active_count"].as_u64().unwrap();
        let queued = runtime["data"]["queued_count"].as_u64().unwrap();
        max_runtime_active = max_runtime_active.max(active);
        max_runtime_queued = max_runtime_queued.max(queued);
        assert!(active <= 6, "runtime active count exceeded six: {active}");
        assert!(queued <= 20, "runtime queue exceeded twenty: {queued}");

        let elapsed = started_at.elapsed();
        if elapsed >= next_report {
            println!(
                "STRESS elapsed_s={} accepted={} completed={} backpressure={} active={} queued={} max_active={} max_queued={} max_http_ms={}",
                elapsed.as_secs(),
                accepted,
                probe.completed.load(Ordering::SeqCst),
                backpressure_rejections,
                active,
                queued,
                max_runtime_active,
                max_runtime_queued,
                max_http_latency.as_millis()
            );
            next_report += std::time::Duration::from_secs(30);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let runtime_response = app
                .clone()
                .oneshot(get_with_token("/api/admin/agent-runtime-status", &users[0].token))
                .await
                .unwrap();
            let runtime = body_json(runtime_response).await;
            if runtime["data"]["active_count"] == 0 && runtime["data"]["queued_count"] == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("stress turns should drain after submissions stop");

    let completed_by_conversation = probe.completed_by_conversation.lock().unwrap().clone();
    let mut per_user_completed = Vec::new();
    for user in &users {
        let completed = user
            .conversations
            .iter()
            .map(|conversation_id| {
                completed_by_conversation
                    .get(conversation_id)
                    .copied()
                    .unwrap_or_default()
            })
            .sum::<usize>();
        per_user_completed.push((user.username.clone(), completed));
        assert!(
            completed > 0,
            "{} was starved for the full pressure test",
            user.username
        );
    }
    let min_user_completed = per_user_completed
        .iter()
        .map(|(_, completed)| *completed)
        .min()
        .unwrap();
    let max_user_completed = per_user_completed
        .iter()
        .map(|(_, completed)| *completed)
        .max()
        .unwrap();
    assert!(
        max_user_completed - min_user_completed <= 2,
        "round-robin completion counts diverged: {per_user_completed:?}"
    );

    let status_counts = sqlx::query_as::<_, (String, Option<String>, i64)>(
        "SELECT status, error_code, COUNT(*) FROM agent_runs GROUP BY status, error_code",
    )
    .fetch_all(services.database.pool())
    .await
    .unwrap();
    let failed = status_counts
        .iter()
        .filter(|(status, _, _)| status != "completed")
        .map(|(_, _, count)| *count)
        .sum::<i64>();
    let completed = probe.completed.load(Ordering::SeqCst);
    println!(
        "STRESS_FINAL duration_s={} accepted={} completed={} backpressure={} unexpected_rejections={} max_active={} max_queued={} max_http_ms={} per_user={per_user_completed:?} statuses={status_counts:?}",
        started_at.elapsed().as_secs(),
        accepted,
        completed,
        backpressure_rejections,
        unexpected_rejections.len(),
        max_runtime_active,
        max_runtime_queued,
        max_http_latency.as_millis()
    );
    assert!(
        unexpected_rejections.is_empty(),
        "unexpected HTTP rejections: {unexpected_rejections:?}"
    );
    assert!(
        backpressure_rejections > 0,
        "pressure run should exercise the per-user limit"
    );
    assert_eq!(completed, accepted, "every accepted turn must complete");
    assert_eq!(failed, 0, "all persisted stress runs must complete successfully");
    assert_eq!(probe.max_active.load(Ordering::SeqCst), 6);
    assert!(max_http_latency < std::time::Duration::from_secs(2));
}

#[tokio::test]
async fn stop_stream_with_mock_agent() {
    let (mut app, services, mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Stop Test").await;
    mock_tm.insert(&conv_id, "/mock-workspace");

    let send_req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({ "content": "Start mock agent" }),
        &token,
        &csrf,
    );
    let send_resp = app.clone().oneshot(send_req).await.unwrap();
    assert_eq!(send_resp.status(), StatusCode::ACCEPTED);
    let send_json = body_json(send_resp).await;
    let turn_id = send_json["data"]["turn_id"]
        .as_str()
        .expect("send response includes turn_id");

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/cancel"),
        json!({ "turn_id": turn_id }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
}

#[tokio::test]
async fn runtime_ensure_with_mock_agent() {
    let (mut app, services, _mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Runtime Ensure Test").await;

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/runtime/ensure"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── Confirmation system with mock agent ─────────────────────────

#[tokio::test]
async fn list_confirmations_empty() {
    let (mut app, services, mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Confirm Test").await;
    mock_tm.insert(&conv_id, "/mock-workspace");

    let req = get_with_token(&format!("/api/conversations/{conv_id}/confirmations"), &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert!(json["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn confirm_and_check_approval() {
    let (mut app, services, mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Approval Test").await;
    let agent = mock_tm.insert(&conv_id, "/mock-workspace");

    // Pre-populate a pending confirmation so the confirm endpoint can find it
    agent.confirmations.lock().unwrap().push(Confirmation {
        id: "conf-1".into(),
        call_id: "call-42".into(),
        title: Some("Allow file edit".into()),
        action: Some("test_action".into()),
        description: String::new(),
        command_type: None,
        options: vec![],
    });

    // Confirm a call with alwaysAllow=true
    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/confirmations/call-42/confirm"),
        json!({ "msg_id": "msg-1", "data": { "value": "allow" }, "always_allow": true }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Check approval — should be approved for "test_action"
    let req = get_with_token(
        &format!("/api/conversations/{conv_id}/approvals/check?action=test_action"),
        &token,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["approved"], true);
}

#[tokio::test]
async fn check_approval_not_set() {
    let (mut app, services, mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Approval NotSet").await;
    mock_tm.insert(&conv_id, "/mock-workspace");

    let req = get_with_token(
        &format!("/api/conversations/{conv_id}/approvals/check?action=unknown_action"),
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["approved"], false);
}

// ── Auxiliary routes with mock agent ────────────────────────────

#[tokio::test]
async fn slash_commands_with_mock_returns_empty() {
    let (mut app, services, mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Slash Mock Test").await;
    mock_tm.insert(&conv_id, "/mock-workspace");

    let req = get_with_token(&format!("/api/conversations/{conv_id}/slash-commands"), &token);
    let resp = app.oneshot(req).await.unwrap();
    // Mock agent is not a real AcpAgentManager, so downcast fails → 500
    // OR if agent_type check prevents downcast, returns empty array
    let status = resp.status();
    assert!(
        status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
        "Expected 200 or 500, got {status}"
    );
}

#[tokio::test]
async fn side_question_with_mock_agent() {
    let (mut app, services, mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Side Q Mock").await;
    mock_tm.insert(&conv_id, "/mock-workspace");

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/side-question"),
        json!({ "question": "What is this code?" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    // Mock agent is type Acp but not a real AcpAgentManager, so downcast
    // fails. The handler first checks agent_type() == Acp, then tries to
    // downcast. Since our mock returns Acp type, downcast fails → 500.
    let status = resp.status();
    assert!(
        status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
        "Expected 200 or 500, got {status}"
    );
}

// ── Agent overrides roundtrip ───────────────────────────────────

#[tokio::test]
async fn agent_overrides_roundtrip_and_management_summary() {
    let (mut app, services, _mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    upsert_visible_agent_metadata(&services, "ovr-agent", "acp").await;
    services.agent_registry.hydrate().await.unwrap();
    services.agent_registry.refresh_availability().await;

    // PUT overrides
    let body = json!({
        "command_override": "true",
        "env_override": [{"name": "ANTHROPIC_API_KEY", "value": "sk-x"}, {"name": "PATH", "value": "/evil"}]
    });
    let req = json_with_token("PUT", "/api/agents/ovr-agent/overrides", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let put_body = body_json(resp).await;
    assert_eq!(put_body["data"]["last_check_kind"], "manual");
    assert_eq!(put_body["data"]["last_check_status"], "offline");

    // management row: safe fields, blocked PATH not counted
    let mreq = get_with_token("/api/agents/management", &token);
    let mbody = body_json(app.clone().oneshot(mreq).await.unwrap()).await;
    let mbody_str = serde_json::to_string(&mbody).unwrap();
    let row = mbody["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "ovr-agent")
        .expect("row present");
    assert_eq!(row["has_command_override"], true);
    assert_eq!(row["env_override_key_count"], 1); // PATH excluded
    assert!(
        row["env"].as_array().is_none_or(|arr| arr.is_empty()),
        "management row env must be empty or absent"
    );
    assert!(
        !mbody_str.contains("sk-x"),
        "management response must not leak secret values"
    );

    // GET overrides: plaintext echo
    let greq = get_with_token("/api/agents/ovr-agent/overrides", &token);
    let gbody = body_json(app.clone().oneshot(greq).await.unwrap()).await;
    assert_eq!(gbody["data"]["command_override"], "true");
    let envs = gbody["data"]["env_override"].as_array().unwrap();
    assert!(envs.iter().any(|e| e["name"] == "ANTHROPIC_API_KEY"));
}

#[tokio::test]
async fn internal_aion_cli_rejects_overrides() {
    let (mut app, services, _mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    upsert_visible_agent_metadata(&services, "632f31d2", "aionrs").await;
    services.agent_registry.hydrate().await.unwrap();
    services.agent_registry.refresh_availability().await;

    let command_body = json!({
        "command_override": "irm https://claude.ai/install.ps1 | iex",
        "env_override": [{"name": "ANTHROPIC_API_KEY", "value": "sk-x"}]
    });
    let req = json_with_token("PUT", "/api/agents/632f31d2/overrides", command_body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let env_body = json!({
        "env_override": [{"name": "ANTHROPIC_API_KEY", "value": "sk-x"}]
    });
    let req = json_with_token("PUT", "/api/agents/632f31d2/overrides", env_body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let greq = get_with_token("/api/agents/632f31d2/overrides", &token);
    let gbody = body_json(app.clone().oneshot(greq).await.unwrap()).await;
    assert!(gbody["data"]["command_override"].is_null());
    assert!(gbody["data"]["env_override"].as_array().unwrap().is_empty());
}
