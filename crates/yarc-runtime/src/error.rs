use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("no command to run: the image has no Entrypoint or Cmd and none was given")]
    NoCommand,
    #[error("invalid environment entry '{0}', expected KEY=VALUE")]
    InvalidEnv(String),
    #[error("working directory '{0}' must be an absolute path")]
    RelativeWorkDir(String),
    #[error("invalid container name '{0}': use letters, digits, '_', '.', '-' (max 64, starting with a letter or digit)")]
    InvalidName(String),
    #[error("a container named '{0}' already exists")]
    AlreadyExists(String),
    #[error("no such container: {0}")]
    NotFound(String),
    #[error("{context}: {source}")]
    Io { context: String, source: std::io::Error },
    #[error("invalid container config {path}: {source}")]
    Json { path: PathBuf, source: serde_json::Error },
}

pub type Result<T> = std::result::Result<T, RuntimeError>;

pub(crate) fn io_err(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> RuntimeError {
    let context = context.into();
    move |source| RuntimeError::Io { context, source }
}
