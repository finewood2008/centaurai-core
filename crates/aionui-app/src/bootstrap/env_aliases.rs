//! Canonical CentaurAI Core environment variables mapped to legacy names.

/// `(canonical, legacy)` environment variable aliases consumed by the binary.
///
/// The canonical name wins when both are present. Values are copied only to
/// the legacy process-local name so existing crates can migrate independently.
const ENV_ALIASES: &[(&str, &str)] = &[
    ("CENTAURAI_CORE_AGENT_SCHEDULER_MODE", "AIONUI_AGENT_SCHEDULER_MODE"),
    ("CENTAURAI_CORE_BASE_URL", "AIONUI_BASE_URL"),
    (
        "CENTAURAI_CORE_BUILTIN_ASSISTANTS_PATH",
        "AIONUI_BUILTIN_ASSISTANTS_PATH",
    ),
    ("CENTAURAI_CORE_BUILTIN_SKILLS_PATH", "AIONUI_BUILTIN_SKILLS_PATH"),
    (
        "CENTAURAI_CORE_BUNDLED_MANAGED_RESOURCES",
        "AIONUI_BUNDLED_MANAGED_RESOURCES",
    ),
    ("CENTAURAI_CORE_BUN_PATH", "AIONUI_BUN_PATH"),
    ("CENTAURAI_CORE_BYPASS_PROBE", "AIONUI_BYPASS_PROBE"),
    ("CENTAURAI_CORE_CACHE_DIR", "AIONUI_CACHE_DIR"),
    ("CENTAURAI_CORE_CONVERSATION_ID", "AIONUI_CONVERSATION_ID"),
    ("CENTAURAI_CORE_EXTENSIONS_PATH", "AIONUI_EXTENSIONS_PATH"),
    ("CENTAURAI_CORE_EXTENSION_STATES_FILE", "AIONUI_EXTENSION_STATES_FILE"),
    ("CENTAURAI_CORE_GITHUB_REPO", "AIONUI_GITHUB_REPO"),
    ("CENTAURAI_CORE_HELPER_BIN", "AIONUI_HELPER_BIN"),
    ("CENTAURAI_CORE_HTTPS", "AIONUI_HTTPS"),
    ("CENTAURAI_CORE_IMG_API_KEY", "AIONUI_IMG_API_KEY"),
    ("CENTAURAI_CORE_IMG_API_URL", "AIONUI_IMG_API_URL"),
    ("CENTAURAI_CORE_IMG_MODEL", "AIONUI_IMG_MODEL"),
    ("CENTAURAI_CORE_IMG_QUALITY", "AIONUI_IMG_QUALITY"),
    ("CENTAURAI_CORE_IMG_SIZE", "AIONUI_IMG_SIZE"),
    ("CENTAURAI_CORE_IMG_STYLE", "AIONUI_IMG_STYLE"),
    ("CENTAURAI_CORE_LOG_DIR", "AIONUI_LOG_DIR"),
    ("CENTAURAI_CORE_MEMORY_USED_PERCENT", "AIONUI_MEMORY_USED_PERCENT"),
    ("CENTAURAI_CORE_MODEL_ROUTES_ENABLED", "AIONUI_MODEL_ROUTES_ENABLED"),
    ("CENTAURAI_CORE_TRUSTED_PROXY_SECRET", "AIONUI_TRUSTED_PROXY_SECRET"),
    ("CENTAURAI_CORE_USER_ID", "AIONUI_USER_ID"),
    ("CENTAURAI_CORE_WORK_DIR", "AIONUI_WORK_DIR"),
];

/// Apply canonical aliases before the async runtime creates worker threads.
///
/// Returns the number of canonical values that were applied. No values are
/// logged because the set includes credentials.
pub(crate) fn apply_centaurai_core_env_aliases() -> usize {
    let mut applied = 0;
    for &(canonical, legacy) in ENV_ALIASES {
        let Some(value) = std::env::var_os(canonical) else {
            continue;
        };
        // SAFETY: main calls this before creating the Tokio runtime or any
        // worker thread, so there are no concurrent environment readers.
        unsafe {
            std::env::set_var(legacy, value);
        }
        applied += 1;
    }
    applied
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn aliases_are_unique_and_use_the_expected_namespaces() {
        let mut canonical = HashSet::new();
        let mut legacy = HashSet::new();
        for &(new_name, old_name) in ENV_ALIASES {
            assert!(new_name.starts_with("CENTAURAI_CORE_"));
            assert!(old_name.starts_with("AIONUI_"));
            assert!(canonical.insert(new_name), "duplicate canonical alias: {new_name}");
            assert!(legacy.insert(old_name), "duplicate legacy alias: {old_name}");
        }
        assert!(canonical.contains("CENTAURAI_CORE_TRUSTED_PROXY_SECRET"));
        assert!(canonical.contains("CENTAURAI_CORE_BUNDLED_MANAGED_RESOURCES"));
    }
}
