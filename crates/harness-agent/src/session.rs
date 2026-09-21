use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use harness_types::Message;
use serde::{Deserialize, Serialize};


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub workspace: String,
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub budget_usd: Option<f64>,
    #[serde(default)]
    pub session_cost_usd: f64,
    #[serde(default)]
    pub web_enabled: bool,
    #[serde(default)]
    pub active_skill: Option<String>,
    pub messages: Vec<Message>,
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

   
    pub fn delete(&self, id: &str) -> Result<bool, SessionError> {
        let path = self.path_for(id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(SessionError::Io {
                path: path.display().to_string(),
                source,
            }),
        }
    }


    pub fn list_all(&self) -> Vec<SessionSummary> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out: Vec<SessionSummary> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| {
                let bytes = std::fs::read(e.path()).ok()?;
                let rec: SessionRecord = serde_json::from_slice(&bytes).ok()?;
                Some(SessionSummary {
                    id: rec.id.clone(),
                    title: rec.title.clone(),
                    updated_at: rec.updated_at,
                    turns: rec.turn_count(),
                    cost_usd: rec.session_cost_usd,
                })
            })
            .collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        out
    }

   
    pub fn prune_older_than_except(&self, max_age_secs: u64, keep: &[String]) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let now = unix_now();
        let mut removed = Vec::new();
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(rec) = serde_json::from_slice::<SessionRecord>(&bytes) else {
                continue;
            };
            if keep.contains(&rec.id) {
                continue;
            }
            // `saturating_sub`: a record written by a machine with a skewed
            // clock can be dated in the future, which must read as age 0
            // (keep) rather than wrapping to a huge age (delete).
            if now.saturating_sub(rec.updated_at) > max_age_secs
                && std::fs::remove_file(&path).is_ok()
            {
                removed.push(rec.id);
            }
        }
        removed
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
        let dir = std::env::temp_dir().join(format!(
            "hivemind_session_test_{name}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
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
            web_enabled: false,
            active_skill: None,
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
        rec.web_enabled = true;
        s.save(&rec).unwrap();

        let loaded = s.load("abc").unwrap();
        assert_eq!(loaded.id, "abc");
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.reasoning_effort.as_deref(), Some("high"));
        assert!(loaded.web_enabled);
        // Cost must survive: resuming under a budget can't silently hand out
        // a fresh allowance.
        assert!((loaded.session_cost_usd - 0.0421).abs() < 1e-9);
    }

    #[test]
    fn a_selected_skill_survives_a_round_trip() {
        let s = store("skill_round_trip");
        let mut rec = record("skl", "/ws/one");
        rec.active_skill = Some("code-review".into());
        s.save(&rec).unwrap();

        assert_eq!(
            s.load("skl").unwrap().active_skill.as_deref(),
            Some("code-review")
        );
    }

    // A session written before skills existed must still load.
    #[test]
    fn a_record_without_the_skill_field_still_loads() {
        let s = store("skill_default");
        let rec = record("old", "/ws/one");
        s.save(&rec).unwrap();
        assert_eq!(s.load("old").unwrap().active_skill, None);
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

    #[test]
    fn a_session_written_before_the_tool_result_envelope_still_loads() {
        let s = store("pre_m1");
        std::fs::create_dir_all(s.dir()).unwrap();
        let pre_m1 = r#"{
          "id": "1785318157-719035",
          "workspace": "/ws/one",
          "model": "hivemind",
          "reasoning_effort": null,
          "budget_usd": null,
          "session_cost_usd": 3e-06,
          "created_at": 1785318157,
          "updated_at": 1785318200,
          "title": "list the files",
          "messages": [
            {"role": "system", "content": "you are an agent"},
            {"role": "user", "content": "list the files"},
            {"role": "assistant", "content": "", "tool_calls": [
              {"id": "c1", "name": "project_map", "args": {}}
            ]},
            {"role": "tool", "content": "(no files found)", "tool_call_id": "c1", "name": "project_map"},
            {"role": "assistant", "content": "The directory is empty."}
          ]
        }"#;
        std::fs::write(s.dir().join("1785318157-719035.json"), pre_m1).unwrap();

        let loaded = s
            .load("1785318157-719035")
            .expect("a pre-M1 session must still load");

        assert_eq!(loaded.messages.len(), 5);
        assert_eq!(loaded.turn_count(), 1);
        let tool_msg = loaded
            .messages
            .iter()
            .find(|m| m.role == harness_types::Role::Tool)
            .expect("the tool message survives the round trip");
        assert_eq!(tool_msg.content, "(no files found)");
        // Spend carries over, so resuming under a budget cannot hand out a
        // fresh allowance.
        assert!((loaded.session_cost_usd - 3e-06).abs() < 1e-12);
        assert!(
            !loaded.web_enabled,
            "pre-web sessions must restore with web off"
        );
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

    /// Age is measured from `updated_at`, so a fixture just has to date
    /// itself -- no sleeping, and no dependence on file mtimes (which is
    /// the whole point of keying on the record instead).
    fn aged(id: &str, workspace: &str, secs_ago: u64) -> SessionRecord {
        let mut r = record(id, workspace);
        r.updated_at = unix_now().saturating_sub(secs_ago);
        r
    }

    const DAY: u64 = 86_400;

    #[test]
    fn prune_deletes_only_what_is_past_the_cutoff() {
        let s = store("prune_cutoff");
        s.save(&aged("old", "/ws", 20 * DAY)).unwrap();
        s.save(&aged("fresh", "/ws", 2 * DAY)).unwrap();

        let removed = s.prune_older_than_except(14 * DAY, &[]);

        assert_eq!(removed, vec!["old".to_string()]);
        assert!(s.load("old").is_err());
        assert!(s.load("fresh").is_ok(), "a recent session must survive");
    }

    #[test]
    fn prune_spans_every_workspace_not_just_one() {
        // Retention is a property of the store; a stale conversation from a
        // project the user never opens again is exactly what it's for.
        let s = store("prune_all_ws");
        s.save(&aged("a", "/ws/one", 30 * DAY)).unwrap();
        s.save(&aged("b", "/ws/two", 30 * DAY)).unwrap();

        let mut removed = s.prune_older_than_except(14 * DAY, &[]);
        removed.sort();
        assert_eq!(removed, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn a_kept_id_survives_however_old_it_is() {
        // The host has this one open in a window right now.
        let s = store("prune_keep");
        s.save(&aged("open", "/ws", 90 * DAY)).unwrap();
        s.save(&aged("closed", "/ws", 90 * DAY)).unwrap();

        let removed = s.prune_older_than_except(14 * DAY, &["open".to_string()]);

        assert_eq!(removed, vec!["closed".to_string()]);
        assert!(
            s.load("open").is_ok(),
            "must not vanish under a live window"
        );
    }

    #[test]
    fn a_future_dated_record_is_kept_not_wrapped_into_deletion() {
        let s = store("prune_future");
        let mut r = record("ahead", "/ws");
        r.updated_at = unix_now() + 10 * DAY;
        s.save(&r).unwrap();

        assert!(s.prune_older_than_except(14 * DAY, &[]).is_empty());
        assert!(s.load("ahead").is_ok());
    }

    #[test]
    fn an_unreadable_record_is_left_alone_rather_than_deleted() {
        let s = store("prune_corrupt");
        std::fs::create_dir_all(s.dir()).unwrap();
        std::fs::write(s.dir().join("broken.json"), b"{not json").unwrap();

        assert!(s.prune_older_than_except(0, &[]).is_empty());
        assert!(s.dir().join("broken.json").exists());
    }

    #[test]
    fn deleting_the_same_session_twice_is_not_an_error() {
        let s = store("delete_twice");
        s.save(&record("gone", "/ws")).unwrap();
        assert!(s.delete("gone").unwrap(), "first delete removed it");
        assert!(
            !s.delete("gone").unwrap(),
            "second is a no-op, not a failure"
        );
    }

    #[test]
    fn list_all_ignores_workspace_and_orders_newest_first() {
        let s = store("list_all");
        s.save(&aged("older", "/ws/one", 5 * DAY)).unwrap();
        s.save(&aged("newer", "/ws/two", DAY)).unwrap();

        let ids: Vec<_> = s.list_all().into_iter().map(|x| x.id).collect();
        assert_eq!(ids, vec!["newer".to_string(), "older".to_string()]);
    }
}
