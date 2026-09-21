use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ReviewTarget, content_hash};

#[derive(Debug, Error)]
pub enum ReviewError {
    #[error("could not {operation}: {source}")]
    Io {
        operation: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path:?} is not a Git repository root")]
    NotRepositoryRoot { path: PathBuf },
    #[error("Git command failed ({command}, exit {status:?}): {stderr}")]
    GitCommand {
        command: String,
        status: Option<i32>,
        stderr: String,
    },
    #[error("invalid Git revision {0:?}")]
    InvalidRevision(String),
    #[error("pull-request targets require the later SCM integration")]
    UnsupportedPullRequest,
    #[error("review contains {actual} changed files, exceeding the limit of {limit}")]
    TooManyFiles { actual: usize, limit: usize },
    #[error("review diff is {actual} bytes, exceeding the limit of {limit} bytes")]
    DiffTooLarge { actual: usize, limit: usize },
    #[error(
        "review file {path:?} is {actual} bytes, exceeding the per-file limit of {limit} bytes"
    )]
    FileTooLarge {
        path: String,
        actual: usize,
        limit: usize,
    },
    #[error("Git output exceeded the bounded allocation of {limit} bytes while running {command}")]
    GitOutputTooLarge { command: String, limit: usize },
    #[error("unsafe repository path returned by Git: {0:?}")]
    UnsafePath(String),
    #[error("Git returned a non-UTF-8 repository path; this target cannot be represented safely")]
    NonUtf8Path,
    #[error("refusing to follow symbolic link {0:?} while collecting a worktree review")]
    SymlinkPath(String),
    #[error("the review target changed while it was being collected; rerun the review")]
    TargetChanged,
    #[error("unmerged path {0:?} cannot be reviewed until its index conflict is resolved")]
    UnmergedPath(String),
    #[error("could not parse Git diff: {0}")]
    Parse(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffLimits {
    pub max_files: usize,
    pub max_bytes: usize,
    /// Maximum bytes loaded from any one source blob. The diff limit alone
    /// is insufficient: a one-line edit in a multi-gigabyte file still has
    /// a tiny patch but must not make context acquisition unbounded.
    pub max_file_bytes: usize,
}

impl Default for DiffLimits {
    fn default() -> Self {
        Self {
            max_files: 100,
            max_bytes: 512 * 1024,
            max_file_bytes: 2 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Unmerged,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLineKind {
    Context,
    Addition,
    Removal,
    NoNewlineMarker,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_line: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    #[serde(default)]
    pub section: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffStats {
    pub files: usize,
    pub hunks: usize,
    pub additions: usize,
    pub deletions: usize,
    pub binary_files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFile {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub change: ChangeKind,
    #[serde(default)]
    pub untracked: bool,
    pub binary: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_mode: Option<String>,
    pub patch: String,
    pub hunks: Vec<DiffHunk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
}

impl ReviewFile {
    /// New-file line numbers directly changed by this diff. Context lines
    /// are excluded: a finding may cite them as evidence, but its primary
    /// location should remain anchored to the reviewed change.
    pub fn changed_new_lines(&self) -> BTreeSet<u32> {
        self.hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .filter_map(|line| {
                (line.kind == DiffLineKind::Addition)
                    .then_some(line.new_line)
                    .flatten()
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedDiff {
    pub target: ReviewTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_revision: Option<String>,
    pub fingerprint: String,
    pub files: Vec<ReviewFile>,
    pub stats: DiffStats,
}

/// Read-only access to a Git repository whose root is exactly the approved
/// workspace. Requiring equality (rather than silently walking to a parent
/// repository) preserves the workspace jail when `--workdir` points at a
/// nested directory.
#[derive(Debug, Clone)]
pub struct GitRepository {
    root: PathBuf,
}

const GIT_METADATA_LIMIT: usize = 2 * 1024 * 1024;
const GIT_STDERR_LIMIT: usize = 64 * 1024;

/// Internal immutable form of a requested target. User-facing ref names are
/// resolved exactly once; every later Git command uses only full object IDs.
#[derive(Debug, Clone)]
enum ResolvedTarget {
    WorkingTree { base: String },
    Staged { base: String },
    Commit { parent: Option<String>, oid: String },
    Range { base: String, head: String },
}

impl ResolvedTarget {
    fn base_revision(&self) -> Option<String> {
        match self {
            Self::WorkingTree { base } | Self::Staged { base } | Self::Range { base, .. } => {
                Some(base.clone())
            }
            Self::Commit { parent, .. } => parent.clone(),
        }
    }

    fn head_revision(&self) -> Option<String> {
        match self {
            Self::WorkingTree { .. } | Self::Staged { .. } => None,
            Self::Commit { oid, .. } => Some(oid.clone()),
            Self::Range { head, .. } => Some(head.clone()),
        }
    }

    fn mutable(&self) -> bool {
        matches!(self, Self::WorkingTree { .. } | Self::Staged { .. })
    }
}

struct GitOutput {
    stdout: Vec<u8>,
}

struct CappedRead {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl GitRepository {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ReviewError> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|source| ReviewError::Io {
                operation: format!("canonicalize repository root {:?}", root.as_ref()),
                source,
            })?;
        let candidate = Self { root: root.clone() };
        let output = candidate.git(&["rev-parse", "--show-toplevel"], GIT_METADATA_LIMIT)?;
        let reported = std::str::from_utf8(&output.stdout)
            .map_err(|_| ReviewError::NonUtf8Path)?
            .trim()
            .to_string();
        let reported =
            PathBuf::from(reported)
                .canonicalize()
                .map_err(|source| ReviewError::Io {
                    operation: "canonicalize Git repository root".into(),
                    source,
                })?;
        if reported != root {
            return Err(ReviewError::NotRepositoryRoot { path: root });
        }
        Ok(candidate)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve two revision names once and return their immutable merge-base
    /// commit. CLI `--base` uses this before acquiring a range so review
    /// semantics match a pull request's changed side rather than every
    /// commit that happens to exist on the named branch.
    pub fn merge_base(&self, base: &str, head: &str) -> Result<String, ReviewError> {
        let base = self.resolve_revision(base)?;
        let head = self.resolve_revision(head)?;
        let output = self.git(&["merge-base", &base, &head], GIT_METADATA_LIMIT)?;
        let oid = std::str::from_utf8(&output.stdout)
            .map_err(|_| ReviewError::Parse("Git returned a non-UTF-8 merge base".into()))?
            .trim();
        if oid.len() < 40 || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ReviewError::Parse(format!(
                "Git returned an invalid merge-base object ID {oid:?}"
            )));
        }
        Ok(oid.to_string())
    }

    pub fn acquire(
        &self,
        target: ReviewTarget,
        limits: DiffLimits,
    ) -> Result<NormalizedDiff, ReviewError> {
        let resolved = self.resolve_target(&target)?;
        let entries = self.changed_entries(&resolved)?;
        if let Some(entry) = entries
            .iter()
            .find(|entry| entry.change == ChangeKind::Unmerged)
        {
            return Err(ReviewError::UnmergedPath(entry.path.clone()));
        }
        if entries.len() > limits.max_files {
            return Err(ReviewError::TooManyFiles {
                actual: entries.len(),
                limit: limits.max_files,
            });
        }
        let mut files = Vec::with_capacity(entries.len());

        for entry in entries {
            let remaining = limits.max_bytes.saturating_sub(total_patch_bytes(&files));
            let patch = self.patch_for(&resolved, &entry, remaining)?;
            let current_bytes =
                self.resolved_file_bytes(&resolved, &entry.path, limits.max_file_bytes)?;
            let binary = patch.contains("GIT binary patch")
                || patch.contains("Binary files ")
                || current_bytes
                    .as_ref()
                    .is_some_and(|bytes| bytes.contains(&0) || std::str::from_utf8(bytes).is_err());
            let hunks = if binary {
                Vec::new()
            } else {
                parse_hunks(&patch)?
            };
            let (old_mode, new_mode) = patch_modes(&patch);
            files.push(ReviewFile {
                path: entry.path,
                old_path: entry.old_path,
                change: entry.change,
                untracked: false,
                binary,
                old_mode,
                new_mode,
                patch,
                hunks,
                content_hash: current_bytes.as_ref().map(content_hash),
            });
            enforce_diff_limit(&files, limits.max_bytes)?;
        }

        if matches!(resolved, ResolvedTarget::WorkingTree { .. }) {
            self.append_untracked(&mut files, limits)?;
        }

        self.ensure_stable(&resolved, &files, limits)?;

        files.sort_by(|left, right| left.path.cmp(&right.path));
        let stats = calculate_stats(&files);
        let fingerprint = diff_fingerprint(&target, &files);
        Ok(NormalizedDiff {
            target,
            base_revision: resolved.base_revision(),
            head_revision: resolved.head_revision(),
            fingerprint,
            files,
            stats,
        })
    }

    /// Load bounded content from the exact snapshot represented by an
    /// acquired diff. Commit/range reads use the already-resolved object ID;
    /// mutable worktree/index reads are checked by the caller against the
    /// file hash recorded in [`ReviewFile`].
    pub fn read_file(
        &self,
        diff: &NormalizedDiff,
        path: &str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, ReviewError> {
        let resolved = self.resolved_from_diff(diff)?;
        self.resolved_file_bytes(&resolved, path, max_bytes)
    }

    fn append_untracked(
        &self,
        files: &mut Vec<ReviewFile>,
        limits: DiffLimits,
    ) -> Result<(), ReviewError> {
        let output = self.git(
            &["ls-files", "--others", "--exclude-standard", "-z"],
            GIT_METADATA_LIMIT,
        )?;
        let already: BTreeSet<&str> = files.iter().map(|file| file.path.as_str()).collect();
        let untracked: Vec<String> = split_nul(&output.stdout)?
            .into_iter()
            .filter(|path| !path.is_empty() && !already.contains(path.as_str()))
            .collect();
        if files.len() + untracked.len() > limits.max_files {
            return Err(ReviewError::TooManyFiles {
                actual: files.len() + untracked.len(),
                limit: limits.max_files,
            });
        }
        for path in untracked {
            validate_repo_path(&path)?;
            let absolute = self.root.join(&path);
            let metadata =
                std::fs::symlink_metadata(&absolute).map_err(|source| ReviewError::Io {
                    operation: format!("inspect untracked file {absolute:?}"),
                    source,
                })?;
            if metadata.file_type().is_symlink() {
                return Err(ReviewError::SymlinkPath(path));
            }
            if metadata.len() > limits.max_file_bytes as u64 {
                return Err(ReviewError::FileTooLarge {
                    path,
                    actual: metadata.len() as usize,
                    limit: limits.max_file_bytes,
                });
            }
            let projected = total_patch_bytes(files).saturating_add(metadata.len() as usize);
            if projected > limits.max_bytes {
                return Err(ReviewError::DiffTooLarge {
                    actual: projected,
                    limit: limits.max_bytes,
                });
            }
            let bytes = std::fs::read(&absolute).map_err(|source| ReviewError::Io {
                operation: format!("read untracked file {absolute:?}"),
                source,
            })?;
            let binary = bytes.contains(&0);
            let patch = if binary {
                format!(
                    "diff --git a/{path} b/{path}\nnew file mode 100644\nBinary files /dev/null and b/{path} differ\n"
                )
            } else {
                synthesize_added_patch(&path, &String::from_utf8_lossy(&bytes))
            };
            let hunks = if binary {
                Vec::new()
            } else {
                parse_hunks(&patch)?
            };
            files.push(ReviewFile {
                path,
                old_path: None,
                change: ChangeKind::Added,
                untracked: true,
                binary,
                old_mode: None,
                new_mode: Some("100644".into()),
                patch,
                hunks,
                content_hash: Some(content_hash(&bytes)),
            });
            enforce_diff_limit(files, limits.max_bytes)?;
        }
        Ok(())
    }

    fn changed_entries(&self, target: &ResolvedTarget) -> Result<Vec<ChangedEntry>, ReviewError> {
        let mut args = match target {
            ResolvedTarget::WorkingTree { base } => vec![
                "diff".to_string(),
                "--no-ext-diff".into(),
                "--no-textconv".into(),
                "--name-status".into(),
                "-z".into(),
                "--find-renames".into(),
                base.clone(),
            ],
            ResolvedTarget::Staged { base } => vec![
                "diff".into(),
                "--cached".into(),
                "--no-ext-diff".into(),
                "--no-textconv".into(),
                "--name-status".into(),
                "-z".into(),
                "--find-renames".into(),
                base.clone(),
            ],
            ResolvedTarget::Commit {
                parent: Some(parent),
                oid,
            }
            | ResolvedTarget::Range {
                base: parent,
                head: oid,
            } => vec![
                "diff".into(),
                "--no-ext-diff".into(),
                "--no-textconv".into(),
                "--name-status".into(),
                "-z".into(),
                "--find-renames".into(),
                parent.clone(),
                oid.clone(),
            ],
            ResolvedTarget::Commit { parent: None, oid } => vec![
                "diff-tree".into(),
                "--root".into(),
                "--no-commit-id".into(),
                "--no-ext-diff".into(),
                "--no-textconv".into(),
                "--name-status".into(),
                "-r".into(),
                "-z".into(),
                "--find-renames".into(),
                oid.clone(),
            ],
        };
        args.push("--".into());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        parse_name_status(&self.git(&refs, GIT_METADATA_LIMIT)?.stdout)
    }

    fn patch_for(
        &self,
        target: &ResolvedTarget,
        entry: &ChangedEntry,
        max_bytes: usize,
    ) -> Result<String, ReviewError> {
        let mut args = match target {
            ResolvedTarget::WorkingTree { base } => vec![
                "diff".to_string(),
                "--no-color".into(),
                "--no-ext-diff".into(),
                "--no-textconv".into(),
                "--find-renames".into(),
                "--unified=3".into(),
                base.clone(),
            ],
            ResolvedTarget::Staged { base } => vec![
                "diff".into(),
                "--cached".into(),
                "--no-color".into(),
                "--no-ext-diff".into(),
                "--no-textconv".into(),
                "--find-renames".into(),
                "--unified=3".into(),
                base.clone(),
            ],
            ResolvedTarget::Commit {
                parent: Some(parent),
                oid,
            }
            | ResolvedTarget::Range {
                base: parent,
                head: oid,
            } => vec![
                "diff".into(),
                "--no-color".into(),
                "--no-ext-diff".into(),
                "--no-textconv".into(),
                "--find-renames".into(),
                "--unified=3".into(),
                parent.clone(),
                oid.clone(),
            ],
            ResolvedTarget::Commit { parent: None, oid } => vec![
                "show".into(),
                "--format=".into(),
                "--no-color".into(),
                "--no-ext-diff".into(),
                "--no-textconv".into(),
                "--find-renames".into(),
                "--unified=3".into(),
                oid.clone(),
            ],
        };
        args.push("--".into());
        if let Some(old) = &entry.old_path {
            args.push(old.clone());
        }
        args.push(entry.path.clone());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.git(&refs, max_bytes)?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn resolve_target(&self, target: &ReviewTarget) -> Result<ResolvedTarget, ReviewError> {
        match target {
            ReviewTarget::WorkingTree => Ok(ResolvedTarget::WorkingTree {
                base: self.resolve_revision("HEAD")?,
            }),
            ReviewTarget::Staged => Ok(ResolvedTarget::Staged {
                base: self.resolve_revision("HEAD")?,
            }),
            ReviewTarget::Commit { sha } => {
                let oid = self.resolve_revision(sha)?;
                let output = self.git(
                    &["rev-list", "--parents", "-n", "1", &oid],
                    GIT_METADATA_LIMIT,
                )?;
                let text = std::str::from_utf8(&output.stdout).map_err(|_| {
                    ReviewError::Parse("Git returned a non-UTF-8 revision record".into())
                })?;
                // Deliberately first-parent for merge commits. A combined
                // diff has different hunk semantics and is unsuitable for
                // one-location findings.
                let parent = text.split_whitespace().nth(1).map(str::to_string);
                Ok(ResolvedTarget::Commit { parent, oid })
            }
            ReviewTarget::Range { base, head } => Ok(ResolvedTarget::Range {
                base: self.resolve_revision(base)?,
                head: self.resolve_revision(head)?,
            }),
            ReviewTarget::PullRequest { .. } => Err(ReviewError::UnsupportedPullRequest),
        }
    }

    fn resolved_from_diff(&self, diff: &NormalizedDiff) -> Result<ResolvedTarget, ReviewError> {
        let missing =
            || ReviewError::Parse("review report is missing its resolved revision".into());
        match &diff.target {
            ReviewTarget::WorkingTree => Ok(ResolvedTarget::WorkingTree {
                base: diff.base_revision.clone().ok_or_else(missing)?,
            }),
            ReviewTarget::Staged => Ok(ResolvedTarget::Staged {
                base: diff.base_revision.clone().ok_or_else(missing)?,
            }),
            ReviewTarget::Commit { .. } => Ok(ResolvedTarget::Commit {
                parent: diff.base_revision.clone(),
                oid: diff.head_revision.clone().ok_or_else(missing)?,
            }),
            ReviewTarget::Range { .. } => Ok(ResolvedTarget::Range {
                base: diff.base_revision.clone().ok_or_else(missing)?,
                head: diff.head_revision.clone().ok_or_else(missing)?,
            }),
            ReviewTarget::PullRequest { .. } => Err(ReviewError::UnsupportedPullRequest),
        }
    }

    fn validate_revision_input(revision: &str) -> Result<(), ReviewError> {
        if revision.is_empty()
            || revision.starts_with('-')
            || revision.chars().any(char::is_control)
            || revision.chars().any(char::is_whitespace)
        {
            return Err(ReviewError::InvalidRevision(revision.to_string()));
        }
        Ok(())
    }

    fn resolve_revision(&self, revision: &str) -> Result<String, ReviewError> {
        Self::validate_revision_input(revision)?;
        let object = format!("{revision}^{{commit}}");
        let result = self.git(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &object,
            ],
            GIT_METADATA_LIMIT,
        );
        let output = match result {
            Ok(output) => output,
            Err(ReviewError::GitCommand { .. }) => {
                return Err(ReviewError::InvalidRevision(revision.to_string()));
            }
            Err(error) => return Err(error),
        };
        let oid = std::str::from_utf8(&output.stdout)
            .map_err(|_| ReviewError::Parse("Git returned a non-UTF-8 object ID".into()))?
            .trim();
        if oid.len() < 40 || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ReviewError::Parse(format!(
                "Git returned an invalid object ID {oid:?}"
            )));
        }
        Ok(oid.to_string())
    }

    fn resolved_file_bytes(
        &self,
        target: &ResolvedTarget,
        path: &str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, ReviewError> {
        validate_repo_path(path)?;
        match target {
            ResolvedTarget::WorkingTree { .. } => self.worktree_file_bytes(path, max_bytes),
            ResolvedTarget::Staged { .. } => self.git_object(&format!(":{path}"), path, max_bytes),
            ResolvedTarget::Commit { oid, .. } | ResolvedTarget::Range { head: oid, .. } => {
                self.git_object(&format!("{oid}:{path}"), path, max_bytes)
            }
        }
    }

    fn worktree_file_bytes(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, ReviewError> {
        let absolute = self.root.join(path);
        let metadata = match std::fs::symlink_metadata(&absolute) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(ReviewError::Io {
                    operation: format!("inspect {absolute:?}"),
                    source,
                });
            }
        };
        if metadata.file_type().is_symlink() {
            return Err(ReviewError::SymlinkPath(path.to_string()));
        }
        if metadata.len() > max_bytes as u64 {
            return Err(ReviewError::FileTooLarge {
                path: path.to_string(),
                actual: metadata.len() as usize,
                limit: max_bytes,
            });
        }
        let bytes = std::fs::read(&absolute).map_err(|source| ReviewError::Io {
            operation: format!("read {absolute:?}"),
            source,
        })?;
        if bytes.len() > max_bytes {
            return Err(ReviewError::FileTooLarge {
                path: path.to_string(),
                actual: bytes.len(),
                limit: max_bytes,
            });
        }
        Ok(Some(bytes))
    }

    fn git_object(
        &self,
        object: &str,
        path: &str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, ReviewError> {
        match self.git(&["cat-file", "-e", object], GIT_METADATA_LIMIT) {
            Ok(_) => {}
            Err(ReviewError::GitCommand { .. }) => return Ok(None),
            Err(error) => return Err(error),
        }
        match self.git(&["cat-file", "blob", object], max_bytes) {
            Ok(output) => Ok(Some(output.stdout)),
            Err(ReviewError::GitOutputTooLarge { .. }) => Err(ReviewError::FileTooLarge {
                path: path.to_string(),
                actual: max_bytes.saturating_add(1),
                limit: max_bytes,
            }),
            Err(error) => Err(error),
        }
    }

    fn ensure_stable(
        &self,
        target: &ResolvedTarget,
        files: &[ReviewFile],
        limits: DiffLimits,
    ) -> Result<(), ReviewError> {
        if !target.mutable() {
            return Ok(());
        }

        let entries = self.changed_entries(target)?;
        let expected: Vec<ChangedEntry> = files
            .iter()
            .filter(|file| !file.untracked)
            .map(|file| ChangedEntry {
                path: file.path.clone(),
                old_path: file.old_path.clone(),
                change: file.change,
            })
            .collect();
        if entries != expected {
            return Err(ReviewError::TargetChanged);
        }

        if matches!(target, ResolvedTarget::WorkingTree { .. }) {
            let expected_untracked: BTreeSet<&str> = files
                .iter()
                .filter(|file| file.untracked)
                .map(|file| file.path.as_str())
                .collect();
            let actual_untracked = self.list_untracked()?;
            if expected_untracked
                != actual_untracked
                    .iter()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>()
            {
                return Err(ReviewError::TargetChanged);
            }
        }

        for file in files {
            if !file.untracked {
                let entry = ChangedEntry {
                    path: file.path.clone(),
                    old_path: file.old_path.clone(),
                    change: file.change,
                };
                if self.patch_for(target, &entry, limits.max_bytes)? != file.patch {
                    return Err(ReviewError::TargetChanged);
                }
            }
            let hash = self
                .resolved_file_bytes(target, &file.path, limits.max_file_bytes)?
                .as_ref()
                .map(content_hash);
            if hash != file.content_hash {
                return Err(ReviewError::TargetChanged);
            }
        }
        Ok(())
    }

    fn list_untracked(&self) -> Result<BTreeSet<String>, ReviewError> {
        let output = self.git(
            &["ls-files", "--others", "--exclude-standard", "-z"],
            GIT_METADATA_LIMIT,
        )?;
        split_nul(&output.stdout).map(|paths| paths.into_iter().collect())
    }

    fn git(&self, args: &[&str], max_stdout: usize) -> Result<GitOutput, ReviewError> {
        self.git_raw(args, max_stdout)
    }

    fn git_raw(&self, args: &[&str], max_stdout: usize) -> Result<GitOutput, ReviewError> {
        let mut child = Command::new("git")
            .arg("--literal-pathspecs")
            .args(args)
            .current_dir(&self.root)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| ReviewError::Io {
                operation: "start Git".into(),
                source,
            })?;
        let stdout = child.stdout.take().expect("piped Git stdout");
        let stderr = child.stderr.take().expect("piped Git stderr");
        let stdout_reader = std::thread::spawn(move || read_capped(stdout, max_stdout));
        let stderr_reader = std::thread::spawn(move || read_capped(stderr, GIT_STDERR_LIMIT));
        let status = child.wait().map_err(|source| ReviewError::Io {
            operation: "wait for Git".into(),
            source,
        })?;
        let stdout = stdout_reader
            .join()
            .map_err(|_| ReviewError::Parse("Git stdout reader panicked".into()))?
            .map_err(|source| ReviewError::Io {
                operation: "read Git stdout".into(),
                source,
            })?;
        let stderr = stderr_reader
            .join()
            .map_err(|_| ReviewError::Parse("Git stderr reader panicked".into()))?
            .map_err(|source| ReviewError::Io {
                operation: "read Git stderr".into(),
                source,
            })?;
        let command = format!("git {}", args.join(" "));
        if !status.success() {
            return Err(ReviewError::GitCommand {
                command,
                status: status.code(),
                stderr: String::from_utf8_lossy(&stderr.bytes).trim().to_string(),
            });
        }
        if stdout.exceeded {
            return Err(ReviewError::GitOutputTooLarge {
                command,
                limit: max_stdout,
            });
        }
        Ok(GitOutput {
            stdout: stdout.bytes,
        })
    }
}

fn read_capped(mut reader: impl Read, limit: usize) -> std::io::Result<CappedRead> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut exceeded = false;
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let keep = remaining.min(read);
        bytes.extend_from_slice(&chunk[..keep]);
        exceeded |= keep < read;
        // Continue draining even after the cap. Stopping here can fill the
        // pipe and deadlock while the parent waits for Git to exit.
    }
    Ok(CappedRead { bytes, exceeded })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChangedEntry {
    path: String,
    old_path: Option<String>,
    change: ChangeKind,
}

fn split_nul(bytes: &[u8]) -> Result<Vec<String>, ReviewError> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(|field| {
            std::str::from_utf8(field)
                .map(str::to_string)
                .map_err(|_| ReviewError::NonUtf8Path)
        })
        .collect()
}

fn parse_name_status(bytes: &[u8]) -> Result<Vec<ChangedEntry>, ReviewError> {
    let fields = split_nul(bytes)?;
    let mut index = 0;
    let mut entries = Vec::new();
    while index < fields.len() {
        let status = &fields[index];
        index += 1;
        let code = status
            .chars()
            .next()
            .ok_or_else(|| ReviewError::Parse("empty status in --name-status output".into()))?;
        let change = match code {
            'A' => ChangeKind::Added,
            'M' => ChangeKind::Modified,
            'D' => ChangeKind::Deleted,
            'R' => ChangeKind::Renamed,
            'C' => ChangeKind::Copied,
            'T' => ChangeKind::TypeChanged,
            'U' => ChangeKind::Unmerged,
            _ => ChangeKind::Unknown,
        };
        if matches!(change, ChangeKind::Renamed | ChangeKind::Copied) {
            let old = fields.get(index).ok_or_else(|| {
                ReviewError::Parse(format!("missing old path after status {status}"))
            })?;
            let new = fields.get(index + 1).ok_or_else(|| {
                ReviewError::Parse(format!("missing new path after status {status}"))
            })?;
            validate_repo_path(old)?;
            validate_repo_path(new)?;
            entries.push(ChangedEntry {
                path: new.clone(),
                old_path: Some(old.clone()),
                change,
            });
            index += 2;
        } else {
            let path = fields
                .get(index)
                .ok_or_else(|| ReviewError::Parse(format!("missing path after status {status}")))?;
            validate_repo_path(path)?;
            entries.push(ChangedEntry {
                path: path.clone(),
                old_path: None,
                change,
            });
            index += 1;
        }
    }
    Ok(entries)
}

fn validate_repo_path(path: &str) -> Result<(), ReviewError> {
    let path_obj = Path::new(path);
    if path.is_empty()
        || path_obj.is_absolute()
        || path.chars().any(char::is_control)
        || path_obj
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ReviewError::UnsafePath(path.to_string()));
    }
    Ok(())
}

fn parse_range(value: &str) -> Result<(u32, u32), ReviewError> {
    let mut pieces = value.splitn(2, ',');
    let start = pieces
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| ReviewError::Parse(format!("invalid hunk range {value:?}")))?;
    let count = pieces
        .next()
        .map(|value| value.parse::<u32>())
        .transpose()
        .map_err(|_| ReviewError::Parse(format!("invalid hunk range {value:?}")))?
        .unwrap_or(1);
    Ok((start, count))
}

fn parse_hunk_header(line: &str) -> Result<(u32, u32, u32, u32, String), ReviewError> {
    let rest = line
        .strip_prefix("@@ -")
        .ok_or_else(|| ReviewError::Parse(format!("invalid hunk header {line:?}")))?;
    let (ranges, section) = rest
        .split_once(" @@")
        .ok_or_else(|| ReviewError::Parse(format!("invalid hunk header {line:?}")))?;
    let (old, new) = ranges
        .split_once(" +")
        .ok_or_else(|| ReviewError::Parse(format!("invalid hunk ranges {ranges:?}")))?;
    let (old_start, old_count) = parse_range(old)?;
    let (new_start, new_count) = parse_range(new)?;
    Ok((
        old_start,
        old_count,
        new_start,
        new_count,
        section.trim().to_string(),
    ))
}

fn parse_hunks(patch: &str) -> Result<Vec<DiffHunk>, ReviewError> {
    let lines: Vec<&str> = patch.lines().collect();
    let mut hunks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if !lines[index].starts_with("@@ -") {
            index += 1;
            continue;
        }
        let (old_start, old_count, new_start, new_count, section) =
            parse_hunk_header(lines[index])?;
        index += 1;
        let mut old_line = old_start;
        let mut new_line = new_start;
        let mut body = Vec::new();
        while index < lines.len()
            && !lines[index].starts_with("@@ -")
            && !lines[index].starts_with("diff --git ")
        {
            let line = lines[index];
            let (kind, content, old, new) = if let Some(content) = line.strip_prefix('+') {
                let current = new_line;
                new_line = new_line.saturating_add(1);
                (DiffLineKind::Addition, content, None, Some(current))
            } else if let Some(content) = line.strip_prefix('-') {
                let current = old_line;
                old_line = old_line.saturating_add(1);
                (DiffLineKind::Removal, content, Some(current), None)
            } else if let Some(content) = line.strip_prefix(' ') {
                let old_current = old_line;
                let new_current = new_line;
                old_line = old_line.saturating_add(1);
                new_line = new_line.saturating_add(1);
                (
                    DiffLineKind::Context,
                    content,
                    Some(old_current),
                    Some(new_current),
                )
            } else if let Some(content) = line.strip_prefix('\\') {
                (DiffLineKind::NoNewlineMarker, content.trim(), None, None)
            } else {
                break;
            };
            body.push(DiffLine {
                kind,
                content: content.to_string(),
                old_line: old,
                new_line: new,
            });
            index += 1;
        }
        hunks.push(DiffHunk {
            old_start,
            old_count,
            new_start,
            new_count,
            section,
            lines: body,
        });
    }
    Ok(hunks)
}

fn patch_modes(patch: &str) -> (Option<String>, Option<String>) {
    let mut old_mode = None;
    let mut new_mode = None;
    for line in patch.lines() {
        if let Some(mode) = line.strip_prefix("old mode ") {
            old_mode = Some(mode.to_string());
        } else if let Some(mode) = line.strip_prefix("new mode ") {
            new_mode = Some(mode.to_string());
        } else if let Some(mode) = line.strip_prefix("new file mode ") {
            new_mode = Some(mode.to_string());
        } else if let Some(mode) = line.strip_prefix("deleted file mode ") {
            old_mode = Some(mode.to_string());
        } else if let Some((_, mode)) = line
            .strip_prefix("index ")
            .and_then(|line| line.rsplit_once(' '))
            && mode.len() == 6
            && mode.bytes().all(|byte| (b'0'..=b'7').contains(&byte))
        {
            old_mode.get_or_insert_with(|| mode.to_string());
            new_mode.get_or_insert_with(|| mode.to_string());
        }
    }
    (old_mode, new_mode)
}

fn synthesize_added_patch(path: &str, content: &str) -> String {
    let line_count = content.lines().count();
    let mut patch = format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{line_count} @@\n"
    );
    for line in content.lines() {
        patch.push('+');
        patch.push_str(line);
        patch.push('\n');
    }
    if !content.is_empty() && !content.ends_with('\n') {
        patch.push_str("\\ No newline at end of file\n");
    }
    patch
}

fn total_patch_bytes(files: &[ReviewFile]) -> usize {
    files.iter().map(|file| file.patch.len()).sum()
}

fn enforce_diff_limit(files: &[ReviewFile], limit: usize) -> Result<(), ReviewError> {
    let actual = total_patch_bytes(files);
    if actual > limit {
        return Err(ReviewError::DiffTooLarge { actual, limit });
    }
    Ok(())
}

fn calculate_stats(files: &[ReviewFile]) -> DiffStats {
    let mut stats = DiffStats {
        files: files.len(),
        ..DiffStats::default()
    };
    for file in files {
        stats.hunks += file.hunks.len();
        stats.binary_files += usize::from(file.binary);
        for line in file.hunks.iter().flat_map(|hunk| hunk.lines.iter()) {
            match line.kind {
                DiffLineKind::Addition => stats.additions += 1,
                DiffLineKind::Removal => stats.deletions += 1,
                DiffLineKind::Context | DiffLineKind::NoNewlineMarker => {}
            }
        }
    }
    stats
}

fn diff_fingerprint(target: &ReviewTarget, files: &[ReviewFile]) -> String {
    let mut material = serde_json::to_vec(target).expect("ReviewTarget serialization cannot fail");
    for file in files {
        material.extend_from_slice(file.path.as_bytes());
        material.push(0);
        if let Some(old) = &file.old_path {
            material.extend_from_slice(old.as_bytes());
        }
        material.push(0);
        material.extend_from_slice(format!("{:?}", file.change).as_bytes());
        material.push(0);
        material.push(u8::from(file.untracked));
        material.push(u8::from(file.binary));
        if let Some(mode) = &file.old_mode {
            material.extend_from_slice(mode.as_bytes());
        }
        material.push(0);
        if let Some(mode) = &file.new_mode {
            material.extend_from_slice(mode.as_bytes());
        }
        material.push(0);
        if let Some(hash) = &file.content_hash {
            material.extend_from_slice(hash.as_bytes());
        }
        material.push(0);
        material.extend_from_slice(file.patch.as_bytes());
        material.push(0xff);
    }
    content_hash(material)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn run(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repository() -> Option<TempDir> {
        if Command::new("git").arg("--version").output().is_err() {
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "--quiet"]);
        run(dir.path(), &["config", "user.name", "HiveMind Test"]);
        run(
            dir.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        std::fs::write(dir.path().join("tracked.txt"), "one\ntwo\nthree\n").unwrap();
        run(dir.path(), &["add", "tracked.txt"]);
        run(dir.path(), &["commit", "--quiet", "-m", "base"]);
        Some(dir)
    }

    #[test]
    fn parses_hunk_ranges_and_line_numbers() {
        let patch = "@@ -10,2 +10,3 @@ function\n keep\n-old\n+new\n+extra\n";
        let hunks = parse_hunks(patch).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].section, "function");
        assert_eq!(hunks[0].lines[1].old_line, Some(11));
        assert_eq!(hunks[0].lines[2].new_line, Some(11));
        assert_eq!(hunks[0].lines[3].new_line, Some(12));
    }

    #[test]
    fn working_tree_includes_tracked_and_untracked_changes() {
        let Some(dir) = repository() else { return };
        std::fs::write(dir.path().join("tracked.txt"), "one\nchanged\nthree\n").unwrap();
        std::fs::write(dir.path().join("new file.txt"), "alpha\nbeta\n").unwrap();
        let repo = GitRepository::open(dir.path()).unwrap();
        let first = repo
            .acquire(ReviewTarget::WorkingTree, DiffLimits::default())
            .unwrap();
        let second = repo
            .acquire(ReviewTarget::WorkingTree, DiffLimits::default())
            .unwrap();

        assert_eq!(first.files.len(), 2);
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.stats.additions, 3);
        assert_eq!(first.stats.deletions, 1);
        let untracked = first
            .files
            .iter()
            .find(|file| file.path == "new file.txt")
            .unwrap();
        assert_eq!(untracked.change, ChangeKind::Added);
        assert_eq!(untracked.changed_new_lines(), BTreeSet::from([1, 2]));
    }

    #[test]
    fn staged_target_reads_index_content_not_later_worktree_edits() {
        let Some(dir) = repository() else { return };
        std::fs::write(dir.path().join("tracked.txt"), "one\nstaged\nthree\n").unwrap();
        run(dir.path(), &["add", "tracked.txt"]);
        std::fs::write(dir.path().join("tracked.txt"), "one\nunstaged\nthree\n").unwrap();
        let repo = GitRepository::open(dir.path()).unwrap();
        let diff = repo
            .acquire(ReviewTarget::Staged, DiffLimits::default())
            .unwrap();
        let file = &diff.files[0];
        assert!(file.patch.contains("+staged"));
        assert!(!file.patch.contains("unstaged"));
        assert_eq!(
            file.content_hash.as_deref(),
            Some(content_hash("one\nstaged\nthree\n").as_str())
        );
    }

    #[test]
    fn rename_is_normalized_with_both_paths() {
        let Some(dir) = repository() else { return };
        run(dir.path(), &["mv", "tracked.txt", "renamed.txt"]);
        run(dir.path(), &["commit", "--quiet", "-m", "rename"]);
        let repo = GitRepository::open(dir.path()).unwrap();
        let diff = repo
            .acquire(
                ReviewTarget::Commit { sha: "HEAD".into() },
                DiffLimits::default(),
            )
            .unwrap();
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].change, ChangeKind::Renamed);
        assert_eq!(diff.files[0].old_path.as_deref(), Some("tracked.txt"));
        assert_eq!(diff.files[0].path, "renamed.txt");
    }

    #[test]
    fn hard_limits_fail_before_returning_a_partial_review() {
        let Some(dir) = repository() else { return };
        std::fs::write(dir.path().join("one.txt"), "one").unwrap();
        std::fs::write(dir.path().join("two.txt"), "two").unwrap();
        let repo = GitRepository::open(dir.path()).unwrap();
        let error = repo
            .acquire(
                ReviewTarget::WorkingTree,
                DiffLimits {
                    max_files: 1,
                    max_bytes: 1024,
                    ..DiffLimits::default()
                },
            )
            .unwrap_err();
        assert!(matches!(error, ReviewError::TooManyFiles { .. }));
    }

    #[test]
    fn opening_a_nested_directory_does_not_escape_the_workspace_jail() {
        let Some(dir) = repository() else { return };
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let error = GitRepository::open(nested).unwrap_err();
        assert!(matches!(error, ReviewError::NotRepositoryRoot { .. }));
    }

    #[test]
    fn root_commit_is_reviewed_as_additions() {
        let Some(dir) = repository() else { return };
        let repo = GitRepository::open(dir.path()).unwrap();
        let diff = repo
            .acquire(
                ReviewTarget::Commit { sha: "HEAD".into() },
                DiffLimits::default(),
            )
            .unwrap();
        assert!(diff.base_revision.is_none());
        assert!(diff.head_revision.is_some());
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].change, ChangeKind::Added);
        assert_eq!(diff.stats.additions, 3);
    }

    #[test]
    fn commit_review_of_a_merge_uses_the_first_parent() {
        let Some(dir) = repository() else { return };
        let original_branch = Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let original_branch = String::from_utf8(original_branch.stdout)
            .unwrap()
            .trim()
            .to_string();
        run(dir.path(), &["switch", "--quiet", "-c", "side"]);
        std::fs::write(dir.path().join("tracked.txt"), "one\nside\nthree\n").unwrap();
        run(dir.path(), &["add", "tracked.txt"]);
        run(dir.path(), &["commit", "--quiet", "-m", "side"]);
        run(dir.path(), &["switch", "--quiet", &original_branch]);
        std::fs::write(dir.path().join("main-only.txt"), "main\n").unwrap();
        run(dir.path(), &["add", "main-only.txt"]);
        run(dir.path(), &["commit", "--quiet", "-m", "main"]);
        run(
            dir.path(),
            &["merge", "--quiet", "--no-ff", "--no-edit", "side"],
        );

        let repo = GitRepository::open(dir.path()).unwrap();
        let diff = repo
            .acquire(
                ReviewTarget::Commit { sha: "HEAD".into() },
                DiffLimits::default(),
            )
            .unwrap();
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].path, "tracked.txt");
        assert!(diff.files[0].patch.contains("+side"));
    }

    #[test]
    fn merge_base_resolves_diverged_branches_to_one_immutable_oid() {
        let Some(dir) = repository() else { return };
        let original_branch = Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let original_branch = String::from_utf8(original_branch.stdout)
            .unwrap()
            .trim()
            .to_string();
        run(dir.path(), &["branch", "base"]);
        std::fs::write(dir.path().join("tracked.txt"), "feature\n").unwrap();
        run(dir.path(), &["add", "tracked.txt"]);
        run(dir.path(), &["commit", "--quiet", "-m", "feature"]);
        run(dir.path(), &["switch", "--quiet", "base"]);
        std::fs::write(dir.path().join("base-only.txt"), "base\n").unwrap();
        run(dir.path(), &["add", "base-only.txt"]);
        run(dir.path(), &["commit", "--quiet", "-m", "base"]);

        let repo = GitRepository::open(dir.path()).unwrap();
        let merge_base = repo.merge_base(&original_branch, "HEAD").unwrap();
        assert_eq!(merge_base.len(), 40);
        let expected = Command::new("git")
            .args(["rev-parse", &format!("{original_branch}~1")])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert_eq!(
            merge_base,
            String::from_utf8(expected.stdout).unwrap().trim()
        );
    }

    #[test]
    fn deleted_files_keep_old_mode_but_have_no_target_hash() {
        let Some(dir) = repository() else { return };
        std::fs::remove_file(dir.path().join("tracked.txt")).unwrap();
        run(dir.path(), &["add", "--all"]);
        run(dir.path(), &["commit", "--quiet", "-m", "delete"]);
        let repo = GitRepository::open(dir.path()).unwrap();
        let diff = repo
            .acquire(
                ReviewTarget::Commit { sha: "HEAD".into() },
                DiffLimits::default(),
            )
            .unwrap();
        assert_eq!(diff.files[0].change, ChangeKind::Deleted);
        assert_eq!(diff.files[0].old_mode.as_deref(), Some("100644"));
        assert!(diff.files[0].new_mode.is_none());
        assert!(diff.files[0].content_hash.is_none());
    }

    #[test]
    fn binary_content_participates_in_the_fingerprint() {
        let Some(dir) = repository() else { return };
        let path = dir.path().join("data.bin");
        std::fs::write(&path, [0, 1, 2, 3]).unwrap();
        let repo = GitRepository::open(dir.path()).unwrap();
        let first = repo
            .acquire(ReviewTarget::WorkingTree, DiffLimits::default())
            .unwrap();
        std::fs::write(&path, [0, 1, 2, 4]).unwrap();
        let second = repo
            .acquire(ReviewTarget::WorkingTree, DiffLimits::default())
            .unwrap();
        assert!(first.files[0].binary);
        assert_ne!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn per_file_limit_blocks_a_large_file_even_when_the_patch_is_small() {
        let Some(dir) = repository() else { return };
        std::fs::write(dir.path().join("large.txt"), "0123456789").unwrap();
        let repo = GitRepository::open(dir.path()).unwrap();
        let error = repo
            .acquire(
                ReviewTarget::WorkingTree,
                DiffLimits {
                    max_file_bytes: 5,
                    ..DiffLimits::default()
                },
            )
            .unwrap_err();
        assert!(matches!(error, ReviewError::FileTooLarge { .. }));
    }

    #[test]
    fn option_like_revisions_are_rejected_before_git_dispatch() {
        let Some(dir) = repository() else { return };
        let repo = GitRepository::open(dir.path()).unwrap();
        let error = repo
            .acquire(
                ReviewTarget::Commit {
                    sha: "--help".into(),
                },
                DiffLimits::default(),
            )
            .unwrap_err();
        assert!(matches!(error, ReviewError::InvalidRevision(_)));
    }

    #[test]
    fn acquisition_does_not_modify_git_status_or_index() {
        let Some(dir) = repository() else { return };
        std::fs::write(dir.path().join("tracked.txt"), "one\nchanged\nthree\n").unwrap();
        std::fs::write(dir.path().join("new.txt"), "new\n").unwrap();
        let status = || {
            Command::new("git")
                .args(["status", "--porcelain=v2", "-z", "--untracked-files=all"])
                .current_dir(dir.path())
                .output()
                .unwrap()
                .stdout
        };
        let before = status();
        let repo = GitRepository::open(dir.path()).unwrap();
        repo.acquire(ReviewTarget::WorkingTree, DiffLimits::default())
            .unwrap();
        assert_eq!(status(), before);
    }

    #[cfg(unix)]
    #[test]
    fn worktree_symlinks_are_never_followed() {
        use std::os::unix::fs::symlink;

        let Some(dir) = repository() else { return };
        let external = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(external.path(), "outside secret").unwrap();
        symlink(external.path(), dir.path().join("link.txt")).unwrap();
        let repo = GitRepository::open(dir.path()).unwrap();
        let error = repo
            .acquire(ReviewTarget::WorkingTree, DiffLimits::default())
            .unwrap_err();
        assert!(matches!(error, ReviewError::SymlinkPath(_)));
    }
}
