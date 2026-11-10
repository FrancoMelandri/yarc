//! Copies a container's stdout/stderr pipes into the log (and optionally to
//! the terminal) without threads: one `poll` loop over both pipes.

use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

use crate::logs::{LogWriter, Stream};
use crate::state::now_ms;

/// Where output is forwarded as it arrives, in addition to the log.
#[derive(Default)]
pub struct Tee {
    pub stdout: Option<Box<dyn Write>>,
    pub stderr: Option<Box<dyn Write>>,
}

/// Reads both pipes until each reaches EOF (all writers closed). Chunks are
/// forwarded to `tee` immediately, so prompts without a newline show up; the
/// log is line-framed. A failing tee (closed terminal) is dropped silently; a
/// failing log is reported at the end, after all output has been drained so the
/// container never blocks on a full pipe.
pub fn pump<W: Write>(stdout: OwnedFd, stderr: OwnedFd, log: &mut LogWriter<W>, tee: Tee) -> io::Result<()> {
    pump_with_tick(stdout, stderr, log, tee, None)
}

/// A callback run roughly every `every` while output is being pumped (used to
/// drive health checks without a second thread).
pub struct Tick<'a> {
    pub every: Duration,
    pub run: &'a mut dyn FnMut(),
}

/// [`pump`] that also calls `tick` periodically, whether or not output arrives.
pub fn pump_with_tick<W: Write>(
    stdout: OwnedFd,
    stderr: OwnedFd,
    log: &mut LogWriter<W>,
    mut tee: Tee,
    mut tick: Option<Tick<'_>>,
) -> io::Result<()> {
    let mut streams = vec![(Stream::Stdout, File::from(stdout)), (Stream::Stderr, File::from(stderr))];
    let mut buf = [0u8; 8192];
    let mut log_error: Option<io::Error> = None;
    let mut last_tick = Instant::now();
    let timeout = tick
        .as_ref()
        .map_or(PollTimeout::NONE, |t| PollTimeout::from(t.every.as_millis().clamp(1, u16::MAX as u128) as u16));

    while !streams.is_empty() {
        let ready: Vec<bool> = {
            let mut fds: Vec<PollFd> = streams.iter().map(|(_, f)| PollFd::new(f.as_fd(), PollFlags::POLLIN)).collect();
            match poll(&mut fds, timeout) {
                Ok(_) => {}
                Err(Errno::EINTR) => {}
                Err(e) => return Err(io::Error::from(e)),
            }
            fds.iter().map(|p| p.revents().is_some_and(|r| !r.is_empty())).collect()
        };
        if let Some(t) = tick.as_mut() {
            if last_tick.elapsed() >= t.every {
                (t.run)();
                last_tick = Instant::now();
            }
        }

        let mut closed = Vec::new();
        for (i, (stream, file)) in streams.iter_mut().enumerate() {
            if !ready[i] {
                continue;
            }
            match file.read(&mut buf) {
                Ok(0) => closed.push(i),
                Ok(n) => {
                    let chunk = &buf[..n];
                    let sink = match stream {
                        Stream::Stdout => &mut tee.stdout,
                        Stream::Stderr => &mut tee.stderr,
                    };
                    if let Some(w) = sink {
                        if w.write_all(chunk).and_then(|()| w.flush()).is_err() {
                            *sink = None;
                        }
                    }
                    if let Err(e) = log.push(*stream, chunk, now_ms()) {
                        log_error.get_or_insert(e);
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => closed.push(i),
            }
        }
        for i in closed.into_iter().rev() {
            streams.remove(i);
        }
    }

    if let Err(e) = log.finish(now_ms()) {
        log_error.get_or_insert(e);
    }
    log_error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::logs::LogEntry;

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
        fn entries(&self) -> Vec<(Stream, String)> {
            self.text()
                .lines()
                .map(|l| serde_json::from_str::<LogEntry>(l).unwrap())
                .map(|e| (e.stream, e.line))
                .collect()
        }
    }

    fn pipe() -> (OwnedFd, std::io::PipeWriter) {
        let (r, w) = std::io::pipe().unwrap();
        (OwnedFd::from(r), w)
    }

    #[test]
    fn both_streams_are_logged_and_teed_until_eof() {
        let (out_r, mut out_w) = pipe();
        let (err_r, mut err_w) = pipe();
        out_w.write_all(b"a\nb").unwrap();
        err_w.write_all(b"x\n").unwrap();
        drop((out_w, err_w));

        let (log, tee_out, tee_err) = (Shared::default(), Shared::default(), Shared::default());
        let tee = Tee { stdout: Some(Box::new(tee_out.clone())), stderr: Some(Box::new(tee_err.clone())) };
        pump(out_r, err_r, &mut LogWriter::new(log.clone()), tee).unwrap();

        assert_eq!((tee_out.text().as_str(), tee_err.text().as_str()), ("a\nb", "x\n"));
        let mut got = log.entries();
        got.sort_by_key(|(s, l)| (*s as u8, l.clone()));
        assert_eq!(
            got,
            vec![
                (Stream::Stdout, "a".to_string()),
                (Stream::Stdout, "b".to_string()), // unterminated, flushed at EOF
                (Stream::Stderr, "x".to_string()),
            ]
        );
    }

    #[test]
    fn output_arriving_over_time_is_collected_and_waits_for_the_last_writer() {
        let (out_r, mut out_w) = pipe();
        let (err_r, err_w) = pipe();
        let writer = std::thread::spawn(move || {
            out_w.write_all(b"1\n").unwrap();
            std::thread::sleep(Duration::from_millis(80));
            out_w.write_all(b"2\n").unwrap();
            drop(err_w); // stderr closes first; stdout must still be read
            std::thread::sleep(Duration::from_millis(80));
            out_w.write_all(b"3\n").unwrap();
        });
        let log = Shared::default();
        pump(out_r, err_r, &mut LogWriter::new(log.clone()), Tee::default()).unwrap();
        writer.join().unwrap();
        let lines: Vec<_> = log.entries().into_iter().map(|(_, l)| l).collect();
        assert_eq!(lines, ["1", "2", "3"]);
    }

    #[test]
    fn the_tick_runs_while_the_container_is_silent() {
        let (out_r, out_w) = pipe();
        let (err_r, err_w) = pipe();
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            drop((out_w, err_w));
        });
        let mut ticks = 0;
        let mut count = || ticks += 1;
        let tick = Tick { every: Duration::from_millis(100), run: &mut count };
        let log = Shared::default();
        pump_with_tick(out_r, err_r, &mut LogWriter::new(log), Tee::default(), Some(tick)).unwrap();
        closer.join().unwrap();
        assert!((3..=6).contains(&ticks), "ticked {ticks} times in ~500 ms at 100 ms");
    }

    #[test]
    fn a_broken_tee_does_not_stop_logging() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (out_r, mut out_w) = pipe();
        let (err_r, err_w) = pipe();
        out_w.write_all(b"still logged\n").unwrap();
        drop((out_w, err_w));
        let log = Shared::default();
        let tee = Tee { stdout: Some(Box::new(Broken)), stderr: None };
        pump(out_r, err_r, &mut LogWriter::new(log.clone()), tee).unwrap();
        assert_eq!(log.entries(), vec![(Stream::Stdout, "still logged".to_string())]);
    }

    #[test]
    fn a_failing_log_is_reported_after_draining() {
        struct FullDisk;
        impl Write for FullDisk {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("disk full"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (out_r, mut out_w) = pipe();
        let (err_r, err_w) = pipe();
        out_w.write_all(b"x\n").unwrap();
        drop((out_w, err_w));
        let tee_out = Shared::default();
        let tee = Tee { stdout: Some(Box::new(tee_out.clone())), stderr: None };
        let result = pump(out_r, err_r, &mut LogWriter::new(FullDisk), tee);
        assert!(result.is_err());
        assert_eq!(tee_out.text(), "x\n", "forwarding continued");
    }
}
