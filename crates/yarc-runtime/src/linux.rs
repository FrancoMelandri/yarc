//! Starting a container on Linux, rootless: user + mount + pid + uts + ipc
//! (+ net) namespaces, `pivot_root`, `execve`.
//!
//! Process layout:
//!
//! ```text
//! yarc (supervisor)      writes uid/gid maps, forwards signals, returns the exit code
//!  └─ monitor            unshare(...), then forks; relays the init's exit status
//!      └─ init (PID 1)   mounts, pivot_root, then execve()s the container command
//! ```
//!
//! The monitor exists because `unshare(CLONE_NEWPID)` only affects *children*:
//! the first child created afterwards becomes PID 1 of the new namespace.
//! A line protocol over a pipe (`unshared`, `pid:<n>`, `error:<msg>`) carries
//! progress and errors from the two children to the supervisor; the pipe is
//! close-on-exec, so EOF means "the command was exec'd".

use std::convert::Infallible;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::sys::prctl;
use nix::sys::signal::{sigaction, signal, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{chdir, execve, fork, getegid, geteuid, pipe2, pivot_root, sethostname, ForkResult, Pid};

use crate::error::{Result, RuntimeError};
use crate::paths::{resolve_executable, safe_join, safe_join_file};
use crate::plan::{check_user, mount_plan, BindKind, MountOpts, SetupOp};
use crate::spec::{ContainerSpec, Network};
use crate::RunOptions;

const FORWARDED: [Signal; 4] = [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP, Signal::SIGQUIT];
/// Exit status of the helper processes when container setup fails.
const SETUP_FAILED: i32 = 125;

/// Runs the container in the foreground and returns its exit status
/// (`128 + n` when it was killed by signal `n`).
pub fn run(spec: &ContainerSpec, opts: &RunOptions) -> Result<i32> {
    check_user(spec.user.as_deref())?;
    if spec.args.is_empty() {
        return Err(RuntimeError::NoCommand);
    }
    let plan = mount_plan(spec);
    let (up_r, up_w) = pipe2(OFlag::O_CLOEXEC).map_err(sys("pipe"))?;
    let (down_r, down_w) = pipe2(OFlag::O_CLOEXEC).map_err(sys("pipe"))?;
    let (uid, gid) = (geteuid().as_raw(), getegid().as_raw());

    // SAFETY: the children only make syscalls and the few allocations needed to
    // set up the container, then exec or `_exit`; they never return into the
    // caller's code. yarc has no other threads alive at this point.
    match unsafe { fork() }.map_err(sys("fork"))? {
        ForkResult::Child => {
            drop(up_r);
            drop(down_w);
            monitor(spec, &plan, opts, File::from(up_w), File::from(down_r))
        }
        ForkResult::Parent { child } => {
            drop(up_w);
            drop(down_r);
            supervise(child, uid, gid, File::from(up_r), File::from(down_w))
        }
    }
}

fn sys(what: &'static str) -> impl Fn(Errno) -> RuntimeError {
    move |e| RuntimeError::Start(format!("{what}: {e}"))
}

// ---------------------------------------------------------------- supervisor

static TARGET: AtomicI32 = AtomicI32::new(0);
static SIGNALS: AtomicU32 = AtomicU32::new(0);

/// Forwards termination signals to the container. PID 1 of a namespace ignores
/// signals it has no handler for, so a second one escalates to SIGKILL
/// (otherwise Ctrl-C could not stop e.g. `sleep`).
extern "C" fn forward(sig: i32) {
    let target = TARGET.load(Ordering::SeqCst);
    if target <= 0 {
        return;
    }
    let sig = if SIGNALS.fetch_add(1, Ordering::SeqCst) >= 1 { nix::libc::SIGKILL } else { sig };
    // SAFETY: kill(2) is async-signal-safe.
    unsafe { nix::libc::kill(target, sig) };
}

/// Installs the forwarding handlers; restores the default ones on drop.
struct SignalGuard(Vec<(Signal, SigAction)>);

impl SignalGuard {
    fn install() -> Self {
        SIGNALS.store(0, Ordering::SeqCst);
        let action = SigAction::new(SigHandler::Handler(forward), SaFlags::SA_RESTART, SigSet::empty());
        let old = FORWARDED
            .iter()
            // SAFETY: `forward` only uses atomics and kill(2).
            .filter_map(|&s| unsafe { sigaction(s, &action) }.ok().map(|old| (s, old)))
            .collect();
        SignalGuard(old)
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        TARGET.store(0, Ordering::SeqCst);
        for (sig, old) in &self.0 {
            // SAFETY: restoring the handler that was installed before.
            let _ = unsafe { sigaction(*sig, old) };
        }
    }
}

fn supervise(monitor: Pid, uid: u32, gid: u32, up: File, mut down: File) -> Result<i32> {
    TARGET.store(monitor.as_raw(), Ordering::SeqCst);
    let _signals = SignalGuard::install();
    let mut up = BufReader::new(up);
    let mut failure: Option<String> = None;

    match read_line(&mut up).as_deref() {
        Some("unshared") => {
            let go = write_id_maps(monitor, uid, gid).and_then(|()| down.write_all(b"go\n"));
            if let Err(e) = go {
                failure = Some(format!("cannot set up the user namespace: {e}"));
            }
        }
        Some(line) => failure = Some(error_text(line)),
        None => failure = Some("the container setup process died unexpectedly".into()),
    }
    // Closing `down` without the "go" line tells the monitor to give up.
    drop(down);

    if failure.is_none() {
        while let Some(line) = read_line(&mut up) {
            if let Some(pid) = line.strip_prefix("pid:").and_then(|p| p.parse::<i32>().ok()) {
                TARGET.store(pid, Ordering::SeqCst);
            } else if line.starts_with("error:") {
                failure = Some(error_text(&line));
            }
        }
    }

    let code = wait_code(monitor);
    match failure {
        Some(msg) => Err(RuntimeError::Start(msg)),
        None => Ok(code),
    }
}

fn read_line(r: &mut impl BufRead) -> Option<String> {
    let mut line = String::new();
    match r.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim_end().to_string()),
    }
}

fn error_text(line: &str) -> String {
    line.strip_prefix("error:").unwrap_or(line).to_string()
}

/// Maps the invoking user (and group) to 0 inside the new user namespace.
/// Unprivileged processes may only write a single-entry map for themselves, and
/// only after `setgroups` is denied.
fn write_id_maps(pid: Pid, uid: u32, gid: u32) -> std::io::Result<()> {
    let base = format!("/proc/{}", pid.as_raw());
    fs::write(format!("{base}/setgroups"), "deny")?;
    fs::write(format!("{base}/uid_map"), format!("0 {uid} 1\n"))?;
    fs::write(format!("{base}/gid_map"), format!("0 {gid} 1\n"))
}

fn wait_code(pid: Pid) -> i32 {
    loop {
        match waitpid(pid, None) {
            Ok(WaitStatus::Exited(_, code)) => return code,
            Ok(WaitStatus::Signaled(_, sig, _)) => return 128 + sig as i32,
            Ok(_) | Err(Errno::EINTR) => continue,
            Err(_) => return 255,
        }
    }
}

// ------------------------------------------------------------------- monitor

fn say(up: &mut File, line: &str) {
    let line = format!("{}\n", line.replace('\n', " "));
    let _ = up.write_all(line.as_bytes());
}

fn die(up: &mut File, msg: &str) -> ! {
    say(up, &format!("error:{msg}"));
    exit(SETUP_FAILED)
}

fn exit(code: i32) -> ! {
    // SAFETY: skips destructors and atexit handlers, which belong to the parent.
    unsafe { nix::libc::_exit(code) }
}

fn monitor(spec: &ContainerSpec, plan: &[SetupOp], opts: &RunOptions, mut up: File, mut down: File) -> ! {
    // Terminal signals (Ctrl-C) reach the whole process group; only the
    // supervisor reacts to them, and it forwards them to the container.
    for sig in FORWARDED {
        // SAFETY: ignoring a signal installs no handler code.
        let _ = unsafe { signal(sig, SigHandler::SigIgn) };
    }
    let _ = prctl::set_pdeathsig(Signal::SIGKILL);

    let mut flags = CloneFlags::CLONE_NEWUSER
        | CloneFlags::CLONE_NEWNS
        | CloneFlags::CLONE_NEWPID
        | CloneFlags::CLONE_NEWUTS
        | CloneFlags::CLONE_NEWIPC;
    if spec.network == Network::None {
        flags |= CloneFlags::CLONE_NEWNET;
    }
    if let Err(e) = unshare(flags) {
        die(&mut up, &format!("unshare: {e} (are unprivileged user namespaces enabled?)"));
    }
    say(&mut up, "unshared");

    let mut go = [0u8; 1];
    if !matches!(down.read(&mut go), Ok(1)) {
        exit(SETUP_FAILED); // the supervisor failed and reports why
    }
    drop(down);

    // SAFETY: see `run`.
    match unsafe { fork() } {
        Err(e) => die(&mut up, &format!("fork: {e}")),
        Ok(ForkResult::Child) => init(spec, plan, opts, up),
        Ok(ForkResult::Parent { child }) => {
            say(&mut up, &format!("pid:{}", child.as_raw()));
            drop(up); // so EOF on the pipe really means "exec happened"
            exit(wait_code(child))
        }
    }
}

// ---------------------------------------------------------------------- init

fn init(spec: &ContainerSpec, plan: &[SetupOp], opts: &RunOptions, mut up: File) -> ! {
    for sig in FORWARDED {
        // Ignored signals would be inherited by the container's command.
        // SAFETY: restoring the default disposition.
        let _ = unsafe { signal(sig, SigHandler::SigDfl) };
    }
    let _ = prctl::set_pdeathsig(Signal::SIGKILL);
    match setup_and_exec(spec, plan, opts) {
        Err(msg) => die(&mut up, &msg),
        Ok(never) => match never {},
    }
}

fn ctx(what: &'static str) -> impl Fn(Errno) -> String {
    move |e| format!("{what}: {e}")
}

fn setup_and_exec(spec: &ContainerSpec, plan: &[SetupOp], opts: &RunOptions) -> std::result::Result<Infallible, String> {
    let root = spec.rootfs.as_path();
    let none: Option<&str> = None;

    // Nothing we mount may leak back to the host's mount namespace.
    mount(none, "/", none, MsFlags::MS_REC | MsFlags::MS_PRIVATE, none).map_err(ctx("making mounts private"))?;
    // pivot_root needs the new root to be a mount point.
    mount(Some(root), root, none, MsFlags::MS_BIND | MsFlags::MS_REC, none).map_err(ctx("bind-mounting the rootfs"))?;

    for op in plan {
        if let Err(msg) = apply(root, op) {
            if !op.optional() {
                return Err(msg);
            }
            if opts.debug {
                eprintln!("yarc: warning: skipped optional setup step: {msg}");
            }
        }
    }

    sethostname(&spec.hostname).map_err(ctx("setting the hostname"))?;
    if spec.network == Network::None {
        loopback_up()?;
    }

    chdir(root).map_err(ctx("entering the rootfs"))?;
    // Stacks the old root on top of the new one at "/", then detaches it.
    pivot_root(".", ".").map_err(ctx("pivot_root"))?;
    umount2(".", MntFlags::MNT_DETACH).map_err(ctx("unmounting the old root"))?;
    chdir("/").map_err(ctx("chdir /"))?;

    if chdir(spec.working_dir.as_str()).is_err() {
        // Like Docker, create a missing working directory.
        fs::create_dir_all(&spec.working_dir).map_err(|e| format!("creating {}: {e}", spec.working_dir))?;
        chdir(spec.working_dir.as_str()).map_err(|e| format!("entering {}: {e}", spec.working_dir))?;
    }

    let to_c = |s: String| CString::new(s).map_err(|_| "command or environment contains a NUL byte".to_string());
    let path_var = spec.env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.as_str()).unwrap_or("");
    let program = resolve_executable(&spec.args[0], path_var, is_executable)
        .ok_or_else(|| format!("executable file not found in $PATH: {}", spec.args[0]))?;
    let program = to_c(program.to_string_lossy().into_owned())?;
    let argv = spec.args.iter().map(|a| to_c(a.clone())).collect::<std::result::Result<Vec<_>, _>>()?;
    let envp = spec.env.iter().map(|(k, v)| to_c(format!("{k}={v}"))).collect::<std::result::Result<Vec<_>, _>>()?;

    execve(&program, &argv, &envp).map_err(|e| format!("cannot execute {}: {e}", spec.args[0]))
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn mount_flags(o: MountOpts) -> MsFlags {
    let mut f = MsFlags::empty();
    f.set(MsFlags::MS_NOSUID, o.nosuid);
    f.set(MsFlags::MS_NODEV, o.nodev);
    f.set(MsFlags::MS_NOEXEC, o.noexec);
    f.set(MsFlags::MS_RDONLY, o.readonly);
    f
}

fn apply(root: &Path, op: &SetupOp) -> std::result::Result<(), String> {
    let none: Option<&str> = None;
    let fail = |what: &str, target: &str, e: &dyn std::fmt::Display| format!("{what} {target}: {e}");
    match op {
        SetupOp::Fs { fstype, target, data, opts, .. } => {
            let dir = safe_join(root, target).map_err(|e| e.to_string())?;
            fs::create_dir_all(&dir).map_err(|e| fail("creating", target, &e))?;
            let data = (!data.is_empty()).then_some(data.as_str());
            mount(Some(*fstype), &dir, Some(*fstype), mount_flags(*opts), data).map_err(|e| fail("mounting", target, &e))
        }
        SetupOp::Bind { source, target, kind, recursive, opts, .. } => {
            let dest = safe_join(root, target).map_err(|e| e.to_string())?;
            match kind {
                BindKind::Dir => fs::create_dir_all(&dest).map_err(|e| fail("creating", target, &e))?,
                BindKind::File => {
                    if let Some(parent) = dest.parent() {
                        fs::create_dir_all(parent).map_err(|e| fail("creating", target, &e))?;
                    }
                    OpenOptions::new().write(true).create(true).truncate(false).open(&dest).map_err(|e| fail("creating", target, &e))?;
                }
            }
            let mut flags = MsFlags::MS_BIND;
            flags.set(MsFlags::MS_REC, *recursive);
            mount(Some(source.as_path()), &dest, none, flags, none).map_err(|e| fail("bind-mounting", target, &e))?;
            if opts.readonly {
                // A bind mount ignores most flags until it is remounted.
                let remount = flags | MsFlags::MS_REMOUNT | mount_flags(*opts);
                mount(none, &dest, none, remount, none).map_err(|e| fail("remounting read-only", target, &e))?;
            }
            Ok(())
        }
        SetupOp::Symlink { path, points_to } => {
            let link = safe_join_file(root, path).map_err(|e| e.to_string())?;
            std::os::unix::fs::symlink(points_to, &link).map_err(|e| fail("symlinking", path, &e))
        }
        SetupOp::WriteFile { path, contents } => write_file(root, path, contents.as_bytes()),
        SetupOp::CopyHost { source, path } => {
            let bytes = fs::read(source).map_err(|e| fail("reading", &source.display().to_string(), &e))?;
            write_file(root, path, &bytes)
        }
    }
}

/// Writes `bytes` to `rel` inside the rootfs, replacing a symlink or file that
/// the image has there (never writing *through* a symlink).
fn write_file(root: &Path, rel: &str, bytes: &[u8]) -> std::result::Result<(), String> {
    let dest = safe_join_file(root, rel).map_err(|e| e.to_string())?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("creating {rel}: {e}"))?;
    }
    if fs::symlink_metadata(&dest).is_ok_and(|m| m.file_type().is_symlink()) {
        fs::remove_file(&dest).map_err(|e| format!("replacing {rel}: {e}"))?;
    }
    fs::write(&dest, bytes).map_err(|e| format!("writing {rel}: {e}"))
}

/// Brings `lo` up in a fresh network namespace (`--network none`).
fn loopback_up() -> std::result::Result<(), String> {
    use nix::libc;
    // SAFETY: plain socket/ioctl calls on a zero-initialised `ifreq` we own.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(format!("loopback socket: {}", Errno::last()));
        }
        let mut req: libc::ifreq = std::mem::zeroed();
        for (slot, byte) in req.ifr_name.iter_mut().zip(b"lo\0") {
            *slot = *byte as libc::c_char;
        }
        let result = if libc::ioctl(fd, libc::SIOCGIFFLAGS, &mut req) < 0 {
            Err(format!("reading loopback flags: {}", Errno::last()))
        } else {
            req.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
            if libc::ioctl(fd, libc::SIOCSIFFLAGS, &mut req) < 0 {
                Err(format!("bringing up loopback: {}", Errno::last()))
            } else {
                Ok(())
            }
        };
        libc::close(fd);
        result
    }
}
