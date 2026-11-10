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
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::sched::{setns, unshare, CloneFlags};
use nix::sys::prctl;
use nix::sys::signal::{sigaction, signal, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{chdir, dup2, execve, fork, getegid, geteuid, pipe2, pivot_root, sethostname, ForkResult, Pid};

use crate::error::{Result, RuntimeError};
use crate::network::ProjectNet;
use crate::paths::{resolve_executable, safe_join, safe_join_file};
use crate::plan::{check_user, mount_plan, BindKind, MountOpts, SetupOp};
use crate::spec::{ContainerSpec, Network};
use crate::StartOptions;

const FORWARDED: [Signal; 4] = [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP, Signal::SIGQUIT];
/// Exit status of the helper processes when container setup fails.
pub(crate) const SETUP_FAILED: i32 = 125;

/// A started container: its command has been exec'd. Dropping this does not
/// stop it; [`Running::wait`] blocks until it exits.
pub struct Running {
    monitor: Pid,
    pid: i32,
    _signals: SignalGuard,
}

impl Running {
    /// Host pid of the container's init (PID 1 inside).
    pub fn pid(&self) -> u32 {
        self.pid as u32
    }

    /// Waits for the container to exit; returns its status (`128 + n` when it
    /// was killed by signal `n`). Termination signals sent to yarc are
    /// forwarded to the container until this returns.
    pub fn wait(self) -> i32 {
        wait_code(self.monitor)
    }
}

/// True if a process with this pid exists (it may belong to another user).
pub fn process_alive(pid: u32) -> bool {
    matches!(nix::sys::signal::kill(Pid::from_raw(pid as i32), None), Ok(()) | Err(Errno::EPERM))
}

/// Starts the container and returns once its command is running (or failed to
/// start). The stdio descriptors in `opts` are closed in the calling process
/// before returning, so a reader sees EOF when the container's side closes.
pub fn start(spec: &ContainerSpec, opts: StartOptions) -> Result<Running> {
    check_user(spec.user.as_deref())?;
    if spec.args.is_empty() {
        return Err(RuntimeError::NoCommand);
    }
    // Resolve the project network *before* forking: a clear error, not a
    // failure inside the helper processes.
    let holder = match spec.network {
        Network::Project => Some(holder_pid(spec)?),
        _ => None,
    };
    let StartOptions { debug, stdin, stdout, stderr } = opts;
    let stdio = Stdio { stdin, stdout, stderr };
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
            monitor(spec, &plan, debug, holder, File::from(up_w), File::from(down_r), stdio)
        }
        ForkResult::Parent { child } => {
            drop(up_w);
            drop(down_r);
            drop(stdio);
            // A project's user namespace was mapped by its holder already.
            let maps = holder.is_none().then_some((uid, gid));
            supervise(child, maps, File::from(up_r), File::from(down_w))
        }
    }
}

/// Host pid of the live holder of the container's project network.
fn holder_pid(spec: &ContainerSpec) -> Result<u32> {
    let dir = spec
        .project_net
        .as_ref()
        .ok_or_else(|| RuntimeError::start("the container is in a project network but records no network directory"))?;
    ProjectNet::from_dir(dir)
        .alive_pid()?
        .ok_or_else(|| RuntimeError::start("the project's network is not running; start the project with `yarc up`"))
}

/// Joins the user and network namespaces of the project's holder. Joining the
/// user namespace needs a single-threaded caller (the monitor is a fresh fork)
/// and is allowed because we own it (same uid as the holder).
fn join_project_network(holder: u32) -> std::result::Result<(), String> {
    let open = |name: &str| {
        File::open(format!("/proc/{holder}/ns/{name}"))
            .map_err(|e| format!("cannot open the {name} namespace of the project network: {e}"))
    };
    let (user, net) = (open("user")?, open("net")?);
    // The user namespace first: it grants the right to join the network one.
    setns(&user, CloneFlags::CLONE_NEWUSER).map_err(|e| format!("joining the project's user namespace: {e}"))?;
    setns(&net, CloneFlags::CLONE_NEWNET).map_err(|e| format!("joining the project's network namespace: {e}"))
}

/// Descriptors that become the container's stdin/stdout/stderr (`None`: inherit).
pub(crate) struct Stdio {
    pub(crate) stdin: Option<OwnedFd>,
    pub(crate) stdout: Option<OwnedFd>,
    pub(crate) stderr: Option<OwnedFd>,
}

pub(crate) fn redirect(stdio: Stdio) -> std::result::Result<(), String> {
    for (fd, target) in [(stdio.stdin, 0), (stdio.stdout, 1), (stdio.stderr, 2)] {
        if let Some(fd) = fd {
            dup2(fd.as_raw_fd(), target).map_err(|e| format!("redirecting fd {target}: {e}"))?;
        }
    }
    Ok(())
}

fn sys(what: &'static str) -> impl Fn(Errno) -> RuntimeError {
    move |e| RuntimeError::start(format!("{what}: {e}"))
}

/// A setup failure inside the container's init, with the exit status to report.
pub(crate) struct Fail {
    pub(crate) code: i32,
    pub(crate) msg: String,
}

impl From<String> for Fail {
    fn from(msg: String) -> Self {
        Fail { code: SETUP_FAILED, msg }
    }
}

// ---------------------------------------------------------------- supervisor

static TARGET: AtomicI32 = AtomicI32::new(0);
static SIGNALS: AtomicU32 = AtomicU32::new(0);
/// Termination signals this process has received since [`reset_received`], whether or not
/// a container was there to forward them to. A restarting supervisor treats one as a stop.
static RECEIVED: AtomicU32 = AtomicU32::new(0);

pub(crate) fn reset_received() {
    RECEIVED.store(0, Ordering::SeqCst);
}

pub(crate) fn termination_received() -> bool {
    RECEIVED.load(Ordering::SeqCst) > 0
}

/// Forwards termination signals to the container. PID 1 of a namespace ignores
/// signals it has no handler for, so a second one escalates to SIGKILL
/// (otherwise Ctrl-C could not stop e.g. `sleep`).
extern "C" fn forward(sig: i32) {
    RECEIVED.fetch_add(1, Ordering::SeqCst);
    let target = TARGET.load(Ordering::SeqCst);
    if target <= 0 {
        return;
    }
    let sig = if SIGNALS.fetch_add(1, Ordering::SeqCst) >= 1 { nix::libc::SIGKILL } else { sig };
    // SAFETY: kill(2) is async-signal-safe.
    unsafe { nix::libc::kill(target, sig) };
}

/// Installs the forwarding handlers; restores the default ones on drop.
pub(crate) struct SignalGuard(Vec<(Signal, SigAction)>);

impl SignalGuard {
    pub(crate) fn install() -> Self {
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

/// `maps`: the (uid, gid) to map to root in the container's new user namespace;
/// `None` when it joined an existing, already mapped one.
fn supervise(monitor: Pid, maps: Option<(u32, u32)>, up: File, mut down: File) -> Result<Running> {
    TARGET.store(monitor.as_raw(), Ordering::SeqCst);
    let signals = SignalGuard::install();
    let mut init_pid = monitor.as_raw();
    let mut up = BufReader::new(up);
    let mut failure: Option<(i32, String)> = None;

    match read_line(&mut up).as_deref() {
        Some("unshared") => {
            let mapped = match maps {
                Some((uid, gid)) => write_id_maps(monitor, uid, gid),
                None => Ok(()),
            };
            let go = mapped.and_then(|()| down.write_all(b"go\n"));
            if let Err(e) = go {
                failure = Some((SETUP_FAILED, format!("cannot set up the user namespace: {e}")));
            }
        }
        Some(line) => failure = Some(parse_error(line)),
        None => failure = Some((SETUP_FAILED, "the container setup process died unexpectedly".into())),
    }
    // Closing `down` without the "go" line tells the monitor to give up.
    drop(down);

    if failure.is_none() {
        while let Some(line) = read_line(&mut up) {
            if let Some(pid) = line.strip_prefix("pid:").and_then(|p| p.parse::<i32>().ok()) {
                init_pid = pid;
                TARGET.store(pid, Ordering::SeqCst);
            } else if line.starts_with("error:") {
                failure = Some(parse_error(&line));
            }
        }
    }

    match failure {
        Some((code, msg)) => {
            wait_code(monitor); // reap the helper processes
            Err(RuntimeError::Start { msg, code })
        }
        None => Ok(Running { monitor, pid: init_pid, _signals: signals }),
    }
}

pub(crate) fn read_line(r: &mut impl BufRead) -> Option<String> {
    let mut line = String::new();
    match r.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim_end().to_string()),
    }
}

/// `error:<code>:<message>` as written by `die`.
pub(crate) fn parse_error(line: &str) -> (i32, String) {
    let rest = line.strip_prefix("error:").unwrap_or(line);
    match rest.split_once(':').and_then(|(c, m)| Some((c.parse().ok()?, m))) {
        Some((code, msg)) => (code, msg.to_string()),
        None => (SETUP_FAILED, rest.to_string()),
    }
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

pub(crate) fn wait_code(pid: Pid) -> i32 {
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

pub(crate) fn say(up: &mut File, line: &str) {
    let line = format!("{}\n", line.replace('\n', " "));
    let _ = up.write_all(line.as_bytes());
}

pub(crate) fn die(up: &mut File, code: i32, msg: &str) -> ! {
    say(up, &format!("error:{code}:{msg}"));
    exit(code)
}

pub(crate) fn exit(code: i32) -> ! {
    // SAFETY: skips destructors and atexit handlers, which belong to the parent.
    unsafe { nix::libc::_exit(code) }
}

fn monitor(
    spec: &ContainerSpec,
    plan: &[SetupOp],
    debug: bool,
    holder: Option<u32>,
    mut up: File,
    mut down: File,
    stdio: Stdio,
) -> ! {
    // Terminal signals (Ctrl-C) reach the whole process group; only the
    // supervisor reacts to them, and it forwards them to the container.
    for sig in FORWARDED {
        // SAFETY: ignoring a signal installs no handler code.
        let _ = unsafe { signal(sig, SigHandler::SigIgn) };
    }
    let _ = prctl::set_pdeathsig(Signal::SIGKILL);

    let mut flags =
        CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWUTS | CloneFlags::CLONE_NEWIPC;
    match holder {
        // Join the project's user + network namespaces; the rest is ours, and
        // owned by that user namespace.
        Some(pid) => {
            if let Err(msg) = join_project_network(pid) {
                die(&mut up, SETUP_FAILED, &msg);
            }
        }
        None => {
            flags |= CloneFlags::CLONE_NEWUSER;
            if spec.network == Network::None {
                flags |= CloneFlags::CLONE_NEWNET;
            }
        }
    }
    if let Err(e) = unshare(flags) {
        die(&mut up, SETUP_FAILED, &format!("unshare: {e} (are unprivileged user namespaces enabled?)"));
    }
    say(&mut up, "unshared");

    let mut go = [0u8; 1];
    if !matches!(down.read(&mut go), Ok(1)) {
        exit(SETUP_FAILED); // the supervisor failed and reports why
    }
    drop(down);

    // SAFETY: see `run`.
    match unsafe { fork() } {
        Err(e) => die(&mut up, SETUP_FAILED, &format!("fork: {e}")),
        Ok(ForkResult::Child) => init(spec, plan, debug, up, stdio),
        Ok(ForkResult::Parent { child }) => {
            say(&mut up, &format!("pid:{}", child.as_raw()));
            drop(up); // so EOF on the pipe really means "exec happened"
            drop(stdio); // the output pipes must only stay open in the container
            exit(wait_code(child))
        }
    }
}

// ---------------------------------------------------------------------- init

fn init(spec: &ContainerSpec, plan: &[SetupOp], debug: bool, mut up: File, stdio: Stdio) -> ! {
    for sig in FORWARDED {
        // Ignored signals would be inherited by the container's command.
        // SAFETY: restoring the default disposition.
        let _ = unsafe { signal(sig, SigHandler::SigDfl) };
    }
    // Rust starts programs with SIGPIPE ignored, and that would leak into the
    // container: `yes | head` must die quietly, not fail with EPIPE.
    // SAFETY: restoring the default disposition.
    let _ = unsafe { signal(Signal::SIGPIPE, SigHandler::SigDfl) };
    let _ = prctl::set_pdeathsig(Signal::SIGKILL);
    if let Err(msg) = redirect(stdio) {
        die(&mut up, SETUP_FAILED, &msg);
    }
    match setup_and_exec(spec, plan, debug) {
        Err(fail) => die(&mut up, fail.code, &fail.msg),
        Ok(never) => match never {},
    }
}

fn ctx(what: &'static str) -> impl Fn(Errno) -> String {
    move |e| format!("{what}: {e}")
}

fn setup_and_exec(spec: &ContainerSpec, plan: &[SetupOp], debug: bool) -> std::result::Result<Infallible, Fail> {
    let root = spec.rootfs.as_path();
    let none: Option<&str> = None;

    // Nothing we mount may leak back to the host's mount namespace.
    mount(none, "/", none, MsFlags::MS_REC | MsFlags::MS_PRIVATE, none).map_err(ctx("making mounts private"))?;
    // pivot_root needs the new root to be a mount point.
    mount(Some(root), root, none, MsFlags::MS_BIND | MsFlags::MS_REC, none).map_err(ctx("bind-mounting the rootfs"))?;

    for op in plan {
        if let Err(msg) = apply(root, op) {
            if !op.optional() {
                return Err(msg.into());
            }
            if debug {
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

    exec_program(&spec.args, &spec.env)
}

/// Replaces this process with `args[0]` (looked up in the `PATH` of `env`, i.e.
/// the container's, not the host's). Only returns on failure, with Docker's
/// exit statuses: 127 not found, 126 not executable.
pub(crate) fn exec_program(args: &[String], env: &[(String, String)]) -> std::result::Result<Infallible, Fail> {
    let to_c = |s: String| CString::new(s).map_err(|_| "command or environment contains a NUL byte".to_string());
    let path_var = env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.as_str()).unwrap_or("");
    let not_found = || Fail { code: 127, msg: format!("executable file not found in $PATH: {}", args[0]) };
    let program = resolve_executable(&args[0], path_var, is_executable).ok_or_else(not_found)?;
    let program = to_c(program.to_string_lossy().into_owned())?;
    let argv = args.iter().map(|a| to_c(a.clone())).collect::<std::result::Result<Vec<_>, _>>()?;
    let envp = env.iter().map(|(k, v)| to_c(format!("{k}={v}"))).collect::<std::result::Result<Vec<_>, _>>()?;

    let errno = match execve(&program, &argv, &envp) {
        Err(e) => e,
        Ok(never) => match never {},
    };
    let code = match errno {
        Errno::ENOENT => 127,
        Errno::EACCES | Errno::ENOEXEC => 126,
        _ => SETUP_FAILED,
    };
    Err(Fail { code, msg: format!("cannot execute {}: {errno}", args[0]) })
}

pub(crate) fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The mount flags of the filesystem holding `path` that cannot be cleared by a
/// remount in a user namespace.
fn locked_flags(path: &Path) -> MsFlags {
    use nix::sys::statvfs::{statvfs, FsFlags};
    let Ok(stat) = statvfs(path) else { return MsFlags::empty() };
    let have = stat.flags();
    let mut out = MsFlags::empty();
    out.set(MsFlags::MS_NOSUID, have.contains(FsFlags::ST_NOSUID));
    out.set(MsFlags::MS_NODEV, have.contains(FsFlags::ST_NODEV));
    out.set(MsFlags::MS_NOEXEC, have.contains(FsFlags::ST_NOEXEC));
    out.set(MsFlags::MS_NOATIME, have.contains(FsFlags::ST_NOATIME));
    out.set(MsFlags::MS_NODIRATIME, have.contains(FsFlags::ST_NODIRATIME));
    out.set(MsFlags::MS_RELATIME, have.contains(FsFlags::ST_RELATIME));
    out
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
            mount(Some(*fstype), &dir, Some(*fstype), mount_flags(*opts), data)
                .map_err(|e| fail("mounting", target, &e))
        }
        SetupOp::Bind { source, target, kind, recursive, opts, .. } => {
            let dest = safe_join(root, target).map_err(|e| e.to_string())?;
            match kind {
                BindKind::Dir => fs::create_dir_all(&dest).map_err(|e| fail("creating", target, &e))?,
                BindKind::File => {
                    if let Some(parent) = dest.parent() {
                        fs::create_dir_all(parent).map_err(|e| fail("creating", target, &e))?;
                    }
                    OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(&dest)
                        .map_err(|e| fail("creating", target, &e))?;
                }
            }
            let mut flags = MsFlags::MS_BIND;
            flags.set(MsFlags::MS_REC, *recursive);
            mount(Some(source.as_path()), &dest, none, flags, none).map_err(|e| fail("bind-mounting", target, &e))?;
            if opts.readonly {
                // A bind mount ignores most flags until it is remounted.
                // Inside a user namespace the flags the source mount already has
                // (nosuid, nodev, noexec, atime options) are locked: a remount
                // that drops them fails with EPERM, so carry them over.
                let remount = flags | MsFlags::MS_REMOUNT | mount_flags(*opts) | locked_flags(source);
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
pub(crate) fn loopback_up() -> std::result::Result<(), String> {
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
