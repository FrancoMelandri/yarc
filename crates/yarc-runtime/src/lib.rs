//! Container runtime abstraction.
//!
//! The in-house implementation (namespaces, cgroups v2, overlayfs, pivot_root)
//! lives behind [`Runtime`] so it can later be compared against / swapped for
//! youki's `libcontainer`.
//!
//! This crate also owns the *description* of a container ([`ContainerSpec`],
//! built by [`resolve_process`]), its on-disk record ([`ContainerStore`]:
//! config, state, log) and, on Linux, [`start`] / [`run_recorded`] to run one.
//! The create/start/kill/delete [`Runtime`] trait is for the detached model.

mod container;
mod error;
#[cfg(target_os = "linux")]
mod exec;
mod health;
#[cfg(target_os = "linux")]
mod lifecycle;
#[cfg(target_os = "linux")]
mod linux;
mod logs;
mod network;
#[cfg(target_os = "linux")]
mod netns;
mod paths;
mod plan;
#[cfg(target_os = "linux")]
mod pump;
mod spec;
mod state;

use std::os::fd::OwnedFd;

pub use container::ContainerStore;
pub use error::{Result, RuntimeError};
#[cfg(target_os = "linux")]
pub use lifecycle::{exec_container, remove_container, run_recorded, run_recorded_with, signal_container, stop_container, Launch};
#[cfg(target_os = "linux")]
pub use linux::{process_alive, start, Running};
pub use logs::{format_timestamp, read_log, read_log_from, LogEntry, LogWriter, Stream};
pub use network::{NetInfo, PortMap, ProjectNet};
#[cfg(target_os = "linux")]
pub use netns::{run_holder, stop_holder};
pub use plan::{check_user, mount_plan, BindKind, MountOpts, SetupOp};
pub use spec::{resolve_process, ContainerSpec, HealthSpec, ImageDefaults, Network, Overrides, Process, DEFAULT_PATH};
pub use state::{describe, now_ms, ContainerState, Health, HealthStatus, Status};

/// How to start a container. The descriptors become the container's stdin,
/// stdout and stderr (`None`: inherit yarc's); [`start`] closes yarc's copies.
#[derive(Debug, Default)]
pub struct StartOptions {
    /// Report optional setup steps that were skipped.
    pub debug: bool,
    pub stdin: Option<OwnedFd>,
    pub stdout: Option<OwnedFd>,
    pub stderr: Option<OwnedFd>,
}

#[cfg(not(target_os = "linux"))]
pub fn process_alive(_pid: u32) -> bool {
    false
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
