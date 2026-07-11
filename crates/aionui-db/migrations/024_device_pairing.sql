CREATE TABLE IF NOT EXISTS devices (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    name TEXT NOT NULL,
    platform TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE CHECK (length(token_hash) = 64),
    last_seen_at INTEGER,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_devices_user_created
    ON devices(user_id, created_at DESC);

CREATE TABLE IF NOT EXISTS device_pairing_sessions (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    code_hash TEXT NOT NULL UNIQUE CHECK (length(code_hash) = 64),
    server_url TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    consumed_at INTEGER,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_device_pairing_sessions_user_created
    ON device_pairing_sessions(user_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_device_pairing_sessions_expiry
    ON device_pairing_sessions(expires_at)
    WHERE consumed_at IS NULL;
