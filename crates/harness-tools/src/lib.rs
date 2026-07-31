//! The `Tool` trait, registry, and builtin tools — analogue of grok-build's
//! `xai-tool-runtime` + the file/shell tools in `xai-grok-tools`.

mod bash;
mod edit;
mod embed_cache;
mod error;
mod fs;
mod pdf;
mod project_map;
mod remote_embed;
mod search;
mod semantic;
mod todo;
mod tool;
mod walk;
mod xlsx;

pub use bash::{ApproveFn, Bash};
pub use edit::EditFile;
pub use embed_cache::EmbedCache;
pub use error::ToolError;
pub use fs::{ListDir, ReadFile, Workspace, WriteFile};
pub use pdf::CreatePdf;
pub use project_map::ProjectMap;
pub use remote_embed::RemoteEmbedder;
pub use search::Search;
pub use semantic::{Embedder, HashingEmbedder, ProgressSink, SemanticSearch};
pub use todo::TodoWrite;
pub use tool::{Registry, Tool, obj_schema};
pub use xlsx::CreateSpreadsheet;
