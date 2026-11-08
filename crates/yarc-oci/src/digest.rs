use std::fmt;

use sha2::{Digest as _, Sha256};

use crate::error::{OciError, Result};

/// A `sha256:<64 hex>` content digest.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Digest(String);

impl Digest {
    pub fn parse(s: &str) -> Result<Self> {
        let hex = s
            .strip_prefix("sha256:")
            .ok_or_else(|| OciError::InvalidDigest(s.into()))?;
        let valid = hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if valid {
            Ok(Digest(hex.into()))
        } else {
            Err(OciError::InvalidDigest(s.into()))
        }
    }

    pub fn of(bytes: &[u8]) -> Self {
        Digest(encode_hex(&Sha256::digest(bytes)))
    }

    /// Finish a streaming hash.
    pub(crate) fn from_hasher(hasher: Sha256) -> Self {
        Digest(encode_hex(&hasher.finalize()))
    }

    pub fn hex(&self) -> &str {
        &self.0
    }

    /// Check that `bytes` hash to this digest. `what` names the content in errors.
    pub fn verify(&self, bytes: &[u8], what: &str) -> Result<()> {
        let actual = Digest::of(bytes);
        if &actual == self {
            Ok(())
        } else {
            Err(OciError::DigestMismatch {
                what: what.into(),
                expected: self.to_string(),
                actual: actual.to_string(),
            })
        }
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", self.0)
    }
}

pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn hashes_known_vector() {
        assert_eq!(Digest::of(b"").to_string(), EMPTY);
    }

    #[test]
    fn parse_roundtrip_and_rejects_garbage() {
        assert_eq!(Digest::parse(EMPTY).unwrap().to_string(), EMPTY);
        for bad in ["", "sha256:", "sha256:xyz", "md5:abcd", &EMPTY.to_uppercase(), &EMPTY[..EMPTY.len() - 1]] {
            assert!(Digest::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn verify_detects_mismatch() {
        let d = Digest::parse(EMPTY).unwrap();
        assert!(d.verify(b"", "x").is_ok());
        assert!(matches!(d.verify(b"tampered", "x"), Err(OciError::DigestMismatch { .. })));
    }
}
