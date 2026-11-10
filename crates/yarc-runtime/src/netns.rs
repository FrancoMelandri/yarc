//! The project network holder: a long-lived process that owns the user and
//! network namespace the project's containers join, and forwards published
//! ports into it.
//!
//! ```text
//! host network namespace                      project network namespace
//!  127.0.0.1:8080  ←— listener (bound first)        127.0.0.1:80 ← the service
//!         └── accept ──► holder thread ── connect ──►  (threads created after
//!                          copies both ways             unshare live in it)
//! ```
//!
//! The listening sockets are created *before* `unshare(CLONE_NEWNET)`, so they
//! stay in the host's network namespace; every socket created afterwards (the
//! connection to the service) is in the project's.

use std::ffi::CString;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use std::{fs, io, thread};

use nix::errno::Errno;
use nix::libc;
use nix::sched::{unshare, CloneFlags};
use nix::sys::signal::{kill, sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{fork, getegid, geteuid, pipe, ForkResult, Pid};

use crate::error::{Result, RuntimeError};
use crate::linux::loopback_up;
use crate::network::{NetInfo, PortMap, ProjectNet, DNS_FORWARD_ADDR};

/// Told once whether the network came up (`Err`: why not).
type Ready<'a> = Box<dyn FnOnce(std::result::Result<(), String>) + 'a>;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: i32) {
    STOP.store(true, Ordering::SeqCst);
}

/// Runs the holder until SIGTERM/SIGINT. `ready` is called exactly once: with
/// `Ok` when the network exists and the ports are published, or with the reason
/// it could not start. Must be called from a single-threaded process.
pub fn run_holder(
    net: &ProjectNet,
    publish: &[PortMap],
    pasta: Option<&Path>,
    ready: impl FnOnce(std::result::Result<(), String>),
) -> Result<()> {
    let fail = |ready: &mut Option<Ready<'_>>, msg: String| -> RuntimeError {
        if let Some(r) = ready.take() {
            r(Err(msg.clone()));
        }
        RuntimeError::start(msg)
    };
    let mut ready: Option<Ready<'_>> = Some(Box::new(ready));

    // Before anything else, while this process is still in the host's namespaces
    // and single-threaded: pasta must start from the host side. Not having it
    // is never fatal; the project just has no outbound network.
    let mut outbound_note = None;
    let mut pasta = match pasta {
        Some(binary) => match Pasta::spawn(binary) {
            Ok(p) => Some(p),
            Err(msg) => {
                outbound_note = Some(msg);
                None
            }
        },
        None => {
            outbound_note = Some("pasta is not installed (package passt) or is disabled with YARC_PASTA=".to_string());
            None
        }
    };

    let Some(_lock) = net.try_acquire()? else {
        return Err(fail(&mut ready, "the project network is already running".into()));
    };

    // Published ports first, in the host's network namespace.
    let mut listeners = Vec::new();
    for map in publish {
        match TcpListener::bind(("127.0.0.1", map.host)) {
            Ok(listener) => listeners.push((listener, *map)),
            Err(e) => return Err(fail(&mut ready, format!("cannot publish 127.0.0.1:{}: {e}", map.host))),
        }
    }

    // A user namespace of our own (the invoking user is root inside, like in
    // every container) that owns a fresh network namespace.
    let (uid, gid) = (geteuid().as_raw(), getegid().as_raw());
    unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNET)
        .map_err(|e| fail(&mut ready, format!("unshare: {e} (are unprivileged user namespaces enabled?)")))?;
    let maps = fs::write("/proc/self/setgroups", "deny")
        .and_then(|()| fs::write("/proc/self/uid_map", format!("0 {uid} 1\n")))
        .and_then(|()| fs::write("/proc/self/gid_map", format!("0 {gid} 1\n")));
    if let Err(e) = maps {
        return Err(fail(&mut ready, format!("cannot map the user namespace: {e}")));
    }
    if let Err(msg) = loopback_up() {
        return Err(fail(&mut ready, msg));
    }

    let mut outbound = false;
    if let Some(p) = pasta.as_mut() {
        match p.attach() {
            Ok(()) => outbound = true,
            Err(msg) => {
                outbound_note = Some(msg);
                drop(pasta.take()); // stops it
            }
        }
    }

    let info = NetInfo { pid: std::process::id(), published: publish.to_vec(), outbound, outbound_note };
    if let Err(e) = net.write_info(&info) {
        return Err(fail(&mut ready, e.to_string()));
    }

    // From here on, threads (and their sockets) are in the project's namespace.
    for (listener, map) in listeners {
        thread::spawn(move || accept_loop(listener, map));
    }

    let action = SigAction::new(SigHandler::Handler(on_signal), SaFlags::empty(), SigSet::empty());
    for sig in [Signal::SIGTERM, Signal::SIGINT] {
        // SAFETY: the handler only stores to an atomic.
        let _ = unsafe { sigaction(sig, &action) };
    }
    if let Some(r) = ready.take() {
        r(Ok(()));
    }
    while !STOP.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn accept_loop(listener: TcpListener, map: PortMap) {
    for client in listener.incoming().flatten() {
        thread::spawn(move || {
            let _ = relay(client, map.container);
        });
    }
}

/// Connects to the service inside the project network and copies both ways
/// until each side has closed.
fn relay(client: TcpStream, container_port: u16) -> io::Result<()> {
    let backend = TcpStream::connect(("127.0.0.1", container_port))?; // refused: nobody listens yet
    let (c, b) = (client.try_clone()?, backend.try_clone()?);
    let upstream = thread::spawn(move || {
        let _ = io::copy(&mut &c, &mut &b);
        let _ = b.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut &backend, &mut &client);
    let _ = client.shutdown(Shutdown::Write);
    let _ = upstream.join();
    Ok(())
}

/// Stops the holder (SIGTERM, then SIGKILL after `timeout`) and waits until its
/// lock is released. Succeeds if no holder is running.
pub fn stop_holder(net: &ProjectNet, timeout: Duration) -> Result<()> {
    let Some(pid) = net.alive_pid()? else { return Ok(()) };
    let target = Pid::from_raw(pid as i32);
    let _ = kill(target, Signal::SIGTERM);
    let deadline = Instant::now() + timeout;
    while net.is_alive()? {
        if Instant::now() >= deadline {
            match kill(target, Signal::SIGKILL) {
                Ok(()) | Err(Errno::ESRCH) => {}
                Err(e) => {
                    return Err(RuntimeError::Io {
                        context: "cannot kill the project network holder".into(),
                        source: e.into(),
                    })
                }
            }
            thread::sleep(Duration::from_millis(200));
            return if net.is_alive()? {
                Err(RuntimeError::Unsupported("the project network holder did not exit".into()))
            } else {
                Ok(())
            };
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

// ---- outbound network (pasta) ------------------------------------------------

/// How long pasta gets to put an interface into the project's namespace.
const PASTA_SETUP_TIMEOUT: Duration = Duration::from_secs(3);

/// Arguments for `pasta` to give the namespace of process `pid` an outbound
/// network: the host's address and routes (IPv4), no port forwarding in either
/// direction (published ports are the holder's job; `auto` forwarding would
/// expose everything the services listen on to the host), kept in the
/// foreground so the holder can supervise it.
pub fn pasta_args(binary: &Path, pid: u32) -> Vec<String> {
    let mut args = vec![binary.display().to_string(), "--foreground".into(), "-4".into(), "--config-net".into()];
    for flag in ["-t", "-u", "-T", "-U"] {
        args.extend([flag.into(), "none".into()]);
    }
    args.extend(["--dns-forward".into(), DNS_FORWARD_ADDR.into()]);
    args.push(pid.to_string());
    args
}

/// A `pasta` process attached to the holder's namespace. Forked *before* the
/// holder leaves the host's namespaces (pasta needs the host side to copy the
/// network from), it waits on a pipe until the namespace exists, then execs.
/// Dropping it stops pasta; if the holder is killed, the kernel does.
struct Pasta {
    child: Pid,
    go: Option<OwnedFd>,
}

impl Pasta {
    fn spawn(binary: &Path) -> std::result::Result<Pasta, String> {
        let (read_end, write_end) = pipe().map_err(|e| format!("pipe: {e}"))?;
        let devnull = fs::OpenOptions::new().write(true).open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
        // Everything the child needs is allocated before fork.
        let holder = std::process::id();
        let args: Vec<CString> = pasta_args(binary, holder)
            .into_iter()
            .map(|a| CString::new(a).map_err(|_| "pasta path contains a NUL byte".to_string()))
            .collect::<std::result::Result<_, _>>()?;
        let mut argv: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
        argv.push(std::ptr::null());

        // SAFETY: the holder is single-threaded here, and the child only calls
        // async-signal-safe functions on data prepared above before it execs.
        match unsafe { fork() }.map_err(|e| format!("fork: {e}"))? {
            ForkResult::Parent { child } => Ok(Pasta { child, go: Some(write_end) }),
            ForkResult::Child => unsafe {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                if libc::getppid() as u32 != holder {
                    libc::_exit(0); // the holder is already gone
                }
                libc::dup2(devnull.as_raw_fd(), 1);
                libc::close(write_end.as_raw_fd());
                let mut byte = 0u8;
                if libc::read(read_end.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) != 1 {
                    libc::_exit(0); // the holder gave up before the namespace existed
                }
                libc::execv(argv[0], argv.as_ptr());
                libc::_exit(127)
            },
        }
    }

    /// Lets pasta go and waits until the namespace has an interface besides `lo`.
    fn attach(&mut self) -> std::result::Result<(), String> {
        let go = self.go.take().ok_or("pasta was already started")?;
        // SAFETY: writing one byte from a live buffer to an fd we own.
        if unsafe { libc::write(go.as_raw_fd(), [1u8].as_ptr().cast(), 1) } != 1 {
            return Err("cannot start pasta".into());
        }
        drop(go);
        let deadline = Instant::now() + PASTA_SETUP_TIMEOUT;
        loop {
            match waitpid(self.child, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => {}
                Ok(WaitStatus::Exited(_, 127)) => return Err("cannot execute pasta".into()),
                Ok(status) => {
                    self.child = Pid::from_raw(0); // reaped
                    return Err(format!("pasta stopped ({status:?}); see the network log"));
                }
                Err(e) => return Err(format!("waiting for pasta: {e}")),
            }
            if has_outbound_interface() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "pasta did not configure a network interface within {}s",
                    PASTA_SETUP_TIMEOUT.as_secs()
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Pasta {
    fn drop(&mut self) {
        if self.child.as_raw() > 0 {
            let _ = kill(self.child, Signal::SIGTERM);
            let _ = waitpid(self.child, None);
        }
    }
}

/// Whether this process's network namespace has an interface other than `lo`.
fn has_outbound_interface() -> bool {
    fs::read_to_string("/proc/self/net/dev").map(|t| interface_besides_lo(&t)).unwrap_or(false)
}

fn interface_besides_lo(proc_net_dev: &str) -> bool {
    proc_net_dev.lines().skip(2).filter_map(|l| l.split(':').next()).any(|name| name.trim() != "lo")
}

#[cfg(test)]
mod pasta_tests {
    use super::*;

    #[test]
    fn pasta_gets_the_holder_pid_last_and_no_port_forwarding() {
        let args = pasta_args(Path::new("/usr/bin/pasta"), 4242);
        assert_eq!(args.first().map(String::as_str), Some("/usr/bin/pasta"));
        assert_eq!(args.last().map(String::as_str), Some("4242"));
        assert!(args.contains(&"--config-net".to_string()) && args.contains(&"--foreground".to_string()));
        let i = args.iter().position(|a| a == "--dns-forward").unwrap();
        assert_eq!(args[i + 1], DNS_FORWARD_ADDR);
        for flag in ["-t", "-u", "-T", "-U"] {
            let i = args.iter().position(|a| a == flag).unwrap_or_else(|| panic!("{flag} missing"));
            assert_eq!(args[i + 1], "none", "{flag} must not forward ports");
        }
    }

    #[test]
    fn detects_a_second_interface_in_proc_net_dev() {
        let header = "Inter-|   Rx\n face |bytes\n";
        let lo = "    lo: 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n";
        let eth = "  eth0: 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n";
        assert!(!interface_besides_lo(&format!("{header}{lo}")));
        assert!(interface_besides_lo(&format!("{header}{lo}{eth}")));
        assert!(!interface_besides_lo(""));
    }
}
