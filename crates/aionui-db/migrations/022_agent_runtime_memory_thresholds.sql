-- Administrator-configurable memory pressure thresholds for agent scheduling.
ALTER TABLE agent_runtime_policy ADD COLUMN memory_constrained_percent REAL NOT NULL DEFAULT 75;
ALTER TABLE agent_runtime_policy ADD COLUMN memory_pause_percent REAL NOT NULL DEFAULT 85;
ALTER TABLE agent_runtime_policy ADD COLUMN memory_reject_percent REAL NOT NULL DEFAULT 90;
