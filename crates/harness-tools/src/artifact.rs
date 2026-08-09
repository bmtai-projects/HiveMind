//! Artifact handles: keep a large tool result on disk, and put a preview
//! plus a handle in the conversation instead.
//!
//! # Why
//!
//! Every message in a session is re-sent on every subsequent turn, so one
//! 2 MB `cargo test` result is not paid for once -- it is paid for again on
//! each remaining turn of the run. Trimming (`trim.rs`) drops such results
//! once they are old, but only *after* they have already been billed several
//! times, and once dropped the content is gone for good.
//!
//! Writing the full text to disk and inlining a preview changes both halves
//! of that: the recurring cost falls to the size of the preview, and the
//! content stops being lost -- `read_artifact` can still fetch any range of
//! it, including after a compaction has discarded the original message.
//!
//! # What makes the preview useful
//!
//! Head-and-tail alone would be a trap for the motivating case. A 40,000-line
//! test run puts its failures in the *middle*; the first 80 lines are the
//! build log and the last 80 are a summary count. A preview that omitted the
//! failures would be smaller and useless, which is worse than expensive --
//! the model would fetch the artifact every time, and cost more than
//! inlining. So the preview also scans the whole text for diagnostics and
//! lifts them out, deduplicated. That clustering is the part that has to
//! keep working; `error_clusters_survive_a_head_and_tail_preview` asserts
//! head+tail alone would have missed them, so it cannot silently regress.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::error::ToolError;
use crate::tool::{Tool, ToolResult, obj_schema};

/// Results at or above this many bytes are offloaded. Chosen from the
/// baseline in `Mydoc/week2-m1-m2-plan.md`: it captures the results holding
/// ~80% of transcript bytes while leaving the median (4,772 bytes) inline,
/// so ordinary reads are completely unaffected.
pub const DEFAULT_ARTIFACT_THRESHOLD_BYTES: usize = 10_000;

/// Lines kept verbatim from each end of an offloaded result.
const PREVIEW_HEAD_LINES: usize = 80;
const PREVIEW_TAIL_LINES: usize = 80;

/// Most distinct diagnostics to lift into the preview. A run with hundreds
/// of *distinct* failures is already unreadable inline; the handle is there
/// for that case.
const MAX_ERROR_CLUSTERS: usize = 20;

/// A stored result, and what the model needs to ask for more of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactHandle {
    /// `artifact://<session-id>/<call-id>/<stream>`
    pub uri: String,
    pub total_lines: usize,
    pub total_bytes: usize,
}

/// Where offloaded results live: `<root>/<session-id>/<call-id>.<stream>.txt`.
///
/// Deliberately a sibling of the session store rather than a directory
/// inside it, so `SessionStore`'s own "a directory of plain files a human
/// can `rm`" property is preserved for both.
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Write `content` and return the handle naming it.
    pub fn store(
        &self,
        session_id: &str,
        call_id: &str,
        stream: &str,
        content: &str,
    ) -> Result<ArtifactHandle, ToolError> {
        let (session, call, stream) = validated_parts(session_id, call_id, stream)?;
        let dir = self.root.join(session);
        std::fs::create_dir_all(&dir).map_err(ToolError::Io)?;
        let path = dir.join(format!("{call}.{stream}.txt"));
        std::fs::write(&path, content).map_err(ToolError::Io)?;
        restrict_to_owner(&path);

        Ok(ArtifactHandle {
            uri: format!("artifact://{session}/{call}/{stream}"),
            total_lines: content.lines().count(),
            total_bytes: content.len(),
        })
    }

    /// Read a line range, with `read_file`'s exact semantics: `offset` is a
    /// 1-indexed first line, `limit` a line count.
    pub fn read_slice(
        &self,
        uri: &str,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<String, ToolError> {
        let path = self.path_for(uri)?;
        let text = std::fs::read_to_string(&path).map_err(|e| {
            ToolError::Message(format!(
                "artifact {uri} is no longer on disk ({e}). Sessions and their \
                 artifacts are pruned together after the retention window."
            ))
        })?;

        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        // `offset: 0` is a 1-indexing slip, not a request for a phantom line
        // -- matching `read_file`'s handling exactly rather than inventing a
        // second convention for the same argument.
        let start = offset.unwrap_or(1).max(1);
        if start > total {
            return Ok(format!(
                "[offset {start} is past the end of the artifact: {total} line(s) total]"
            ));
        }
        let end = match limit {
            Some(n) => (start + n.max(1) - 1).min(total),
            None => total,
        };

        let mut out = lines[start - 1..end].join("\n");
        if end < total {
            out.push_str(&format!(
                "\n[{} more line(s); read on with offset={}]",
                total - end,
                end + 1
            ));
        }
        Ok(out)
    }

    /// Drop every artifact belonging to these sessions. Called with whatever
    /// `SessionStore::prune_older_than_except` removed, so artifacts cannot
    /// outlive the conversation that produced them -- leaking them forever
    /// would be a worse bug than the cost this feature exists to fix.
    pub fn remove_sessions(&self, session_ids: &[String]) -> usize {
        let mut removed = 0;
        for id in session_ids {
            let Ok(part) = safe_component(id) else {
                continue;
            };
            if std::fs::remove_dir_all(self.root.join(part)).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    /// Drop artifact directories that no live session claims and that
    /// nothing has written to for `max_age_secs`.
    ///
    /// `remove_sessions` alone is not enough: an unpersisted `-p` run stores
    /// under a per-process id that no session record will ever name, and a
    /// crash between writing an artifact and saving the session leaves the
    /// same kind of orphan. Without this sweep those accumulate forever --
    /// silently, since nothing references them.
    ///
    /// Age is taken from the newest file in the directory, so a long-running
    /// session's directory is never collected out from under it.
    pub fn prune_orphans(&self, max_age_secs: u64, live_session_ids: &[String]) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return 0;
        };
        let now = std::time::SystemTime::now();
        let mut removed = 0;
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if live_session_ids.iter().any(|id| id == name) {
                continue;
            }
            let Some(newest) = newest_mtime(&path) else {
                continue;
            };
            // A directory dated in the future (clock skew, restored backup)
            // reads as age 0 and is kept, matching SessionStore's own
            // saturating handling rather than deleting user data on a
            // arithmetic wrap.
            let age = now.duration_since(newest).map(|d| d.as_secs()).unwrap_or(0);
            if age > max_age_secs && std::fs::remove_dir_all(&path).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    /// Resolve a handle to a real path, refusing anything that would escape
    /// the store. The URI reaches here straight from model-authored tool
    /// arguments, so it is untrusted input: `artifact://../../.ssh/id_rsa`
    /// must not resolve, and neither must an absolute path smuggled through
    /// a component.
    fn path_for(&self, uri: &str) -> Result<PathBuf, ToolError> {
        let rest = uri.strip_prefix("artifact://").ok_or_else(|| {
            ToolError::Message(format!(
                "not an artifact handle: {uri}. Expected artifact://<session>/<call>/<stream>"
            ))
        })?;
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() != 3 {
            return Err(ToolError::Message(format!(
                "malformed artifact handle: {uri}. Expected artifact://<session>/<call>/<stream>"
            )));
        }
        let (session, call, stream) = validated_parts(parts[0], parts[1], parts[2])?;
        Ok(self.root.join(session).join(format!("{call}.{stream}.txt")))
    }
}

/// Newest mtime among a directory's files, or `None` if it has none --
/// which is itself a reason not to guess at an age and delete.
fn newest_mtime(dir: &Path) -> Option<std::time::SystemTime> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter_map(|m| m.modified().ok())
        .max()
}

/// Every URI component must be a plain name. Rejecting a leading `.` takes
/// out `.` and `..` -- and therefore every traversal built from them -- in
/// the same rule that rejects hidden files, rather than trying to spot
/// traversal after the fact by inspecting the joined path.
fn safe_component(s: &str) -> Result<&str, ToolError> {
    let ok = !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if ok {
        Ok(s)
    } else {
        Err(ToolError::Message(format!(
            "unsafe artifact path component: {s:?}"
        )))
    }
}

fn validated_parts<'a>(
    session: &'a str,
    call: &'a str,
    stream: &'a str,
) -> Result<(&'a str, &'a str, &'a str), ToolError> {
    Ok((
        safe_component(session)?,
        safe_component(call)?,
        safe_component(stream)?,
    ))
}

/// Artifacts are new user data on disk and can hold anything a command
/// printed, including secrets. Match the credentials file's posture rather
/// than inheriting whatever the umask happens to be.
#[cfg(unix)]
fn restrict_to_owner(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

/// POSIX mode bits don't exist on the Windows release target. The parent
/// directory lives under the user's own profile, which is the protection
/// that actually applies there -- a fake `set_permissions` call would only
/// look reassuring.
#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) {}

/// A diagnostic worth lifting out of the middle of a large output.
///
/// Matched case-insensitively against the whole line. Kept deliberately
/// narrow: `warning:` is excluded because a build with 400 warnings and one
/// error would otherwise bury the error under warnings, which is the exact
/// failure this is meant to prevent.
const ERROR_MARKERS: &[&str] = &[
    "error[",
    "error:",
    "error ",
    "failed",
    "failure",
    "panicked at",
    "assertion",
    "exception",
    "traceback",
    "fatal",
    "undefined reference",
    "segmentation fault",
    "cannot find",
    "not found",
    "test result: fail",
];

fn looks_diagnostic(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    ERROR_MARKERS.iter().any(|m| lower.contains(m))
}

/// Collapse repeats so 500 copies of one error read as one entry with a
/// count, instead of filling the preview budget with the same sentence.
/// Digits are folded so lines differing only by a line number, address, or
/// duration cluster together.
fn normalize_for_dedupe(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut last_was_digit = false;
    for c in line.trim().chars() {
        if c.is_ascii_digit() {
            if !last_was_digit {
                out.push('#');
            }
            last_was_digit = true;
        } else {
            out.push(c);
            last_was_digit = false;
        }
    }
    out
}

struct Cluster {
    first_line_no: usize,
    text: String,
    count: usize,
}

fn error_clusters(lines: &[&str]) -> Vec<Cluster> {
    let mut clusters: Vec<Cluster> = Vec::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, line) in lines.iter().enumerate() {
        if !looks_diagnostic(line) {
            continue;
        }
        let key = normalize_for_dedupe(line);
        if let Some(&idx) = seen.get(&key) {
            clusters[idx].count += 1;
            continue;
        }
        if clusters.len() == MAX_ERROR_CLUSTERS {
            continue;
        }
        seen.insert(key, clusters.len());
        clusters.push(Cluster {
            first_line_no: i + 1,
            text: line.trim().to_string(),
            count: 1,
        });
    }
    clusters
}

fn human_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

/// Which text, if any, should be archived instead of inlined.
///
/// The whole offload policy in one pure function, so the decision can be
/// tested directly rather than only through a live agent turn. Returns the
/// text to archive, or `None` to leave the result exactly as the tool
/// produced it.
///
/// Two things it gets right that a naive `summary.len() > n` would not:
/// the size test is against the *pre-truncation* output when a tool clamped
/// itself (otherwise a 2 MB `cargo test` looks like a harmless 60 KB and is
/// never archived), and a threshold of `0` disables the feature rather than
/// archiving everything.
pub fn text_to_offload(result: &ToolResult, threshold_bytes: usize) -> Option<&str> {
    if threshold_bytes == 0 {
        return None;
    }
    let full = result.full_output.as_deref().unwrap_or(&result.summary);
    (full.len() >= threshold_bytes).then_some(full)
}

/// The text that replaces an offloaded result in the conversation.
///
/// Small enough that re-sending it every turn is cheap, and complete enough
/// that the model usually does not need to fetch the artifact at all -- an
/// extra round trip it can't skip would eat the saving (V7).
pub fn preview(content: &str, handle: &ArtifactHandle) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    // Short enough to show whole: the handle is still recorded, but there is
    // nothing to elide, so don't pretend there is.
    if total <= PREVIEW_HEAD_LINES + PREVIEW_TAIL_LINES {
        return format!(
            "{content}\n\n[artifact] {} — {} line(s), {}. Full copy retained; \
             re-read any range with read_artifact.",
            handle.uri,
            total,
            human_bytes(handle.total_bytes)
        );
    }

    let head = lines[..PREVIEW_HEAD_LINES].join("\n");
    let tail = lines[total - PREVIEW_TAIL_LINES..].join("\n");
    let elided = total - PREVIEW_HEAD_LINES - PREVIEW_TAIL_LINES;

    let clusters = error_clusters(&lines);
    let mut middle = String::new();
    if clusters.is_empty() {
        middle.push_str(&format!("\n[… {elided} line(s) elided …]\n"));
    } else {
        middle.push_str(&format!(
            "\n[… {elided} line(s) elided. {} distinct diagnostic(s) found across the \
             whole output, including the elided part:]\n",
            clusters.len()
        ));
        for c in &clusters {
            let repeat = if c.count > 1 {
                format!(" (×{})", c.count)
            } else {
                String::new()
            };
            middle.push_str(&format!(
                "  line {}: {}{}\n",
                c.first_line_no, c.text, repeat
            ));
        }
        if clusters.len() == MAX_ERROR_CLUSTERS {
            middle.push_str("  […more diagnostics beyond this cap; read the artifact…]\n");
        }
    }

    format!(
        "{head}\n{middle}\n{tail}\n\n[artifact] {} — {} line(s), {}. \
         This is a preview; read any range with read_artifact(handle, offset, limit).",
        handle.uri,
        total,
        human_bytes(handle.total_bytes)
    )
}

// ---------------------------------------------------------------------------
// read_artifact
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ReadArtifactArgs {
    handle: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

/// Fetch any range of a previously offloaded result.
///
/// `offset`/`limit` deliberately mirror `read_file` rather than inventing a
/// second paging convention: the model already knows that shape, and the
/// tool manifest stays internally consistent.
pub struct ReadArtifact(pub Arc<ArtifactStore>);

#[async_trait]
impl Tool for ReadArtifact {
    fn name(&self) -> &str {
        "read_artifact"
    }

    fn description(&self) -> &str {
        "Read part of a large tool result that was stored as an artifact. When a result is too \
         big to inline, the transcript shows a preview ending in an `[artifact] artifact://...` \
         handle -- pass that handle here to read the full text, including the part the preview \
         elided. `offset` is the 1-indexed first line and `limit` the number of lines, exactly \
         as in read_file. Omit both to read from the start. The preview already lifts out every \
         distinct diagnostic found anywhere in the output, so check it before fetching: if what \
         you need is already there, this call is a wasted round trip."
    }

    fn schema(&self) -> serde_json::Value {
        obj_schema(
            &[
                (
                    "handle",
                    serde_json::json!({
                        "type": "string",
                        "description": "the artifact:// handle shown in the result preview",
                    }),
                ),
                (
                    "offset",
                    serde_json::json!({
                        "type": "integer",
                        "minimum": 1,
                        "description": "1-indexed line to start at. Omit to start at the top.",
                    }),
                ),
                (
                    "limit",
                    serde_json::json!({
                        "type": "integer",
                        "minimum": 1,
                        "description": "how many lines to read from offset. Omit to read to the end.",
                    }),
                ),
            ],
            &["handle"],
        )
    }

    async fn execute(&self, args: &RawValue) -> Result<ToolResult, ToolError> {
        let a: ReadArtifactArgs = serde_json::from_str(args.get())?;
        Ok(ToolResult::ok(
            self.0.read_slice(&a.handle, a.offset, a.limit)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "hm-artifact-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn store() -> ArtifactStore {
        ArtifactStore::new(tmp())
    }

    #[test]
    fn a_stored_artifact_round_trips() {
        let s = store();
        let content = (1..=100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let h = s.store("sess1", "call1", "output", &content).unwrap();
        assert_eq!(h.uri, "artifact://sess1/call1/output");
        assert_eq!(h.total_lines, 100);

        let slice = s.read_slice(&h.uri, Some(10), Some(3)).unwrap();
        assert!(slice.starts_with("line 10\nline 11\nline 12"), "{slice}");
        assert!(slice.contains("read on with offset=13"), "{slice}");
    }

    #[test]
    fn offset_and_limit_match_read_file_semantics() {
        let s = store();
        let content = (1..=10)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let h = s.store("sess1", "c", "output", &content).unwrap();

        // offset 0 is a 1-indexing slip, not a phantom line.
        assert!(
            s.read_slice(&h.uri, Some(0), Some(1))
                .unwrap()
                .starts_with("l1")
        );
        // A limit past the end clamps rather than erroring.
        assert!(
            s.read_slice(&h.uri, Some(9), Some(999))
                .unwrap()
                .contains("l10")
        );
        // An offset past the end says so.
        assert!(
            s.read_slice(&h.uri, Some(50), None)
                .unwrap()
                .contains("past the end")
        );
    }

    /// The handle arrives as model-authored tool arguments, so it is
    /// untrusted. Traversal must be refused, not merely fail to find a file.
    #[test]
    fn a_handle_cannot_escape_the_store() {
        let s = store();
        for hostile in [
            "artifact://../../../etc/passwd/x/y",
            "artifact://sess/../../../etc/passwd/output",
            "artifact://./x/output",
            "artifact:///etc/passwd/x/y",
            "artifact://sess/call",
            "file:///etc/passwd",
            "artifact://sess/call/output/extra",
        ] {
            assert!(
                s.read_slice(hostile, None, None).is_err(),
                "accepted hostile handle: {hostile}"
            );
        }
    }

    /// An unpersisted `-p` run stores under a per-process id no session
    /// record will ever name. Without this sweep those pile up forever --
    /// which is how the feature would have quietly become a disk leak.
    #[test]
    fn orphaned_directories_are_swept_but_live_sessions_are_left_alone() {
        let s = store();
        let orphan = s
            .store("tmp-999-1", "c", "output", "from a -p run")
            .unwrap();
        let live = s
            .store("realsession", "c", "output", "still in use")
            .unwrap();

        // Nothing is old enough yet, so nothing goes.
        assert_eq!(s.prune_orphans(3600, &["realsession".to_string()]), 0);
        assert!(s.read_slice(&orphan.uri, None, None).is_ok());

        // Age both directories by a month. Real mtimes rather than a zero
        // window: `age > max_age` is strict here exactly as it is in
        // SessionStore::prune_older_than_except, so a zero window would
        // prove nothing about ageing.
        let month_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86_400);
        for dir in ["tmp-999-1", "realsession"] {
            for e in std::fs::read_dir(s.root().join(dir)).unwrap().flatten() {
                std::fs::File::options()
                    .write(true)
                    .open(e.path())
                    .unwrap()
                    .set_modified(month_ago)
                    .unwrap();
            }
        }

        // The orphan goes; the one a live session still claims stays.
        assert_eq!(
            s.prune_orphans(14 * 86_400, &["realsession".to_string()]),
            1
        );
        assert!(
            s.read_slice(&orphan.uri, None, None).is_err(),
            "an unclaimed artifact directory survived the sweep"
        );
        assert!(
            s.read_slice(&live.uri, None, None).is_ok(),
            "the sweep took a live session's artifacts"
        );
    }

    #[test]
    fn an_ephemeral_namespace_is_a_valid_path_component() {
        // Exactly the shape Agent generates when persistence is off.
        let s = store();
        let id = format!("tmp-{}-{}", std::process::id(), 1_786_264_515u64);
        let h = s.store(&id, "call_cde20ea6", "output", "x").unwrap();
        assert!(s.read_slice(&h.uri, None, None).is_ok());
    }

    #[test]
    fn removing_a_session_takes_its_artifacts_with_it() {
        let s = store();
        let h = s.store("doomed", "c1", "output", "x").unwrap();
        s.store("kept", "c1", "output", "y").unwrap();
        assert!(s.read_slice(&h.uri, None, None).is_ok());

        assert_eq!(s.remove_sessions(&["doomed".to_string()]), 1);
        assert!(
            s.read_slice(&h.uri, None, None).is_err(),
            "the pruned session's artifact is still readable"
        );
        assert!(
            s.read_slice("artifact://kept/c1/output", None, None)
                .is_ok(),
            "pruning one session took another's artifacts with it"
        );
    }

    /// V8, and the point of the whole preview design: failures buried in the
    /// middle of a huge log must survive into the preview. The second half of
    /// this asserts head+tail alone would have missed them, so the clustering
    /// cannot quietly regress to head/tail and still pass.
    #[test]
    fn error_clusters_survive_a_head_and_tail_preview() {
        let mut lines: Vec<String> = Vec::new();
        for i in 1..=40_000 {
            match i {
                12_043 => lines.push("test tests::parses_config ... FAILED".into()),
                20_112 => lines.push("error[E0308]: mismatched types".into()),
                31_007 => lines.push("thread 'main' panicked at src/lib.rs:42".into()),
                _ => lines.push(format!("     Compiling crate_{i} v0.1.0")),
            }
        }
        let content = lines.join("\n");
        let s = store();
        let h = s.store("sess", "call", "output", &content).unwrap();
        let p = preview(&content, &h);

        for needle in ["parses_config", "E0308", "panicked at"] {
            assert!(
                p.contains(needle),
                "preview lost the buried failure: {needle}"
            );
        }

        // The control: none of the three is reachable from the ends alone.
        let all: Vec<&str> = content.lines().collect();
        let ends = format!(
            "{}\n{}",
            all[..PREVIEW_HEAD_LINES].join("\n"),
            all[all.len() - PREVIEW_TAIL_LINES..].join("\n")
        );
        for needle in ["parses_config", "E0308", "panicked at"] {
            assert!(
                !ends.contains(needle),
                "test is not proving anything -- {needle} is in head+tail already"
            );
        }

        assert!(p.contains(&h.uri), "preview must carry the handle");
        assert!(
            p.len() < content.len() / 50,
            "preview is {} bytes against {} -- not a saving",
            p.len(),
            content.len()
        );
    }

    #[test]
    fn repeated_diagnostics_collapse_into_one_counted_entry() {
        let mut lines: Vec<String> = Vec::new();
        for i in 1..=1_000 {
            lines.push(format!("     Compiling crate_{i}"));
        }
        for i in 1..=300 {
            lines.push(format!("error: unresolved import at line {i}"));
        }
        for i in 1..=1_000 {
            lines.push(format!("     Finishing crate_{i}"));
        }
        let content = lines.join("\n");
        let s = store();
        let h = s.store("sess", "call", "output", &content).unwrap();
        let p = preview(&content, &h);

        assert!(p.contains("unresolved import"), "{p}");
        assert!(
            p.contains("×300"),
            "300 copies of one error should collapse to a count: {p}"
        );
        assert!(
            p.matches("unresolved import").count() <= 2,
            "the same error was listed repeatedly instead of clustered"
        );
    }

    /// Warnings must not crowd out the one error -- the reason `warning:` is
    /// deliberately absent from ERROR_MARKERS.
    #[test]
    fn a_flood_of_warnings_does_not_bury_the_single_error() {
        let mut lines: Vec<String> = Vec::new();
        for i in 1..=5_000 {
            lines.push(format!("warning: unused variable x{i}"));
        }
        lines.insert(2_500, "error[E0432]: the real problem".into());
        let content = lines.join("\n");
        let s = store();
        let h = s.store("sess", "call", "output", &content).unwrap();
        let p = preview(&content, &h);
        assert!(p.contains("E0432"), "the single real error was lost: {p}");
    }

    #[test]
    fn a_short_result_is_shown_whole_rather_than_pretending_to_elide() {
        let content = "just three\nshort\nlines";
        let s = store();
        let h = s.store("sess", "call", "output", content).unwrap();
        let p = preview(content, &h);
        assert!(p.contains("just three") && p.contains("lines"));
        assert!(
            !p.contains("elided"),
            "claimed to elide from a 3-line result"
        );
    }

    #[tokio::test]
    async fn the_tool_reads_a_slice_through_real_json_args() {
        let s = Arc::new(store());
        let content = (1..=50)
            .map(|i| format!("row {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let h = s.store("sess", "call", "output", &content).unwrap();
        let tool = ReadArtifact(s);
        let args = serde_json::json!({ "handle": h.uri, "offset": 5, "limit": 2 });
        let raw = RawValue::from_string(args.to_string()).unwrap();
        let out = tool.execute(&raw).await.unwrap();
        assert!(out.summary.starts_with("row 5\nrow 6"), "{}", out.summary);
    }

    #[tokio::test]
    async fn a_missing_artifact_explains_itself_instead_of_erroring_blankly() {
        let s = Arc::new(store());
        let tool = ReadArtifact(s);
        let args = serde_json::json!({ "handle": "artifact://gone/call/output" });
        let raw = RawValue::from_string(args.to_string()).unwrap();
        let err = tool.execute(&raw).await.unwrap_err();
        assert!(
            format!("{err}").contains("pruned"),
            "unhelpful message: {err}"
        );
    }
}
