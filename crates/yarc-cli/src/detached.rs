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

/// Delay between restarts: 1 s doubling to 60 s. The hidden `YARC_RESTART_BACKOFF_MS`
/// replaces the initial delay (and scales the cap and the reset), for tests.
#[cfg(target_os = "linux")]
fn restart_backoff() -> yarc_runtime::Backoff {
    use std::time::Duration;
    match std::env::var("YARC_RESTART_BACKOFF_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
        Some(ms) if ms > 0 => yarc_runtime::Backoff::with_initial(Duration::from_millis(ms)),
        _ => yarc_runtime::Backoff::default(),
    }
}

/// Entry point of the hidden `__shim` command.
#[cfg(target_os = "linux")]
pub fn shim(root: &Path, args: ShimArgs) -> Result<()> {
    use std::cell::Cell;
    use std::io::{self, Write};

    use yarc_runtime::{supervise, ContainerStore, Launch};

    let containers = ContainerStore::open(root)?;
    let spec = containers.load(&args.id)?;
    if args.rm && !spec.restart.is_no() {
        anyhow::bail!("container {} has a restart policy ({}) and cannot be removed on exit", args.id, spec.restart);
    }

    let reported = Cell::new(false);
    let report = |line: String| {
        reported.set(true);
        // The CLI may already have gone away; there is nobody to tell then.
        let mut out = io::stdout();
        let _ = writeln!(out, "{}", line.replace('\n', " "));
        let _ = out.flush();
    };

    let launch = Launch { debug: args.debug, detached: true };
    let result = supervise(&containers, &spec, &launch, restart_backoff(), |started| {
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

/// `8080:80` → host port 8080, container port 80.
pub fn parse_publish(s: &str) -> Result<yarc_runtime::PortMap> {
    let parse = |p: &str| p.parse::<u16>().ok().filter(|n| *n != 0);
    match s.split_once(':').and_then(|(h, c)| Some((parse(h)?, parse(c)?))) {
        Some((host, container)) => Ok(yarc_runtime::PortMap { host, container }),
        None => anyhow::bail!("invalid port mapping '{s}', expected HOST:CONTAINER"),
    }
}

/// Starts the project's network holder (`yarc __holder`) detached and waits
/// until it reports `ready`, or the reason it could not start (e.g. a host
/// port that is already in use).
#[cfg(target_os = "linux")]
pub fn start_holder(root: &Path, project: &str, publish: &[yarc_runtime::PortMap], pasta: Option<&Path>) -> Result<()> {
    use std::fs::OpenOptions;
    use std::io::{self, BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    use anyhow::Context;

    let net = yarc_runtime::ProjectNet::new(root, project)?;
    net.create_dir()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(net.log_path())
        .with_context(|| format!("cannot open {}", net.log_path().display()))?;
    let mut cmd = Command::new(std::env::current_exe().context("cannot locate the yarc executable")?);
    cmd.arg("--root").arg(root).arg("__holder").arg(project);
    for p in publish {
        cmd.arg("--publish").arg(format!("{}:{}", p.host, p.container));
    }
    if let Some(pasta) = pasta {
        cmd.arg("--pasta").arg(pasta);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(log);
    // SAFETY: setsid(2) is async-signal-safe; the holder must outlive this
    // command and the terminal it was started from.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("cannot start the project network holder")?;
    let mut report = String::new();
    BufReader::new(child.stdout.take().expect("stdout is piped"))
        .read_line(&mut report)
        .context("cannot read the holder's report")?;
    match report.trim() {
        "ready" => Ok(()),
        line => {
            let _ = child.wait();
            match line.strip_prefix("error ") {
                Some(msg) => anyhow::bail!("{msg}"),
                None => anyhow::bail!(
                    "the project network holder exited before reporting (see {})",
                    net.log_path().display()
                ),
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn start_holder(
    _root: &Path,
    _project: &str,
    _publish: &[yarc_runtime::PortMap],
    _pasta: Option<&Path>,
) -> Result<()> {
    anyhow::bail!("running projects needs Linux; run yarc inside WSL")
}

/// Entry point of the hidden `__holder` command.
#[cfg(target_os = "linux")]
pub fn holder(root: &Path, args: crate::cli::HolderArgs) -> Result<()> {
    use std::io::{self, Write};

    let net = yarc_runtime::ProjectNet::new(root, &args.project)?;
    let publish = args.publish.iter().map(|p| parse_publish(p)).collect::<Result<Vec<_>>>()?;
    yarc_runtime::run_holder(&net, &publish, args.pasta.as_deref(), |outcome| {
        let mut out = io::stdout();
        let _ = match outcome {
            Ok(()) => writeln!(out, "ready"),
            Err(msg) => writeln!(out, "error {}", msg.replace('\n', " ")),
        };
        let _ = out.flush();
    })?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn holder(_root: &Path, _args: crate::cli::HolderArgs) -> Result<()> {
    anyhow::bail!("running projects needs Linux; run yarc inside WSL")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_arguments() {
        let ok = parse_publish("8080:80").unwrap();
        assert_eq!((ok.host, ok.container), (8080, 80));
        for bad in ["80", "a:b", "0:80", "80:0", "70000:1", "1:2:3", ":80", "80:", ""] {
            assert!(parse_publish(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn started_report() {
        assert_eq!(parse_report("started 4242\n").unwrap(), 4242);
    }

    #[test]
    fn error_report_keeps_code_and_message() {
        let RuntimeError::Start { msg, code } = parse_report("error 127 executable file not found: x\n").unwrap_err()
        else {
            panic!()
        };
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

/// Where to find `pasta`: `YARC_PASTA` (an empty value turns outbound networking
/// off), otherwise the first `pasta` on `PATH`.
pub fn find_pasta() -> Option<std::path::PathBuf> {
    locate_pasta(std::env::var_os("YARC_PASTA"), std::env::var_os("PATH"))
}

fn locate_pasta(explicit: Option<std::ffi::OsString>, path: Option<std::ffi::OsString>) -> Option<std::path::PathBuf> {
    if let Some(p) = explicit {
        return (!p.is_empty()).then(|| std::path::PathBuf::from(p));
    }
    std::env::split_paths(&path?).map(|d| d.join("pasta")).find(|c| c.is_file())
}

#[cfg(test)]
mod pasta_tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn explicit_path_wins_and_empty_disables() {
        assert_eq!(locate_pasta(Some("/x/pasta".into()), Some("/bin".into())), Some("/x/pasta".into()));
        assert_eq!(locate_pasta(Some(OsString::new()), Some("/bin".into())), None);
    }

    #[test]
    fn path_is_searched_for_a_file_named_pasta() {
        let dir = std::env::temp_dir().join(format!("yarc-pasta-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(locate_pasta(None, Some(dir.clone().into_os_string())), None);
        std::fs::write(dir.join("pasta"), "").unwrap();
        assert_eq!(locate_pasta(None, Some(dir.clone().into_os_string())), Some(dir.join("pasta")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
