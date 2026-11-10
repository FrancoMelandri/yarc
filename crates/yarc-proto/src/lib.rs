//! Request/response types shared by the CLI and the agent.
//!
//! A client (the CLI on another OS) and the agent (`yarc agent --stdio` on
//! Linux) exchange newline-delimited JSON frames over a pipe: see `message`
//! for the frames and `codec` for the framing. This crate does no I/O of its
//! own besides reading and writing those frames.

mod codec;
mod message;

pub use codec::{check_version, read_frame, write_frame, ProtoError, MAX_FRAME_BYTES};
pub use message::{AgentFrame, ClientFrame, Command, CreateSpec, Event, Globals, RequestId, PROTOCOL_VERSION};

#[cfg(test)]
mod tests;
