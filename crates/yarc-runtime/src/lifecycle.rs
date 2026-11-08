//! Running a container and recording what happens: state file, log file.
//! The foreground CLI and (from M3b) the detached shim share this one path.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::OwnedFd;
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

use crate::container::ContainerStore;
use crate::error::{io_err, Result, RuntimeError};
use crate::exec::{spawn_exec, ExecChild};
use crate::health::HealthTracker;
use crate::linux::start;
use crate::logs::LogWriter;
use crate::pump::{pump, pump_with_tick, Tee, Tick};
use crate::spec::{ContainerSpec, HealthSpec};
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
    let mut health = spec.health.clone().map(|h| HealthRunner::new(store, spec, running.pid(), live.clone(), h));
    let first = health.as_ref().map_or_else(|| live.clone(), HealthRunner::snapshot);
    if let Err(err) = store.save_state(&spec.id, &first) {
        // Not fatal: the container is already running and must be waited for.
        eprintln!("warning: cannot record state of {}: {err}", spec.id);
    }
    on_started(Ok(running.pid()));

    let tee = if launch.detached {
        Tee::default()
    } else {
        Tee { stdout: Some(Box::new(io::stdout())), stderr: Some(Box::new(io::stderr())) }
    };
    let mut log = LogWriter::new(log_file);
    let (out_r, err_r) = (OwnedFd::from(out_r), OwnedFd::from(err_r));
    let pumped = match health.as_mut() {
        Some(runner) => {
            let mut run = || runner.tick();
            let tick = Tick { every: HEALTH_TICK, run: &mut run };
            pump_with_tick(out_r, err_r, &mut log, tee, Some(tick))
        }
        None => pump(out_r, err_r, &mut log, tee),
    };
    if let Some(runner) = health.as_mut() {
        runner.shutdown(); // a check still running must not outlive the container
    }
    let code = running.wait();

    store.save_state(&spec.id, &live.exited(code, now_ms()))?;
    if let Err(err) = pumped {
        eprintln!("warning: output of {} was not fully recorded: {err}", spec.id);
    }
    Ok(code)
}

/// How often the supervisor looks at the health checks.
const HEALTH_TICK: Duration = Duration::from_millis(200);

/// Runs a container's health checks from the supervisor's poll loop: no thread,
/// at most one check at a time, never blocking the output pump (a check is
/// started, then polled on later ticks).
struct HealthRunner<'a> {
    store: &'a ContainerStore,
    spec: &'a ContainerSpec,
    init_pid: u32,
    base: ContainerState,
    tracker: HealthTracker,
    check: Option<ExecChild>,
}

impl<'a> HealthRunner<'a> {
    fn new(store: &'a ContainerStore, spec: &'a ContainerSpec, init_pid: u32, base: ContainerState, health: HealthSpec) -> Self {
        HealthRunner { store, spec, init_pid, base, tracker: HealthTracker::new(health, now_ms()), check: None }
    }

    /// The state to record: the running state plus the current health.
    fn snapshot(&self) -> ContainerState {
        self.base.with_health(Some(self.tracker.health().clone()))
    }

    fn tick(&mut self) {
        let now = now_ms();
        let mut changed = false;
        if let Some(check) = self.check.as_mut() {
            if let Some(code) = check.try_wait() {
                self.check = None;
                changed = self.tracker.finished(now, Some(code));
            } else if self.tracker.timed_out(now) {
                check.kill();
                if let Some(check) = self.check.take() {
                    check.wait();
                }
                changed = self.tracker.finished(now, None);
            }
        } else if self.tracker.due(now) {
            match spawn_exec(self.spec, self.init_pid, &self.tracker.spec().cmd, &[], None, quiet_stdio()) {
                Ok(check) => {
                    self.tracker.started_check(now);
                    self.check = Some(check);
                }
                Err(RuntimeError::Start { code, .. }) => changed = self.tracker.finished(now, Some(code)),
                Err(_) => changed = self.tracker.finished(now, None),
            }
        }
        if changed {
            if let Err(err) = self.store.save_state(&self.spec.id, &self.snapshot()) {
                eprintln!("warning: cannot record health of {}: {err}", self.spec.id);
            }
        }
    }

    fn shutdown(&mut self) {
        if let Some(check) = self.check.take() {
            check.kill();
            check.wait();
        }
    }
}

/// stdin from, stdout/stderr to `/dev/null` (a check's output is not recorded);
/// falls back to inheriting if that cannot be opened.
fn quiet_stdio() -> crate::linux::Stdio {
    let open = |write: bool| File::options().read(!write).write(write).open("/dev/null").ok().map(OwnedFd::from);
    crate::linux::Stdio { stdin: open(false), stdout: open(true), stderr: open(true) }
}

/// How long a SIGKILL'd container may take to disappear before we give up.
const KILL_GRACE: Duration = Duration::from_secs(10);

/// Host pid of the init of a running container. A recorded pid is only trusted
/// while the supervisor lock is held, so a recycled pid is never used.
fn running_pid(store: &ContainerStore, id: &str) -> Result<u32> {
    store.load(id)?; // unknown id → "no such container"
    match store.load_state(id)? {
        Some(state) if state.is_running(store.is_supervised(id)?) => state.pid,
        _ => None,
    }
    .ok_or_else(|| RuntimeError::NotRunning(id.to_string()))
}

/// Runs `args` inside a running container, with its environment plus `env`,
/// attached to yarc's stdin/stdout/stderr; returns the exit status.
pub fn exec_container(
    store: &ContainerStore,
    id: &str,
    args: &[String],
    env: &[(String, String)],
    workdir: Option<&str>,
) -> Result<i32> {
    let spec = store.load(id)?;
    let pid = running_pid(store, id)?;
    let stdio = crate::linux::Stdio { stdin: None, stdout: None, stderr: None };
    Ok(crate::exec::spawn_exec(&spec, pid, args, env, workdir, stdio)?.wait())
}

/// Sends `sig` (a signal number) to the container's init process. Signals from
/// the host reach PID 1 of the container only if it has a handler for them;
/// SIGKILL and SIGSTOP always do.
pub fn signal_container(store: &ContainerStore, id: &str, sig: i32) -> Result<()> {
    let pid = running_pid(store, id)?;
    let signal = Signal::try_from(sig).map_err(|_| RuntimeError::Unsupported(format!("invalid signal number {sig}")))?;
    match kill(Pid::from_raw(pid as i32), signal) {
        Ok(()) => Ok(()),
        Err(Errno::ESRCH) => Err(RuntimeError::NotRunning(id.to_string())), // exited just now
        Err(e) => Err(RuntimeError::Io { context: format!("cannot signal container {id}"), source: e.into() }),
    }
}

/// Stops a container: SIGTERM, then SIGKILL if it is still there after
/// `timeout`. Stopping a container that is not running succeeds.
pub fn stop_container(store: &ContainerStore, id: &str, timeout: Duration) -> Result<()> {
    match signal_container(store, id, Signal::SIGTERM as i32) {
        Ok(()) => {}
        Err(RuntimeError::NotRunning(_)) => return Ok(()),
        Err(e) => return Err(e),
    }
    if store.wait_exit(id, Some(timeout))? {
        return Ok(());
    }
    match signal_container(store, id, Signal::SIGKILL as i32) {
        Ok(()) | Err(RuntimeError::NotRunning(_)) => {}
        Err(e) => return Err(e),
    }
    if store.wait_exit(id, Some(KILL_GRACE))? {
        Ok(())
    } else {
        Err(RuntimeError::Unsupported(format!("container {id} did not exit after SIGKILL")))
    }
}

/// Removes a container and its root filesystem. A running container is
/// refused unless `force`, which kills it first. The supervisor lock is held
/// during removal so nobody can start the container meanwhile.
pub fn remove_container(store: &ContainerStore, id: &str, force: bool) -> Result<()> {
    store.load(id)?;
    if store.is_supervised(id)? {
        if !force {
            return Err(RuntimeError::StillRunning(id.to_string()));
        }
        match signal_container(store, id, Signal::SIGKILL as i32) {
            Ok(()) | Err(RuntimeError::NotRunning(_)) => {}
            Err(e) => return Err(e),
        }
        if !store.wait_exit(id, Some(KILL_GRACE))? {
            return Err(RuntimeError::Unsupported(format!("container {id} did not exit after SIGKILL")));
        }
    }
    let Some(_lock) = store.try_acquire(id)? else {
        return Err(RuntimeError::StillRunning(id.to_string()));
    };
    store.remove(id)
}
