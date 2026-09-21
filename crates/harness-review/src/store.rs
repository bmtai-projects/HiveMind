#[cfg(unix)]
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

use crate::{REVIEW_SCHEMA_VERSION, ReviewReport, content_hash};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("could not canonicalize workspace {path:?}: {source}")]
    Workspace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("review store I/O at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("review report could not be serialized: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("review {review_id:?} was not found")]
    NotFound { review_id: String },
    #[error("review {review_id:?} is corrupt: {source}")]
    Corrupt {
        review_id: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("review {review_id:?} has unsupported schema version {found:?}")]
    UnsupportedSchema { review_id: String, found: String },
    #[error("review file requested as {expected:?} contains id {actual:?}")]
    IdMismatch { expected: String, actual: String },
    #[error("review id must not be empty")]
    EmptyReviewId,
}

/// Local report persistence, partitioned by canonical workspace identity.
///
/// The caller normally passes `harness_config::default_reviews_dir()` as the
/// root, which deliberately lives outside the repository. Workspace paths
/// and review IDs are hashed before becoming directory/file names, so an ID
/// such as `../../anything` can never escape the store.
#[derive(Debug, Clone)]
pub struct ReviewStore {
    dir: PathBuf,
}

impl ReviewStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Stable full SHA-256 key for a canonical workspace path.
    pub fn workspace_key(workspace: impl AsRef<Path>) -> Result<String, StoreError> {
        let requested = workspace.as_ref();
        let canonical = requested
            .canonicalize()
            .map_err(|source| StoreError::Workspace {
                path: requested.to_path_buf(),
                source,
            })?;
        Ok(content_hash(path_bytes(&canonical)))
    }

    /// Save one complete report with same-directory temp-file + rename
    /// semantics, so readers see either the previous complete JSON document
    /// or this complete one, never a partially written report.
    pub fn save(
        &self,
        workspace: impl AsRef<Path>,
        report: &ReviewReport,
    ) -> Result<(), StoreError> {
        validate_id(&report.review_id.0)?;
        validate_schema(report)?;

        let workspace_dir = self.workspace_dir(workspace)?;
        std::fs::create_dir_all(&workspace_dir).map_err(|source| StoreError::Io {
            path: workspace_dir.clone(),
            source,
        })?;
        make_owner_only_dir(&workspace_dir);

        let json = serde_json::to_vec_pretty(report).map_err(StoreError::Serialize)?;
        let final_path = report_path(&workspace_dir, &report.review_id.0);
        let nonce = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let tmp_path = workspace_dir.join(format!(
            ".{}-{}-{nonce}.tmp",
            content_hash(report.review_id.0.as_bytes()),
            std::process::id()
        ));

        if let Err(error) = write_private_file(&tmp_path, &json) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(error);
        }
        if let Err(source) = std::fs::rename(&tmp_path, &final_path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(StoreError::Io {
                path: final_path,
                source,
            });
        }
        sync_directory(&workspace_dir);
        Ok(())
    }

    pub fn load(
        &self,
        workspace: impl AsRef<Path>,
        review_id: &str,
    ) -> Result<ReviewReport, StoreError> {
        validate_id(review_id)?;
        let workspace_dir = self.workspace_dir(workspace)?;
        let path = report_path(&workspace_dir, review_id);
        let bytes = std::fs::read(&path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                StoreError::NotFound {
                    review_id: review_id.to_string(),
                }
            } else {
                StoreError::Io {
                    path: path.clone(),
                    source,
                }
            }
        })?;
        decode_report(&bytes, review_id)
    }

    /// Complete readable reports for one workspace, newest first. A corrupt
    /// sibling is skipped so it cannot hide every otherwise valid report;
    /// direct [`Self::load`] still reports that corruption precisely.
    pub fn list(&self, workspace: impl AsRef<Path>) -> Result<Vec<ReviewReport>, StoreError> {
        let workspace_dir = self.workspace_dir(workspace)?;
        let entries = match std::fs::read_dir(&workspace_dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(StoreError::Io {
                    path: workspace_dir,
                    source,
                });
            }
        };

        let mut reports: Vec<ReviewReport> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .filter_map(|entry| {
                let bytes = std::fs::read(entry.path()).ok()?;
                let report: ReviewReport = serde_json::from_slice(&bytes).ok()?;
                (report.schema_version == REVIEW_SCHEMA_VERSION).then_some(report)
            })
            .collect();
        reports.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.review_id.0.cmp(&right.review_id.0))
        });
        Ok(reports)
    }

    pub fn list_for_workspace(
        &self,
        workspace: impl AsRef<Path>,
    ) -> Result<Vec<ReviewReport>, StoreError> {
        self.list(workspace)
    }

    pub fn latest(&self, workspace: impl AsRef<Path>) -> Result<Option<ReviewReport>, StoreError> {
        Ok(self.list(workspace)?.into_iter().next())
    }

    pub fn latest_for_workspace(
        &self,
        workspace: impl AsRef<Path>,
    ) -> Result<Option<ReviewReport>, StoreError> {
        self.latest(workspace)
    }

    fn workspace_dir(&self, workspace: impl AsRef<Path>) -> Result<PathBuf, StoreError> {
        Ok(self.dir.join(Self::workspace_key(workspace)?))
    }
}

fn validate_id(review_id: &str) -> Result<(), StoreError> {
    if review_id.is_empty() {
        return Err(StoreError::EmptyReviewId);
    }
    Ok(())
}

fn validate_schema(report: &ReviewReport) -> Result<(), StoreError> {
    if report.schema_version != REVIEW_SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema {
            review_id: report.review_id.0.clone(),
            found: report.schema_version.clone(),
        });
    }
    Ok(())
}

fn report_path(workspace_dir: &Path, review_id: &str) -> PathBuf {
    workspace_dir.join(format!("{}.json", content_hash(review_id.as_bytes())))
}

fn decode_report(bytes: &[u8], expected_id: &str) -> Result<ReviewReport, StoreError> {
    let report: ReviewReport =
        serde_json::from_slice(bytes).map_err(|source| StoreError::Corrupt {
            review_id: expected_id.to_string(),
            source,
        })?;
    validate_schema(&report)?;
    if report.review_id.0 != expected_id {
        return Err(StoreError::IdMismatch {
            expected: expected_id.to_string(),
            actual: report.review_id.0,
        });
    }
    Ok(report)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(bytes).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.sync_all().map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(unix)]
fn make_owner_only_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn make_owner_only_dir(_path: &Path) {}

#[cfg(unix)]
fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) {}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt as _;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(not(any(unix, windows)))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ReviewId, ReviewSummary, ReviewTarget, UsageSummary};
    use tempfile::TempDir;

    fn report(id: &str, created_at: u64) -> ReviewReport {
        ReviewReport::new(
            ReviewId(id.into()),
            ReviewTarget::WorkingTree,
            Some("base".into()),
            None,
            "diff".into(),
            "workspace".into(),
            ReviewSummary::default(),
            Vec::new(),
            UsageSummary::default(),
            created_at,
        )
    }

    fn fixture() -> (TempDir, TempDir, ReviewStore) {
        let workspace = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let store = ReviewStore::new(storage.path().join("reviews"));
        (workspace, storage, store)
    }

    #[test]
    fn report_round_trips_through_workspace_scoped_storage() {
        let (workspace, _storage, store) = fixture();
        let expected = report("review_1", 42);
        store.save(workspace.path(), &expected).unwrap();
        let loaded = store.load(workspace.path(), "review_1").unwrap();
        assert_eq!(loaded.review_id.0, expected.review_id.0);
        assert_eq!(loaded.created_at, 42);
        assert_eq!(loaded.schema_version, REVIEW_SCHEMA_VERSION);
    }

    #[test]
    fn two_workspaces_never_see_each_others_reports() {
        let (_workspace, storage, store) = fixture();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        store.save(first.path(), &report("first", 1)).unwrap();
        store.save(second.path(), &report("second", 2)).unwrap();

        assert_eq!(store.list(first.path()).unwrap().len(), 1);
        assert_eq!(store.list(second.path()).unwrap().len(), 1);
        assert!(matches!(
            store.load(first.path(), "second"),
            Err(StoreError::NotFound { .. })
        ));
        assert!(!store.dir().starts_with(first.path()));
        assert!(store.dir().starts_with(storage.path()));
    }

    #[test]
    fn latest_is_deterministic_and_uses_report_creation_time() {
        let (workspace, _storage, store) = fixture();
        store.save(workspace.path(), &report("older", 10)).unwrap();
        store.save(workspace.path(), &report("newer", 90)).unwrap();
        let listed = store.list(workspace.path()).unwrap();
        assert_eq!(listed[0].review_id.0, "newer");
        assert_eq!(
            store.latest(workspace.path()).unwrap().unwrap().review_id.0,
            "newer"
        );
    }

    #[test]
    fn corrupt_sibling_is_skipped_but_direct_load_reports_corruption() {
        let (workspace, _storage, store) = fixture();
        store.save(workspace.path(), &report("good", 1)).unwrap();
        let workspace_dir = store.workspace_dir(workspace.path()).unwrap();
        std::fs::create_dir_all(&workspace_dir).unwrap();
        let bad_path = report_path(&workspace_dir, "bad");
        std::fs::write(&bad_path, b"not json").unwrap();

        assert_eq!(store.list(workspace.path()).unwrap().len(), 1);
        assert!(matches!(
            store.load(workspace.path(), "bad"),
            Err(StoreError::Corrupt { .. })
        ));
    }

    #[test]
    fn review_id_never_becomes_a_filesystem_path() {
        let (workspace, _storage, store) = fixture();
        let hostile = "../../outside";
        store.save(workspace.path(), &report(hostile, 1)).unwrap();
        assert_eq!(
            store.load(workspace.path(), hostile).unwrap().review_id.0,
            hostile
        );
        assert!(!store.dir().join("outside.json").exists());
    }

    #[test]
    fn atomic_save_leaves_no_temporary_files() {
        let (workspace, _storage, store) = fixture();
        store.save(workspace.path(), &report("atomic", 1)).unwrap();
        let workspace_dir = store.workspace_dir(workspace.path()).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(workspace_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary files remain: {leftovers:?}"
        );
    }

    #[test]
    fn unsupported_schema_is_rejected_before_writing() {
        let (workspace, _storage, store) = fixture();
        let mut future = report("future", 1);
        future.schema_version = "99.0".into();
        assert!(matches!(
            store.save(workspace.path(), &future),
            Err(StoreError::UnsupportedSchema { .. })
        ));
        assert!(store.list(workspace.path()).unwrap().is_empty());
    }

    #[test]
    fn missing_workspace_report_is_a_typed_not_found() {
        let (workspace, _storage, store) = fixture();
        assert!(matches!(
            store.load(workspace.path(), "missing"),
            Err(StoreError::NotFound { .. })
        ));
    }

    #[test]
    fn canonical_workspace_aliases_share_one_namespace() {
        let (workspace, _storage, _store) = fixture();
        let alias = workspace.path().join(".");
        assert_eq!(
            ReviewStore::workspace_key(workspace.path()).unwrap(),
            ReviewStore::workspace_key(alias).unwrap()
        );
    }
}
