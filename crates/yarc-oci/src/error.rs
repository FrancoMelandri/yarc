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
