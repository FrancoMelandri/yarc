//! The request loop. See the crate documentation.

use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use yarc_proto::{
    check_version, read_frame, write_frame, AgentFrame, ClientFrame, Command, Event, Globals, ProtoError, RequestId,
    PROTOCOL_VERSION,
};

use crate::argv::to_args;
use crate::utf8::Utf8Chunker;

/// Exit status for a request that could not be started at all (as `docker run` does).
const START_FAILED: i32 = 125;
/// How long children get to react to SIGINT once the client has gone away.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

type Running = Mutex<HashMap<RequestId, u32>>;

struct Out<W>(Mutex<W>);

impl<W: Write> Out<W> {
    /// A client that went away cannot be told anything; its requests get
    /// interrupted when the input ends.
    fn send(&self, frame: &AgentFrame) {
        let mut w = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let _ = write_frame(&mut *w, frame);
    }

    fn event(&self, id: RequestId, event: Event) {
        self.send(&AgentFrame::Event { id, event });
    }

    fn fatal(&self, message: String) {
        self.send(&AgentFrame::Fatal { message });
    }
}

fn lock(running: &Running) -> std::sync::MutexGuard<'_, HashMap<RequestId, u32>> {
    running.lock().unwrap_or_else(|e| e.into_inner())
}

/// Serves requests until `input` ends. Each request runs `exe` (the `yarc`
/// binary) as a child process. Returns an error for a protocol violation (after
/// telling the client with a `Fatal` frame), `Ok` when the client simply closed
/// the stream.
pub fn serve<R: BufRead, W: Write + Send>(mut input: R, output: W, exe: &Path) -> Result<(), ProtoError> {
    let out = Out(Mutex::new(output));

    match read_frame::<_, ClientFrame>(&mut input) {
        Ok(Some(ClientFrame::Hello { version, .. })) => {
            if let Err(e) = check_version(version) {
                out.fatal(e.to_string());
                return Err(e);
            }
        }
        Ok(Some(_)) => {
            out.fatal("the first frame must be hello".into());
            return Err(ProtoError::Malformed("the first frame must be hello".into()));
        }
        Ok(None) => return Ok(()),
        Err(e) => {
            out.fatal(e.to_string());
            return Err(e);
        }
    }
    out.send(&AgentFrame::Hello { version: PROTOCOL_VERSION, agent: format!("yarc-agent {}", env!("CARGO_PKG_VERSION")) });

    let running: Running = Mutex::new(HashMap::new());
    thread::scope(|scope| {
        let result = loop {
            let frame = match read_frame::<_, ClientFrame>(&mut input) {
                Ok(Some(frame)) => frame,
                Ok(None) => break Ok(()),
                Err(e) => {
                    out.fatal(e.to_string());
                    break Err(e);
                }
            };
            match frame {
                ClientFrame::Hello { .. } => {
                    out.fatal("unexpected second hello".into());
                    break Err(ProtoError::Malformed("unexpected second hello".into()));
                }
                ClientFrame::Cancel { id } => {
                    // under the lock: the entry goes away before the child is reaped
                    if let Some(pid) = lock(&running).get(&id) {
                        signal(*pid, libc::SIGINT);
                    }
                }
                ClientFrame::Request { id, globals, command } => {
                    // spawned here, not in the thread, so a Cancel read right
                    // after this frame already finds the child
                    match start(exe, &globals, &command) {
                        Ok(child) if lock(&running).contains_key(&id) => {
                            kill_now(child);
                            out.event(id, Event::Failed { message: format!("request {id} is already running"), code: START_FAILED });
                        }
                        Ok(child) => {
                            lock(&running).insert(id, child.id());
                            let (out, running) = (&out, &running);
                            scope.spawn(move || run_request(out, running, id, child));
                        }
                        Err(message) => out.event(id, Event::Failed { message, code: START_FAILED }),
                    }
                }
            }
        };
        shut_down(&running);
        result
    })
}

fn start(exe: &Path, globals: &Globals, command: &Command) -> Result<Child, String> {
    let args = to_args(globals, command).map_err(|e| e.to_string())?;
    std::process::Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // its own group: a signal aimed at the agent must not also hit it
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", exe.display()))
}

fn kill_now(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn signal(pid: u32, sig: i32) {
    // SAFETY: plain kill(2) on a pid that is still ours (not reaped yet).
    unsafe {
        libc::kill(pid as i32, sig);
    }
}

/// The client is gone: interrupt what still runs, kill what ignores that.
fn shut_down(running: &Running) {
    for pid in lock(running).values() {
        signal(*pid, libc::SIGINT);
    }
    let deadline = std::time::Instant::now() + SHUTDOWN_GRACE;
    while !lock(running).is_empty() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    for pid in lock(running).values() {
        signal(*pid, libc::SIGKILL);
    }
}

fn run_request<W: Write + Send>(out: &Out<W>, running: &Running, id: RequestId, mut child: Child) {
    let exited = AtomicBool::new(false);
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    let code = thread::scope(|s| {
        s.spawn(|| pump(stdout, &exited, |text| out.event(id, Event::Stdout { text })));
        s.spawn(|| pump(stderr, &exited, |text| out.event(id, Event::Stderr { text })));

        wait_without_reaping(child.id());
        // forget the pid before reaping it, so a late Cancel cannot hit a reused one
        lock(running).remove(&id);
        let status = child.wait();
        exited.store(true, Ordering::SeqCst);
        match status {
            Ok(st) => st.code().or_else(|| st.signal().map(|n| 128 + n)).unwrap_or(1),
            Err(_) => 1,
        }
    });
    out.event(id, Event::Exit { code });
}

/// Blocks until the child has exited, leaving it unreaped (so its pid stays ours).
fn wait_without_reaping(pid: u32) {
    // SAFETY: waitid(2) with a zeroed siginfo_t it fills in.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        while libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT) != 0 {
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return;
            }
        }
    }
}

/// Forwards everything read from `pipe` as text until it closes. A grandchild
/// may keep the pipe open after the command itself has exited (a daemon
/// started by `up -d`); once `exited` is set, a quiet pipe ends the pump.
fn pump<P: Read + AsRawFd>(mut pipe: P, exited: &AtomicBool, mut emit: impl FnMut(String)) {
    let mut chunker = Utf8Chunker::default();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let mut pfd = libc::pollfd { fd: pipe.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut pfd, 1, 100) };
        if ready == 0 {
            if exited.load(Ordering::SeqCst) {
                break;
            }
            continue;
        }
        if ready < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if let Some(text) = chunker.push(&buf[..n]) {
                    emit(text);
                }
            }
        }
    }
    if let Some(text) = chunker.finish() {
        emit(text);
    }
}

#[cfg(test)]
mod tests;
