use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::error::ToolError;
use crate::tool::{Tool, obj_schema};

/// Approval gate for shell commands. Runs synchronously (typically an
/// interactive stdin prompt), so [`Bash`] always calls it through
/// `spawn_blocking` and serializes it behind a lock — see
/// [`Bash::approved`].
pub type ApproveFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;

const MAX_OUTPUT_BYTES: usize = 60_000;

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
}

impl Bash {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            timeout: Duration::from_secs(120),
            approve: None,
            approval_lock: Mutex::new(()),
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
}

#[derive(Deserialize)]
struct BashArgs {
    command: String,
}

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &str {
        "run_shell"
    }
    fn description(&self) -> &str {
        "Run a shell command in the workspace and return combined stdout+stderr. Use for builds, tests, git, and inspection."
    }
    fn schema(&self) -> serde_json::Value {
        obj_schema(
            &[(
                "command",
                serde_json::json!({"type": "string", "description": "the shell command to execute"}),
            )],
            &["command"],
        )
    }
    async fn execute(&self, args: &RawValue) -> Result<String, ToolError> {
        let a: BashArgs = serde_json::from_str(args.get())?;
        if a.command.is_empty() {
            return Err(ToolError::Message("command is required".into()));
        }
        if !self.approved(&a.command).await {
            return Ok("command denied by user".to_string());
        }

        let mut cmd = Command::new("bash");
        cmd.arg("-lc")
            .arg(&a.command)
            .current_dir(&self.workspace_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            // If the timeout future below is dropped before completion,
            // the child is killed rather than left running detached.
            .kill_on_drop(true);

        let child = cmd.spawn().map_err(ToolError::Io)?;
        let run = child.wait_with_output();

        match tokio::time::timeout(self.timeout, run).await {
            Ok(Ok(output)) => Ok(format_output(&output)),
            Ok(Err(e)) => Err(ToolError::Io(e)),
            Err(_) => Ok(format!("[timed out after {}s]", self.timeout.as_secs())),
        }
    }
}

fn format_output(output: &std::process::Output) -> String {
    let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
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
    if !output.status.success() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("[exit: {}]", output.status));
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
