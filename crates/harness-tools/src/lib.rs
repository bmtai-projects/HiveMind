//! The `Tool` trait, registry, and builtin tools — analogue of grok-build's
//! `xai-tool-runtime` + the file/shell tools in `xai-grok-tools`.

mod bash;
mod error;
mod fs;
mod tool;

pub use bash::{ApproveFn, Bash};
pub use error::ToolError;
pub use fs::{ListDir, ReadFile, Workspace, WriteFile};
pub use tool::{Registry, Tool, obj_schema};
