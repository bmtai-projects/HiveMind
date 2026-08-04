use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::error::ToolError;
use crate::tool::{Tool, ToolResult, ToolStatus, obj_schema};

/// Approval gate for shell commands. Runs synchronously (typically an
/// interactive stdin prompt), so [`Bash`] always calls it through
/// `spawn_blocking` and serializes it behind a lock — see
/// [`Bash::approved`].
pub type ApproveFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;

const MAX_OUTPUT_BYTES: usize = 60_000;

/// A command started with `background: true`, kept only so it can be killed
/// when the session ends (see [`Bash::drop`]).
struct BackgroundProc {
    /// Also the process-group id -- every background command is its own
    /// group leader, so killing `-pid` takes the whole tree with it.
    pid: u32,
    command: String,
    log: PathBuf,
}

/// Runs a shell command in the workspace root. The highest-risk tool, so
/// every command is gated by [`Bash::approve`] unless explicitly disabled
/// (headless/`--yolo` runs pass `None`).
pub struct Bash {
    pub workspace_root: PathBuf,
    pub timeout: Duration,
    pub approve: Option<ApproveFn>,
    /// Multiple `run_shell` calls can be dispatched concurrently (see
    /// [`crate::Registry::dispatch_many`]); this serializes only the
    /// approval *prompt* so concurrent commands don't interleave garbled
    /// text on the terminal. Execution itself still proceeds concurrently
    /// once approved.
    approval_lock: Mutex<()>,
    /// Long-running commands the model explicitly backgrounded. A `std`
    /// mutex, not tokio's: the only writer outside `execute` is `Drop`,
    /// which is synchronous and cannot await.
    background: std::sync::Mutex<Vec<BackgroundProc>>,
}

impl Bash {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            timeout: Duration::from_secs(120),
            approve: None,
            approval_lock: Mutex::new(()),
            background: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn with_approval(mut self, approve: ApproveFn) -> Self {
        self.approve = Some(approve);
        self
    }

    pub fn with_timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    async fn approved(&self, cmd: &str) -> bool {
        let Some(approve) = self.approve.clone() else {
            return true;
        };
        let _guard = self.approval_lock.lock().await;
        let cmd = cmd.to_string();
        tokio::task::spawn_blocking(move || approve(&cmd))
            .await
            .unwrap_or(false)
    }

    /// Start a command that outlives this call: its own process group, output
    /// redirected to a log file the model can read later, and a record kept
    /// so it can be killed at session end.
    ///
    /// This exists because the alternative -- `node server.js &` inside an
    /// ordinary call -- is a trap in both directions. The backgrounded
    /// process inherits the pipes this tool reads to EOF, so the call blocks
    /// until it dies (measured: a shell that exits in 11ms still held the
    /// reader for the background job's full 5s); and once the call gives up,
    /// the orphan keeps running and keeps its port bound, so every later
    /// attempt fails with EADDRINUSE. Both were observed in real sessions.
    fn spawn_background(&self, command: &str) -> Result<String, ToolError> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let log = std::env::temp_dir().join(format!("hivemind-bg-{stamp}.log"));
        let file = std::fs::File::create(&log).map_err(ToolError::Io)?;
        let err_file = file.try_clone().map_err(ToolError::Io)?;

        let mut cmd = shell_command(command);
        cmd.current_dir(strip_verbatim(&self.workspace_root))
            .stdout(Stdio::from(file))
            .stderr(Stdio::from(err_file))
            .stdin(Stdio::null())
            // Deliberately NOT kill_on_drop: surviving this call is the
            // entire point. Cleanup is Drop's job, at session end.
            .kill_on_drop(false);
        own_process_group(&mut cmd);

        // Restarting a server is the common case (edit the file, run it
        // again), and leaving the previous copy alive would hand the model
        // the exact port conflict this whole mechanism exists to prevent --
        // except now self-inflicted. Same command means "replace it".
        let replaced = {
            let mut running = self
                .background
                .lock()
                .expect("background registry mutex poisoned");
            let mut replaced = false;
            running.retain(|p| {
                if p.command == command {
                    kill_process_group(p.pid);
                    let _ = std::fs::remove_file(&p.log);
                    replaced = true;
                    return false;
                }
                true
            });
            replaced
        };

        let child = cmd.spawn().map_err(ToolError::Io)?;
        let pid = child.id().unwrap_or(0);
        // The handle is dropped here on purpose. With kill_on_drop(false)
        // that leaves the process running and reparented; `pid` is the group
        // leader, which is all Drop needs to reap the whole tree later.
        drop(child);

        self.background
            .lock()
            .expect("background registry mutex poisoned")
            .push(BackgroundProc {
                pid,
                command: command.to_string(),
                log: log.clone(),
            });

        let note = if replaced {
            "replaced the previous run of this same command, then "
        } else {
            ""
        };
        Ok(format!(
            "{note}started in background (pid {pid}); it keeps running for the rest of this session.\n\
             Output is being written to {}\n\
             Read that file to check on it (e.g. `tail -20 {}`), and give it a moment to start \
             before connecting.",
            log.display(),
            log.display(),
        ))
    }
}

/// Kill a whole process group, so nothing a command spawned is left behind.
///
/// Unix sends the signal to `-pid` (the group). On Windows, where process
/// groups don't work this way, `taskkill /T` walks the process tree instead.
/// Failure is ignored throughout: the usual cause is that everything already
/// exited, which is the outcome we wanted anyway.
///
/// `pub(crate)`: `create_diagram` (`diagram.rs`) reuses this rather than
/// reimplementing the same unsafe platform code a second time -- it shells
/// out to `mmdc`, which launches a full headless Chromium under the hood,
/// so a timeout there has exactly the same orphan-process risk `run_shell`
/// already solved here.
pub(crate) fn kill_process_group(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(unix)]
    {
        // Safety: a plain libc call with no memory operands. A negative pid
        // addresses the process group; ESRCH (nothing left alive) is fine.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Put the child in its own process group so [`kill_process_group`] can
/// reap its descendants without touching the agent itself. No-op on
/// Windows, which has no equivalent (see `kill_process_group`).
pub(crate) fn own_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

impl Drop for Bash {
    fn drop(&mut self) {
        let procs = match self.background.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            // Poisoned only if a writer panicked; the list is still readable
            // and leaking real processes is worse than ignoring the poison.
            Err(e) => std::mem::take(&mut *e.into_inner()),
        };
        for p in procs {
            kill_process_group(p.pid);
            let _ = std::fs::remove_file(&p.log);
        }
    }
}

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    /// Start the command and return immediately instead of waiting for it.
    #[serde(default)]
    background: bool,
}

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &str {
        "run_shell"
    }
    fn description(&self) -> &str {
        "Run a shell command in the workspace and return combined stdout+stderr. Use for builds, \
         tests, git, and inspection. Set `background: true` for anything that does not exit on its \
         own -- a dev server, a watcher -- which returns immediately and keeps it running for the \
         rest of the session; never background it yourself with a trailing `&`."
    }
    fn schema(&self) -> serde_json::Value {
        obj_schema(
            &[
                (
                    "command",
                    serde_json::json!({"type": "string", "description": "the shell command to execute"}),
                ),
                (
                    "background",
                    serde_json::json!({
                        "type": "boolean",
                        "description": "start it and return immediately, leaving it running for the rest of the session (servers, watchers). Output goes to a log file whose path is returned. Default false."
                    }),
                ),
            ],
            &["command"],
        )
    }
    async fn execute(&self, args: &RawValue) -> Result<ToolResult, ToolError> {
        let a: BashArgs = serde_json::from_str(args.get())?;
        if a.command.is_empty() {
            return Err(ToolError::Message("command is required".into()));
        }
        if !self.approved(&a.command).await {
            // Denied, not Failed: the command never ran, and re-running it
            // will be denied again. The harness must not count this toward
            // the escalation counter -- a user saying no is not the model
            // being stuck.
            return Ok(ToolResult {
                status: ToolStatus::Denied,
                summary: "command denied by user".to_string(),
                retryable: false,
                ..Default::default()
            });
        }

        if a.background {
            // Spawning succeeded or it didn't; either way the command's own
            // exit status is unknown by design (it is still running).
            return self.spawn_background(&a.command).map(ToolResult::ok);
        }

        let mut cmd = shell_command(&a.command);
        cmd.current_dir(strip_verbatim(&self.workspace_root))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            // If the timeout future below is dropped before completion,
            // the child is killed rather than left running detached.
            .kill_on_drop(true);
        own_process_group(&mut cmd);

        let mut child = cmd.spawn().map_err(ToolError::Io)?;
        let pgid = child.id().unwrap_or(0);
        // Read the pipes on their own tasks rather than through
        // `wait_with_output()`. That waits for EOF, and EOF only arrives once
        // *every* writer closes -- including a process the command left
        // running in the background, which inherits these same pipes. That is
        // what turned `node server.js &` into a full 120s timeout even though
        // the shell itself exited in milliseconds.
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let out_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(p) = stdout.as_mut() {
                let _ = p.read_to_end(&mut buf).await;
            }
            buf
        });
        let err_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(p) = stderr.as_mut() {
                let _ = p.read_to_end(&mut buf).await;
            }
            buf
        });

        let status = match tokio::time::timeout(self.timeout, child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(e)) => {
                kill_process_group(pgid);
                return Err(ToolError::Io(e));
            }
            Err(_) => None,
        };

        // Reap whatever the command left running, in both the normal and the
        // timed-out case. This is what stops an abandoned server from holding
        // its port against every later command in the session. Killing before
        // draining is safe: bytes already written stay readable in the pipe
        // buffer after the writer dies.
        kill_process_group(pgid);

        let stdout = out_task.await.unwrap_or_default();
        let stderr = err_task.await.unwrap_or_default();

        let Some(status) = status else {
            // Timeout is its own status: unlike a plain failure, the work may
            // have partly happened, and a retry (or `background: true`) can
            // still be the right move.
            return Ok(ToolResult {
                status: ToolStatus::Timeout,
                summary: format!(
                    "[timed out after {}s; the command and anything it started were killed. \
                     If this was a server or watcher, re-run it with `background: true` instead.]\n{}",
                    self.timeout.as_secs(),
                    format_output(&stdout, &stderr, None)
                ),
                retryable: true,
                ..Default::default()
            });
        };

        // The whole point of M1, in one line: the exit code is *reported*
        // here, where it is known for certain, instead of being re-derived
        // downstream by looking for "[exit:" in the text. Any tool output
        // that merely contains that marker -- a log, our own source, a
        // search hit -- used to read as a failed command.
        Ok(ToolResult {
            status: if status.success() {
                ToolStatus::Ok
            } else {
                ToolStatus::Failed
            },
            summary: format_output(&stdout, &stderr, Some(status)),
            retryable: false,
            ..Default::default()
        })
    }
}

/// Windows has no `bash`; `cmd.exe /C` is the only shell guaranteed to be
/// present. Elsewhere `bash -lc` is kept as-is so login-shell PATH setup
/// (nvm, pyenv, ...) still applies.
///
/// `pub` so hooks (`harness_agent::hooks`) spawn through the exact same
/// selection rather than keeping their own copy. They previously hardcoded
/// `bash`, which meant hooks silently never ran on Windows — invisible
/// while hooks failed open, but a hard stop the moment one can fail closed.
pub fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    }

    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("bash");
        cmd.arg("-lc").arg(command);
        cmd
    }
}

/// Wraps `s` (a real filesystem path, not arbitrary text) in double quotes
/// for embedding in a [`shell_command`] string, for the common case of a
/// path containing a space. Rejects (`None`) anything containing a literal
/// `"` or a control character rather than attempting to escape it.
///
/// `bash -lc` and `cmd /C` don't share one escaping grammar, so there is no
/// single correct answer for "escape a quote inside a quoted string" that
/// works identically on both — but there doesn't need to be: a real
/// filesystem path can't legally contain `"` on Windows at all, so refusing
/// it here costs nothing for the actual use case (`create_diagram`'s
/// resolved output paths) and avoids the class of bug plain wrapping alone
/// would silently mishandle.
pub(crate) fn shell_quote_path(s: &str) -> Option<String> {
    if s.is_empty() || s.contains('"') || s.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(format!("\"{s}\""))
}

/// `Path::canonicalize` on Windows returns a `\\?\C:\...` verbatim path, and
/// `cmd.exe` refuses to start in one ("UNC paths are not supported"). Strip
/// the prefix back to a plain `C:\...` for use as a working directory.
///
/// `pub` for the same reason as [`shell_command`]: hooks spawn a shell in
/// the workspace root too, and the CLI canonicalizes that root before
/// handing it over — so on Windows every hook would fail to spawn without
/// this. Any new spawn site that sets `current_dir` from a canonicalized
/// path needs it as well.
pub fn strip_verbatim(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        if let Some(Component::Prefix(p)) = path.components().next() {
            match p.kind() {
                Prefix::VerbatimDisk(drive) => {
                    let rest: PathBuf = path.components().skip(1).collect();
                    return PathBuf::from(format!("{}:\\", drive as char)).join(rest);
                }
                // A verbatim UNC share (`\\?\UNC\server\share`) has no plain
                // equivalent that gains anything, so it's left alone.
                _ => {}
            }
        }
        path.to_path_buf()
    }

    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

/// `status` is `None` when the command timed out and never produced one --
/// the caller reports that itself, so no exit line is appended here.
fn format_output(stdout: &[u8], stderr: &[u8], status: Option<std::process::ExitStatus>) -> String {
    let mut stdout = String::from_utf8_lossy(stdout).into_owned();
    let mut stderr = String::from_utf8_lossy(stderr).into_owned();
    truncate_in_place(&mut stdout);
    truncate_in_place(&mut stderr);

    let mut out = String::new();
    if !stdout.is_empty() {
        out.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("[stderr]\n");
        out.push_str(&stderr);
    }
    if let Some(status) = status
        && !status.success()
    {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("[exit: {status}]"));
    }
    if out.is_empty() {
        out.push_str("(no output; exit 0)");
    }
    out
}

/// Truncate at a valid UTF-8 char boundary at or before `MAX_OUTPUT_BYTES`.
fn truncate_in_place(s: &mut String) {
    if s.len() > MAX_OUTPUT_BYTES {
        let mut idx = MAX_OUTPUT_BYTES;
        while idx > 0 && !s.is_char_boundary(idx) {
            idx -= 1;
        }
        s.truncate(idx);
        s.push_str("\n[truncated]");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::value::RawValue;

    fn args(json: serde_json::Value) -> Box<RawValue> {
        RawValue::from_string(json.to_string()).unwrap()
    }

    #[test]
    fn shell_quote_path_wraps_a_plain_path() {
        assert_eq!(
            shell_quote_path("/tmp/a.svg"),
            Some("\"/tmp/a.svg\"".to_string())
        );
    }

    #[test]
    fn shell_quote_path_wraps_a_path_containing_a_space() {
        assert_eq!(
            shell_quote_path("/tmp/my diagrams/a.svg"),
            Some("\"/tmp/my diagrams/a.svg\"".to_string())
        );
    }

    #[test]
    fn shell_quote_path_rejects_an_embedded_quote() {
        assert_eq!(shell_quote_path("/tmp/weird\"name/a.svg"), None);
    }

    #[test]
    fn shell_quote_path_rejects_empty_and_control_characters() {
        assert_eq!(shell_quote_path(""), None);
        assert_eq!(shell_quote_path("/tmp/a\nb.svg"), None);
    }

    fn bash() -> Bash {
        Bash::new(std::env::temp_dir()).with_timeout(Duration::from_secs(10))
    }

    /// Is `pid` still alive? Used to assert on real reaping rather than on
    /// this module's own bookkeeping.
    #[cfg(unix)]
    fn alive(pid: u32) -> bool {
        // Signal 0 checks for existence without delivering anything.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    #[tokio::test]
    async fn ordinary_commands_still_return_output_and_exit_status() {
        let out = bash()
            .execute(&args(serde_json::json!({"command": "echo hello"})))
            .await
            .unwrap()
            .summary;
        assert!(out.contains("hello"), "{out}");

        let failed = bash()
            .execute(&args(
                serde_json::json!({"command": "echo oops >&2; exit 3"}),
            ))
            .await
            .unwrap()
            .summary;
        assert!(failed.contains("oops"), "{failed}");
        assert!(
            failed.contains("[exit:"),
            "a non-zero exit must be visible: {failed}"
        );
    }

    /// The measured bug: `wait_with_output()` waits for pipe EOF, and a
    /// backgrounded process inherits those pipes, so the call used to block
    /// for the background job's whole lifetime (a 120s timeout in the real
    /// session that prompted this). The shell exits immediately; so must we.
    #[tokio::test]
    async fn a_trailing_ampersand_no_longer_blocks_until_the_background_job_ends() {
        let started = std::time::Instant::now();
        let out = bash()
            .execute(&args(
                serde_json::json!({"command": "sleep 30 & echo started"}),
            ))
            .await
            .unwrap()
            .summary;
        let elapsed = started.elapsed();

        assert!(out.contains("started"), "{out}");
        assert!(
            elapsed < Duration::from_secs(5),
            "returned in {elapsed:?}; the backgrounded `sleep 30` is holding the pipe again"
        );
    }

    /// The other half of the same bug: the orphan used to survive the call
    /// and keep its port bound, which is what produced the real session's
    /// EADDRINUSE retry cascade.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_process_left_running_by_a_command_is_reaped_not_orphaned() {
        let started = std::time::Instant::now();
        let out = bash()
            .execute(&args(serde_json::json!({
                "command": "sleep 30 & echo \"pid=$!\""
            })))
            .await
            .unwrap()
            .summary;
        // Without this the test can pass for the wrong reason: if the call
        // blocks on the pipe instead of reaping, `sleep 30` finishes on its
        // own before the liveness check below and looks correctly cleaned up.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "call took {:?} -- it waited for the background job rather than reaping it",
            started.elapsed()
        );

        let pid: u32 = out
            .lines()
            .find_map(|l| l.trim().strip_prefix("pid="))
            .and_then(|p| p.trim().parse().ok())
            .unwrap_or_else(|| panic!("no pid in output: {out}"));

        // The kill is synchronous inside execute(), but the OS still needs a
        // moment to tear the process down before it stops answering signal 0.
        for _ in 0..50 {
            if !alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Don't leak it into the rest of the suite if the assertion fails.
        kill_process_group(pid);
        panic!("pid {pid} outlived its run_shell call");
    }

    /// A timed-out command must not leave its work running either -- that was
    /// the path the real session actually hit.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_timed_out_command_takes_its_children_with_it() {
        let started = std::time::Instant::now();
        let out = Bash::new(std::env::temp_dir())
            .with_timeout(Duration::from_millis(300))
            .execute(&args(serde_json::json!({
                "command": "sleep 30 & echo \"pid=$!\"; sleep 30"
            })))
            .await
            .unwrap()
            .summary;
        // Same vacuous-pass guard as the test above: the timeout has to
        // actually cut the call short, not be followed by a 30s pipe drain.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timed-out call still took {:?} to return",
            started.elapsed()
        );

        assert!(out.contains("timed out"), "{out}");
        let pid: u32 = out
            .lines()
            .find_map(|l| l.trim().strip_prefix("pid="))
            .and_then(|p| p.trim().parse().ok())
            .unwrap_or_else(|| panic!("no pid in output: {out}"));

        for _ in 0..50 {
            if !alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        kill_process_group(pid);
        panic!("pid {pid} survived a timed-out run_shell call");
    }

    /// `background: true` is the supported way to keep something running, so
    /// it must genuinely survive the call that started it -- the one case
    /// where reaping would be wrong.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_background_command_survives_the_call_and_dies_with_the_session() {
        let tool = bash();
        let out = tool
            .execute(&args(serde_json::json!({
                "command": "sleep 30", "background": true
            })))
            .await
            .unwrap()
            .summary;

        let pid: u32 = out
            .split_once("pid ")
            .and_then(|(_, rest)| rest.split(')').next())
            .and_then(|p| p.trim().parse().ok())
            .unwrap_or_else(|| panic!("no pid in output: {out}"));
        assert!(
            alive(pid),
            "a backgrounded command must outlive its own call"
        );
        assert!(
            out.contains("hivemind-bg-"),
            "must report a log path: {out}"
        );

        // Session end.
        drop(tool);
        for _ in 0..50 {
            if !alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        kill_process_group(pid);
        panic!("pid {pid} survived the session that owned it");
    }

    /// Re-running the same background command is "restart it", not "race the
    /// old copy for its port".
    #[cfg(unix)]
    #[tokio::test]
    async fn restarting_the_same_background_command_replaces_the_previous_one() {
        let tool = bash();
        let cmd = serde_json::json!({"command": "sleep 31", "background": true});

        let first = tool.execute(&args(cmd.clone())).await.unwrap().summary;
        let first_pid: u32 = first
            .split_once("pid ")
            .and_then(|(_, rest)| rest.split(')').next())
            .and_then(|p| p.trim().parse().ok())
            .unwrap();

        let second = tool.execute(&args(cmd)).await.unwrap().summary;
        assert!(
            second.contains("replaced"),
            "should say it replaced: {second}"
        );

        for _ in 0..50 {
            if !alive(first_pid) {
                drop(tool);
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        drop(tool);
        panic!("the previous background copy (pid {first_pid}) was left running");
    }

    #[tokio::test]
    async fn a_denied_command_never_runs() {
        let tool = bash().with_approval(Arc::new(|_| false));
        let marker = std::env::temp_dir().join("hivemind_denied_marker.txt");
        let _ = std::fs::remove_file(&marker);

        let out = tool
            .execute(&args(serde_json::json!({
                "command": format!("touch {}", marker.display())
            })))
            .await
            .unwrap()
            .summary;

        assert!(out.contains("denied"), "{out}");
        assert!(!marker.exists(), "a denied command must not have run");
    }
}
