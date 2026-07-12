CREATE TABLE decision_retrieval_bundles (
    id TEXT PRIMARY KEY NOT NULL,
    decision_id TEXT NOT NULL REFERENCES decisions(id) ON DELETE CASCADE,
    query TEXT NOT NULL,
    hits_json TEXT NOT NULL,
    token_budget INTEGER NOT NULL,
    cloud_authorized INTEGER NOT NULL DEFAULT 0,
    space_ids_json TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_decision_retrieval_bundles_decision
    ON decision_retrieval_bundles(decision_id, created_at DESC);

-- Every persisted evidence item and model turn carries its input lineage.
-- A repeated source_id across refreshes is deliberately not sufficient to
-- establish cloud consent: the exact retrieval bundle is the authority.
ALTER TABLE decision_evidence
    ADD COLUMN retrieval_bundle_id TEXT REFERENCES decision_retrieval_bundles(id) ON DELETE SET NULL;
ALTER TABLE decision_evidence
    ADD COLUMN lineage_bundle_ids_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE decision_evidence
    ADD COLUMN cloud_egress_allowed INTEGER NOT NULL DEFAULT 0;
ALTER TABLE decision_evidence
    ADD COLUMN origin_location TEXT NOT NULL DEFAULT 'unknown'
        CHECK (origin_location IN ('user', 'knowledge', 'local', 'external', 'unknown'));
ALTER TABLE decision_evidence
    ADD COLUMN end_seconds REAL;
ALTER TABLE decision_evidence
    ADD COLUMN uri TEXT;

ALTER TABLE decision_turns
    ADD COLUMN retrieval_bundle_id TEXT REFERENCES decision_retrieval_bundles(id) ON DELETE SET NULL;
ALTER TABLE decision_turns
    ADD COLUMN evidence_ids_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE decision_turns
    ADD COLUMN lineage_bundle_ids_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE decision_turns
    ADD COLUMN input_cloud_egress_allowed INTEGER NOT NULL DEFAULT 0;
ALTER TABLE decision_turns
    ADD COLUMN resolved_location TEXT NOT NULL DEFAULT 'unknown'
        CHECK (resolved_location IN ('local', 'external', 'unknown'));

CREATE TABLE decision_egress_audit (
    id TEXT PRIMARY KEY NOT NULL,
    decision_id TEXT NOT NULL REFERENCES decisions(id) ON DELETE CASCADE,
    session_id TEXT NOT NULL REFERENCES decision_sessions(id) ON DELETE CASCADE,
    brain_id TEXT NOT NULL REFERENCES decision_brains(id) ON DELETE CASCADE,
    turn_id TEXT NOT NULL REFERENCES decision_turns(id) ON DELETE CASCADE,
    provider_id TEXT NOT NULL,
    model TEXT NOT NULL,
    location TEXT NOT NULL CHECK (location IN ('local', 'external', 'unknown')),
    retrieval_bundle_id TEXT REFERENCES decision_retrieval_bundles(id) ON DELETE SET NULL,
    knowledge_egress_allowed INTEGER NOT NULL DEFAULT 0,
    hit_count INTEGER NOT NULL DEFAULT 0,
    evidence_ids_json TEXT NOT NULL DEFAULT '[]',
    lineage_bundle_ids_json TEXT NOT NULL DEFAULT '[]',
    space_ids_json TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_decision_egress_audit_decision
    ON decision_egress_audit(decision_id, created_at);

-- Shared durable idempotency ledger. `request_fingerprint` prevents a client
-- from accidentally reusing one operation id for different content, while
-- the user-scoped unique key prevents cross-owner disclosure.
CREATE TABLE idempotency_records (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    operation_scope TEXT NOT NULL,
    operation_key TEXT NOT NULL,
    request_fingerprint TEXT NOT NULL,
    resource_id TEXT NOT NULL,
    response_json TEXT,
    state TEXT NOT NULL DEFAULT 'completed' CHECK (state IN ('pending', 'completed')),
    lease_owner TEXT,
    lease_expires_at INTEGER,
    created_at INTEGER NOT NULL,
    UNIQUE(user_id, operation_scope, operation_key)
);

CREATE INDEX idx_idempotency_records_created
    ON idempotency_records(created_at);
