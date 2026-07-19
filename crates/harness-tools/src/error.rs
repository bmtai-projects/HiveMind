use thiserror::Error;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("{0}")]
    Message(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid arguments: {0}")]
    InvalidArgs(#[from] serde_json::Error),
}
