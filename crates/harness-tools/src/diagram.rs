//! `create_diagram` — render Mermaid diagram source (flowchart, sequence,
//! class, ER, state, gantt, ...) to an image file.
//!
//! # Why this isn't `create_pdf`/`create_spreadsheet`'s "no external
//! runtime" shape
//!
//! Those two tools are pure Rust because their output format is simple
//! enough to generate directly. Mermaid rendering is not: doing it
//! offline and dependency-free means either shipping a full layout engine
//! (mermaid.js's own reference implementation is ~20k lines even in a
//! from-scratch Rust port — see the design discussion this tool came out
//! of) or accepting a real gap from mermaid.js's actual behavior. Neither
//! is proportionate to what a coding agent needs here.
//!
//! Instead this tool shells out to `mmdc` (`@mermaid-js/mermaid-cli`) —
//! the real reference renderer — **if it's on `PATH`**, and degrades
//! gracefully when it isn't:
//!
//! - The raw Mermaid source is *always* written first, as its own `.mmd`
//!   file. This alone satisfies "always produce a valid, usable artifact"
//!   even with zero tooling installed — the source is directly viewable at
//!   <https://mermaid.live> or in any Mermaid-aware editor.
//! - Rendering to an actual image is best-effort on top of that. A
//!   missing `mmdc`, a syntax error in the diagram, or a render timeout
//!   all degrade to a clear, actionable note in the tool's *successful*
//!   result — never a hard tool failure — because the source was written
//!   either way and a failed extra step doesn't invalidate that.
//!
//! # Process-group safety
//!
//! `mmdc` launches a full headless Chromium under Puppeteer to do the
//! actual rendering. A hung or killed-on-timeout `mmdc` can easily leave
//! that Chromium process running. This reuses `crate::bash`'s own
//! process-group spawn/kill/timeout machinery (the same mechanism
//! `run_shell` uses to avoid leaking background servers) rather than a
//! second, unaudited copy of the same unsafe platform code.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio::io::AsyncReadExt;

use crate::bash::{kill_process_group, own_process_group, shell_command, shell_quote_path};
use crate::error::ToolError;
use crate::fs::Workspace;
use crate::tool::{Tool, obj_schema};

/// Upper bound on accepted diagram source. Generous for anything
/// hand-authored or model-generated; exists so a pathological input can't
/// be handed to a subprocess unbounded.
const MAX_DIAGRAM_BYTES: usize = 100 * 1024;

/// How long to wait for `mmdc --version` before concluding it isn't
/// usably installed. Short and strict on purpose: this check exists
/// specifically to fail *fast* rather than let a missing tool discover
/// itself only after a slow Puppeteer cold start (see [`RENDER_TIMEOUT`]).
const AVAILABILITY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to allow the real render. `mmdc` launches a headless Chromium
/// under Puppeteer; a cold cache (first run, or a fresh CI container) can
/// spend several seconds just starting the browser before rendering
/// begins at all, so this is deliberately generous rather than tuned to
/// the fast-path time.
const RENDER_TIMEOUT: Duration = Duration::from_secs(45);

/// Recognized first-token diagram-type keywords, used only for an
/// *advisory* pre-flight check (see [`sniff_diagram_type`]) — never to
/// reject a write. Mermaid's grammar keeps gaining diagram types; the real
/// authority on whether source is valid is `mmdc` itself, not this list.
const KNOWN_DIAGRAM_KEYWORDS: &[&str] = &[
    "flowchart",
    "graph",
    "sequenceDiagram",
    "classDiagram",
    "classDiagram-v2",
    "stateDiagram",
    "stateDiagram-v2",
    "erDiagram",
    "journey",
    "gantt",
    "pie",
    "quadrantChart",
    "requirementDiagram",
    "gitGraph",
    "mindmap",
    "timeline",
    "sankey-beta",
    "xychart-beta",
    "block-beta",
    "packet-beta",
    "kanban",
    "architecture-beta",
    "radar-beta",
    "C4Context",
    "C4Container",
    "C4Component",
    "C4Dynamic",
];

#[derive(Deserialize)]
struct DiagramArgs {
    path: String,
    diagram: String,
}

/// What kind of output `path` asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputKind {
    Svg,
    Png,
    /// `path` ends in `.mmd` -- the caller wants the source only, no
    /// render attempted. Handling this explicitly (rather than rejecting
    /// `.mmd` as an unsupported extension) covers a real, ordinary
    /// request: "give me the diagram source, I'll render it myself."
    SourceOnly,
}

fn output_kind(path: &str) -> Result<OutputKind, ToolError> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".svg") {
        Ok(OutputKind::Svg)
    } else if lower.ends_with(".png") {
        Ok(OutputKind::Png)
    } else if lower.ends_with(".mmd") {
        Ok(OutputKind::SourceOnly)
    } else {
        Err(ToolError::Message(format!(
            "{path}: path must end in .svg, .png, or .mmd (source only, no render)"
        )))
    }
}

pub struct CreateDiagram(pub Workspace);

#[async_trait]
impl Tool for CreateDiagram {
    fn conflict_key(&self, args: &RawValue) -> Option<String> {
        crate::fs::path_conflict_key(&self.0, args)
    }
    fn name(&self) -> &str {
        "create_diagram"
    }
    fn description(&self) -> &str {
        "Render Mermaid diagram source (flowchart, sequence, class, ER, state, gantt, and more) to \
         an image. `path` must end in .svg, .png, or .mmd (source only, no render attempted). For \
         .svg/.png, the raw Mermaid source is also always written alongside it as its own .mmd file \
         -- so the result is never wasted even if rendering isn't possible. Rendering needs `mmdc` \
         (mermaid-cli) on PATH; when it's missing or the diagram has a syntax error, the tool still \
         succeeds and explains what to do instead (the .mmd source is always usable on its own at \
         https://mermaid.live or in any Mermaid-aware editor). To set a theme or colors, put a \
         Mermaid init directive as the first line of `diagram`, e.g. \
         `%%{init: {'theme':'dark'}}%%`."
    }
    fn schema(&self) -> serde_json::Value {
        obj_schema(
            &[
                (
                    "path",
                    serde_json::json!({"type": "string", "description": "workspace-relative output path, ending in .svg, .png, or .mmd"}),
                ),
                (
                    "diagram",
                    serde_json::json!({"type": "string", "description": "Mermaid diagram source text"}),
                ),
            ],
            &["path", "diagram"],
        )
    }

    async fn execute(&self, args: &RawValue) -> Result<String, ToolError> {
        let a: DiagramArgs = serde_json::from_str(args.get())?;
        if a.path.is_empty() {
            return Err(ToolError::Message("path is required".into()));
        }
        if a.diagram.trim().is_empty() {
            return Err(ToolError::Message(
                "diagram is empty -- nothing to render".into(),
            ));
        }
        if a.diagram.len() > MAX_DIAGRAM_BYTES {
            return Err(ToolError::Message(format!(
                "diagram is {} bytes, over the {MAX_DIAGRAM_BYTES}-byte limit -- split it into \
                 smaller diagrams",
                a.diagram.len()
            )));
        }
        let kind = output_kind(&a.path)?;
        let advisory = sniff_diagram_type(&a.diagram);

        // Same create-parent-dirs-then-resolve dance as `write_file`/`create_pdf`:
        // the path may not exist yet, so `Workspace::resolve`'s canonicalize
        // step needs the parent directory to already be there.
        let candidate = Path::new(&a.path);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.0.root.join(candidate)
        };
        if let Some(parent) = joined.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let output_path = self.0.resolve(&a.path)?;

        if kind == OutputKind::SourceOnly {
            tokio::fs::write(&output_path, &a.diagram).await?;
            self.0.read_set.record(&output_path, &a.diagram);
            return Ok(format!(
                "wrote {} ({} bytes){}",
                a.path,
                a.diagram.len(),
                advisory.unwrap_or_default()
            ));
        }

        // The source is written *before* attempting to render, and its
        // result is never rolled back on a render failure -- a partially
        // successful outcome (source written, image not) is still a
        // genuinely useful one, not a failure to unwind.
        let source_path = output_path.with_extension("mmd");
        tokio::fs::write(&source_path, &a.diagram).await?;
        self.0.read_set.record(&source_path, &a.diagram);
        let source_label = display_relative(&a.path, "mmd");

        match render_via_mmdc(&source_path, &output_path).await {
            RenderOutcome::Rendered => Ok(format!(
                "wrote {source_label} (source) and rendered {}{}",
                a.path,
                advisory.unwrap_or_default()
            )),
            RenderOutcome::ToolMissing => Ok(format!(
                "wrote {source_label} -- mmdc (mermaid-cli) isn't installed, so no image was \
                 rendered. Install it with `npm install -g @mermaid-js/mermaid-cli` and try again, \
                 or view/render the source as-is at https://mermaid.live, or in an editor with \
                 Mermaid preview (e.g. VS Code's Markdown preview).{}",
                advisory.unwrap_or_default()
            )),
            RenderOutcome::Failed(reason) => Ok(format!(
                "wrote {source_label} -- mmdc failed to render it: {reason}. This usually means a \
                 syntax error in the diagram; fix it and try again, or paste the source at \
                 https://mermaid.live to see the parser's own error.{}",
                advisory.unwrap_or_default()
            )),
        }
    }
}

/// `a.path` with its extension swapped to `ext`, formatted for the result
/// message. Purely cosmetic (the real write already used `source_path`);
/// kept separate so the message reads as a workspace-relative path like
/// every other tool's output, not an absolute one.
fn display_relative(path: &str, ext: &str) -> String {
    match path.rsplit_once('.') {
        Some((stem, _)) => format!("{stem}.{ext}"),
        None => format!("{path}.{ext}"),
    }
}

/// Advisory-only pre-flight: does `source` start (after stripping a
/// leading `%%{...}%%` init directive and `%%` comment lines) with a
/// recognized Mermaid diagram-type keyword? Returns a note to append to
/// the result when it doesn't -- never blocks the write, and never the
/// sole authority on validity (`mmdc`'s own parser is, when it runs).
fn sniff_diagram_type(source: &str) -> Option<&'static str> {
    let mut body = source.trim_start();
    if let Some(rest) = body.strip_prefix("%%{")
        && let Some(end) = rest.find("}%%")
    {
        body = rest[end + 3..].trim_start();
    }
    while let Some(rest) = body.strip_prefix("%%") {
        body = match rest.find('\n') {
            Some(nl) => rest[nl + 1..].trim_start(),
            None => "",
        };
    }
    if body.is_empty() {
        return Some(
            "\n(note: after any init directive/comments, no diagram content was left -- if this \
             fails to render, check the source)",
        );
    }
    let first_word = body
        .split(|c: char| c.is_whitespace() || c == ':')
        .next()
        .unwrap_or("");
    let recognized = KNOWN_DIAGRAM_KEYWORDS
        .iter()
        .any(|k| first_word.eq_ignore_ascii_case(k));
    if recognized {
        None
    } else {
        Some(
            "\n(note: the source doesn't start with a diagram type this tool recognizes -- if it \
             fails to render, check the first line)",
        )
    }
}

enum RenderOutcome {
    Rendered,
    ToolMissing,
    Failed(String),
}

async fn render_via_mmdc(source_path: &Path, output_path: &Path) -> RenderOutcome {
    let (Some(src), Some(out)) = (
        source_path.to_str().and_then(shell_quote_path),
        output_path.to_str().and_then(shell_quote_path),
    ) else {
        // Not reachable via this tool's own writes (Workspace-resolved
        // paths never contain a quote or non-UTF-8 bytes in practice), but
        // failing closed here rather than building a malformed shell
        // string is the correct response if it ever is.
        return RenderOutcome::Failed(
            "the resolved output path can't be safely passed to mmdc (unusual characters)".into(),
        );
    };

    // Checked first, and with a short timeout, specifically so a missing
    // `mmdc` fails fast rather than only being discovered after paying for
    // a slow Puppeteer/Chromium cold start on the real render below.
    if !mmdc_available().await {
        return RenderOutcome::ToolMissing;
    }

    // `-b transparent`: a diagram meant to be pasted into a README or
    // viewed in either a light or dark viewer should not carry a hardcoded
    // white background that clashes with a dark one.
    let Some(result) = run_reaped(
        &format!("mmdc -i {src} -o {out} -b transparent"),
        RENDER_TIMEOUT,
    )
    .await
    else {
        return RenderOutcome::ToolMissing;
    };

    if result.timed_out {
        return RenderOutcome::Failed(format!("timed out after {}s", RENDER_TIMEOUT.as_secs()));
    }
    if !result.success {
        return RenderOutcome::Failed(tail(&result.stderr));
    }
    match tokio::fs::metadata(output_path).await {
        Ok(m) if m.len() > 0 => RenderOutcome::Rendered,
        _ => RenderOutcome::Failed("mmdc exited successfully but produced no output file".into()),
    }
}

/// Whether `mmdc --version` runs and succeeds. A dedicated probe rather
/// than inferring availability from the real render's failure mode: exit
/// codes for "command not found" are not portably distinguishable from an
/// ordinary tool failure across `bash` and `cmd.exe`, but a clean,
/// dedicated version check is unambiguous on both.
async fn mmdc_available() -> bool {
    run_reaped("mmdc --version", AVAILABILITY_TIMEOUT)
        .await
        .is_some_and(|r| r.success)
}

/// What running one command to completion (or timeout) produced.
struct SpawnResult {
    success: bool,
    timed_out: bool,
    stderr: Vec<u8>,
}

/// Spawn `command` (through [`shell_command`], in its own process group),
/// wait up to `timeout`, and reap the whole process group unconditionally
/// afterward -- success, failure, or timeout alike. `None` only when the
/// shell itself couldn't be spawned (not "command not found", which the
/// shell reports as a normal nonzero exit, but the outer `bash`/`cmd.exe`
/// process failing to start at all).
///
/// The single call site both [`mmdc_available`] and [`render_via_mmdc`]
/// share, so the process-group reaping guarantee lives in exactly one
/// place rather than being repeated (and potentially drifting) across two.
async fn run_reaped(command: &str, timeout: Duration) -> Option<SpawnResult> {
    let mut cmd = shell_command(command);
    cmd.stdout(Stdio::null())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);
    own_process_group(&mut cmd);

    let mut child = cmd.spawn().ok()?;
    let pgid = child.id().unwrap_or(0);
    let mut stderr = child.stderr.take();
    let err_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(p) = stderr.as_mut() {
            let _ = p.read_to_end(&mut buf).await;
        }
        buf
    });

    let status = tokio::time::timeout(timeout, child.wait()).await;
    // Reap unconditionally -- exactly the same reasoning as `run_shell`:
    // whatever the command left running (an orphaned Chromium, for mmdc)
    // must not survive this call, timed out or not.
    kill_process_group(pgid);
    let stderr = err_task.await.unwrap_or_default();

    Some(match status {
        Ok(Ok(s)) => SpawnResult {
            success: s.success(),
            timed_out: false,
            stderr,
        },
        Ok(Err(_)) => SpawnResult {
            success: false,
            timed_out: false,
            stderr,
        },
        Err(_) => SpawnResult {
            success: false,
            timed_out: true,
            stderr,
        },
    })
}

/// Last few lines of `mmdc`'s stderr, for a result message that stays
/// short even when the underlying tool (or the Chromium it drives) is
/// verbose on failure.
fn tail(stderr: &[u8]) -> String {
    const MAX_LINES: usize = 8;
    const MAX_CHARS: usize = 2_000;
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(MAX_LINES);
    let mut joined = lines[start..].join("\n");
    if joined.trim().is_empty() {
        return "mmdc exited with an error and produced no output on stderr".to_string();
    }
    if joined.len() > MAX_CHARS {
        joined.truncate(MAX_CHARS);
        joined.push_str(" [truncated]");
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Is `pid` still alive? Same idiom as `crate::bash`'s own tests --
    /// asserts on the real OS process, not on this module's bookkeeping.
    #[cfg(unix)]
    fn alive(pid: u32) -> bool {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    /// `run_reaped` is the one call site both `mmdc_available` and
    /// `render_via_mmdc` funnel through, so this is what actually proves
    /// the process-group reaping guarantee -- without needing a real
    /// `mmdc` install, since a hung `sleep` demonstrates exactly the same
    /// failure mode a hung Puppeteer/Chromium would.
    ///
    /// Goes through the real `run_reaped` code path (not a hand-rolled
    /// spawn+kill), and verifies the *actual OS process* is gone
    /// afterward -- not just that the function returned `timed_out`.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_reaped_kills_the_process_it_started_on_timeout() {
        // stdout is /dev/null'd inside run_reaped, so the spawned shell's
        // own pid ($$) is routed through a file instead of captured output.
        // Filename is plain alphanumerics on purpose -- this is embedded
        // directly into a shell command string below, unquoted, the same
        // way a careless caller could get this wrong (production call
        // sites never do this; they always go through `shell_quote_path`
        // first).
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid_file = std::env::temp_dir().join(format!(
            "hivemind-diagram-reap-test-{}-{nonce}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&pid_file);
        let command = format!("echo $$ > {} ; sleep 30", pid_file.display());

        let started = std::time::Instant::now();
        let result = run_reaped(&command, Duration::from_millis(300))
            .await
            .expect("the shell itself must spawn");
        assert!(result.timed_out);
        // The load-bearing assertion. Without this, a version of
        // `run_reaped` that forgets to kill the process group still
        // "passes" -- it just blocks on draining `stderr` until the
        // un-killed `sleep 30` exits on its own 30 seconds later, then
        // finds the process dead for the wrong reason. Caught by mutation
        // testing: removing the `kill_process_group` call left every
        // assertion below passing, just ~30s slower.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?} -- run_reaped must not block on an unreaped child's pipes",
            started.elapsed()
        );

        for _ in 0..20 {
            if pid_file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("pid file should have been written before the sleep")
            .trim()
            .parse()
            .unwrap();
        let _ = std::fs::remove_file(&pid_file);

        // SIGKILL delivery isn't synchronous from the sender's side, so
        // poll briefly rather than asserting immediately.
        for _ in 0..20 {
            if !alive(pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !alive(pid),
            "the process run_reaped started must not survive its own timeout"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_reaped_reports_success_and_no_timeout_for_an_ordinary_command() {
        let result = run_reaped("exit 0", Duration::from_secs(5)).await.unwrap();
        assert!(result.success);
        assert!(!result.timed_out);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_reaped_reports_failure_without_timing_out_for_a_bad_exit_code() {
        let result = run_reaped("echo bad >&2; exit 1", Duration::from_secs(5))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(!result.timed_out);
        assert!(String::from_utf8_lossy(&result.stderr).contains("bad"));
    }

    fn ws(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!("hivemind_diagram_test_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Workspace::new(dir)
    }

    fn args(json: serde_json::Value) -> Box<RawValue> {
        RawValue::from_string(json.to_string()).unwrap()
    }

    // --- output_kind --------------------------------------------------

    #[test]
    fn recognizes_every_supported_extension_case_insensitively() {
        assert_eq!(output_kind("a.svg").unwrap(), OutputKind::Svg);
        assert_eq!(output_kind("a.SVG").unwrap(), OutputKind::Svg);
        assert_eq!(output_kind("a.png").unwrap(), OutputKind::Png);
        assert_eq!(output_kind("a.PNG").unwrap(), OutputKind::Png);
        assert_eq!(output_kind("a.mmd").unwrap(), OutputKind::SourceOnly);
    }

    #[test]
    fn an_unsupported_extension_is_a_clear_error() {
        let err = output_kind("a.pdf").unwrap_err().to_string();
        assert!(err.contains(".svg"));
        assert!(err.contains(".png"));
        assert!(err.contains(".mmd"));
    }

    // --- sniff_diagram_type ---------------------------------------------

    #[test]
    fn a_recognized_diagram_type_gets_no_advisory() {
        assert!(sniff_diagram_type("flowchart LR\nA-->B").is_none());
        assert!(sniff_diagram_type("  sequenceDiagram\nA->>B: hi").is_none());
        assert!(sniff_diagram_type("graph TD\nA-->B").is_none());
    }

    #[test]
    fn an_unrecognized_first_token_gets_an_advisory_note() {
        let note = sniff_diagram_type("this is not mermaid at all").unwrap();
        assert!(note.contains("diagram type"));
    }

    #[test]
    fn an_init_directive_is_skipped_before_sniffing() {
        assert!(sniff_diagram_type("%%{init: {'theme':'dark'}}%%\nflowchart LR\nA-->B").is_none());
    }

    #[test]
    fn leading_comment_lines_are_skipped_before_sniffing() {
        assert!(
            sniff_diagram_type("%% just a comment\n%% another one\nflowchart LR\nA-->B").is_none()
        );
    }

    #[test]
    fn init_directive_and_comments_together_are_both_skipped() {
        assert!(
            sniff_diagram_type(
                "%%{init: {'theme':'dark'}}%%\n%% a note\nsequenceDiagram\nA->>B: hi"
            )
            .is_none()
        );
    }

    #[test]
    fn source_that_is_only_an_init_directive_gets_an_advisory_not_a_panic() {
        let note = sniff_diagram_type("%%{init: {'theme':'dark'}}%%").unwrap();
        assert!(note.contains("no diagram content"));
    }

    // --- tail --------------------------------------------------------------

    #[test]
    fn tail_keeps_only_the_last_few_lines() {
        let stderr: String = (0..50).map(|i| format!("line {i}\n")).collect();
        let out = tail(stderr.as_bytes());
        assert!(out.contains("line 49"));
        assert!(!out.contains("line 0\n"));
    }

    #[test]
    fn tail_of_empty_stderr_is_still_a_useful_message() {
        assert!(!tail(&[]).is_empty());
    }

    #[test]
    fn tail_truncates_a_very_long_single_line() {
        let stderr = "x".repeat(10_000);
        let out = tail(stderr.as_bytes());
        assert!(out.len() < 3_000);
        assert!(out.ends_with("[truncated]"));
    }

    // --- execute: validation & source-only path (no subprocess involved) --

    #[tokio::test]
    async fn empty_path_is_rejected() {
        let w = ws("empty_path");
        let err = CreateDiagram(w)
            .execute(&args(
                serde_json::json!({"path": "", "diagram": "flowchart LR\nA-->B"}),
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("path is required"));
    }

    #[tokio::test]
    async fn empty_diagram_is_rejected() {
        let w = ws("empty_diagram");
        let err = CreateDiagram(w)
            .execute(&args(
                serde_json::json!({"path": "a.svg", "diagram": "   "}),
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[tokio::test]
    async fn oversized_diagram_is_rejected_before_any_write() {
        let w = ws("oversized");
        let huge = "x".repeat(MAX_DIAGRAM_BYTES + 1);
        let err = CreateDiagram(w.clone())
            .execute(&args(serde_json::json!({"path": "a.svg", "diagram": huge})))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("limit"));
        assert!(
            !w.root.join("a.mmd").exists(),
            "must reject before writing anything"
        );
    }

    #[tokio::test]
    async fn unsupported_extension_is_rejected_before_any_write() {
        let w = ws("bad_ext");
        let err = CreateDiagram(w.clone())
            .execute(&args(
                serde_json::json!({"path": "a.pdf", "diagram": "flowchart LR\nA-->B"}),
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains(".svg"));
        assert!(!w.root.join("a.pdf").exists());
    }

    #[tokio::test]
    async fn mmd_path_writes_source_only_with_no_render_attempt() {
        let w = ws("source_only");
        let out = CreateDiagram(w.clone())
            .execute(&args(serde_json::json!({
                "path": "flow.mmd", "diagram": "flowchart LR\nA-->B"
            })))
            .await
            .unwrap();
        assert!(out.contains("wrote flow.mmd"));
        assert_eq!(
            std::fs::read_to_string(w.root.join("flow.mmd")).unwrap(),
            "flowchart LR\nA-->B"
        );
        // Nothing else should appear -- no sibling .mmd-of-a-.mmd, no image.
        let entries: Vec<_> = std::fs::read_dir(&w.root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["flow.mmd"]);
    }

    #[tokio::test]
    async fn nested_directories_are_created_for_the_mmd_path() {
        let w = ws("nested_mmd");
        CreateDiagram(w.clone())
            .execute(&args(serde_json::json!({
                "path": "docs/diagrams/flow.mmd", "diagram": "flowchart LR\nA-->B"
            })))
            .await
            .unwrap();
        assert!(w.root.join("docs/diagrams/flow.mmd").exists());
    }

    #[tokio::test]
    async fn an_unrecognized_diagram_type_still_writes_the_source_with_an_advisory() {
        let w = ws("advisory_source_only");
        let out = CreateDiagram(w.clone())
            .execute(&args(serde_json::json!({
                "path": "weird.mmd", "diagram": "not mermaid at all"
            })))
            .await
            .unwrap();
        assert!(out.contains("diagram type"));
        assert!(w.root.join("weird.mmd").exists());
    }

    #[tokio::test]
    async fn source_is_recorded_in_the_read_set_so_a_later_edit_is_not_falsely_stale() {
        let w = ws("readset");
        CreateDiagram(w.clone())
            .execute(&args(serde_json::json!({
                "path": "flow.mmd", "diagram": "flowchart LR\nA-->B"
            })))
            .await
            .unwrap();
        let resolved = w.resolve("flow.mmd").unwrap();
        assert!(!w.read_set.is_stale(&resolved, "flowchart LR\nA-->B"));
    }
}
