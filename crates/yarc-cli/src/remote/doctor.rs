//! `yarc doctor`: is this machine (or its WSL distro) able to run containers?

use std::io;
use std::process::Command;

use super::wsl::Wsl;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// Works, but something is degraded.
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

/// Where the probes run: a shell on this machine or in the WSL distro.
pub trait Runner {
    /// Runs `script` (with `$0` set to `arg`); the success flag and the output.
    fn sh(&self, script: &str, arg: &str) -> io::Result<(bool, String)>;
}

pub struct Local;

impl Runner for Local {
    fn sh(&self, script: &str, arg: &str) -> io::Result<(bool, String)> {
        finish(Command::new("sh").args(["-c", script, arg]).output()?)
    }
}

impl Runner for Wsl {
    fn sh(&self, script: &str, arg: &str) -> io::Result<(bool, String)> {
        finish(self.command("sh", &["-c", script, arg]).output()?)
    }
}

fn finish(out: std::process::Output) -> io::Result<(bool, String)> {
    let mut text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        text = String::from_utf8_lossy(&out.stderr).trim().to_string();
    }
    Ok((out.status.success(), text))
}

/// Runs all checks. `bin` is the `yarc` to look for on the Linux side;
/// `remote` adds the check that the distro can be reached at all.
pub fn checks(runner: &dyn Runner, bin: &str, remote: bool) -> Vec<Check> {
    let mut out = Vec::new();

    if remote {
        let reached = probe(
            runner,
            "WSL distro",
            "uname -sr",
            "",
            Status::Fail,
            "install WSL 2 and a distro (`wsl --install`), or pick one with YARC_WSL_DISTRO",
        );
        let unreachable = reached.status == Status::Fail;
        out.push(reached);
        if unreachable {
            return out; // nothing else can be checked
        }
    }

    let mut version = probe(
        runner,
        "yarc inside WSL",
        "\"$0\" --version",
        bin,
        Status::Fail,
        "build yarc inside WSL and point YARC_WSL_BIN at it (an absolute path)",
    );
    let ours = format!("yarc {}", env!("CARGO_PKG_VERSION"));
    if version.status == Status::Ok && version.detail != ours {
        version.detail =
            format!("{} (this yarc is {ours}; the protocol version is checked on connect)", version.detail);
        version.status = Status::Warn;
    }
    out.push(version);

    out.push(probe(
        runner,
        "user namespaces",
        "unshare -Ur true",
        "",
        Status::Fail,
        "containers need unprivileged user namespaces (kernel.unprivileged_userns_clone / AppArmor settings)",
    ));
    out.push(probe(
        runner,
        "pasta (outbound network)",
        "command -v pasta",
        "",
        Status::Warn,
        "install the 'passt' package for outbound networking and DNS in project networks; without it services only see each other",
    ));
    out
}

/// One probe: `Ok` with the command's output, or `on_fail` with the output and the hint.
fn probe(runner: &dyn Runner, name: &'static str, script: &str, arg: &str, on_fail: Status, hint: &str) -> Check {
    match runner.sh(script, arg) {
        Ok((true, text)) => Check { name, status: Status::Ok, detail: text },
        Ok((false, text)) => Check { name, status: on_fail, detail: detail(&text, hint) },
        Err(e) => Check { name, status: on_fail, detail: detail(&e.to_string(), hint) },
    }
}

fn detail(output: &str, hint: &str) -> String {
    if output.is_empty() {
        hint.to_string()
    } else {
        format!("{output} - {hint}")
    }
}

pub fn render(checks: &[Check]) -> String {
    let mut out = String::new();
    for c in checks {
        let mark = match c.status {
            Status::Ok => "ok",
            Status::Warn => "!!",
            Status::Fail => "xx",
        };
        out.push_str(&format!("[{mark}] {:<26} {}\n", c.name, c.detail));
    }
    out
}

/// 0 unless something required is missing.
pub fn exit_code(checks: &[Check]) -> i32 {
    i32::from(checks.iter().any(|c| c.status == Status::Fail))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers by the script it is given: `(success, output)`.
    struct Fake(Vec<(&'static str, bool, &'static str)>);

    impl Runner for Fake {
        fn sh(&self, script: &str, _arg: &str) -> io::Result<(bool, String)> {
            match self.0.iter().find(|(s, _, _)| *s == script) {
                Some((_, ok, text)) => Ok((*ok, text.to_string())),
                None => Err(io::Error::new(io::ErrorKind::NotFound, "wsl.exe not found")),
            }
        }
    }

    fn healthy() -> Fake {
        Fake(vec![
            ("uname -sr", true, "Linux 6.6.0-microsoft-standard-WSL2"),
            ("\"$0\" --version", true, concat!("yarc ", env!("CARGO_PKG_VERSION"))),
            ("unshare -Ur true", true, ""),
            ("command -v pasta", true, "/usr/bin/pasta"),
        ])
    }

    fn status(checks: &[Check], name: &str) -> Status {
        checks.iter().find(|c| c.name == name).unwrap().status
    }

    #[test]
    fn a_healthy_distro_passes_everything() {
        let c = checks(&healthy(), "yarc", true);
        assert!(c.iter().all(|c| c.status == Status::Ok), "{c:?}");
        assert_eq!(exit_code(&c), 0);
        assert!(render(&c).contains("[ok] WSL distro"));
    }

    #[test]
    fn an_unreachable_distro_stops_the_checks_with_a_hint() {
        let c = checks(&Fake(vec![]), "yarc", true);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].status, Status::Fail);
        assert!(c[0].detail.contains("wsl --install"), "{}", c[0].detail);
        assert_eq!(exit_code(&c), 1);
    }

    #[test]
    fn missing_pasta_is_only_a_warning() {
        let mut fake = healthy();
        fake.0.retain(|(s, _, _)| *s != "command -v pasta");
        fake.0.push(("command -v pasta", false, ""));
        let c = checks(&fake, "yarc", true);
        assert_eq!(status(&c, "pasta (outbound network)"), Status::Warn);
        assert_eq!(exit_code(&c), 0);
    }

    #[test]
    fn missing_binary_or_user_namespaces_fail() {
        let mut fake = healthy();
        fake.0.retain(|(s, _, _)| !s.contains("--version") && !s.contains("unshare"));
        fake.0.push(("\"$0\" --version", false, "sh: yarc: not found"));
        fake.0.push(("unshare -Ur true", false, "Operation not permitted"));
        let c = checks(&fake, "yarc", true);
        assert_eq!(status(&c, "yarc inside WSL"), Status::Fail);
        assert!(c.iter().find(|c| c.name == "yarc inside WSL").unwrap().detail.contains("YARC_WSL_BIN"));
        assert_eq!(status(&c, "user namespaces"), Status::Fail);
        assert_eq!(exit_code(&c), 1);
    }

    #[test]
    fn another_yarc_version_is_a_warning_naming_both() {
        let mut fake = healthy();
        fake.0.retain(|(s, _, _)| !s.contains("--version"));
        fake.0.push(("\"$0\" --version", true, "yarc 9.9.9"));
        let c = checks(&fake, "yarc", true);
        assert_eq!(status(&c, "yarc inside WSL"), Status::Warn);
        let d = &c.iter().find(|c| c.name == "yarc inside WSL").unwrap().detail;
        assert!(d.contains("9.9.9") && d.contains(env!("CARGO_PKG_VERSION")), "{d}");
    }

    #[test]
    fn natively_on_linux_the_distro_check_is_skipped() {
        let c = checks(&healthy(), "/usr/bin/yarc", false);
        assert!(c.iter().all(|c| c.name != "WSL distro"));
    }
}
