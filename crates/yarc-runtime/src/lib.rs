//! Container runtime abstraction.
//!
//! The in-house implementation (namespaces, cgroups v2, overlayfs, pivot_root)
//! lives behind [`Runtime`] so it can later be compared against / swapped for
//! youki's `libcontainer`.
//!
//! This crate also owns the *description* of a container ([`ContainerSpec`],
//! built by [`resolve_process`]) and its on-disk record ([`ContainerStore`]).
//! [`run`] starts one container in the foreground (Linux only); the
//! create/start/kill/delete [`Runtime`] trait is for the detached model of M3.

mod container;
mod error;
#[cfg(target_os = "linux")]
mod linux;
mod paths;
mod plan;
mod spec;

pub use container::ContainerStore;
pub use error::{Result, RuntimeError};
pub use plan::{check_user, mount_plan, BindKind, MountOpts, SetupOp};
pub use spec::{resolve_process, ContainerSpec, ImageDefaults, Network, Overrides, Process, DEFAULT_PATH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunOptions {
    /// Report optional setup steps that were skipped.
    pub debug: bool,
}

/// Runs the container in the foreground and returns its exit status
/// (`128 + n` if it was killed by signal `n`).
#[cfg(target_os = "linux")]
pub fn run(spec: &ContainerSpec, opts: &RunOptions) -> Result<i32> {
    linux::run(spec, opts)
}

#[cfg(not(target_os = "linux"))]
pub fn run(_spec: &ContainerSpec, _opts: &RunOptions) -> Result<i32> {
    Err(RuntimeError::Unsupported("running containers needs Linux; run yarc inside WSL".into()))
}

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
