//! Driving a Linux `yarc` from another OS (or on purpose from Linux, for tests).
//!
//! `yarc.exe` does not run containers: it starts `yarc agent --stdio` inside a
//! WSL distro (`wsl`), sends it the request (`forward`, with the paths of this
//! machine translated by `winpath`), and relays the output and exit status
//! (`session`). `doctor` checks that all of that can work.

pub mod console;
pub mod doctor;
pub mod forward;
pub mod session;
pub mod winpath;
pub mod wsl;

/// Commands are forwarded to the agent on every OS but Linux, or on Linux when
/// `YARC_REMOTE` is set (to try the path without Windows).
pub fn enabled() -> bool {
    cfg!(not(target_os = "linux")) || std::env::var_os("YARC_REMOTE").is_some_and(|v| !v.is_empty())
}
