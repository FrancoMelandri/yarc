//! Container runtime abstraction.
//!
//! The in-house implementation (namespaces, cgroups v2, overlayfs, pivot_root)
//! lives behind [`Runtime`] so it can later be compared against / swapped for
//! youki's `libcontainer`.

use std::path::PathBuf;

/// Everything needed to start one container.
#[derive(Debug, Clone)]
pub struct ContainerSpec {
    pub id: String,
    pub rootfs: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Created,
    Running,
    Exited(i32),
}

pub trait Runtime {
    type Error;

    fn create(&mut self, spec: &ContainerSpec) -> Result<(), Self::Error>;
    fn start(&mut self, id: &str) -> Result<(), Self::Error>;
    fn state(&self, id: &str) -> Result<State, Self::Error>;
    fn kill(&mut self, id: &str, signal: i32) -> Result<(), Self::Error>;
    fn delete(&mut self, id: &str) -> Result<(), Self::Error>;
}
