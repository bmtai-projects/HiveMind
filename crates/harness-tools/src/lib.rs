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
mod project_map;
mod read_program;
mod readset;
mod remote_embed;
mod search;
pub mod secrets;
mod semantic;
mod todo;
mod tool;
mod walk;
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
pub use fs::{ListDir, ReadFile, Workspace, WriteFile};
pub use pdf::CreatePdf;
pub use project_map::ProjectMap;
pub use read_program::{ReadProgram, ReadProgramPolicy};
pub use readset::ReadSet;
pub use remote_embed::RemoteEmbedder;
pub use search::Search;
pub use semantic::{Embedder, HashingEmbedder, ProgressSink, SemanticSearch};
pub use todo::TodoWrite;
pub use tool::{FileChange, FileChangeKind, Registry, Tool, ToolResult, ToolStatus, obj_schema};
pub use web::{HostedWebClient, WebFetch, WebSearch};
pub use xlsx::CreateSpreadsheet;
