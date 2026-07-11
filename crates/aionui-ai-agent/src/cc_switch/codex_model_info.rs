use std::collections::HashMap;
use std::fs;

use aionui_api_types::{ModelInfoEntry, ModelInfoPayload};
use rusqlite::Connection;
use tracing::warn;

use super::CcSwitchPaths;

/// Parse the `model` field from a Codex config TOML string.
fn parse_model_from_config(config_str: &str) -> Option<String> {
    let config: toml::Value = toml::from_str(config_str).ok()?;
    config
        .get("model")?
        .as_str()
        .map(|s| s.to_owned())
        .filter(|s| !s.trim().is_empty())
}

/// Read model labels from the `model_pricing` table (shared with Claude path).
fn read_model_labels(conn: &Connection) -> HashMap<String, String> {
    let mut stmt = match conn.prepare("SELECT model_id, display_name FROM model_pricing") {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    let rows = stmt
        .query_map([], |row| {
            let model_id: String = row.get(0)?;
            let display_name: Option<String> = row.get(1)?;
            Ok((model_id, display_name))
        })
        .ok();

    let Some(rows) = rows else {
        return HashMap::new();
    };

    rows.filter_map(|r| r.ok())
        .filter(|(id, _)| !id.trim().is_empty())
        .map(|(id, name)| {
            let label = name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| id.clone());
            (id, label)
        })
        .collect()
}

/// Read available models from the Codex `models_cache.json` file.
///
/// Returns `Vec<(slug, display_name)>` for models visible in the list,
/// sorted by priority (lowest first = most important).
fn read_codex_models_cache(path: &std::path::Path) -> Vec<(String, String)> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let cache: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "codex: failed to parse models_cache.json");
            return Vec::new();
        }
    };

    let mut models: Vec<(String, String, i64)> = Vec::new();
    if let Some(arr) = cache.get("models").and_then(|m| m.as_array()) {
        for m in arr {
            let visibility = m.get("visibility").and_then(|v| v.as_str()).unwrap_or("");
            if visibility != "list" && visibility != "default" {
                continue;
            }
            let slug = m.get("slug").and_then(|s| s.as_str()).unwrap_or("");
            let display = m.get("display_name").and_then(|d| d.as_str()).unwrap_or(slug);
            let priority = m.get("priority").and_then(|p| p.as_i64()).unwrap_or(i64::MAX);
            if !slug.is_empty() {
                models.push((slug.to_owned(), display.to_owned(), priority));
            }
        }
    }

    models.sort_by_key(|(_, _, p)| *p);
    models.into_iter().map(|(slug, display, _)| (slug, display)).collect()
}

/// Read Codex model info from cc-switch, enriched with models_cache.json.
pub fn read_codex_model_info_with_paths(paths: &CcSwitchPaths) -> Option<ModelInfoPayload> {
    let settings_content = fs::read_to_string(&paths.settings_path).ok()?;
    let settings: serde_json::Value = serde_json::from_str(&settings_content).ok()?;
    let provider_id = settings
        .get("currentProviderCodex")?
        .as_str()
        .filter(|s| !s.trim().is_empty())?;

    if !paths.database_path.exists() {
        return None;
    }

    let conn = Connection::open_with_flags(&paths.database_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| warn!(error = %e, "cc-switch: failed to open database for codex model info"))
        .ok()?;

    let settings_config_json: String = conn
        .query_row(
            "SELECT settings_config FROM providers WHERE id = ?1 AND app_type = 'codex' LIMIT 1",
            [provider_id],
            |row| row.get(0),
        )
        .ok()?;

    let config_val: serde_json::Value = serde_json::from_str(&settings_config_json).ok()?;

    // Try to extract the model from the config TOML field
    let model_from_config = config_val
        .get("config")
        .and_then(|v| v.as_str())
        .and_then(parse_model_from_config);

    let labels = read_model_labels(&conn);

    // Enrich with models_cache.json data
    let cache_models = read_codex_models_cache(&paths.codex_models_cache_path);

    // If we got a model from config, build a single-model payload
    if let Some(ref model_id) = model_from_config {
        let label = labels
            .get(model_id.as_str())
            .cloned()
            .or_else(|| {
                cache_models
                    .iter()
                    .find(|(slug, _)| slug == model_id)
                    .map(|(_, display)| display.clone())
            })
            .unwrap_or_else(|| model_id.clone());

        return Some(ModelInfoPayload {
            current_model_id: Some(model_id.clone()),
            current_model_label: Some(label.clone()),
            available_models: vec![ModelInfoEntry {
                id: model_id.clone(),
                label,
            }],
        });
    }

    // Fallback: build from models_cache.json if available
    if !cache_models.is_empty() {
        let available: Vec<ModelInfoEntry> = cache_models
            .into_iter()
            .map(|(slug, display)| ModelInfoEntry {
                id: slug,
                label: display,
            })
            .collect();

        let current = &available[0];
        return Some(ModelInfoPayload {
            current_model_id: Some(current.id.clone()),
            current_model_label: Some(current.label.clone()),
            available_models: available,
        });
    }

    None
}

pub fn read_codex_model_info() -> Option<ModelInfoPayload> {
    let paths = CcSwitchPaths::system()?;
    read_codex_model_info_with_paths(&paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_model_from_config_extracts_model() {
        let config = r#"
model = "gpt-5.6-sol"
model_reasoning_effort = "xhigh"
service_tier = "fast"
"#;
        assert_eq!(parse_model_from_config(config), Some("gpt-5.6-sol".into()));
    }

    #[test]
    fn parse_model_from_config_no_model_returns_none() {
        let config = r#"
service_tier = "fast"
"#;
        assert!(parse_model_from_config(config).is_none());
    }

    #[test]
    fn parse_model_from_config_empty_returns_none() {
        assert!(parse_model_from_config("").is_none());
    }

    #[test]
    fn parse_model_from_config_invalid_toml_returns_none() {
        assert!(parse_model_from_config("not valid toml {{{").is_none());
    }
}
