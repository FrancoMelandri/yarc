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

use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use std::{fs, io, thread};

use nix::errno::Errno;
use nix::sched::{unshare, CloneFlags};
use nix::sys::signal::{kill, sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::unistd::{getegid, geteuid, Pid};

use crate::error::{Result, RuntimeError};
use crate::linux::loopback_up;
use crate::network::{NetInfo, PortMap, ProjectNet};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: i32) {
    STOP.store(true, Ordering::SeqCst);
}

/// Runs the holder until SIGTERM/SIGINT. `ready` is called exactly once: with
/// `Ok` when the network exists and the ports are published, or with the reason
/// it could not start. Must be called from a single-threaded process.
pub fn run_holder(net: &ProjectNet, publish: &[PortMap], ready: impl FnOnce(std::result::Result<(), String>)) -> Result<()> {
    let fail = |ready: &mut Option<Box<dyn FnOnce(std::result::Result<(), String>) + '_>>, msg: String| -> RuntimeError {
        if let Some(r) = ready.take() {
            r(Err(msg.clone()));
        }
        RuntimeError::start(msg)
    };
    let mut ready: Option<Box<dyn FnOnce(std::result::Result<(), String>) + '_>> = Some(Box::new(ready));

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

    let info = NetInfo { pid: std::process::id(), published: publish.to_vec() };
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
    for conn in listener.incoming() {
        if let Ok(client) = conn {
            thread::spawn(move || {
                let _ = relay(client, map.container);
            });
        }
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
                Err(e) => return Err(RuntimeError::Io { context: "cannot kill the project network holder".into(), source: e.into() }),
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
