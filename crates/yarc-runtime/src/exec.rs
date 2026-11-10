//! Running an extra process inside a running container (`yarc exec`, health
//! checks): join its namespaces, enter its root, exec.
//!
//! ```text
//! caller (yarc / shim)
//!  └─ helper        setns(user, mnt, uts, ipc, [net,] pid)
//!      └─ process   fchdir + chroot into the container's root, chdir, execve
//! ```
//!
//! Joining a PID namespace only affects *children*, hence the second fork. The
//! helper must be single-threaded for `setns(CLONE_NEWUSER)`, which a fresh
//! fork of our single-threaded supervisor is. The user namespace is joined
//! first: being the owner of it (same uid as the container's creator) is what
//! grants the capabilities needed for the other `setns` calls, and the
//! container's uid mapping then applies (we become root inside, as `init` is).
//! Progress and errors travel over a close-on-exec pipe exactly as in `linux.rs`.

use std::convert::Infallible;
use std::fs::File;
use std::io::BufReader;
use std::os::fd::AsRawFd;

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::sched::{setns, CloneFlags};
use nix::sys::prctl;
use nix::sys::signal::{kill, signal, SigHandler, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{chdir, chroot, fchdir, fork, pipe2, ForkResult, Pid};

use crate::error::{Result, RuntimeError};
use crate::linux::{die, exec_program, exit, parse_error, read_line, redirect, wait_code, Fail, Stdio, SETUP_FAILED};
use crate::spec::{ContainerSpec, Network};

/// A process started inside a container.
pub struct ExecChild {
    pid: Pid,
}

impl ExecChild {
    /// The exit status if the process has finished (`128 + n` for signal `n`).
    pub fn try_wait(&mut self) -> Option<i32> {
        match waitpid(self.pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(_, code)) => Some(code),
            Ok(WaitStatus::Signaled(_, sig, _)) => Some(128 + sig as i32),
            Ok(WaitStatus::StillAlive) => None,
            Ok(_) => None,
            Err(_) => Some(255), // already reaped or never ours
        }
    }

    pub fn wait(self) -> i32 {
        wait_code(self.pid)
    }

    /// Kills the helper; the process inside dies with it (parent-death signal).
    pub fn kill(&self) {
        let _ = kill(self.pid, Signal::SIGKILL);
    }
}

/// Starts `args` inside the container whose init has host pid `init_pid`, with
/// the container's environment plus `extra_env`, in `workdir` (default: the
/// container's). Returns once the program is running or failed to start.
pub(crate) fn spawn_exec(
    spec: &ContainerSpec,
    init_pid: u32,
    args: &[String],
    extra_env: &[(String, String)],
    workdir: Option<&str>,
    stdio: Stdio,
) -> Result<ExecChild> {
    if args.is_empty() {
        return Err(RuntimeError::NoCommand);
    }
    let (up_r, up_w) = pipe2(OFlag::O_CLOEXEC).map_err(|e| RuntimeError::start(format!("pipe: {e}")))?;

    // SAFETY: the child only joins namespaces and execs (or `_exit`s); it never
    // returns into the caller. The caller is single-threaded (supervisor loop).
    match unsafe { fork() }.map_err(|e| RuntimeError::start(format!("fork: {e}")))? {
        ForkResult::Child => {
            drop(up_r);
            helper(spec, init_pid, args, extra_env, workdir, File::from(up_w), stdio)
        }
        ForkResult::Parent { child } => {
            drop(up_w);
            drop(stdio);
            let mut lines = BufReader::new(File::from(up_r));
            let mut failure = None;
            while let Some(line) = read_line(&mut lines) {
                if line.starts_with("error:") {
                    failure = Some(parse_error(&line));
                }
            }
            match failure {
                Some((code, msg)) => {
                    wait_code(child);
                    Err(RuntimeError::Start { msg, code })
                }
                None => Ok(ExecChild { pid: child }),
            }
        }
    }
}

fn helper(
    spec: &ContainerSpec,
    init_pid: u32,
    args: &[String],
    extra_env: &[(String, String)],
    workdir: Option<&str>,
    mut up: File,
    stdio: Stdio,
) -> ! {
    let _ = prctl::set_pdeathsig(Signal::SIGKILL);
    let root = match join_namespaces(spec, init_pid) {
        Ok(root) => root,
        Err(msg) => die(&mut up, SETUP_FAILED, &msg),
    };
    // SAFETY: see `spawn_exec`.
    match unsafe { fork() } {
        Err(e) => die(&mut up, SETUP_FAILED, &format!("fork: {e}")),
        Ok(ForkResult::Child) => process(spec, root, args, extra_env, workdir, up, stdio),
        Ok(ForkResult::Parent { child }) => {
            drop(up); // EOF on the pipe means "exec happened"
            drop(stdio);
            exit(wait_code(child))
        }
    }
}

/// Opens everything under `/proc/<pid>` first (afterwards `/proc` is not ours
/// any more), then joins the namespaces. Returns the container's root directory.
fn join_namespaces(spec: &ContainerSpec, pid: u32) -> std::result::Result<File, String> {
    let open = |what: &str, path: String| {
        File::open(&path).map_err(|e| format!("cannot open {what} of container process {pid}: {e}"))
    };

    let root = open("root directory", format!("/proc/{pid}/root"))?;
    let mut wanted = vec![
        ("user", CloneFlags::CLONE_NEWUSER),
        ("mnt", CloneFlags::CLONE_NEWNS),
        ("uts", CloneFlags::CLONE_NEWUTS),
        ("ipc", CloneFlags::CLONE_NEWIPC),
    ];
    if spec.network != Network::Host {
        wanted.push(("net", CloneFlags::CLONE_NEWNET));
    }
    wanted.push(("pid", CloneFlags::CLONE_NEWPID));

    let mut namespaces = Vec::new();
    for (name, flag) in wanted {
        namespaces.push((name, flag, open(&format!("{name} namespace"), format!("/proc/{pid}/ns/{name}"))?));
    }
    for (name, flag, file) in &namespaces {
        setns(file, *flag).map_err(|e| format!("joining the {name} namespace: {e}"))?;
    }
    Ok(root)
}

fn process(
    spec: &ContainerSpec,
    root: File,
    args: &[String],
    extra_env: &[(String, String)],
    workdir: Option<&str>,
    mut up: File,
    stdio: Stdio,
) -> ! {
    // Rust starts programs with SIGPIPE ignored; the command must not inherit that.
    // SAFETY: restoring the default disposition.
    let _ = unsafe { signal(Signal::SIGPIPE, SigHandler::SigDfl) };
    let _ = prctl::set_pdeathsig(Signal::SIGKILL);
    if let Err(msg) = redirect(stdio) {
        die(&mut up, SETUP_FAILED, &msg);
    }
    match enter_and_exec(spec, root, args, extra_env, workdir) {
        Err(fail) => die(&mut up, fail.code, &fail.msg),
        Ok(never) => match never {},
    }
}

fn enter_and_exec(
    spec: &ContainerSpec,
    root: File,
    args: &[String],
    extra_env: &[(String, String)],
    workdir: Option<&str>,
) -> std::result::Result<Infallible, Fail> {
    let ctx = |what: &'static str| move |e: Errno| format!("{what}: {e}");
    // Our root is still the host's: switch to the container's.
    fchdir(root.as_raw_fd()).map_err(ctx("entering the container's root"))?;
    chroot(".").map_err(ctx("chroot"))?;
    drop(root);
    let dir = workdir.unwrap_or(&spec.working_dir);
    if chdir(dir).is_err() {
        chdir("/").map_err(ctx("chdir /"))?;
    }

    let mut env = spec.env.clone();
    for (k, v) in extra_env {
        match env.iter_mut().find(|(ek, _)| ek == k) {
            Some(slot) => slot.1 = v.clone(),
            None => env.push((k.clone(), v.clone())),
        }
    }
    exec_program(args, &env)
}
