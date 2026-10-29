//! The `Tool` trait, registry, and builtin tools — analogue of grok-build's
//! `xai-tool-runtime` + the file/shell tools in `xai-grok-tools`.

mod artifact;
mod bash;
mod diagram;
mod edit;
mod embed_cache;
mod error;
mod fs;
mod pdf;
mod pdf_read;
mod project_map;
mod read_program;
mod readset;
mod search;
pub mod secrets;
mod semantic;
mod todo;
mod tool;
mod walk;
/// Exported so `harness-cli`'s `@path` mention expansion shows exactly the
/// files the read-only tools would find — one walk policy, not two that can
/// disagree about what "the codebase" is.
pub use walk::walk_files;
mod web;
mod xlsx;

pub use artifact::{
    ArtifactHandle, ArtifactStore, DEFAULT_ARTIFACT_THRESHOLD_BYTES, ReadArtifact, preview,
    text_to_offload,
};
pub use bash::{ApproveFn, BackgroundProcesses, Bash, shell_command, strip_verbatim};
pub use diagram::CreateDiagram;
pub use edit::EditFile;
pub use embed_cache::EmbedCache;
pub use error::ToolError;
pub use fs::{ListDir, MAX_READ_BYTES, ReadFile, Workspace, WriteFile};
pub use pdf::CreatePdf;
pub use pdf_read::{run_worker as run_pdf_worker, set_worker_exe as set_pdf_worker_exe};
pub use project_map::ProjectMap;
pub use read_program::{ReadProgram, ReadProgramPolicy};
pub use readset::ReadSet;
pub use search::Search;
pub use semantic::{Embedder, HashingEmbedder, ProgressSink, SemanticSearch};
pub use todo::TodoWrite;
pub use tool::{FileChange, FileChangeKind, Registry, Tool, ToolResult, ToolStatus, obj_schema};
pub use web::{HostedWebClient, WebFetch, WebSearch};
pub use xlsx::CreateSpreadsheet;
