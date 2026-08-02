//! Tracks what the agent has actually *read*, so an edit built on a stale
//! view of a file can be caught instead of silently applied.
//!
//! # Why this isn't a model-supplied hash
//!
//! The obvious design — `edit_file(path, expected_hash, ...)` — cannot
//! work: tool arguments are emitted by the model, and a model has no way to
//! compute a hash of a file it read. Asked for one it would omit the field
//! or invent it, and a guard that the guarded party fills in is not a
//! guard. So the hash is recorded by the harness at `read_file` time and
//! checked by the harness at `edit_file` time; the model never sees it and
//! cannot get it wrong.
//!
//! # What this actually catches
//!
//! `edit_file` already requires `old_string` to match the file exactly, so
//! most staleness is caught for free: if the region being edited changed,
//! the match simply fails. The gap this closes is narrower and sneakier —
//! the file changed *somewhere else* while `old_string` still matches:
//!
//! - a formatter (often one of our own `PostToolUse` hooks) reflowed the file
//! - the user edited it in their editor mid-turn
//! - a second agent session, or a `run_shell` command, rewrote it
//! - a build step regenerated it
//!
//! In each case the edit still applies cleanly, but it applies to a file
//! the model no longer understands, and the resulting damage is silent.
//!
//! # What it deliberately does *not* do
//!
//! An unread file is **allowed**, not blocked. Editing a file located via
//! `search` or `project_map` without a separate `read_file` is a legitimate
//! and common flow, and turning this into a "you must read first" rule
//! would break it for no safety gain. This is a staleness detector, not a
//! read-before-write mandate.
//!
//! It is also not a security control. The fingerprint is a fast
//! non-cryptographic hash chosen to detect accidental change; someone who
//! wants to construct a collision can. Nothing here is defending against
//! that threat.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::embed_cache::{Key, key_of};

/// Content fingerprints of files the agent has seen this session, shared by
/// every tool holding a clone of the same [`crate::Workspace`].
///
/// Cloning shares the underlying map rather than copying it — every file
/// tool is constructed from a clone of one `Workspace`, so sharing is the
/// entire point.
#[derive(Clone, Default)]
pub struct ReadSet(Arc<Mutex<HashMap<PathBuf, Key>>>);

/// Cap on tracked files, so a session that reads thousands of files can't
/// grow this without bound. Far above any realistic working set; when it is
/// hit the map is simply cleared, which costs nothing worse than a few
/// missed staleness checks (the failure mode is "allowed through", exactly
/// as if the files had never been read).
const MAX_TRACKED: usize = 4_096;

impl ReadSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember what `path` looked like at the moment the agent saw it.
    ///
    /// Called after a successful `read_file`, and also after a successful
    /// `write_file`/`edit_file` — a tool that just wrote the file knows its
    /// contents exactly, and re-recording is what makes consecutive edits
    /// to one file work. Without it the second edit would compare against
    /// the pre-first-edit fingerprint and report the agent's own change as
    /// interference.
    pub fn record(&self, path: &Path, content: &str) {
        let mut map = self.0.lock().expect("read-set mutex poisoned");
        if map.len() >= MAX_TRACKED && !map.contains_key(path) {
            map.clear();
        }
        map.insert(path.to_path_buf(), key_of(content));
    }

    /// `true` when `path` was read earlier and `current` is not what was
    /// read. `false` for an untracked file (see the module docs on why an
    /// unread file is allowed) and for one that is unchanged.
    pub fn is_stale(&self, path: &Path, current: &str) -> bool {
        let map = self.0.lock().expect("read-set mutex poisoned");
        match map.get(path) {
            Some(seen) => *seen != key_of(current),
            None => false,
        }
    }

    /// Drop `path`'s fingerprint. Used when a file is written by a path
    /// that can't vouch for the final bytes on disk.
    pub fn forget(&self, path: &Path) {
        self.0.lock().expect("read-set mutex poisoned").remove(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn a_file_that_was_never_read_is_not_stale() {
        let rs = ReadSet::new();
        assert!(
            !rs.is_stale(&p("/a/b.rs"), "anything"),
            "editing a file found via search must not be blocked"
        );
    }

    #[test]
    fn an_unchanged_file_is_not_stale() {
        let rs = ReadSet::new();
        rs.record(&p("/a/b.rs"), "fn main() {}");
        assert!(!rs.is_stale(&p("/a/b.rs"), "fn main() {}"));
    }

    #[test]
    fn a_file_changed_since_it_was_read_is_stale() {
        let rs = ReadSet::new();
        rs.record(&p("/a/b.rs"), "fn main() {}");
        assert!(rs.is_stale(&p("/a/b.rs"), "fn main() { changed(); }"));
    }

    #[test]
    fn re_recording_after_a_write_clears_staleness() {
        // This is what makes two consecutive edits to one file work: the
        // first edit's own change must not read as someone else's.
        let rs = ReadSet::new();
        rs.record(&p("/a/b.rs"), "one");
        rs.record(&p("/a/b.rs"), "two");
        assert!(!rs.is_stale(&p("/a/b.rs"), "two"));
    }

    #[test]
    fn tracking_is_per_path_not_global() {
        let rs = ReadSet::new();
        rs.record(&p("/a/b.rs"), "b");
        rs.record(&p("/a/c.rs"), "c");
        assert!(!rs.is_stale(&p("/a/b.rs"), "b"));
        assert!(rs.is_stale(&p("/a/c.rs"), "changed"));
    }

    #[test]
    fn clones_share_one_map() {
        // Every file tool holds a different clone of the same Workspace;
        // if clones didn't share, the read side and the edit side would
        // each keep their own useless copy.
        let rs = ReadSet::new();
        let clone = rs.clone();
        rs.record(&p("/a/b.rs"), "original");
        assert!(clone.is_stale(&p("/a/b.rs"), "modified"));
    }

    #[test]
    fn a_whitespace_only_change_is_still_a_change() {
        // The formatter case, which is the single most likely real cause.
        let rs = ReadSet::new();
        rs.record(&p("/a/b.rs"), "fn main(){}");
        assert!(rs.is_stale(&p("/a/b.rs"), "fn main() {}"));
    }

    #[test]
    fn forget_makes_a_tracked_file_untracked_again() {
        let rs = ReadSet::new();
        rs.record(&p("/a/b.rs"), "one");
        rs.forget(&p("/a/b.rs"));
        assert!(!rs.is_stale(&p("/a/b.rs"), "two"));
    }

    #[test]
    fn the_tracking_cap_bounds_memory_without_breaking_correctness() {
        let rs = ReadSet::new();
        for i in 0..MAX_TRACKED + 50 {
            rs.record(&p(&format!("/a/{i}.rs")), "contents");
        }
        let len = rs.0.lock().unwrap().len();
        assert!(len <= MAX_TRACKED, "map grew past the cap: {len}");
        // Eviction has to fail *open* -- an evicted file reads as untracked,
        // which allows the edit, rather than as changed, which would block
        // a perfectly good one.
        let evicted = p("/a/0.rs");
        assert!(!rs.is_stale(&evicted, "something else entirely"));
    }
}
