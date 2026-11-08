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
    #[error("container {0} is already running")]
    AlreadyRunning(String),
    #[error("no such container: {0}")]
    NotFound(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("refusing to use '{0}' inside the rootfs: it goes through a symlink or '..'")]
    UnsafePath(String),
    /// The container could not be started. `code` is the exit status yarc
    /// should use, as Docker does: 127 command not found, 126 not executable,
    /// 125 anything else.
    #[error("cannot start container: {msg}")]
    Start { msg: String, code: i32 },
    #[error("{context}: {source}")]
    Io { context: String, source: std::io::Error },
    #[error("invalid container config {path}: {source}")]
    Json { path: PathBuf, source: serde_json::Error },
}

impl RuntimeError {
    pub fn start(msg: impl Into<String>) -> Self {
        RuntimeError::Start { msg: msg.into(), code: 125 }
    }
}

pub type Result<T> = std::result::Result<T, RuntimeError>;

pub(crate) fn io_err(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> RuntimeError {
    let context = context.into();
    move |source| RuntimeError::Io { context, source }
}
