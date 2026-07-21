//! Shared workspace-walk policy: which files the read-only code tools look at,
//! and which directories they skip. Centralized so `search` and
//! `semantic_search` can't drift apart on what "the codebase" means. (The REPL
//! file-completer in `harness-cli` keeps its own copy — a different crate —
//! but deliberately uses the same names.)

use std::path::{Path, PathBuf};

use walkdir::{DirEntry, WalkDir};

/// Directories never worth walking — VCS internals, build output, vendored
/// deps, virtualenvs. Pruning them keeps a walk fast and its results
/// signal-dense, and avoids descending a multi-GB `target/` in a Rust
/// workspace.
pub const IGNORED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".DS_Store",
    "dist",
    "build",
    ".venv",
];

/// Skip files larger than this before reading them — almost always generated,
/// minified, or binary, and not what a code tool is for.
pub const MAX_FILE_BYTES: u64 = 2_000_000;

fn is_ignored(entry: &DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|n| IGNORED_DIRS.contains(&n))
        .unwrap_or(false)
}

/// Walk `root`, yielding regular files that pass the ignore/size policy above.
/// Walk errors (an unreadable subtree, a vanished file) are skipped rather
/// than fatal — a best-effort view of the workspace is what these tools want.
pub fn walk_files(root: &Path) -> impl Iterator<Item = PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_ignored(e))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.metadata()
                .map(|m| m.len() <= MAX_FILE_BYTES)
                .unwrap_or(false)
        })
        .map(walkdir::DirEntry::into_path)
}
