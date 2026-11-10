//! Starting the agent inside a WSL distro with `wsl.exe`.
//!
//! Settings come from the environment:
//! - `YARC_WSL_DISTRO`: the distro [default: WSL's default distro];
//! - `YARC_WSL_BIN`: the Linux `yarc` to run, an absolute path or a name on the
//!   distro's PATH [default: `yarc`];
//! - `YARC_WSL_EXE`: the launcher [default: `wsl.exe`]; for tests, a script
//!   that accepts the same arguments.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use super::console::Console;
use super::forward::Forward;
use super::session;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wsl {
    pub exe: OsString,
    pub distro: Option<String>,
    pub bin: String,
}

impl Wsl {
    pub fn from_env() -> Wsl {
        Wsl::from_lookup(|k| std::env::var_os(k))
    }

    fn from_lookup(env: impl Fn(&str) -> Option<OsString>) -> Wsl {
        let text = |k: &str| env(k).filter(|v| !v.is_empty());
        Wsl {
            exe: text("YARC_WSL_EXE").unwrap_or_else(|| "wsl.exe".into()),
            distro: text("YARC_WSL_DISTRO").map(|d| d.to_string_lossy().into_owned()),
            bin: text("YARC_WSL_BIN").map(|b| b.to_string_lossy().into_owned()).unwrap_or_else(|| "yarc".into()),
        }
    }

    /// `wsl.exe [-d distro] --exec <program> <args...>`: no shell in between,
    /// so no quoting.
    pub fn command(&self, program: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.exe);
        if let Some(d) = &self.distro {
            cmd.arg("-d").arg(d);
        }
        cmd.arg("--exec").arg(program).args(args);
        cmd
    }

    /// Runs the request through a fresh agent and returns the command's exit status.
    ///
    /// Ctrl-C (and closing the terminal) is forwarded to the agent as a `Cancel`,
    /// which interrupts the command exactly as at a Linux terminal. A third
    /// Ctrl-C gives up on the agent and exits at once.
    pub fn run(&self, request: &Forward) -> Result<i32> {
        let mut cmd = self.command(&self.bin, &["agent", "--stdio"]);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
        // On Windows `wsl.exe` gets a console of its own. Sharing ours, it changes
        // its input mode (Ctrl-C then stops being a signal) and its output mode
        // (bare line feeds staircase). Its stderr is relayed by `relay_stderr`.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd.stderr(if cfg!(windows) { Stdio::piped() } else { Stdio::inherit() });
        let mut child = cmd.spawn().with_context(|| {
            format!("cannot run {}; is WSL installed? (see `yarc doctor`)", self.exe.to_string_lossy())
        })?;

        let input = Arc::new(Mutex::new(Some(child.stdin.take().expect("stdin is piped"))));
        let stdout = std::io::BufReader::new(child.stdout.take().expect("stdout is piped"));
        let relay = child.stderr.take().map(|e| std::thread::spawn(move || relay_stderr(e)));

        let interrupts = Arc::new(AtomicUsize::new(0));
        {
            let (input, interrupts) = (Arc::clone(&input), Arc::clone(&interrupts));
            // an error only means a handler is already installed
            let _ = ctrlc::set_handler(move || {
                if interrupts.fetch_add(1, Ordering::SeqCst) >= 2 {
                    std::process::exit(130);
                }
                session::cancel(&input);
            });
        }

        // on a Windows console a bare line feed would staircase (see `console`)
        let crlf = |terminal: bool| cfg!(windows) && terminal;
        let mut out = Console::new(std::io::stdout().lock(), crlf(std::io::stdout().is_terminal()));
        let mut err = Console::new(std::io::stderr().lock(), crlf(std::io::stderr().is_terminal()));
        let result = session::run(stdout, &input, request, &mut out, &mut err);
        session::close(&input); // the agent ends when its input closes
        let _ = child.wait();
        if let Some(relay) = relay {
            let _ = relay.join();
        }
        result
    }
}

/// Copies what `wsl.exe` itself says on stderr (distro not found, ...) to ours.
fn relay_stderr(mut from: std::process::ChildStderr) {
    use std::io::{Read, Write};

    let crlf = cfg!(windows) && std::io::stderr().is_terminal();
    let mut to = Console::new(std::io::stderr(), crlf);
    let mut buf = [0u8; 4096];
    while let Ok(n) = from.read(&mut buf) {
        if n == 0 {
            break;
        }
        let _ = to.write_all(wsl_text(&buf[..n]).as_bytes());
        let _ = to.flush();
    }
}

/// `wsl.exe` writes its own messages as UTF-16LE when its output is a pipe; what
/// the Linux side writes is UTF-8.
pub fn wsl_text(bytes: &[u8]) -> String {
    let utf16 = bytes.len() >= 2 && (bytes[1] == 0 || bytes.starts_with(&[0xFF, 0xFE]));
    if !utf16 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
    String::from_utf16_lossy(&units).replace(['\u{feff}', '\0'], "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| OsString::from(v))
    }

    fn argv(c: &Command) -> Vec<String> {
        c.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn defaults() {
        let w = Wsl::from_lookup(env(&[]));
        assert_eq!((w.exe.to_str(), w.distro.as_deref(), w.bin.as_str()), (Some("wsl.exe"), None, "yarc"));
        assert_eq!(argv(&w.command(&w.bin, &["agent", "--stdio"])), ["--exec", "yarc", "agent", "--stdio"]);
    }

    #[test]
    fn the_environment_selects_distro_binary_and_launcher() {
        let w = Wsl::from_lookup(env(&[
            ("YARC_WSL_DISTRO", "Ubuntu-24.04"),
            ("YARC_WSL_BIN", "/home/me/yarc-target/debug/yarc"),
            ("YARC_WSL_EXE", "/tmp/fake-wsl"),
        ]));
        let cmd = w.command(&w.bin, &["agent", "--stdio"]);
        assert_eq!(cmd.get_program(), "/tmp/fake-wsl");
        assert_eq!(argv(&cmd), ["-d", "Ubuntu-24.04", "--exec", "/home/me/yarc-target/debug/yarc", "agent", "--stdio"]);
    }

    #[test]
    fn empty_variables_count_as_unset() {
        let w = Wsl::from_lookup(env(&[("YARC_WSL_DISTRO", ""), ("YARC_WSL_BIN", ""), ("YARC_WSL_EXE", "")]));
        assert_eq!(w, Wsl::from_lookup(env(&[])));
    }
}

#[cfg(test)]
mod text_tests {
    use super::wsl_text;

    #[test]
    fn utf8_passes_through() {
        assert_eq!(wsl_text("plain ✓\n".as_bytes()), "plain ✓\n");
        assert_eq!(wsl_text(b"x"), "x");
    }

    #[test]
    fn utf16_messages_from_wsl_are_decoded() {
        let utf16: Vec<u8> = "There is no distribution\r\n".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(wsl_text(&utf16), "There is no distribution\r\n");
        let mut with_bom = vec![0xFF, 0xFE];
        with_bom.extend("hi".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(wsl_text(&with_bom), "hi");
    }
}
