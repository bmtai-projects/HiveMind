//! The `Tool` trait, registry, and builtin tools — analogue of grok-build's
//! `xai-tool-runtime` + the file/shell tools in `xai-grok-tools`.

mod bash;
mod edit;
mod error;
mod fs;
mod pdf;
mod project_map;
mod search;
mod semantic;
mod todo;
mod tool;
mod walk;
mod xlsx;

pub use bash::{ApproveFn, Bash};
pub use edit::EditFile;
pub use error::ToolError;
pub use fs::{ListDir, ReadFile, Workspace, WriteFile};
pub use pdf::CreatePdf;
pub use project_map::ProjectMap;
pub use search::Search;
pub use semantic::{Embedder, HashingEmbedder, SemanticSearch};
pub use todo::TodoWrite;
pub use tool::{Registry, Tool, obj_schema};
pub use xlsx::CreateSpreadsheet;
