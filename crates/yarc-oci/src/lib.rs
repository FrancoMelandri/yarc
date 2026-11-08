//! OCI registry client, content-addressed blob store, layer unpacking.
//!
//! Design note: everything that does not need the network (reference parsing,
//! challenge parsing, manifest/platform selection, digest checks) is pure and
//! synchronous. HTTP is isolated behind [`Transport`], so an async transport
//! can be added later without touching that logic.

pub mod auth;
pub mod client;
pub mod digest;
pub mod error;
pub mod manifest;
pub mod reference;
pub mod transport;

pub use client::{RegistryClient, ResolvedImage};
pub use digest::Digest;
pub use error::{OciError, Result};
pub use manifest::{ContainerConfig, Descriptor, ImageConfig, ImageManifest, Platform};
pub use reference::{Reference, Target};
pub use transport::{Response, Transport, UreqTransport};
