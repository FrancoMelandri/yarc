//! Container logs: JSON lines, one per output line, with timestamp and stream
//! (like Docker's `json-file` driver).
//!
//! ```text
//! {"ts":1700000000123,"stream":"stdout","line":"hello"}
//! ```

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{io_err, Result};

/// Output longer than this without a newline is split into several entries.
pub const MAX_LINE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    fn index(self) -> usize {
        match self {
            Stream::Stdout => 0,
            Stream::Stderr => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// Unix time in milliseconds when the line was read.
    pub ts: u64,
    pub stream: Stream,
    pub line: String,
}

/// Turns raw output chunks into log entries. Each stream keeps its own partial
/// line, so interleaved chunks never mix; every complete line is written with a
/// single `write` so concurrent readers never see half an entry.
pub struct LogWriter<W: Write> {
    out: W,
    pending: [Vec<u8>; 2],
}

impl<W: Write> LogWriter<W> {
    pub fn new(out: W) -> Self {
        LogWriter { out, pending: [Vec::new(), Vec::new()] }
    }

    pub fn push(&mut self, stream: Stream, data: &[u8], ts: u64) -> io::Result<()> {
        let buf = &mut self.pending[stream.index()];
        buf.extend_from_slice(data);
        let mut batch = Vec::new();
        loop {
            let window = &buf[..buf.len().min(MAX_LINE + 1)];
            let line: Vec<u8> = if let Some(pos) = window.iter().position(|&b| b == b'\n') {
                let mut taken: Vec<u8> = buf.drain(..=pos).collect();
                taken.pop(); // the newline
                taken
            } else if buf.len() > MAX_LINE {
                buf.drain(..MAX_LINE).collect()
            } else {
                break;
            };
            encode(&mut batch, stream, &line, ts);
        }
        if batch.is_empty() { Ok(()) } else { self.out.write_all(&batch) }
    }

    /// Writes what is left of unterminated lines (call at end of output).
    pub fn finish(&mut self, ts: u64) -> io::Result<()> {
        let mut batch = Vec::new();
        for stream in [Stream::Stdout, Stream::Stderr] {
            let rest = std::mem::take(&mut self.pending[stream.index()]);
            if !rest.is_empty() {
                encode(&mut batch, stream, &rest, ts);
            }
        }
        if batch.is_empty() { Ok(()) } else { self.out.write_all(&batch) }
    }
}

fn encode(batch: &mut Vec<u8>, stream: Stream, line: &[u8], ts: u64) {
    let entry = LogEntry { ts, stream, line: String::from_utf8_lossy(line).into_owned() };
    // Serializing plain strings and numbers cannot fail.
    serde_json::to_writer(&mut *batch, &entry).expect("log entry serializes");
    batch.push(b'\n');
}

/// Reads a log file. Unparsable lines (e.g. one being written right now) are
/// skipped; a missing file is an empty log.
pub fn read_log(path: &Path) -> Result<Vec<LogEntry>> {
    let text = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(format!("cannot read {}", path.display()))(e)),
    };
    Ok(text
        .split(|&b| b == b'\n')
        .filter_map(|line| serde_json::from_slice::<LogEntry>(line).ok())
        .collect())
}

/// `2023-11-14T22:13:20.123Z`
pub fn format_timestamp(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        ms % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(f: impl FnOnce(&mut LogWriter<Vec<u8>>)) -> Vec<LogEntry> {
        let mut w = LogWriter::new(Vec::new());
        f(&mut w);
        w.out
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_slice(l).unwrap())
            .collect()
    }

    fn e(ts: u64, stream: Stream, line: &str) -> LogEntry {
        LogEntry { ts, stream, line: line.into() }
    }

    #[test]
    fn complete_lines_become_entries() {
        let got = written(|w| w.push(Stream::Stdout, b"one\ntwo\n", 5).unwrap());
        assert_eq!(got, vec![e(5, Stream::Stdout, "one"), e(5, Stream::Stdout, "two")]);
    }

    #[test]
    fn partial_lines_wait_for_their_newline_per_stream() {
        let got = written(|w| {
            w.push(Stream::Stdout, b"hel", 1).unwrap();
            w.push(Stream::Stderr, b"err\n", 2).unwrap();
            w.push(Stream::Stdout, b"lo\nwor", 3).unwrap();
            w.push(Stream::Stdout, b"ld\n", 4).unwrap();
        });
        assert_eq!(
            got,
            vec![e(2, Stream::Stderr, "err"), e(3, Stream::Stdout, "hello"), e(4, Stream::Stdout, "world")]
        );
    }

    #[test]
    fn finish_flushes_unterminated_output() {
        let got = written(|w| {
            w.push(Stream::Stdout, b"prompt> ", 1).unwrap();
            w.push(Stream::Stderr, b"oops", 1).unwrap();
            w.finish(9).unwrap();
        });
        assert_eq!(got, vec![e(9, Stream::Stdout, "prompt> "), e(9, Stream::Stderr, "oops")]);
        assert!(written(|w| w.finish(1).unwrap()).is_empty());
    }

    #[test]
    fn empty_lines_are_kept_and_carriage_returns_too() {
        let got = written(|w| w.push(Stream::Stdout, b"\na\r\n\n", 1).unwrap());
        assert_eq!(got.iter().map(|x| x.line.as_str()).collect::<Vec<_>>(), ["", "a\r", ""]);
    }

    #[test]
    fn very_long_lines_are_split() {
        let data = vec![b'x'; MAX_LINE * 2 + 10];
        let got = written(|w| {
            w.push(Stream::Stdout, &data, 1).unwrap();
            w.push(Stream::Stdout, b"\n", 1).unwrap();
        });
        let sizes: Vec<_> = got.iter().map(|x| x.line.len()).collect();
        assert_eq!(sizes, [MAX_LINE, MAX_LINE, 10]);
    }

    #[test]
    fn a_line_of_exactly_the_maximum_size_is_one_entry() {
        let mut data = vec![b'y'; MAX_LINE];
        data.push(b'\n');
        let got = written(|w| w.push(Stream::Stdout, &data, 1).unwrap());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].line.len(), MAX_LINE);
    }

    #[test]
    fn invalid_utf8_is_replaced_not_dropped() {
        let got = written(|w| w.push(Stream::Stdout, b"a\xffb\n", 1).unwrap());
        assert_eq!(got[0].line, "a\u{fffd}b");
    }

    #[test]
    fn json_is_escaped_and_roundtrips_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.log");
        let mut w = LogWriter::new(fs::File::create(&path).unwrap());
        w.push(Stream::Stdout, b"say \"hi\"\t\\o/ \xe2\x9c\x93\n", 7).unwrap();
        drop(w);
        // a torn last line is ignored
        fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"ts\":1,\"str").unwrap();
        assert_eq!(read_log(&path).unwrap(), vec![e(7, Stream::Stdout, "say \"hi\"\t\\o/ ✓")]);
    }

    #[test]
    fn a_missing_log_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_log(&dir.path().join("none.log")).unwrap().is_empty());
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_timestamp(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(format_timestamp(951_782_400_000), "2000-02-29T00:00:00.000Z"); // leap day
        assert_eq!(format_timestamp(951_868_800_000), "2000-03-01T00:00:00.000Z");
    }
}
