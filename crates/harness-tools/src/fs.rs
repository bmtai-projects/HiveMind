use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::error::ToolError;
use crate::readset::ReadSet;
use crate::tool::{Tool, obj_schema};

/// Confines file tools to a root directory. All paths are resolved relative
/// to `root` and may not escape it.
///
/// Also carries the session's [`ReadSet`]. Every file tool is built from a
/// clone of one `Workspace`, so hanging the read tracking here is what lets
/// `read_file` and `edit_file` — separate tools that never talk to each
/// other — share a view of what has been seen. Cloning shares that view
/// rather than copying it.
#[derive(Clone, Default)]
pub struct Workspace {
    pub root: PathBuf,
    pub read_set: ReadSet,
}

/// Shared [`crate::tool::Tool::conflict_key`] implementation for every tool
/// that writes to a single `path` argument. Keys on the *canonical*
/// resolved path, so two calls naming one file differently (`./a.txt` vs
/// `a.txt`) still collide and get serialized rather than racing.
///
/// `None` when the args don't parse or the path escapes the workspace: such
/// a call fails on its own during `execute`, so it can never win a write
/// race it wasn't going to participate in.
pub(crate) fn path_conflict_key(ws: &Workspace, args: &RawValue) -> Option<String> {
    #[derive(Deserialize)]
    struct PathOnly {
        path: String,
    }
    let parsed: PathOnly = serde_json::from_str(args.get()).ok()?;
    let resolved = ws.resolve(&parsed.path).ok()?;
    Some(resolved.to_string_lossy().into_owned())
}

impl Workspace {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            read_set: ReadSet::new(),
        }
    }

    /// Resolve `rel` against the workspace root, rejecting anything that
    /// escapes it. `pub` so hosts (e.g. the CLI's `@file` mention expansion)
    /// can reuse the same path-safety check instead of re-implementing it.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, ToolError> {
        if rel.is_empty() {
            return Err(ToolError::Message("path is required".into()));
        }
        let candidate = Path::new(rel);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.root.join(candidate)
        };

        let root_abs = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        // The target may not exist yet (write_file creating a new file), so
        // canonicalize its parent instead and re-attach the file name.
        let joined_abs = match joined.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                let parent = joined.parent().unwrap_or(&joined);
                let parent_abs = parent.canonicalize().map_err(ToolError::Io)?;
                match joined.file_name() {
                    Some(name) => parent_abs.join(name),
                    None => parent_abs,
                }
            }
        };

        if joined_abs != root_abs && !joined_abs.starts_with(&root_abs) {
            return Err(ToolError::Message(format!(
                "path {rel:?} escapes the workspace root"
            )));
        }
        Ok(joined_abs)
    }
}

const MAX_READ_BYTES: usize = 60_000;

#[derive(Deserialize)]
struct PathArgs {
    path: String,
}

pub struct ReadFile(pub Workspace);

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read the full contents of a text file at the given workspace-relative path."
    }
    fn schema(&self) -> serde_json::Value {
        obj_schema(
            &[(
                "path",
                serde_json::json!({"type": "string", "description": "workspace-relative file path"}),
            )],
            &["path"],
        )
    }
    async fn execute(&self, args: &RawValue) -> Result<String, ToolError> {
        let a: PathArgs = serde_json::from_str(args.get())?;
        let p = self.0.resolve(&a.path)?;
        let bytes = tokio::fs::read(&p).await?;
        // Fingerprint the *whole* file, including on the truncated path: the
        // question this answers later is "did this file change since we
        // looked at it", which a prefix can't answer. That the model saw
        // only part of it is a separate limitation, unaffected either way.
        self.0.read_set.record(&p, &String::from_utf8_lossy(&bytes));
        if bytes.len() > MAX_READ_BYTES {
            let head = String::from_utf8_lossy(&bytes[..MAX_READ_BYTES]).into_owned();
            return Ok(format!(
                "{head}\n\n[truncated: {} bytes total]",
                bytes.len()
            ));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

pub struct WriteFile(pub Workspace);

#[async_trait]
impl Tool for WriteFile {
    fn conflict_key(&self, args: &RawValue) -> Option<String> {
        path_conflict_key(&self.0, args)
    }
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Create or overwrite a text file at the given workspace-relative path with the provided content."
    }
    fn schema(&self) -> serde_json::Value {
        obj_schema(
            &[
                (
                    "path",
                    serde_json::json!({"type": "string", "description": "workspace-relative file path"}),
                ),
                (
                    "content",
                    serde_json::json!({"type": "string", "description": "full file content to write"}),
                ),
            ],
            &["path", "content"],
        )
    }
    async fn execute(&self, args: &RawValue) -> Result<String, ToolError> {
        let a: WriteArgs = serde_json::from_str(args.get())?;
        if a.path.is_empty() {
            return Err(ToolError::Message("path is required".into()));
        }
        let candidate = Path::new(&a.path);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.0.root.join(candidate)
        };
        if let Some(parent) = joined.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Re-resolve now that the parent exists, to enforce the workspace boundary.
        let p = self.0.resolve(&a.path)?;
        tokio::fs::write(&p, &a.content).await?;
        // A tool that just wrote the file knows exactly what's in it, so
        // this counts as having seen it -- otherwise a write-then-edit
        // sequence would leave the edit unguarded.
        self.0.read_set.record(&p, &a.content);

        let mut out = format!("wrote {} bytes to {}", a.content.len(), a.path);
        // Appended to a *successful* result rather than raised as an error:
        // the write is legitimate far more often than not (fixtures, docs,
        // examples), so this informs without blocking. See `crate::secrets`.
        if let Some(note) = crate::secrets::warning(&crate::secrets::scan(&a.content)) {
            out.push_str(&note);
        }
        Ok(out)
    }
}

pub struct ListDir(pub Workspace);

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &str {
        "list_dir"
    }
    fn description(&self) -> &str {
        "List the entries of a directory (workspace-relative). Directories are suffixed with '/'."
    }
    fn schema(&self) -> serde_json::Value {
        obj_schema(
            &[(
                "path",
                serde_json::json!({"type": "string", "description": "workspace-relative directory path; '.' for root"}),
            )],
            &["path"],
        )
    }
    async fn execute(&self, args: &RawValue) -> Result<String, ToolError> {
        let mut a: PathArgs = serde_json::from_str(args.get())?;
        if a.path.is_empty() {
            a.path = ".".to_string();
        }
        let p = self.0.resolve(&a.path)?;
        let mut entries = tokio::fs::read_dir(&p).await?;
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let mut name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                name.push('/');
            }
            names.push(name);
        }
        names.sort();
        if names.is_empty() {
            return Ok("(empty)".to_string());
        }
        Ok(names.join("\n"))
    }
}
