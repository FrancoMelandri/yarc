//! Detached containers.
//!
//! `yarc run -d` / `yarc container start -d` spawn `yarc __shim <id>` in a new
//! session and return as soon as the shim reports on its stdout whether the
//! container's command is running (`started <pid>`) or could not start
//! (`error <code> <message>`). The shim then stays alive as the container's
//! supervisor: it holds the container's lock, records the output and state
//! (`run_recorded`), and removes the container afterwards with `--rm`.
//! Its own diagnostics go to `containers/<id>/shim.log`.

use std::path::Path;

use anyhow::Result;
use yarc_runtime::RuntimeError;

use crate::cli::ShimArgs;

/// Interprets the single line a shim prints before detaching.
fn parse_report(line: &str) -> std::result::Result<u32, RuntimeError> {
    let line = line.trim();
    if let Some(pid) = line.strip_prefix("started ").and_then(|p| p.parse().ok()) {
        return Ok(pid);
    }
    if let Some(rest) = line.strip_prefix("error ") {
        if let Some((code, msg)) = rest.split_once(' ').and_then(|(c, m)| Some((c.parse().ok()?, m))) {
            return Err(RuntimeError::Start { msg: msg.to_string(), code });
        }
    }
    Err(RuntimeError::start("the shim exited before reporting (see shim.log in the container directory)"))
}

#[cfg(target_os = "linux")]
pub fn start(root: &Path, id: &str, debug: bool, rm: bool) -> Result<()> {
    use std::fs::OpenOptions;
    use std::io::{self, BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    use anyhow::Context;
    use yarc_runtime::ContainerStore;

    let containers = ContainerStore::open(root)?;
    let spec = containers.load(id)?;
    if !spec.rootfs.is_dir() {
        anyhow::bail!("the root filesystem of container {id} is missing ({})", spec.rootfs.display());
    }
    if containers.is_supervised(id)? {
        return Err(RuntimeError::AlreadyRunning(id.to_string()).into());
    }

    let shim_log_path = containers.container_dir(id).join("shim.log");
    let shim_log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&shim_log_path)
        .with_context(|| format!("cannot open {}", shim_log_path.display()))?;
    let mut cmd = Command::new(std::env::current_exe().context("cannot locate the yarc executable")?);
    cmd.arg("--root").arg(root).arg("__shim").arg(id);
    if debug {
        cmd.arg("--debug");
    }
    if rm {
        cmd.arg("--rm");
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(shim_log);
    // SAFETY: setsid(2) is async-signal-safe. A new session means no controlling
    // terminal: closing the terminal or pressing Ctrl-C must not reach the shim.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("cannot start the shim")?;

    let mut report = String::new();
    let stdout = child.stdout.take().expect("stdout is piped");
    BufReader::new(stdout).read_line(&mut report).context("cannot read the shim's report")?;
    match parse_report(&report) {
        Ok(_) => Ok(()), // the shim carries on in the background
        Err(err) => {
            let _ = child.wait(); // it exits right after reporting a failure
            Err(err.into())
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn start(_root: &Path, _id: &str, _debug: bool, _rm: bool) -> Result<()> {
    anyhow::bail!("running containers needs Linux; run yarc inside WSL")
}

/// Entry point of the hidden `__shim` command.
#[cfg(target_os = "linux")]
pub fn shim(root: &Path, args: ShimArgs) -> Result<()> {
    use std::cell::Cell;
    use std::io::{self, Write};

    use yarc_runtime::{run_recorded_with, ContainerStore, Launch};

    let containers = ContainerStore::open(root)?;
    let spec = containers.load(&args.id)?;

    let reported = Cell::new(false);
    let report = |line: String| {
        reported.set(true);
        // The CLI may already have gone away; there is nobody to tell then.
        let mut out = io::stdout();
        let _ = writeln!(out, "{}", line.replace('\n', " "));
        let _ = out.flush();
    };

    let launch = Launch { debug: args.debug, detached: true };
    let result = run_recorded_with(&containers, &spec, &launch, |started| {
        report(match started {
            Ok(pid) => format!("started {pid}"),
            Err(RuntimeError::Start { msg, code }) => format!("error {code} {msg}"),
            Err(other) => format!("error 125 {other}"),
        })
    });

    // Never remove a container that another supervisor is running.
    let owned = !matches!(result, Err(RuntimeError::AlreadyRunning(_)));
    if args.rm && owned {
        if let Err(err) = containers.remove(&args.id) {
            eprintln!("warning: could not remove container {}: {err}", args.id);
        }
    }
    match result {
        Ok(_) => Ok(()),
        Err(err) => {
            if !reported.get() {
                report(format!("error 125 {err}"));
            }
            Err(err.into())
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn shim(_root: &Path, _args: ShimArgs) -> Result<()> {
    anyhow::bail!("running containers needs Linux; run yarc inside WSL")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn started_report() {
        assert_eq!(parse_report("started 4242\n").unwrap(), 4242);
    }

    #[test]
    fn error_report_keeps_code_and_message() {
        let RuntimeError::Start { msg, code } = parse_report("error 127 executable file not found: x\n").unwrap_err() else { panic!() };
        assert_eq!((code, msg.as_str()), (127, "executable file not found: x"));
    }

    #[test]
    fn missing_or_garbled_reports_are_a_generic_failure() {
        for bad in ["", "\n", "started x", "error", "error abc msg", "hello"] {
            let RuntimeError::Start { code, msg } = parse_report(bad).unwrap_err() else { panic!("{bad}") };
            assert_eq!(code, 125, "{bad}");
            assert!(msg.contains("shim"), "{bad}: {msg}");
        }
    }
}
