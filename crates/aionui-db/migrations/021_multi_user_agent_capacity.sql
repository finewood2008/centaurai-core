-- Persisted agent turn admission and logical provider routing.
CREATE TABLE IF NOT EXISTS agent_runs (
    id TEXT PRIMARY KEY NOT NULL,
    turn_id TEXT NOT NULL UNIQUE,
    user_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'conversation',
    status TEXT NOT NULL CHECK (status IN (
        'queued', 'dispatching', 'running', 'completed', 'failed',
        'cancelled', 'timed_out'
    )),
    request_json TEXT NOT NULL,
    message_id TEXT,
    error_code TEXT,
    effective_model TEXT,
    fallback_used INTEGER NOT NULL DEFAULT 0,
    queued_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_agent_runs_user_status_queued
    ON agent_runs(user_id, status, queued_at);
CREATE INDEX IF NOT EXISTS idx_agent_runs_status_queued
    ON agent_runs(status, queued_at);
CREATE INDEX IF NOT EXISTS idx_agent_runs_conversation
    ON agent_runs(conversation_id, queued_at DESC);

CREATE TABLE IF NOT EXISTS agent_runtime_policy (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    mode TEXT NOT NULL DEFAULT 'enforce' CHECK (mode IN ('off', 'shadow', 'enforce')),
    global_active_limit INTEGER NOT NULL DEFAULT 6,
    per_user_active_limit INTEGER NOT NULL DEFAULT 1,
    per_user_queue_limit INTEGER NOT NULL DEFAULT 1,
    global_queue_limit INTEGER NOT NULL DEFAULT 20,
    queue_timeout_ms INTEGER NOT NULL DEFAULT 900000,
    confirmation_timeout_ms INTEGER NOT NULL DEFAULT 300000,
    resident_task_limit INTEGER NOT NULL DEFAULT 6,
    resident_idle_timeout_ms INTEGER NOT NULL DEFAULT 180000,
    updated_at INTEGER NOT NULL
);

INSERT OR IGNORE INTO agent_runtime_policy (singleton, updated_at)
VALUES (1, CAST(strftime('%s', 'now') AS INTEGER) * 1000);

CREATE TABLE IF NOT EXISTS model_routes (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL UNIQUE,
    enabled INTEGER NOT NULL DEFAULT 1,
    required_capabilities TEXT NOT NULL DEFAULT '[]',
    fallback_after_ms INTEGER NOT NULL DEFAULT 15000,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS model_route_members (
    id TEXT PRIMARY KEY NOT NULL,
    route_id TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    model TEXT NOT NULL,
    tier TEXT NOT NULL CHECK (tier IN ('primary', 'fallback')),
    weight INTEGER NOT NULL DEFAULT 1 CHECK (weight > 0),
    max_concurrency INTEGER NOT NULL DEFAULT 1 CHECK (max_concurrency > 0),
    rpm_limit INTEGER,
    tpm_limit INTEGER,
    enabled INTEGER NOT NULL DEFAULT 1,
    disabled_reason TEXT,
    cooldown_until INTEGER,
    consecutive_5xx INTEGER NOT NULL DEFAULT 0,
    consecutive_429 INTEGER NOT NULL DEFAULT 0,
    active_count INTEGER NOT NULL DEFAULT 0,
    request_window_started_at INTEGER,
    request_count INTEGER NOT NULL DEFAULT 0,
    token_count INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (route_id) REFERENCES model_routes(id) ON DELETE CASCADE,
    FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE,
    UNIQUE(route_id, provider_id, model)
);

CREATE INDEX IF NOT EXISTS idx_model_route_members_route_tier
    ON model_route_members(route_id, tier, enabled);

CREATE TABLE IF NOT EXISTS conversation_model_assignments (
    conversation_id TEXT PRIMARY KEY NOT NULL,
    route_id TEXT NOT NULL,
    member_id TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    model TEXT NOT NULL,
    fallback_used INTEGER NOT NULL DEFAULT 0,
    assigned_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE,
    FOREIGN KEY (route_id) REFERENCES model_routes(id) ON DELETE CASCADE,
    FOREIGN KEY (member_id) REFERENCES model_route_members(id) ON DELETE RESTRICT,
    FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE RESTRICT
);

CREATE TABLE IF NOT EXISTS model_route_turn_metrics (
    id TEXT PRIMARY KEY NOT NULL,
    run_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    route_id TEXT NOT NULL,
    member_id TEXT NOT NULL,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    latency_ms INTEGER,
    success INTEGER NOT NULL DEFAULT 0,
    fallback_used INTEGER NOT NULL DEFAULT 0,
    error_code TEXT,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (run_id) REFERENCES agent_runs(id) ON DELETE CASCADE,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (route_id) REFERENCES model_routes(id) ON DELETE CASCADE,
    FOREIGN KEY (member_id) REFERENCES model_route_members(id) ON DELETE RESTRICT
);

-- One compatibility release: migrate LAN conversations whose old untrusted
-- owner marker now resolves to a real user. The JSON field is intentionally
-- retained so older Team renderers can still read the row.
UPDATE conversations
SET user_id = json_extract(extra, '$.frontend_owner_user_id')
WHERE user_id = 'system_default_user'
  AND json_valid(extra)
  AND typeof(json_extract(extra, '$.frontend_owner_user_id')) = 'text'
  AND EXISTS (
      SELECT 1 FROM users
      WHERE users.id = json_extract(conversations.extra, '$.frontend_owner_user_id')
  );
