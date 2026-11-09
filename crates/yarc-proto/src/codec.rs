//! Framing: one JSON document per line (`\n`-terminated, never containing a
//! raw newline), so a reader needs nothing but `read_line`.

use std::fmt;
use std::io::{self, BufRead, Read, Write};

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::message::PROTOCOL_VERSION;

/// Longest frame accepted; a bigger line is a protocol error, not an allocation.
pub const MAX_FRAME_BYTES: usize = 1 << 20;

#[derive(Debug)]
pub enum ProtoError {
    Io(io::Error),
    /// The line is not a valid frame.
    Malformed(String),
    TooLarge,
    VersionMismatch { ours: u32, theirs: u32 },
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::Io(e) => write!(f, "i/o error: {e}"),
            ProtoError::Malformed(m) => write!(f, "malformed frame: {m}"),
            ProtoError::TooLarge => write!(f, "frame larger than {MAX_FRAME_BYTES} bytes"),
            ProtoError::VersionMismatch { ours, theirs } => {
                write!(f, "protocol version mismatch: this side speaks {ours}, the other {theirs}")
            }
        }
    }
}

impl std::error::Error for ProtoError {}

impl From<io::Error> for ProtoError {
    fn from(e: io::Error) -> Self {
        ProtoError::Io(e)
    }
}

/// Writes one frame and flushes it.
pub fn write_frame<W: Write, T: Serialize>(out: &mut W, frame: &T) -> Result<(), ProtoError> {
    let mut line = serde_json::to_vec(frame).map_err(|e| ProtoError::Malformed(e.to_string()))?;
    if line.len() >= MAX_FRAME_BYTES {
        return Err(ProtoError::TooLarge);
    }
    line.push(b'\n');
    out.write_all(&line)?;
    out.flush()?;
    Ok(())
}

/// Reads one frame; `None` at a clean end of input. Blank lines are skipped.
pub fn read_frame<R: BufRead, T: DeserializeOwned>(input: &mut R) -> Result<Option<T>, ProtoError> {
    loop {
        let mut line = Vec::new();
        // take() bounds the allocation for a peer that never sends a newline
        let n = input.by_ref().take(MAX_FRAME_BYTES as u64 + 1).read_until(b'\n', &mut line)?;
        if n == 0 {
            return Ok(None);
        }
        if line.len() > MAX_FRAME_BYTES {
            return Err(ProtoError::TooLarge);
        }
        let text = String::from_utf8(line).map_err(|e| ProtoError::Malformed(e.to_string()))?;
        if text.trim().is_empty() {
            continue;
        }
        return serde_json::from_str(text.trim_end()).map(Some).map_err(|e| ProtoError::Malformed(e.to_string()));
    }
}

/// The versions must be equal: there is no negotiation yet.
pub fn check_version(theirs: u32) -> Result<(), ProtoError> {
    if theirs == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ProtoError::VersionMismatch { ours: PROTOCOL_VERSION, theirs })
    }
}
