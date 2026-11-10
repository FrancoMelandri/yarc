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
pub mod load;
pub mod manifest;
pub mod pull;
pub mod reference;
pub mod store;
pub mod transport;
#[cfg(unix)]
pub mod unpack;

#[cfg(test)]
mod testutil;

pub use client::{RegistryClient, ResolvedImage};
pub use digest::Digest;
pub use error::{OciError, Result};
pub use load::{load_archive, load_dir, load_path, LoadReport, LoadedImage};
pub use manifest::{ContainerConfig, Descriptor, ImageConfig, ImageManifest, Platform};
pub use pull::{pull, PullReport};
pub use reference::{Reference, Target};
pub use store::{BlobStore, RemoveReport, StoredImage};
pub use transport::{Response, Transport, UreqTransport};
#[cfg(unix)]
pub use unpack::{unpack_image, unpack_layer, UnpackReport};
