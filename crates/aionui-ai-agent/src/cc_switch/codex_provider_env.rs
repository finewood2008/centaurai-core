use std::collections::HashMap;
use std::fs;

use rusqlite::Connection;
use tracing::{info, warn};

use super::CcSwitchPaths;

/// Read Codex provider env vars from cc-switch.
///
/// Reads the current Codex provider's `auth` field from cc-switch DB
/// and returns any configured env vars (OPENAI_API_KEY, OPENAI_BASE_URL, etc.).
pub fn read_codex_provider_env_with_paths(paths: &CcSwitchPaths) -> HashMap<String, String> {
    let settings_content = match fs::read_to_string(&paths.settings_path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    let settings: serde_json::Value = match serde_json::from_str(&settings_content) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "cc-switch: failed to parse settings.json for codex env");
            return HashMap::new();
        }
    };

    let provider_id = match settings
        .get("currentProviderCodex")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
    {
        Some(id) => id.to_owned(),
        None => return HashMap::new(),
    };

    if !paths.database_path.exists() {
        warn!(
            provider_id,
            "cc-switch: settings.json references codex provider but database file not found"
        );
        return HashMap::new();
    }

    read_codex_env_from_db(&paths.database_path, &provider_id)
}

fn read_codex_env_from_db(db_path: &std::path::Path, provider_id: &str) -> HashMap<String, String> {
    let conn = match Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "cc-switch: failed to open database for codex env");
            return HashMap::new();
        }
    };

    let settings_config_json: Option<String> = conn
        .query_row(
            "SELECT settings_config FROM providers WHERE id = ?1 AND app_type = 'codex' LIMIT 1",
            [provider_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();

    let Some(json_str) = settings_config_json else {
        warn!(provider_id, "cc-switch: codex provider not found in database");
        return HashMap::new();
    };

    let config: serde_json::Value = match serde_json::from_str(&json_str) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, provider_id, "cc-switch: failed to parse codex provider settings_config");
            return HashMap::new();
        }
    };

    // Extract auth env vars
    let env = match config.get("auth").and_then(|a| a.as_object()) {
        Some(auth_obj) => auth_obj
            .iter()
            .filter_map(|(k, v)| {
                v.as_str()
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| (k.clone(), s.to_owned()))
            })
            .collect(),
        None => HashMap::new(),
    };

    if env.is_empty() {
        info!(
            provider_id,
            "cc-switch: codex provider has no auth env vars (using native API)"
        );
    } else {
        let keys: Vec<&str> = env.keys().map(|k| k.as_str()).collect();
        info!(provider_id, ?keys, "cc-switch: codex provider env vars loaded");
    }

    env
}

pub fn read_codex_provider_env() -> HashMap<String, String> {
    let Some(paths) = CcSwitchPaths::system() else {
        return HashMap::new();
    };
    read_codex_provider_env_with_paths(&paths)
}
