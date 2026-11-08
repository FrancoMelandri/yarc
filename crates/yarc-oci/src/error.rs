use thiserror::Error;

#[derive(Debug, Error)]
pub enum OciError {
    #[error("invalid image reference '{0}': {1}")]
    Reference(String, String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("registry returned HTTP {status} for {url}")]
    Http { status: u16, url: String },
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("invalid json in {what}: {msg}")]
    Json { what: String, msg: String },
    #[error("digest mismatch for {what}: expected {expected}, got {actual}")]
    DigestMismatch { what: String, expected: String, actual: String },
    #[error("size mismatch for {what}: expected {expected} bytes, got {actual}")]
    SizeMismatch { what: String, expected: u64, actual: u64 },
    #[error("i/o error on {path}: {msg}")]
    Io { path: String, msg: String },
    #[error("image '{0}' is not in the local store (run `yarc pull` first)")]
    NotFound(String),
    #[error("unsafe path in layer: '{0}'")]
    UnsafePath(String),
    #[error("too many levels of symbolic links while resolving '{0}'")]
    TooManySymlinks(String),
    #[error("invalid layer: {0}")]
    BadLayer(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("destination '{0}' exists and is not an empty directory")]
    DestNotEmpty(String),
    #[error("invalid digest '{0}'")]
    InvalidDigest(String),
    #[error("no image for platform {os}/{arch}")]
    NoMatchingPlatform { os: String, arch: String },
    #[error("{0} exceeds the size limit of {1} bytes")]
    TooLarge(String, u64),
    #[error("unsupported manifest: {0}")]
    UnsupportedManifest(String),
}

pub type Result<T> = std::result::Result<T, OciError>;
