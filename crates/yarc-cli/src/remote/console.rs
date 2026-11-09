//! Output to a Windows console.
//!
//! While `wsl.exe` runs it leaves the shared console in a mode where a bare
//! `\n` only moves down a line (no return to column 0), so output from Linux
//! staircases. When the destination is a console, line feeds are written as
//! CR LF instead. A redirected stream is left alone.

use std::io::{self, Write};

pub struct Console<W: Write> {
    inner: W,
    crlf: bool,
    /// The previous byte written was `\r`, so a `\n` that follows is already a CR LF.
    after_cr: bool,
}

impl<W: Write> Console<W> {
    /// `crlf`: write `\n` as `\r\n`.
    pub fn new(inner: W, crlf: bool) -> Self {
        Console { inner, crlf, after_cr: false }
    }
}

impl<W: Write> Write for Console<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.crlf {
            return self.inner.write(buf);
        }
        let mut converted = Vec::with_capacity(buf.len() + buf.len() / 16);
        for &b in buf {
            if b == b'\n' && !self.after_cr {
                converted.push(b'\r');
            }
            converted.push(b);
            self.after_cr = b == b'\r';
        }
        self.inner.write_all(&converted)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn through(crlf: bool, chunks: &[&str]) -> String {
        let mut out = Vec::new();
        let mut c = Console::new(&mut out, crlf);
        for chunk in chunks {
            c.write_all(chunk.as_bytes()).unwrap();
        }
        c.flush().unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn line_feeds_become_crlf_on_a_console() {
        assert_eq!(through(true, &["a\nb\n"]), "a\r\nb\r\n");
        assert_eq!(through(true, &["\n\n"]), "\r\n\r\n");
    }

    #[test]
    fn existing_crlf_is_not_doubled_even_across_chunks() {
        assert_eq!(through(true, &["a\r\nb"]), "a\r\nb");
        assert_eq!(through(true, &["a\r", "\nb"]), "a\r\nb");
    }

    #[test]
    fn a_lone_carriage_return_is_kept_for_progress_lines() {
        assert_eq!(through(true, &["10%\r20%\r"]), "10%\r20%\r");
    }

    #[test]
    fn redirected_output_is_untouched() {
        assert_eq!(through(false, &["a\nb\r\n"]), "a\nb\r\n");
    }

    #[test]
    fn multibyte_text_survives() {
        assert_eq!(through(true, &["✓ ok\n"]), "✓ ok\r\n");
    }
}
