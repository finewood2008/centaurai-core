use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aionui_runtime::{Builder, kill_process_tree};
use serde_json::Value;
use tokio::process::Child;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::client::WorkerClient;
use crate::{KnowledgeConfigError, KnowledgeLifecycleError};

const WORKER_BINARY_ENV: &str = "CENTAURAI_CORE_KNOWLEDGE_WORKER_BIN";
const CORE_SOCKET_ENV: &str = "CENTAURAI_KNOWLEDGE_WORKER_SOCKET";
const CORE_URL_ENV: &str = "CENTAURAI_KNOWLEDGE_WORKER_URL";
const WORKER_SOCKET_ENV: &str = "CENTAURAI_KNOWLEDGE_SOCKET";
const DATA_DIR_ENV: &str = "CENTAURAI_KNOWLEDGE_DATA_DIR";
const INTERNAL_TOKEN_ENV: &str = "CENTAURAI_KNOWLEDGE_INTERNAL_TOKEN";

#[derive(Clone)]
struct ManagedWorkerConfig {
    binary: PathBuf,
    socket: PathBuf,
    data_dir: PathBuf,
    internal_token: String,
}

impl std::fmt::Debug for ManagedWorkerConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedWorkerConfig")
            .field("binary", &self.binary)
            .field("socket", &self.socket)
            .field("data_dir", &self.data_dir)
            .field("internal_token", &"[REDACTED]")
            .finish()
    }
}

impl ManagedWorkerConfig {
    fn from_environment(core_data_dir: &Path) -> Result<Option<Self>, KnowledgeConfigError> {
        let Some(binary) = nonempty_os_env(WORKER_BINARY_ENV) else {
            return Ok(None);
        };
        let token = std::env::var(INTERNAL_TOKEN_ENV)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .ok_or(KnowledgeConfigError::MissingInternalToken)?;
        let socket = configured_worker_socket(core_data_dir)?;
        let data_dir = nonempty_os_env(DATA_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or(absolutize(core_data_dir)?.join("knowledge"));
        Self::new(PathBuf::from(binary), socket, data_dir, token).map(Some)
    }

    fn new(
        binary: PathBuf,
        socket: PathBuf,
        data_dir: PathBuf,
        internal_token: String,
    ) -> Result<Self, KnowledgeConfigError> {
        #[cfg(not(unix))]
        return Err(KnowledgeConfigError::ManagedWorkerUnsupported);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            if !binary.is_absolute() {
                return Err(KnowledgeConfigError::InvalidWorkerBinaryPath);
            }
            let binary = binary
                .canonicalize()
                .map_err(|_| KnowledgeConfigError::WorkerBinaryUnavailable)?;
            let metadata = std::fs::metadata(&binary).map_err(|_| KnowledgeConfigError::WorkerBinaryUnavailable)?;
            if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
                return Err(KnowledgeConfigError::WorkerBinaryUnavailable);
            }
            if !socket.is_absolute() {
                return Err(KnowledgeConfigError::InvalidSocketPath);
            }
            if !data_dir.is_absolute() {
                return Err(KnowledgeConfigError::InvalidKnowledgeDataDir);
            }
            if internal_token.trim().is_empty() {
                return Err(KnowledgeConfigError::MissingInternalToken);
            }
            Ok(Self {
                binary,
                socket,
                data_dir,
                internal_token,
            })
        }
    }
}

fn nonempty_os_env(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

pub(crate) fn managed_worker_requested() -> bool {
    nonempty_os_env(WORKER_BINARY_ENV).is_some()
}

fn absolutize(path: &Path) -> Result<PathBuf, KnowledgeConfigError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .map_err(|_| KnowledgeConfigError::InvalidKnowledgeDataDir)
    }
}

pub(crate) fn configured_worker_socket(core_data_dir: &Path) -> Result<PathBuf, KnowledgeConfigError> {
    let socket = nonempty_os_env(CORE_SOCKET_ENV)
        .map(PathBuf::from)
        .unwrap_or(absolutize(core_data_dir)?.join("run/knowledge-worker.sock"));
    if socket.is_absolute() {
        Ok(socket)
    } else {
        Err(KnowledgeConfigError::InvalidSocketPath)
    }
}

#[derive(Debug, Clone, Copy)]
struct SupervisorPolicy {
    startup_timeout: Duration,
    child_readiness_timeout: Duration,
    probe_interval: Duration,
    probe_timeout: Duration,
    restart_initial: Duration,
    restart_max: Duration,
    stable_reset_after: Duration,
}

impl Default for SupervisorPolicy {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(30),
            child_readiness_timeout: Duration::from_secs(15),
            probe_interval: Duration::from_millis(200),
            probe_timeout: Duration::from_secs(1),
            restart_initial: Duration::from_millis(250),
            restart_max: Duration::from_secs(30),
            stable_reset_after: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleState {
    Starting,
    Ready,
    Failed,
}

/// Owns the lifecycle of a configured `centaurai-knowledge-worker` process.
///
/// Configuration comes only from the Core process environment. No public API
/// accepts a binary, socket, data directory, or internal token.
pub struct KnowledgeWorkerSupervisor {
    shutdown_tx: watch::Sender<bool>,
    task: Mutex<Option<JoinHandle<Result<(), KnowledgeLifecycleError>>>>,
}

impl std::fmt::Debug for KnowledgeWorkerSupervisor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("KnowledgeWorkerSupervisor")
            .finish_non_exhaustive()
    }
}

impl KnowledgeWorkerSupervisor {
    pub async fn start_from_environment(core_data_dir: &Path) -> Result<Option<Arc<Self>>, KnowledgeLifecycleError> {
        let Some(config) = ManagedWorkerConfig::from_environment(core_data_dir)? else {
            return Ok(None);
        };
        Self::start(config, SupervisorPolicy::default()).await.map(Some)
    }

    async fn start(
        config: ManagedWorkerConfig,
        policy: SupervisorPolicy,
    ) -> Result<Arc<Self>, KnowledgeLifecycleError> {
        prepare_directories(&config)?;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (state_tx, mut state_rx) = watch::channel(LifecycleState::Starting);
        let task = tokio::spawn(run_supervisor(config, policy, shutdown_rx, state_tx));
        let supervisor = Arc::new(Self {
            shutdown_tx,
            task: Mutex::new(Some(task)),
        });

        let ready = tokio::time::timeout(policy.startup_timeout, async {
            loop {
                match *state_rx.borrow() {
                    LifecycleState::Ready => return Ok(()),
                    LifecycleState::Failed => return Err(KnowledgeLifecycleError::SupervisorStopped),
                    LifecycleState::Starting => {}
                }
                state_rx
                    .changed()
                    .await
                    .map_err(|_| KnowledgeLifecycleError::SupervisorStopped)?;
            }
        })
        .await;

        match ready {
            Ok(Ok(())) => Ok(supervisor),
            Ok(Err(error)) => {
                let joined = supervisor.shutdown().await;
                Err(joined.err().unwrap_or(error))
            }
            Err(_) => {
                let _ = supervisor.shutdown().await;
                Err(KnowledgeLifecycleError::ReadinessTimeout)
            }
        }
    }

    pub async fn shutdown(&self) -> Result<(), KnowledgeLifecycleError> {
        let _ = self.shutdown_tx.send(true);
        let task = self.task.lock().expect("supervisor task mutex poisoned").take();
        let Some(task) = task else {
            return Ok(());
        };
        task.await.map_err(|_| KnowledgeLifecycleError::SupervisorTask)?
    }
}

impl Drop for KnowledgeWorkerSupervisor {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        if let Some(task) = self.task.get_mut().expect("supervisor task mutex poisoned").take() {
            task.abort();
        }
    }
}

fn prepare_directories(config: &ManagedWorkerConfig) -> Result<(), KnowledgeLifecycleError> {
    let socket_parent = config.socket.parent().ok_or(KnowledgeConfigError::InvalidSocketPath)?;
    std::fs::create_dir_all(socket_parent).map_err(KnowledgeLifecycleError::Prepare)?;
    std::fs::create_dir_all(&config.data_dir).map_err(KnowledgeLifecycleError::Prepare)?;
    Ok(())
}

async fn run_supervisor(
    config: ManagedWorkerConfig,
    policy: SupervisorPolicy,
    mut shutdown: watch::Receiver<bool>,
    state: watch::Sender<LifecycleState>,
) -> Result<(), KnowledgeLifecycleError> {
    let readiness_client = WorkerClient::for_unix_socket(config.socket.clone(), config.internal_token.clone())?;
    let mut consecutive_failures = 0u32;

    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        if let Err(error) = prepare_socket_for_spawn(&config.socket, &readiness_client, policy.probe_timeout).await {
            let _ = state.send(LifecycleState::Failed);
            return Err(error);
        }

        let mut child = match spawn_worker(&config) {
            Ok(child) => child,
            Err(_) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                if wait_backoff(&mut shutdown, restart_backoff(consecutive_failures, policy)).await {
                    return Ok(());
                }
                continue;
            }
        };

        match await_readiness(&mut child, &readiness_client, &mut shutdown, policy).await {
            ReadinessOutcome::Shutdown => {
                stop_child(&mut child).await;
                remove_socket_if_socket(&config.socket)?;
                return Ok(());
            }
            ReadinessOutcome::Exited | ReadinessOutcome::TimedOut => {
                stop_child(&mut child).await;
                consecutive_failures = consecutive_failures.saturating_add(1);
                if wait_backoff(&mut shutdown, restart_backoff(consecutive_failures, policy)).await {
                    return Ok(());
                }
                continue;
            }
            ReadinessOutcome::Ready => {
                let _ = state.send(LifecycleState::Ready);
            }
        }

        let ready_at = Instant::now();
        let shutdown_requested = tokio::select! {
            _ = wait_for_shutdown(&mut shutdown) => true,
            _ = child.wait() => false,
        };
        if shutdown_requested {
            stop_child(&mut child).await;
            remove_socket_if_socket(&config.socket)?;
            return Ok(());
        }

        let _ = state.send(LifecycleState::Starting);
        consecutive_failures = if ready_at.elapsed() >= policy.stable_reset_after {
            1
        } else {
            consecutive_failures.saturating_add(1)
        };
        if wait_backoff(&mut shutdown, restart_backoff(consecutive_failures, policy)).await {
            remove_socket_if_socket(&config.socket)?;
            return Ok(());
        }
    }
}

fn spawn_worker(config: &ManagedWorkerConfig) -> std::io::Result<Child> {
    let mut command = Builder::new(&config.binary);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove(WORKER_BINARY_ENV)
        .env_remove(CORE_SOCKET_ENV)
        .env_remove(CORE_URL_ENV)
        .env_remove("CENTAURAI_KNOWLEDGE_HOST")
        .env_remove("CENTAURAI_KNOWLEDGE_PORT")
        .env_remove("CENTAURAI_KNOWLEDGE_ALLOW_UNSAFE_BIND")
        .env(WORKER_SOCKET_ENV, &config.socket)
        .env(DATA_DIR_ENV, &config.data_dir)
        .env(INTERNAL_TOKEN_ENV, &config.internal_token);
    command.spawn()
}

enum ReadinessOutcome {
    Ready,
    Exited,
    TimedOut,
    Shutdown,
}

async fn await_readiness(
    child: &mut Child,
    client: &WorkerClient,
    shutdown: &mut watch::Receiver<bool>,
    policy: SupervisorPolicy,
) -> ReadinessOutcome {
    let deadline = Instant::now() + policy.child_readiness_timeout;
    loop {
        if *shutdown.borrow() {
            return ReadinessOutcome::Shutdown;
        }
        if child.try_wait().ok().flatten().is_some() {
            return ReadinessOutcome::Exited;
        }
        if probe_ready(client, policy.probe_timeout).await {
            return ReadinessOutcome::Ready;
        }
        if Instant::now() >= deadline {
            return ReadinessOutcome::TimedOut;
        }
        tokio::select! {
            _ = tokio::time::sleep(policy.probe_interval) => {}
            _ = wait_for_shutdown(shutdown) => return ReadinessOutcome::Shutdown,
        }
    }
}

async fn probe_ready(client: &WorkerClient, timeout: Duration) -> bool {
    let response = tokio::time::timeout(timeout, client.get::<Value>("/api/health", None)).await;
    matches!(
        response,
        Ok(Ok(body))
            if body.get("status").and_then(Value::as_str) == Some("ok")
                && body.get("service").and_then(Value::as_str) == Some("centaurai-knowledge-worker")
    )
}

async fn prepare_socket_for_spawn(
    socket: &Path,
    client: &WorkerClient,
    probe_timeout: Duration,
) -> Result<(), KnowledgeLifecycleError> {
    let metadata = match std::fs::symlink_metadata(socket) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(KnowledgeLifecycleError::Prepare(error)),
    };
    if !is_socket(&metadata) {
        return Err(KnowledgeLifecycleError::UnsafeSocketPath);
    }
    if probe_ready(client, probe_timeout).await {
        return Err(KnowledgeLifecycleError::SocketInUse);
    }
    std::fs::remove_file(socket).map_err(KnowledgeLifecycleError::Prepare)
}

fn remove_socket_if_socket(socket: &Path) -> Result<(), KnowledgeLifecycleError> {
    match std::fs::symlink_metadata(socket) {
        Ok(metadata) if is_socket(&metadata) => std::fs::remove_file(socket).map_err(KnowledgeLifecycleError::Prepare),
        Ok(_) | Err(_) => Ok(()),
    }
}

#[cfg(unix)]
fn is_socket(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;
    metadata.file_type().is_socket()
}

#[cfg(not(unix))]
fn is_socket(_metadata: &std::fs::Metadata) -> bool {
    false
}

async fn stop_child(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = kill_process_tree(child).await;
    }
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            break;
        }
    }
}

async fn wait_backoff(shutdown: &mut watch::Receiver<bool>, delay: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = wait_for_shutdown(shutdown) => true,
    }
}

fn restart_backoff(failures: u32, policy: SupervisorPolicy) -> Duration {
    let exponent = failures.saturating_sub(1).min(16);
    policy
        .restart_initial
        .saturating_mul(1u32 << exponent)
        .min(policy.restart_max)
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    use super::*;

    fn write_script(root: &Path, body: &str) -> PathBuf {
        let path = root.join("worker-fixture.sh");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "#!/bin/sh").unwrap();
        writeln!(file, "{body}").unwrap();
        file.sync_all().unwrap();
        let mut permissions = file.metadata().unwrap().permissions();
        permissions.set_mode(0o755);
        drop(file);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    async fn wait_for_file(path: &Path) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker fixture did not create its record file");
    }

    fn test_policy() -> SupervisorPolicy {
        SupervisorPolicy {
            startup_timeout: Duration::from_secs(2),
            child_readiness_timeout: Duration::from_millis(300),
            probe_interval: Duration::from_millis(20),
            probe_timeout: Duration::from_millis(100),
            restart_initial: Duration::from_millis(20),
            restart_max: Duration::from_millis(80),
            stable_reset_after: Duration::from_secs(1),
        }
    }

    async fn serve_health(socket: PathBuf, mut stop: watch::Receiver<bool>) {
        let listener = UnixListener::bind(socket).unwrap();
        let body = r#"{"status":"ok","service":"centaurai-knowledge-worker"}"#;
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted.unwrap();
                    let mut request = [0u8; 1024];
                    let _ = stream.read(&mut request).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
                _ = wait_for_shutdown(&mut stop) => break,
            }
        }
    }

    #[tokio::test]
    async fn managed_worker_receives_private_contract_and_stops_with_core() {
        let temp = tempfile::tempdir().unwrap();
        let record = temp.path().join("worker-env.txt");
        let script = write_script(
            temp.path(),
            &format!(
                "printf '%s|%s|%s|%s\\n' \"$CENTAURAI_KNOWLEDGE_SOCKET\" \"$CENTAURAI_KNOWLEDGE_DATA_DIR\" \"$CENTAURAI_KNOWLEDGE_INTERNAL_TOKEN\" \"$$\" > '{}'; while :; do sleep 1; done",
                record.display()
            ),
        );
        let socket = temp.path().join("run/knowledge.sock");
        let data_dir = temp.path().join("knowledge");
        let config =
            ManagedWorkerConfig::new(script, socket.clone(), data_dir.clone(), "fixture-token".into()).unwrap();

        let start = tokio::spawn(KnowledgeWorkerSupervisor::start(config, test_policy()));
        tokio::time::sleep(Duration::from_millis(80)).await;
        let (health_stop_tx, health_stop_rx) = watch::channel(false);
        let health = tokio::spawn(serve_health(socket.clone(), health_stop_rx));
        let supervisor = start.await.unwrap().unwrap();

        wait_for_file(&record).await;
        let recorded = std::fs::read_to_string(&record).unwrap();
        let fields: Vec<_> = recorded.trim().split('|').collect();
        assert_eq!(fields[0], socket.to_string_lossy());
        assert_eq!(fields[1], data_dir.to_string_lossy());
        assert_eq!(fields[2], "fixture-token");
        let pid = fields[3].to_owned();

        supervisor.shutdown().await.unwrap();
        let _ = health_stop_tx.send(true);
        health.await.unwrap();
        assert!(
            !std::process::Command::new("kill")
                .arg("-0")
                .arg(&pid)
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
    }

    #[tokio::test]
    async fn failed_children_restart_with_backoff_until_startup_deadline() {
        let temp = tempfile::tempdir().unwrap();
        let record = temp.path().join("starts.txt");
        let script = write_script(temp.path(), &format!("echo started >> '{}'; exit 1", record.display()));
        let config = ManagedWorkerConfig::new(
            script,
            temp.path().join("run/knowledge.sock"),
            temp.path().join("knowledge"),
            "fixture-token".into(),
        )
        .unwrap();
        let error = KnowledgeWorkerSupervisor::start(config, test_policy())
            .await
            .unwrap_err();
        assert!(matches!(error, KnowledgeLifecycleError::ReadinessTimeout));
        wait_for_file(&record).await;
        let starts = std::fs::read_to_string(record).unwrap();
        assert!(starts.lines().count() >= 3);
    }

    #[test]
    fn restart_backoff_is_exponential_and_capped() {
        let policy = test_policy();
        assert_eq!(restart_backoff(1, policy), Duration::from_millis(20));
        assert_eq!(restart_backoff(2, policy), Duration::from_millis(40));
        assert_eq!(restart_backoff(3, policy), Duration::from_millis(80));
        assert_eq!(restart_backoff(20, policy), Duration::from_millis(80));
    }

    #[test]
    fn config_rejects_relative_or_non_executable_worker_paths() {
        let temp = tempfile::tempdir().unwrap();
        assert!(matches!(
            ManagedWorkerConfig::new(
                PathBuf::from("worker"),
                temp.path().join("worker.sock"),
                temp.path().join("data"),
                "token".into()
            ),
            Err(KnowledgeConfigError::InvalidWorkerBinaryPath)
        ));
        let file = temp.path().join("not-executable");
        std::fs::write(&file, "no").unwrap();
        assert!(matches!(
            ManagedWorkerConfig::new(
                file,
                temp.path().join("worker.sock"),
                temp.path().join("data"),
                "token".into()
            ),
            Err(KnowledgeConfigError::WorkerBinaryUnavailable)
        ));
    }
}
