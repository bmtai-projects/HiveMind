//! The `Tool` trait, registry, and builtin tools — analogue of grok-build's
//! `xai-tool-runtime` + the file/shell tools in `xai-grok-tools`.

mod bash;
mod edit;
mod error;
mod fs;
mod search;
mod semantic;
mod tool;
mod walk;

pub use bash::{ApproveFn, Bash};
pub use edit::EditFile;
pub use error::ToolError;
pub use fs::{ListDir, ReadFile, Workspace, WriteFile};
pub use search::Search;
pub use semantic::{Embedder, HashingEmbedder, SemanticSearch};
pub use tool::{Registry, Tool, obj_schema};
