//! Running a container and recording what happens: state file, log file.
//! The foreground CLI and (from M3b) the detached shim share this one path.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

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
use crate::restart::{Backoff, Decision, RestartTracker, RunEnd};
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
    let ended = run_once(store, spec, launch, 0, on_started)?;
    store.save_state(&spec.id, &ended.live.exited(ended.code, now_ms()))?;
    Ok(ended.code)
}

/// One run of the container, as seen by its supervisor.
struct Ended {
    code: i32,
    /// The state recorded while it ran; the caller decides what comes next (exited, or
    /// waiting to be restarted) and records that.
    live: ContainerState,
}

/// Starts the container and waits for it, recording its output and `running` state (with
/// `restarts` restarts so far). The caller must hold the container's lock. A start failure
/// is recorded as an exit here (and reported through `on_started`) because the CLI reads it
/// as soon as it has been told.
fn run_once(
    store: &ContainerStore,
    spec: &ContainerSpec,
    launch: &Launch,
    restarts: u32,
    on_started: impl FnOnce(std::result::Result<u32, &RuntimeError>),
) -> Result<Ended> {
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
    let starting = ContainerState::running(me, None, started).with_restarts(restarts);
    store.save_state(&spec.id, &starting)?;

    let opts = StartOptions {
        debug: launch.debug,
        stdin,
        stdout: Some(OwnedFd::from(out_w)),
        stderr: Some(OwnedFd::from(err_w)),
    };
    let oom_before = if spec.resources.memory.is_some() { crate::limits::oom_kills() } else { None };
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

    if spec.resources.memory.is_some() && crate::limits::in_scope() {
        crate::limits::prefer_as_oom_victim(running.pid());
    }
    let live = ContainerState::running(me, Some(running.pid()), started).with_restarts(restarts);
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

    if let Err(err) = pumped {
        eprintln!("warning: output of {} was not fully recorded: {err}", spec.id);
    }
    // The scope's counter only grows (it spans every run of a restarted container).
    let oom_killed = matches!((oom_before, crate::limits::oom_kills()), (Some(before), Some(after)) if after > before);
    Ok(Ended { code, live: live.with_oom_killed(oom_killed) })
}

/// True when the container should not be run again: a person asked for it to stop, or this
/// process was told to terminate.
fn stop_wanted(store: &ContainerStore, id: &str) -> bool {
    store.stop_requested(id) || crate::linux::termination_received()
}

/// How often a waiting supervisor looks for a stop request.
const STOP_POLL: Duration = Duration::from_millis(100);

/// Waits `after` before the next run. Returns `false` when it should not happen: a stop was
/// requested, or the container was removed meanwhile.
fn wait_to_restart(store: &ContainerStore, id: &str, after: Duration) -> bool {
    let deadline = Instant::now() + after;
    loop {
        if stop_wanted(store, id) || !store.config_path(id).exists() {
            return false;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(STOP_POLL));
    }
}

/// Runs the container under its restart policy until the policy says to stop; returns the
/// last exit status. The container's lock is taken once and held throughout, so it never
/// looks stopped between runs. With `RestartPolicy::No` this is a single [`run_once`].
///
/// `on_started` reports the first start only. A start failure on the first run is an error
/// (no restart loop on a typo); on later runs it counts as a failed run.
pub fn supervise(
    store: &ContainerStore,
    spec: &ContainerSpec,
    launch: &Launch,
    backoff: Backoff,
    on_started: impl FnOnce(std::result::Result<u32, &RuntimeError>),
) -> Result<i32> {
    let id = spec.id.as_str();
    let Some(_lock) = store.try_acquire(id)? else {
        return Err(RuntimeError::AlreadyRunning(id.to_string()));
    };
    store.clear_stop_request(id); // left over from an earlier life
    crate::linux::reset_received();
    // Keep the termination handlers installed between runs too: a signal then must count
    // as a stop, not kill the supervisor with the container's state half written.
    let _signals = crate::linux::SignalGuard::install();

    let mut tracker = RestartTracker::new(spec.restart, backoff);
    let mut report = Some(on_started);
    let mut performed = 0u32;
    loop {
        let began = Instant::now();
        let first = report.take();
        let ended = run_once(store, spec, launch, performed, |started| {
            if let Ok(pid) = started {
                // A stop that arrived after the last check but before this start: pass it on.
                if stop_wanted(store, id) {
                    let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
                }
            }
            if let Some(report) = first {
                report(started);
            }
        });
        let (code, base) = match ended {
            Ok(e) => (e.code, e.live),
            Err(RuntimeError::Start { code, msg }) if performed > 0 => {
                eprintln!("restart of {id} failed: {msg}");
                let base = store
                    .load_state(id)?
                    .unwrap_or_else(|| ContainerState::running(std::process::id(), None, now_ms()));
                (code, base.with_restarts(performed))
            }
            Err(e) => return Err(e),
        };

        let end = RunEnd { exit_code: code, ran_for: began.elapsed(), stop_requested: stop_wanted(store, id) };
        let after = match tracker.on_exit(end) {
            Decision::Stop => {
                store.clear_stop_request(id);
                store.save_state(id, &base.exited(code, now_ms()))?;
                return Ok(code);
            }
            Decision::Restart { after } => after,
        };

        let now = now_ms();
        eprintln!("restart {} of {id} after exit {code}, in {after:?}", performed + 1);
        store.save_state(id, &base.restarting(code, now, now + after.as_millis() as u64))?;
        if !wait_to_restart(store, id, after) {
            store.clear_stop_request(id);
            if store.config_path(id).exists() {
                store.save_state(id, &base.exited(code, now_ms()))?;
            }
            return Ok(code);
        }
        performed += 1;
    }
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
    fn new(
        store: &'a ContainerStore,
        spec: &'a ContainerSpec,
        init_pid: u32,
        base: ContainerState,
        health: HealthSpec,
    ) -> Self {
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
///
/// SIGTERM and SIGKILL also record a stop request first, so a supervisor with a restart
/// policy lets the container stay down (`stop`, `rm -f` and `kill` all end up here).
pub fn signal_container(store: &ContainerStore, id: &str, sig: i32) -> Result<()> {
    if sig == Signal::SIGTERM as i32 || sig == Signal::SIGKILL as i32 {
        store.request_stop(id)?;
    }
    let pid = running_pid(store, id)?;
    let signal =
        Signal::try_from(sig).map_err(|_| RuntimeError::Unsupported(format!("invalid signal number {sig}")))?;
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
        // Not running: either gone, or between two runs (waiting to restart), when there is
        // no process to signal and the stop request alone ends it. Wait covers both.
        Err(RuntimeError::NotRunning(_)) => {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Network, Process};

    fn store_with(id: &str) -> (tempfile::TempDir, ContainerStore) {
        let root = tempfile::tempdir().unwrap();
        let store = ContainerStore::open(root.path()).unwrap();
        store.allocate(Some(id)).unwrap();
        let p = Process { args: vec!["x".into()], env: vec![], working_dir: "/".into(), user: None };
        store.save(&ContainerSpec::new(id, "img", store.rootfs_path(id), p, Network::Host)).unwrap();
        (root, store)
    }

    #[test]
    fn the_wait_before_a_restart_runs_its_course() {
        let (_r, store) = store_with("c");
        let t = Instant::now();
        assert!(wait_to_restart(&store, "c", Duration::from_millis(150)));
        assert!(t.elapsed() >= Duration::from_millis(150));
        assert!(wait_to_restart(&store, "c", Duration::ZERO));
    }

    #[test]
    fn a_stop_request_ends_the_wait_at_once() {
        let (_r, store) = store_with("c");
        let _lock = store.try_acquire("c").unwrap().unwrap(); // supervised, as in real life
        store.request_stop("c").unwrap();
        let t = Instant::now();
        assert!(!wait_to_restart(&store, "c", Duration::from_secs(60)));
        assert!(t.elapsed() < Duration::from_secs(1));

        // ... also when it arrives while waiting
        store.clear_stop_request("c");
        let s2 = store.clone();
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            s2.request_stop("c").unwrap();
        });
        let t = Instant::now();
        assert!(!wait_to_restart(&store, "c", Duration::from_secs(60)));
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        setter.join().unwrap();
    }

    #[test]
    fn removing_the_container_ends_the_wait() {
        let (_r, store) = store_with("c");
        let s2 = store.clone();
        let remover = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            s2.remove("c").unwrap();
        });
        let t = Instant::now();
        assert!(!wait_to_restart(&store, "c", Duration::from_secs(60)));
        assert!(t.elapsed() < Duration::from_secs(2));
        remover.join().unwrap();
    }

    #[test]
    fn only_term_and_kill_record_a_stop_request() {
        let (_r, store) = store_with("c");
        let _lock = store.try_acquire("c").unwrap().unwrap();
        // not running (no state): the signal fails, but a request is still recorded for TERM/KILL
        for (sig, recorded) in [(Signal::SIGUSR1, false), (Signal::SIGHUP, false), (Signal::SIGTERM, true)] {
            store.clear_stop_request("c");
            assert!(matches!(signal_container(&store, "c", sig as i32), Err(RuntimeError::NotRunning(_))));
            assert_eq!(store.stop_requested("c"), recorded, "{sig:?}");
        }
        store.clear_stop_request("c");
        let _ = signal_container(&store, "c", Signal::SIGKILL as i32);
        assert!(store.stop_requested("c"));
    }

    #[test]
    fn stopping_a_container_nobody_supervises_succeeds_and_leaves_no_request() {
        let (_r, store) = store_with("c");
        stop_container(&store, "c", Duration::from_secs(1)).unwrap();
        assert!(!store.stop_requested("c"));
        assert!(matches!(stop_container(&store, "nope", Duration::from_secs(1)), Err(RuntimeError::NotFound(_))));
    }

    #[test]
    fn stopping_a_container_between_two_runs_waits_for_the_supervisor_to_leave() {
        let (_r, store) = store_with("c");
        let lock = store.try_acquire("c").unwrap().unwrap(); // the supervisor, waiting to restart
        let s2 = store.clone();
        let supervisor = std::thread::spawn(move || {
            // what `supervise` does: notice the request, give up the lock
            assert!(!wait_to_restart(&s2, "c", Duration::from_secs(60)));
            drop(lock);
        });
        let t = Instant::now();
        stop_container(&store, "c", Duration::from_secs(5)).unwrap();
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        assert!(!store.is_supervised("c").unwrap());
        supervisor.join().unwrap();
    }
}
