CREATE TABLE decisions (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    question TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'draft'
        CHECK (status IN ('draft', 'running', 'paused', 'completed', 'cancelled', 'failed')),
    brain_count INTEGER NOT NULL DEFAULT 3 CHECK (brain_count BETWEEN 1 AND 7),
    knowledge_json TEXT NOT NULL DEFAULT '{}',
    roles_json TEXT NOT NULL DEFAULT '[]',
    tools_json TEXT NOT NULL DEFAULT '[]',
    conclusion TEXT,
    selected_candidate_id TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX idx_decisions_user_updated ON decisions(user_id, updated_at DESC);

CREATE TABLE decision_sessions (
    id TEXT PRIMARY KEY NOT NULL,
    decision_id TEXT NOT NULL REFERENCES decisions(id) ON DELETE CASCADE,
    status TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1,
    current_prompt TEXT NOT NULL,
    started_at INTEGER,
    completed_at INTEGER,
    updated_at INTEGER NOT NULL
);

CREATE INDEX idx_decision_sessions_decision ON decision_sessions(decision_id, revision DESC);

CREATE TABLE decision_brains (
    id TEXT PRIMARY KEY NOT NULL,
    decision_id TEXT NOT NULL REFERENCES decisions(id) ON DELETE CASCADE,
    kind TEXT NOT NULL DEFAULT 'provider_model',
    provider_id TEXT NOT NULL,
    model TEXT NOT NULL,
    role_id TEXT NOT NULL,
    agent_id TEXT,
    tool_ids_json TEXT NOT NULL DEFAULT '[]',
    state TEXT NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'running', 'completed', 'failed', 'cancelled')),
    error_code TEXT,
    fallback_provider_id TEXT,
    ordinal INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(decision_id, ordinal)
);

CREATE INDEX idx_decision_brains_decision ON decision_brains(decision_id, ordinal);

CREATE TABLE decision_turns (
    id TEXT PRIMARY KEY NOT NULL,
    decision_id TEXT NOT NULL REFERENCES decisions(id) ON DELETE CASCADE,
    session_id TEXT NOT NULL REFERENCES decision_sessions(id) ON DELETE CASCADE,
    brain_id TEXT REFERENCES decision_brains(id) ON DELETE SET NULL,
    kind TEXT NOT NULL,
    content TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL,
    attempt INTEGER NOT NULL DEFAULT 1,
    error_code TEXT,
    provider_id TEXT,
    model TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX idx_decision_turns_session ON decision_turns(session_id, created_at);
CREATE INDEX idx_decision_turns_brain ON decision_turns(brain_id, created_at);

CREATE TABLE decision_evidence (
    id TEXT PRIMARY KEY NOT NULL,
    decision_id TEXT NOT NULL REFERENCES decisions(id) ON DELETE CASCADE,
    turn_id TEXT REFERENCES decision_turns(id) ON DELETE SET NULL,
    source_id TEXT NOT NULL,
    title TEXT NOT NULL,
    snippet TEXT NOT NULL,
    score REAL NOT NULL,
    media_type TEXT NOT NULL,
    page INTEGER,
    chapter TEXT,
    timestamp_ms INTEGER,
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_decision_evidence_decision ON decision_evidence(decision_id, created_at);

CREATE TABLE decision_candidates (
    id TEXT PRIMARY KEY NOT NULL,
    decision_id TEXT NOT NULL REFERENCES decisions(id) ON DELETE CASCADE,
    brain_id TEXT REFERENCES decision_brains(id) ON DELETE SET NULL,
    title TEXT NOT NULL,
    content TEXT NOT NULL,
    rank INTEGER NOT NULL DEFAULT 0,
    selected INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_decision_candidates_decision ON decision_candidates(decision_id, rank, created_at);

CREATE TABLE decision_resolutions (
    id TEXT PRIMARY KEY NOT NULL,
    decision_id TEXT NOT NULL UNIQUE REFERENCES decisions(id) ON DELETE CASCADE,
    summary TEXT NOT NULL,
    status TEXT NOT NULL,
    partial INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE decision_action_items (
    id TEXT PRIMARY KEY NOT NULL,
    resolution_id TEXT NOT NULL REFERENCES decision_resolutions(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    owner TEXT,
    due_at INTEGER,
    status TEXT NOT NULL DEFAULT 'open',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX idx_decision_action_items_resolution ON decision_action_items(resolution_id, created_at);
