//! Supervision of the Obsidian Headless (`ob`) sync daemon.
//!
//! The MCP server owns the sync daemon: `ob sync --continuous` is spawned as a
//! child process at startup and restarted with exponential backoff whenever it
//! exits. Credentials and E2EE stay entirely inside `ob` — `ob login` and
//! `ob sync-setup` are performed out-of-band on the host before this server
//! starts; this module never handles them.
//!
//! [`sync_status`] and [`sync_now`] run one-shot `ob` commands on demand for
//! the corresponding MCP tools. Headless is an open beta and its output
//! format is not a stable contract, so the raw text is captured and surfaced
//! as-is rather than parsed into structured fields.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{ChildStderr, ChildStdout, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Cap on captured output from one-shot `ob` commands, protecting MCP
/// clients' context windows from runaway CLI output.
pub const MAX_CAPTURE: usize = 64 * 1024;

/// Timeouts for the one-shot commands so a hung `ob` can never block a tool
/// call forever; on timeout the command is killed and reported as such.
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(30);
/// A one-shot sync may legitimately take minutes on a large vault or a slow
/// link, so it gets a much longer leash than `sync-status`.
pub const SYNC_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Result of a one-shot `ob` command, with capped output.
#[derive(Debug, Clone)]
pub struct SyncRunOutcome {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Captured stdout (capped at [`MAX_CAPTURE`] bytes).
    pub stdout: String,
    /// Captured stderr, or the spawn error message if the command could not
    /// be started at all.
    pub stderr: String,
}

impl SyncRunOutcome {
    fn spawn_error(err: &io::Error) -> Self {
        Self {
            success: false,
            exit_code: None,
            timed_out: false,
            stdout: String::new(),
            stderr: format!("failed to run ob: {err}"),
        }
    }

    /// Human-facing text for tool responses: stdout, falling back to stderr.
    pub fn best_output(&self) -> &str {
        let stdout = self.stdout.trim();
        if !stdout.is_empty() {
            stdout
        } else {
            self.stderr.trim()
        }
    }
}

/// Runs `ob` with the given arguments, capturing its output until it exits or
/// `timeout` elapses (in which case it is killed).
pub async fn run_ob(ob_bin: &str, args: &[OsString], timeout: Duration) -> SyncRunOutcome {
    let mut child = match Command::new(ob_bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(err) => return SyncRunOutcome::spawn_error(&err),
    };

    // Drain both pipes on a separate task so the child can never deadlock on
    // a full pipe while we wait for its exit.
    let reader = spawn_pipe_drain(child.stdout.take(), child.stderr.take());

    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            let (stdout, stderr) = reader.await.unwrap_or_default();
            SyncRunOutcome {
                success: status.success(),
                exit_code: status.code(),
                timed_out: false,
                stdout,
                stderr,
            }
        }
        Ok(Err(err)) => SyncRunOutcome::spawn_error(&err),
        Err(_elapsed) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let (stdout, stderr) = reader.await.unwrap_or_default();
            SyncRunOutcome {
                success: false,
                exit_code: None,
                timed_out: true,
                stdout,
                stderr,
            }
        }
    }
}

/// Runs `ob sync-status --path <vault>` once.
pub async fn sync_status(ob_bin: &str, vault_root: &Path) -> SyncRunOutcome {
    let args = ob_path_args("sync-status", vault_root);
    run_ob(ob_bin, &args, STATUS_TIMEOUT).await
}

/// Runs a one-shot `ob sync --path <vault>` (no `--continuous`).
pub async fn sync_now(ob_bin: &str, vault_root: &Path) -> SyncRunOutcome {
    let args = ob_path_args("sync", vault_root);
    run_ob(ob_bin, &args, SYNC_TIMEOUT).await
}

fn ob_path_args(subcommand: &str, vault_root: &Path) -> Vec<OsString> {
    vec![
        OsString::from(subcommand),
        OsString::from("--path"),
        vault_root.as_os_str().to_os_string(),
    ]
}

/// Reads both pipes concurrently, each capped at [`MAX_CAPTURE`] bytes, and
/// returns them as lossy UTF-8 strings.
fn spawn_pipe_drain(
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
) -> JoinHandle<(String, String)> {
    async fn read_capped(
        stream: Option<impl AsyncRead + Unpin + Send>,
        buf: &mut Vec<u8>,
    ) -> io::Result<()> {
        match stream {
            // `take` bounds the read so a pathological process streaming
            // gigabytes cannot exhaust memory before its timeout fires.
            Some(stream) => stream
                .take(MAX_CAPTURE as u64)
                .read_to_end(buf)
                .await
                .map(|_| ()),
            None => Ok(()),
        }
    }

    tokio::spawn(async move {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (a, b) = tokio::join!(read_capped(stdout, &mut out), read_capped(stderr, &mut err));
        if let (Err(a), Err(b)) = (a, b) {
            tracing::warn!(error_a = %a, error_b = %b, "failed to drain ob output pipes");
        }
        (capped_string(out), capped_string(err))
    })
}

fn capped_string(bytes: Vec<u8>) -> String {
    let text = String::from_utf8_lossy(&bytes);
    if bytes.len() >= MAX_CAPTURE {
        // A truncated multibyte sequence becomes a replacement character in
        // the lossy conversion; the marker tells the reader output was cut.
        format!("{text}\n[output truncated at {MAX_CAPTURE} bytes]")
    } else {
        text.into_owned()
    }
}

/// Tunables for [`SyncSupervisor`], exposed so tests can shrink the timings.
#[derive(Debug, Clone)]
pub struct SupervisorOptions {
    /// Delay before the first restart attempt after an unhealthy exit.
    pub initial_backoff: Duration,
    /// Upper bound on the restart delay; the ladder doubles until this.
    pub max_backoff: Duration,
    /// A daemon run that stayed up at least this long is considered healthy;
    /// the backoff resets for the restart that follows it.
    pub healthy_run: Duration,
}

impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            healthy_run: Duration::from_secs(5 * 60),
        }
    }
}

/// Snapshot of the sync daemon's supervision state, surfaced by the
/// `sync_status` MCP tool alongside the one-shot `ob` output.
#[derive(Debug, Clone, Default)]
pub struct SupervisorState {
    /// Whether the `ob sync --continuous` child is currently running.
    pub running: bool,
    /// Restarts performed after the first run attempt, including retries
    /// after spawn failures (e.g. `ob` not installed).
    pub restarts: u32,
    /// Exit code of the last daemon run, if it exited normally.
    pub last_exit_code: Option<i32>,
    /// Last error from spawning or waiting on the daemon, if any.
    pub last_error: Option<String>,
    /// Wall-clock time the daemon was last started.
    pub last_started_at: Option<SystemTime>,
    /// How long the last daemon run stayed up.
    pub last_uptime: Option<Duration>,
    /// Backoff delay scheduled before the next restart attempt, when not
    /// currently running.
    pub last_backoff: Option<Duration>,
}

/// Handle to the supervised `ob sync --continuous` daemon. Cheap to keep
/// behind an `Arc` and share with the MCP session handlers.
#[derive(Debug)]
pub struct SyncSupervisor {
    state: Arc<Mutex<SupervisorState>>,
    shutdown: watch::Sender<bool>,
    // Taken by `stop()`; a Mutex lets the handle be shared behind an Arc
    // while still awaiting the task exactly once.
    task: Mutex<Option<JoinHandle<()>>>,
}

impl SyncSupervisor {
    /// Starts supervising the sync daemon with default options.
    pub fn start(ob_bin: &str, vault_root: &Path) -> Self {
        Self::start_with(ob_bin, vault_root, SupervisorOptions::default())
    }

    /// Starts supervising the sync daemon, restarting it with backoff
    /// whenever it exits. Returns immediately; the daemon runs on a tokio
    /// task until [`SyncSupervisor::stop`] is called.
    pub fn start_with(ob_bin: &str, vault_root: &Path, opts: SupervisorOptions) -> Self {
        let state = Arc::new(Mutex::new(SupervisorState::default()));
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(supervise(
            ob_bin.to_owned(),
            vault_root.to_path_buf(),
            opts,
            Arc::clone(&state),
            shutdown_rx,
        ));
        Self {
            state,
            shutdown,
            task: Mutex::new(Some(task)),
        }
    }

    /// Current supervision state.
    pub fn state(&self) -> SupervisorState {
        self.state
            .lock()
            .expect("supervisor state lock poisoned")
            .clone()
    }

    /// Signals the daemon to stop, kills the `ob` child, and waits for the
    /// supervisor task to finish. Returns a snapshot of the final
    /// supervision state. Idempotent: a second call is a no-op.
    pub async fn stop(&self) -> SupervisorState {
        let _ = self.shutdown.send(true);
        // Take the handle out of the lock before awaiting, so the (sync)
        // MutexGuard is never held across an await point.
        let task = self
            .task
            .lock()
            .expect("supervisor task lock poisoned")
            .take();
        if let Some(task) = task
            && let Err(err) = task.await
        {
            tracing::warn!(error = %err, "sync supervisor task panicked");
        }
        self.state()
    }
}

fn set_state(state: &Mutex<SupervisorState>, f: impl FnOnce(&mut SupervisorState)) {
    f(&mut state.lock().expect("supervisor state lock poisoned"));
}

/// Supervision loop: spawn, log output, wait for exit, back off, repeat.
async fn supervise(
    ob_bin: String,
    vault_root: PathBuf,
    opts: SupervisorOptions,
    state: Arc<Mutex<SupervisorState>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut backoff = opts.initial_backoff;
    loop {
        let spawned = Command::new(&ob_bin)
            .arg("sync")
            .arg("--continuous")
            .arg("--path")
            .arg(&vault_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();

        let mut child = match spawned {
            Ok(child) => child,
            Err(err) => {
                tracing::error!(error = %err, "failed to spawn ob sync daemon");
                set_state(&state, |s| {
                    s.last_error = Some(err.to_string());
                    s.last_backoff = Some(backoff);
                });
                if !backoff_then_stop(&mut backoff, &opts, &mut shutdown).await {
                    return;
                }
                set_state(&state, |s| s.restarts += 1);
                continue;
            }
        };

        set_state(&state, |s| {
            s.running = true;
            s.last_error = None;
            s.last_started_at = Some(SystemTime::now());
        });
        tracing::info!("ob sync daemon started");

        if let Some(stdout) = child.stdout.take() {
            log_lines(stdout, "stdout");
        }
        if let Some(stderr) = child.stderr.take() {
            log_lines(stderr, "stderr");
        }

        let started = Instant::now();
        let exited = tokio::select! {
            status = child.wait() => status,
            _ = shutdown.changed() => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                set_state(&state, |s| s.running = false);
                tracing::info!("ob sync daemon stopped");
                return;
            }
        };

        let uptime = started.elapsed();
        let status = match exited {
            Ok(status) => status,
            Err(err) => {
                tracing::error!(error = %err, "failed to wait on ob sync daemon");
                set_state(&state, |s| {
                    s.running = false;
                    s.last_error = Some(err.to_string());
                    s.last_uptime = Some(uptime);
                    s.last_backoff = Some(backoff);
                });
                if !backoff_then_stop(&mut backoff, &opts, &mut shutdown).await {
                    return;
                }
                set_state(&state, |s| s.restarts += 1);
                continue;
            }
        };

        set_state(&state, |s| {
            s.running = false;
            s.last_exit_code = status.code();
            s.last_uptime = Some(uptime);
        });
        tracing::warn!(
            code = status.code(),
            uptime_secs = uptime.as_secs(),
            "ob sync daemon exited; restarting with backoff"
        );

        // A healthy run means the exit was transient (network blip, ob
        // upgrade), so restart promptly; a short-lived run escalates.
        if uptime >= opts.healthy_run {
            backoff = opts.initial_backoff;
        }
        set_state(&state, |s| s.last_backoff = Some(backoff));
        if !backoff_then_stop(&mut backoff, &opts, &mut shutdown).await {
            return;
        }
        set_state(&state, |s| s.restarts += 1);
    }
}

/// Waits `backoff` (unless shutdown is signaled first), then grows the
/// backoff ladder. Returns false when shutdown was requested.
async fn backoff_then_stop(
    backoff: &mut Duration,
    opts: &SupervisorOptions,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(*backoff) => {}
        _ = shutdown.changed() => return false,
    }
    *backoff = backoff.saturating_mul(2).min(opts.max_backoff);
    true
}

/// Logs a daemon output stream line by line under the `ob_sync` target.
fn log_lines(stream: impl AsyncRead + Unpin + Send + 'static, label: &'static str) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::info!(target: "ob_sync", stream = label, "{line}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write script");
        let mut perms = std::fs::metadata(&path)
            .expect("script metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod script");
        path
    }

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn script_bin(path: &Path) -> String {
        path.to_str().expect("utf-8 temp path").to_owned()
    }

    // Polls until `check` passes or a deadline elapses, so tests tolerate
    // scheduling jitter without fixed sleeps.
    async fn wait_for(check: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !check() {
            assert!(
                Instant::now() < deadline,
                "condition not reached within 5s; state never became true"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn non_utf8_ob_output_is_captured_lossily() {
        let dir = temp_dir();
        // Invalid UTF-8 on both pipes must not fail the run: output is
        // captured lossily, keeping the readable parts (and the invalid
        // bytes become U+FFFD rather than aborting the tool call).
        let script = write_script(
            dir.path(),
            "ob-lossy",
            "#!/bin/sh\nprintf 'ok \\377\\376 out\\n'\nprintf 'bad \\377\\n' >&2\nexit 0\n",
        );
        let outcome = sync_status(&script_bin(&script), dir.path()).await;
        assert!(outcome.success, "stderr: {:?}", outcome.stderr);
        assert!(
            outcome.stdout.contains("ok"),
            "stdout readable parts must survive: {:?}",
            outcome.stdout
        );
        assert!(
            outcome.stdout.contains('\u{fffd}'),
            "invalid bytes must be lossy-converted: {:?}",
            outcome.stdout
        );
        assert!(
            outcome.stderr.contains('\u{fffd}'),
            "stderr invalid bytes must be lossy-converted: {:?}",
            outcome.stderr
        );
    }

    #[tokio::test]
    async fn sync_status_captures_output() {
        let dir = temp_dir();
        let script = write_script(dir.path(), "ob", "#!/bin/sh\necho 'vault: synced'\n");
        let outcome = sync_status(&script_bin(&script), dir.path()).await;
        assert!(outcome.success);
        assert_eq!(outcome.exit_code, Some(0));
        assert!(!outcome.timed_out);
        assert!(outcome.best_output().contains("vault: synced"));
    }

    #[tokio::test]
    async fn sync_now_passes_path_and_not_continuous() {
        let dir = temp_dir();
        // Records its arguments next to the script for inspection.
        let script = write_script(
            dir.path(),
            "ob",
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/args.txt\"\necho done\n",
        );
        let outcome = sync_now(&script_bin(&script), dir.path()).await;
        assert!(outcome.success, "stderr: {}", outcome.stderr);

        let args: Vec<String> = std::fs::read_to_string(dir.path().join("args.txt"))
            .expect("args file")
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(args, vec!["sync", "--path", dir.path().to_str().unwrap()]);
        assert!(!args.contains(&"--continuous".to_string()));
    }

    #[tokio::test]
    async fn missing_ob_reports_spawn_error() {
        let dir = temp_dir();
        let missing = dir.path().join("no-such-ob");
        let outcome = sync_status(missing.to_str().unwrap(), dir.path()).await;
        assert!(!outcome.success);
        assert!(outcome.stderr.contains("failed to run ob"));
    }

    #[tokio::test]
    async fn hung_command_is_killed_at_timeout() {
        let dir = temp_dir();
        // `exec` so the kill on timeout hits the sleep itself, not a shell
        // that would leave the sleep orphaned.
        let script = write_script(dir.path(), "ob", "#!/bin/sh\nexec sleep 30\n");
        let args = vec![OsString::from("sync-status")];
        let outcome = run_ob(&script_bin(&script), &args, Duration::from_millis(200)).await;
        assert!(!outcome.success);
        assert!(outcome.timed_out);
    }

    #[tokio::test]
    async fn long_output_is_capped() {
        let dir = temp_dir();
        // `yes` floods stdout far past the cap quickly. The 2s timeout is
        // generous so a busy test runtime can't starve the pipe drain and
        // make the test flaky; `yes` fills 64 KB in microseconds when
        // scheduled at all.
        let script = write_script(dir.path(), "ob", "#!/bin/sh\nexec yes\n");
        let args = vec![OsString::from("sync-status")];
        let outcome = run_ob(&script_bin(&script), &args, Duration::from_secs(2)).await;
        assert!(
            outcome.stdout.len() >= MAX_CAPTURE,
            "len={} timed_out={} stdout_tail={:?} stderr_tail={:?}",
            outcome.stdout.len(),
            outcome.timed_out,
            &outcome.stdout[outcome.stdout.len().saturating_sub(200)..],
            &outcome.stderr[outcome.stderr.len().saturating_sub(200)..]
        );
        assert!(outcome.stdout.contains("output truncated"));
    }

    #[tokio::test]
    async fn supervisor_runs_and_stops_daemon() {
        let dir = temp_dir();
        let script = write_script(dir.path(), "ob", "#!/bin/sh\nsleep 30\n");
        let supervisor = SyncSupervisor::start(&script_bin(&script), dir.path());
        wait_for(|| supervisor.state().running).await;
        assert_eq!(supervisor.state().restarts, 0);
        let final_state = supervisor.stop().await;
        assert!(!final_state.running);
    }

    #[tokio::test]
    async fn supervisor_restarts_exited_daemon_with_backoff() {
        let dir = temp_dir();
        let script = write_script(dir.path(), "ob", "#!/bin/sh\nexit 3\n");
        let opts = SupervisorOptions {
            initial_backoff: Duration::from_millis(20),
            max_backoff: Duration::from_millis(100),
            healthy_run: Duration::from_secs(60),
        };
        let supervisor = SyncSupervisor::start_with(&script_bin(&script), dir.path(), opts);
        wait_for(|| supervisor.state().restarts >= 2).await;
        let state = supervisor.state();
        assert_eq!(state.last_exit_code, Some(3));
        assert!(state.last_backoff.is_some());
        assert!(state.last_uptime.is_some());
        supervisor.stop().await;
    }

    #[tokio::test]
    async fn spawn_failure_is_retried() {
        let dir = temp_dir();
        let missing = dir.path().join("no-such-ob").to_str().unwrap().to_owned();
        let opts = SupervisorOptions {
            initial_backoff: Duration::from_millis(20),
            max_backoff: Duration::from_millis(100),
            healthy_run: Duration::from_secs(60),
        };
        let supervisor = SyncSupervisor::start_with(&missing, dir.path(), opts);
        wait_for(|| supervisor.state().restarts >= 1).await;
        let state = supervisor.state();
        assert!(!state.running);
        assert!(state.last_error.is_some());
        supervisor.stop().await;
    }

    #[tokio::test]
    async fn healthy_run_resets_backoff() {
        let dir = temp_dir();
        let script = write_script(dir.path(), "ob", "#!/bin/sh\nsleep 0.3\n");
        let opts = SupervisorOptions {
            initial_backoff: Duration::from_millis(20),
            max_backoff: Duration::from_secs(60),
            // Any run of 0.3s counts as healthy, so every restart waits the
            // initial backoff instead of escalating.
            healthy_run: Duration::from_millis(100),
        };
        let supervisor = SyncSupervisor::start_with(&script_bin(&script), dir.path(), opts);
        wait_for(|| supervisor.state().restarts >= 2).await;
        let state = supervisor.state();
        assert_eq!(state.last_backoff, Some(Duration::from_millis(20)));
        assert!(state.last_uptime.unwrap() >= Duration::from_millis(100));
        supervisor.stop().await;
    }
}
