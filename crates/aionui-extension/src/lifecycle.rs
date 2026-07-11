use std::future::Future;
use std::io;
use std::path::Path;
use std::time::Duration;

use aionui_runtime::Builder as CmdBuilder;
use tracing::{info, warn};

use crate::constants::{
    LIFECYCLE_ON_ACTIVATE_TIMEOUT_SECS, LIFECYCLE_ON_DEACTIVATE_TIMEOUT_SECS, LIFECYCLE_ON_INSTALL_TIMEOUT_SECS,
    LIFECYCLE_ON_UNINSTALL_TIMEOUT_SECS,
};
use crate::error::ExtensionError;
use crate::types::LifecycleHooks;

/// A just-installed executable can briefly return `ETXTBSY` on Unix while a
/// concurrent writer or filesystem scanner still holds it open. Keep retries
/// short and bounded; all attempts remain inside the hook's existing timeout.
const HOOK_ETXTBSY_RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(10), Duration::from_millis(25)];

/// Which lifecycle hook to execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    OnInstall,
    OnUninstall,
    OnActivate,
    OnDeactivate,
}

impl HookKind {
    /// Default timeout in seconds for this hook kind.
    pub fn timeout_secs(self) -> u64 {
        match self {
            Self::OnInstall => LIFECYCLE_ON_INSTALL_TIMEOUT_SECS,
            Self::OnUninstall => LIFECYCLE_ON_UNINSTALL_TIMEOUT_SECS,
            Self::OnActivate => LIFECYCLE_ON_ACTIVATE_TIMEOUT_SECS,
            Self::OnDeactivate => LIFECYCLE_ON_DEACTIVATE_TIMEOUT_SECS,
        }
    }

    /// Human-readable label for logging and error messages.
    pub fn label(self) -> &'static str {
        match self {
            Self::OnInstall => "onInstall",
            Self::OnUninstall => "onUninstall",
            Self::OnActivate => "onActivate",
            Self::OnDeactivate => "onDeactivate",
        }
    }
}

/// Resolve the hook script path from the manifest for a given hook kind.
pub fn resolve_hook_path(hooks: &LifecycleHooks, kind: HookKind) -> Option<&str> {
    let value = match kind {
        HookKind::OnInstall => hooks.on_install.as_deref(),
        HookKind::OnUninstall => hooks.on_uninstall.as_deref(),
        HookKind::OnActivate => hooks.on_activate.as_deref(),
        HookKind::OnDeactivate => hooks.on_deactivate.as_deref(),
    };
    value.filter(|s| !s.is_empty())
}

/// Execute a lifecycle hook script in a child process.
///
/// - `ext_dir`: absolute path to the extension root directory (used as cwd).
/// - `hook_path`: script path relative to `ext_dir`.
/// - `kind`: which hook is being executed (determines timeout and label).
/// - `extension_name`: used for logging and error context.
///
/// Returns `Ok(())` on success. Returns an error if the script is not found,
/// times out, or exits with a non-zero status.
pub async fn execute_hook(
    ext_dir: &Path,
    hook_path: &str,
    kind: HookKind,
    extension_name: &str,
) -> Result<(), ExtensionError> {
    let script = ext_dir.join(hook_path);

    if !script.exists() {
        warn!(
            extension = extension_name,
            hook = kind.label(),
            path = %script.display(),
            "lifecycle hook script not found, skipping"
        );
        return Err(ExtensionError::HookNotFound(script.display().to_string()));
    }

    let timeout_secs = kind.timeout_secs();
    let label = kind.label();

    info!(
        extension = extension_name,
        hook = label,
        path = %script.display(),
        timeout_secs,
        "executing lifecycle hook"
    );

    let child_future = retry_text_file_busy(
        || {
            let mut builder = CmdBuilder::clean_cli(&script);
            builder.current_dir(ext_dir);
            builder.output()
        },
        |attempt, delay, error| {
            warn!(
                extension = extension_name,
                hook = label,
                path = %script.display(),
                attempt,
                max_attempts = HOOK_ETXTBSY_RETRY_DELAYS.len() + 1,
                retry_after_ms = delay.as_millis(),
                raw_os_error = ?error.raw_os_error(),
                "lifecycle hook executable is temporarily busy; retrying"
            );
        },
    );

    let result = tokio::time::timeout(Duration::from_secs(timeout_secs), child_future).await;

    match result {
        Err(_elapsed) => {
            warn!(
                extension = extension_name,
                hook = label,
                timeout_secs,
                "lifecycle hook timed out"
            );
            Err(ExtensionError::HookTimeout {
                extension_name: extension_name.to_owned(),
                hook: label.to_owned(),
                timeout_secs,
            })
        }
        Ok(Err(io_err)) => {
            warn!(
                extension = extension_name,
                hook = label,
                error = %io_err,
                "lifecycle hook I/O error"
            );
            Err(ExtensionError::HookFailed {
                extension_name: extension_name.to_owned(),
                hook: label.to_owned(),
                reason: io_err.to_string(),
            })
        }
        Ok(Ok(output)) => {
            if output.status.success() {
                info!(
                    extension = extension_name,
                    hook = label,
                    "lifecycle hook completed successfully"
                );
                Ok(())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let code = output
                    .status
                    .code()
                    .map_or_else(|| "signal".to_owned(), |c| c.to_string());
                warn!(
                    extension = extension_name,
                    hook = label,
                    exit_code = %code,
                    stderr = %stderr,
                    "lifecycle hook exited with error"
                );
                Err(ExtensionError::HookFailed {
                    extension_name: extension_name.to_owned(),
                    hook: label.to_owned(),
                    reason: format!("exit code {code}: {}", stderr.trim()),
                })
            }
        }
    }
}

async fn retry_text_file_busy<T, F, Fut, R>(mut operation: F, mut on_retry: R) -> io::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = io::Result<T>>,
    R: FnMut(usize, Duration, &io::Error),
{
    for (retry_index, delay) in HOOK_ETXTBSY_RETRY_DELAYS.iter().enumerate() {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) if is_text_file_busy(&error) => {
                on_retry(retry_index + 1, *delay, &error);
                tokio::time::sleep(*delay).await;
            }
            Err(error) => return Err(error),
        }
    }

    operation().await
}

fn is_text_file_busy(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::ETXTBSY)
    }
    #[cfg(not(unix))]
    {
        let _ = error;
        false
    }
}

/// Determine whether the `onInstall` hook should run.
///
/// Returns `true` when:
/// - There is no persisted version (first-time install).
/// - The persisted version differs from the current manifest version.
pub fn needs_install_hook(current_version: &str, persisted_version: Option<&str>) -> bool {
    match persisted_version {
        None => true,
        Some(prev) => prev != current_version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::process::Command;

    fn fixture_script(name: &str) -> String {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/lifecycle")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_text_file_busy_retries_until_third_attempt_succeeds() {
        let mut attempts = 0;
        let result = retry_text_file_busy(
            || {
                attempts += 1;
                std::future::ready(if attempts < 3 {
                    Err(io::Error::from_raw_os_error(libc::ETXTBSY))
                } else {
                    Ok(())
                })
            },
            |_, _, _| {},
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(attempts, 3);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_text_file_busy_retry_is_bounded_to_three_attempts() {
        let mut attempts = 0;
        let result: io::Result<()> = retry_text_file_busy(
            || {
                attempts += 1;
                std::future::ready(Err(io::Error::from_raw_os_error(libc::ETXTBSY)))
            },
            |_, _, _| {},
        )
        .await;

        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ETXTBSY));
        assert_eq!(attempts, 3);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_other_spawn_errors_are_not_retried() {
        let mut attempts = 0;
        let result: io::Result<()> = retry_text_file_busy(
            || {
                attempts += 1;
                std::future::ready(Err(io::Error::from_raw_os_error(libc::EACCES)))
            },
            |_, _, _| {},
        )
        .await;

        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EACCES));
        assert_eq!(attempts, 1);
    }

    // -----------------------------------------------------------------------
    // needs_install_hook
    // -----------------------------------------------------------------------

    #[test]
    fn test_needs_install_first_time() {
        assert!(needs_install_hook("1.0.0", None));
    }

    #[test]
    fn test_needs_install_version_changed() {
        assert!(needs_install_hook("2.0.0", Some("1.0.0")));
    }

    #[test]
    fn test_no_install_same_version() {
        assert!(!needs_install_hook("1.0.0", Some("1.0.0")));
    }

    #[test]
    fn test_needs_install_downgrade() {
        assert!(needs_install_hook("0.9.0", Some("1.0.0")));
    }

    // -----------------------------------------------------------------------
    // HookKind
    // -----------------------------------------------------------------------

    #[test]
    fn test_hook_kind_timeout_values() {
        assert_eq!(HookKind::OnInstall.timeout_secs(), 120);
        assert_eq!(HookKind::OnUninstall.timeout_secs(), 60);
        assert_eq!(HookKind::OnActivate.timeout_secs(), 30);
        assert_eq!(HookKind::OnDeactivate.timeout_secs(), 30);
    }

    #[test]
    fn test_hook_kind_labels() {
        assert_eq!(HookKind::OnInstall.label(), "onInstall");
        assert_eq!(HookKind::OnUninstall.label(), "onUninstall");
        assert_eq!(HookKind::OnActivate.label(), "onActivate");
        assert_eq!(HookKind::OnDeactivate.label(), "onDeactivate");
    }

    // -----------------------------------------------------------------------
    // resolve_hook_path
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_hook_path_present() {
        let hooks = LifecycleHooks {
            on_install: Some("scripts/install.sh".into()),
            on_activate: Some("scripts/activate.sh".into()),
            on_deactivate: None,
            on_uninstall: None,
        };
        assert_eq!(
            resolve_hook_path(&hooks, HookKind::OnInstall),
            Some("scripts/install.sh")
        );
        assert_eq!(
            resolve_hook_path(&hooks, HookKind::OnActivate),
            Some("scripts/activate.sh")
        );
        assert_eq!(resolve_hook_path(&hooks, HookKind::OnDeactivate), None);
        assert_eq!(resolve_hook_path(&hooks, HookKind::OnUninstall), None);
    }

    #[test]
    fn test_resolve_hook_path_empty_string() {
        let hooks = LifecycleHooks {
            on_install: Some(String::new()),
            on_activate: None,
            on_deactivate: None,
            on_uninstall: None,
        };
        assert_eq!(resolve_hook_path(&hooks, HookKind::OnInstall), None);
    }

    // -----------------------------------------------------------------------
    // execute_hook (async unit tests)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_execute_hook_script_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let result = execute_hook(dir.path(), "nonexistent.sh", HookKind::OnActivate, "test-ext").await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ExtensionError::HookNotFound(_)));
    }

    #[tokio::test]
    async fn test_execute_hook_success() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = fixture_script("success.sh");

        let result = execute_hook(dir.path(), &script_path, HookKind::OnActivate, "test-ext").await;

        assert!(result.is_ok(), "success hook failed: {result:?}");
    }

    #[tokio::test]
    async fn test_execute_hook_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = fixture_script("fail.sh");

        let result = execute_hook(dir.path(), &script_path, HookKind::OnInstall, "test-ext").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            ExtensionError::HookFailed {
                extension_name,
                hook,
                reason,
            } => {
                assert_eq!(extension_name, "test-ext");
                assert_eq!(hook, "onInstall");
                assert!(reason.contains("setup failed"));
            }
            other => panic!("expected HookFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_execute_hook_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = fixture_script("slow.sh");

        // Use a very short timeout override via a direct timeout wrapper
        let ext_dir = dir.path().to_owned();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            Command::new(script_path)
                .current_dir(&ext_dir)
                .kill_on_drop(true)
                .output(),
        )
        .await;

        assert!(result.is_err(), "should have timed out");
    }

    #[tokio::test]
    async fn test_execute_hook_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = fixture_script("cwd.sh");

        let result = execute_hook(dir.path(), &script_path, HookKind::OnActivate, "test-ext").await;

        assert!(result.is_ok(), "working-directory hook failed: {result:?}");
        let marker = dir.path().join("cwd_out.txt");
        assert!(marker.exists());
        let cwd_content = std::fs::read_to_string(&marker).unwrap();
        // The cwd written by the script should match the extension dir
        // (may have symlink resolution differences, compare canonical)
        let expected = dir.path().canonicalize().unwrap();
        let actual_trimmed = cwd_content.trim();
        let actual = Path::new(actual_trimmed).canonicalize().unwrap();
        assert_eq!(actual, expected);
    }
}
