//! Session persistence: the conversation survives the process.
//!
//! Context is a *purchased* asset here — every message in a long session was
//! paid for at the token rate, and a compaction summary literally cost a
//! model call to produce. Losing all of it because a terminal closed (or a
//! VS Code window reloaded) throws away real money, so state is written at
//! every turn boundary rather than only at a clean exit.
//!
//! Deliberately plain JSON files, not a database: the whole store is a
//! directory of records that a human can read, diff, back up, or delete with
//! `rm`. At the sizes compaction already bounds sessions to, indexing buys
//! nothing.
//!
//! # What is *not* persisted
//!
//! `/undo` checkpoints stay in memory only. They hold pre-edit file
//! snapshots, and restoring a file from a previous process — against a
//! working tree that may have been edited, committed, or branched since —
//! is a materially different and riskier promise than replaying a
//! conversation. Resuming restores what was *said*, never what was on disk.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use harness_types::Message;
use serde::{Deserialize, Serialize};

/// One saved conversation. Everything needed to pick up exactly where the
/// session left off, including cost accounting — resuming under a `--budget`
/// must not silently reset spend to zero and hand out a fresh allowance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    /// Canonical workspace path this session belongs to, so `--continue` in
    /// one project can never resume another project's conversation.
    pub workspace: String,
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub budget_usd: Option<f64>,
    #[serde(default)]
    pub session_cost_usd: f64,
    pub messages: Vec<Message>,
    /// Unix seconds. Stored as plain integers to keep this crate free of a
    /// date-time dependency; formatting for humans is the host's business.
    pub created_at: u64,
    pub updated_at: u64,
    /// First line of the first user message, for `hivemind sessions`.
    #[serde(default)]
    pub title: String,
}

impl SessionRecord {
    /// Number of real turns exchanged, ignoring the system prompt — what a
    /// human means by "how long was this session".
    pub fn turn_count(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| m.role == harness_types::Role::User)
            .count()
    }
}

/// Lightweight listing entry — avoids deserializing every message just to
/// print a menu.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub updated_at: u64,
    pub turns: usize,
    pub cost_usd: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session store {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("session {0} not found")]
    NotFound(String),
    #[error("session {id} is corrupt and cannot be read: {source}")]
    Corrupt {
        id: String,
        #[source]
        source: serde_json::Error,
    },
}

/// A directory of session records.
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A fresh id: unix-seconds prefix keeps the directory listing in
    /// chronological order, and a hash suffix keeps two sessions started in
    /// the same second from colliding.
    pub fn new_id(workspace: &str) -> String {
        let now = unix_now();
        let mut h = DefaultHasher::new();
        workspace.hash(&mut h);
        // Nanos, not seconds -- two `hivemind` processes launched in the same
        // second in the same workspace would otherwise hash identically.
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
            .hash(&mut h);
        format!("{now}-{:06x}", h.finish() & 0xff_ffff)
    }

    fn path_for(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Write atomically: serialize to a temp file in the same directory,
    /// then rename over the target. A crash (or a kill mid-save, which is
    /// exactly what happens when a user Ctrl-C's) can then leave either the
    /// old complete file or the new complete file — never a half-written one
    /// that fails to parse on resume.
    pub fn save(&self, record: &SessionRecord) -> Result<(), SessionError> {
        std::fs::create_dir_all(&self.dir).map_err(|source| SessionError::Io {
            path: self.dir.display().to_string(),
            source,
        })?;

        let final_path = self.path_for(&record.id);
        let tmp_path = self.dir.join(format!(".{}.tmp", record.id));
        let json = serde_json::to_vec_pretty(record).expect("SessionRecord is always serializable");

        std::fs::write(&tmp_path, &json).map_err(|source| SessionError::Io {
            path: tmp_path.display().to_string(),
            source,
        })?;

        // A conversation can contain anything the agent read -- source, keys,
        // customer data. Same posture as credentials.toml: owner-only on
        // Unix. (No NTFS equivalent applied on Windows, where per-user
        // profile ACLs already cover this.)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600));
        }

        std::fs::rename(&tmp_path, &final_path).map_err(|source| SessionError::Io {
            path: final_path.display().to_string(),
            source,
        })
    }

    pub fn load(&self, id: &str) -> Result<SessionRecord, SessionError> {
        let path = self.path_for(id);
        let bytes = std::fs::read(&path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                SessionError::NotFound(id.to_string())
            } else {
                SessionError::Io {
                    path: path.display().to_string(),
                    source,
                }
            }
        })?;
        serde_json::from_slice(&bytes).map_err(|source| SessionError::Corrupt {
            id: id.to_string(),
            source,
        })
    }

    /// Sessions for one workspace, newest first. Unreadable or corrupt files
    /// are skipped rather than failing the whole listing — one bad record
    /// must not make `--continue` unusable.
    pub fn list_for_workspace(&self, workspace: &str) -> Vec<SessionSummary> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out: Vec<SessionSummary> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| {
                let bytes = std::fs::read(e.path()).ok()?;
                let rec: SessionRecord = serde_json::from_slice(&bytes).ok()?;
                (rec.workspace == workspace).then(|| SessionSummary {
                    id: rec.id.clone(),
                    title: rec.title.clone(),
                    updated_at: rec.updated_at,
                    turns: rec.turn_count(),
                    cost_usd: rec.session_cost_usd,
                })
            })
            .collect();
        // Reverse key: newest first, which is the order a resume menu wants.
        out.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        out
    }

    /// The session `--continue` resumes: most recently updated in this
    /// workspace.
    pub fn latest_for_workspace(&self, workspace: &str) -> Option<SessionRecord> {
        let newest = self.list_for_workspace(workspace).into_iter().next()?;
        self.load(&newest.id).ok()
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A short human label for a session, taken from its first real user
/// message. Single-line and bounded so a listing stays a listing.
pub fn derive_title(messages: &[Message]) -> String {
    let first = messages
        .iter()
        .find(|m| m.role == harness_types::Role::User)
        .map(|m| m.content.as_str())
        .unwrap_or("");
    let line = first.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let trimmed = line.trim();
    if trimmed.chars().count() <= 60 {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(60).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> SessionStore {
        let dir = std::env::temp_dir().join(format!("hivemind_session_test_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        SessionStore::new(dir)
    }

    fn record(id: &str, workspace: &str) -> SessionRecord {
        SessionRecord {
            id: id.to_string(),
            workspace: workspace.to_string(),
            model: "hivemind".into(),
            reasoning_effort: None,
            budget_usd: None,
            session_cost_usd: 0.0,
            messages: vec![Message::system("sys"), Message::user("build a thing")],
            created_at: 100,
            updated_at: 100,
            title: "build a thing".into(),
        }
    }

    #[test]
    fn saves_and_loads_a_session_round_trip() {
        let s = store("round_trip");
        let mut rec = record("abc", "/ws/one");
        rec.session_cost_usd = 0.0421;
        rec.reasoning_effort = Some("high".into());
        s.save(&rec).unwrap();

        let loaded = s.load("abc").unwrap();
        assert_eq!(loaded.id, "abc");
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.reasoning_effort.as_deref(), Some("high"));
        // Cost must survive: resuming under a budget can't silently hand out
        // a fresh allowance.
        assert!((loaded.session_cost_usd - 0.0421).abs() < 1e-9);
    }

    #[test]
    fn listing_is_scoped_to_one_workspace() {
        let s = store("scoping");
        s.save(&record("a", "/ws/one")).unwrap();
        s.save(&record("b", "/ws/two")).unwrap();

        let one = s.list_for_workspace("/ws/one");
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].id, "a");
        // The whole point: --continue in one project must never surface
        // another project's conversation.
        assert!(s.list_for_workspace("/ws/three").is_empty());
    }

    #[test]
    fn latest_for_workspace_picks_the_most_recently_updated() {
        let s = store("latest");
        let mut old = record("old", "/ws");
        old.updated_at = 100;
        let mut new = record("new", "/ws");
        new.updated_at = 900;
        s.save(&old).unwrap();
        s.save(&new).unwrap();

        assert_eq!(s.latest_for_workspace("/ws").unwrap().id, "new");
    }

    #[test]
    fn a_corrupt_file_is_skipped_by_listing_instead_of_breaking_it() {
        let s = store("corrupt_listing");
        s.save(&record("good", "/ws")).unwrap();
        std::fs::write(s.dir().join("broken.json"), b"{ this is not json").unwrap();

        let listed = s.list_for_workspace("/ws");
        assert_eq!(
            listed.len(),
            1,
            "one bad record must not hide the good ones"
        );
        assert_eq!(listed[0].id, "good");
    }

    #[test]
    fn loading_a_corrupt_session_reports_it_rather_than_panicking() {
        let s = store("corrupt_load");
        std::fs::create_dir_all(s.dir()).unwrap();
        std::fs::write(s.dir().join("bad.json"), b"not json at all").unwrap();
        assert!(matches!(s.load("bad"), Err(SessionError::Corrupt { .. })));
    }

    #[test]
    fn loading_a_missing_session_is_not_found_not_an_io_error() {
        let s = store("missing");
        std::fs::create_dir_all(s.dir()).unwrap();
        assert!(matches!(s.load("nope"), Err(SessionError::NotFound(_))));
    }

    #[test]
    fn saving_twice_overwrites_cleanly_and_leaves_no_temp_files() {
        let s = store("atomic");
        let mut rec = record("x", "/ws");
        s.save(&rec).unwrap();
        rec.messages.push(Message::assistant("done"));
        rec.updated_at = 200;
        s.save(&rec).unwrap();

        assert_eq!(s.load("x").unwrap().messages.len(), 3);
        let leftovers: Vec<_> = std::fs::read_dir(s.dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "atomic save must not leave temp files"
        );
    }

    #[test]
    fn new_ids_are_unique_within_the_same_second_and_workspace() {
        let a = SessionStore::new_id("/ws");
        let b = SessionStore::new_id("/ws");
        assert_ne!(a, b, "two sessions started back-to-back must not collide");
    }

    #[test]
    fn title_is_the_first_user_line_bounded() {
        let msgs = vec![
            Message::system("you are an agent"),
            Message::user("build a calculator\nwith modular files"),
        ];
        assert_eq!(derive_title(&msgs), "build a calculator");

        let long = vec![Message::user("x".repeat(200))];
        let t = derive_title(&long);
        assert!(t.chars().count() <= 61, "got {} chars", t.chars().count());
        assert!(t.ends_with('…'));
    }

    #[test]
    fn title_of_a_conversation_with_no_user_message_is_empty_not_a_panic() {
        assert_eq!(derive_title(&[Message::system("sys")]), "");
        assert_eq!(derive_title(&[]), "");
    }

    #[cfg(unix)]
    #[test]
    fn saved_sessions_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let s = store("perms");
        s.save(&record("p", "/ws")).unwrap();
        let mode = std::fs::metadata(s.dir().join("p.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "a transcript can contain anything it read");
    }
}
