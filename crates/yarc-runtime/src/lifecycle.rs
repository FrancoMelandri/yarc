//! Running a container and recording what happens: state file, log file.
//! The foreground CLI and (from M3b) the detached shim share this one path.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::OwnedFd;

use crate::container::ContainerStore;
use crate::error::{io_err, Result, RuntimeError};
use crate::linux::start;
use crate::logs::LogWriter;
use crate::pump::{pump, Tee};
use crate::spec::ContainerSpec;
use crate::state::{now_ms, ContainerState};
use crate::StartOptions;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Launch {
    pub debug: bool,
    /// No terminal: stdin is `/dev/null` and output only goes to the log.
    pub detached: bool,
}

/// Runs `spec` to completion, recording `state.json` and appending its output
/// to `container.log`. Unless detached, output is also shown on yarc's own
/// stdout/stderr and stdin is inherited. Returns the container's exit status.
pub fn run_recorded(store: &ContainerStore, spec: &ContainerSpec, launch: &Launch) -> Result<i32> {
    run_recorded_with(store, spec, launch, |_| {})
}

/// Like [`run_recorded`], and calls `on_started` once the outcome of starting
/// is known: `Ok(pid)` of the container's init, or the start error. It is not
/// called when something fails before that (e.g. the container is already
/// running). The detached shim uses it to tell the CLI "it's up".
pub fn run_recorded_with(
    store: &ContainerStore,
    spec: &ContainerSpec,
    launch: &Launch,
    on_started: impl FnOnce(std::result::Result<u32, &RuntimeError>),
) -> Result<i32> {
    // Held until this function returns: "supervised" == "someone holds this".
    let Some(_lock) = store.try_acquire(&spec.id)? else {
        return Err(RuntimeError::AlreadyRunning(spec.id.clone()));
    };

    let log_path = store.log_path(&spec.id);
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(io_err(format!("cannot open {}", log_path.display())))?;
    let stdin = if launch.detached {
        Some(OwnedFd::from(File::open("/dev/null").map_err(io_err("cannot open /dev/null"))?))
    } else {
        None
    };
    let (out_r, out_w) = io::pipe().map_err(io_err("cannot create a pipe"))?;
    let (err_r, err_w) = io::pipe().map_err(io_err("cannot create a pipe"))?;

    let me = std::process::id();
    let started = now_ms();
    let starting = ContainerState::running(me, None, started);
    store.save_state(&spec.id, &starting)?;

    let opts = StartOptions {
        debug: launch.debug,
        stdin,
        stdout: Some(OwnedFd::from(out_w)),
        stderr: Some(OwnedFd::from(err_w)),
    };
    let running = match start(spec, opts) {
        Ok(running) => running,
        Err(err) => {
            let code = match &err {
                RuntimeError::Start { code, .. } => *code,
                _ => 125,
            };
            let _ = store.save_state(&spec.id, &starting.exited(code, now_ms()));
            on_started(Err(&err));
            return Err(err);
        }
    };

    let live = ContainerState::running(me, Some(running.pid()), started);
    if let Err(err) = store.save_state(&spec.id, &live) {
        // Not fatal: the container is already running and must be waited for.
        eprintln!("warning: cannot record state of {}: {err}", spec.id);
    }
    on_started(Ok(running.pid()));

    let tee = if launch.detached {
        Tee::default()
    } else {
        Tee { stdout: Some(Box::new(io::stdout())), stderr: Some(Box::new(io::stderr())) }
    };
    let pumped = pump(OwnedFd::from(out_r), OwnedFd::from(err_r), &mut LogWriter::new(log_file), tee);
    let code = running.wait();

    store.save_state(&spec.id, &live.exited(code, now_ms()))?;
    if let Err(err) = pumped {
        eprintln!("warning: output of {} was not fully recorded: {err}", spec.id);
    }
    Ok(code)
}
