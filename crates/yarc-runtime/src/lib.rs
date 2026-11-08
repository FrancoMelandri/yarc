//! Container runtime abstraction.
//!
//! The in-house implementation (namespaces, cgroups v2, overlayfs, pivot_root)
//! lives behind [`Runtime`] so it can later be compared against / swapped for
//! youki's `libcontainer`.
//!
//! This crate also owns the *description* of a container ([`ContainerSpec`],
//! built by [`resolve_process`]) and its on-disk record ([`ContainerStore`]).

mod container;
mod error;
mod spec;

pub use container::ContainerStore;
pub use error::{Result, RuntimeError};
pub use spec::{resolve_process, ContainerSpec, ImageDefaults, Network, Overrides, Process, DEFAULT_PATH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Created,
    Running,
    Exited(i32),
}

pub trait Runtime {
    type Error;

    fn create(&mut self, spec: &ContainerSpec) -> std::result::Result<(), Self::Error>;
    fn start(&mut self, id: &str) -> std::result::Result<(), Self::Error>;
    fn state(&self, id: &str) -> std::result::Result<State, Self::Error>;
    fn kill(&mut self, id: &str, signal: i32) -> std::result::Result<(), Self::Error>;
    fn delete(&mut self, id: &str) -> std::result::Result<(), Self::Error>;
}
