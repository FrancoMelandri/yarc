//! The Windows-client path (`YARC_REMOTE=1`) exercised on Linux with a stand-in
//! for `wsl.exe`: a script that accepts `[-d DISTRO] --exec PROGRAM ARGS...` and
//! runs PROGRAM. Needs no network and no user namespaces.

#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

const FAKE_WSL: &str = r#"#!/bin/sh
if [ "$1" = "-d" ]; then shift 2; fi
[ "$1" = "--exec" ] || { echo "fake wsl: expected --exec" >&2; exit 64; }
shift
exec "$@"
"#;

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let wsl = dir.path().join("wsl.exe");
        std::fs::write(&wsl, FAKE_WSL).unwrap();
        std::fs::set_permissions(&wsl, std::fs::Permissions::from_mode(0o755)).unwrap();
        Env { dir }
    }

    fn yarc(&self, args: &[&str]) -> Output {
        self.yarc_with(args, &self.dir.path().join("wsl.exe"))
    }

    fn yarc_with(&self, args: &[&str], launcher: &Path) -> Output {
        Command::new(env!("CARGO_BIN_EXE_yarc"))
            .env("YARC_REMOTE", "1")
            .env("YARC_WSL_EXE", launcher)
            .env("YARC_WSL_DISTRO", "Ubuntu-24.04")
            .env("YARC_WSL_BIN", env!("CARGO_BIN_EXE_yarc"))
            .env_remove("YARC_ROOT")
            .args(args)
            .output()
            .unwrap()
    }

    fn root(&self) -> String {
        self.dir.path().join("store").display().to_string()
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn commands_run_in_the_agent_with_their_output_and_status() {
    let e = Env::new();
    let root = e.root();
    let out = e.yarc(&["--root", &root, "image", "ls"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).starts_with("IMAGE"), "{}", text(&out.stdout));
    assert!(Path::new(&root).join("blobs").is_dir(), "the agent used the store it was given");

    let out = e.yarc(&["--root", &root, "image", "rm", "nope:1"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("not in the local store"), "{}", text(&out.stderr));
}

#[test]
fn project_files_are_sent_as_absolute_paths() {
    let e = Env::new();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("yarc.yaml"), "services:\n  web:\n    image: alpine:3.20\n").unwrap();
    // `down` with nothing running: succeeds, and proves the relative default file was found
    let out = Command::new(env!("CARGO_BIN_EXE_yarc"))
        .current_dir(project.path())
        .env("YARC_REMOTE", "1")
        .env("YARC_WSL_EXE", e.dir.path().join("wsl.exe"))
        .env("YARC_WSL_BIN", env!("CARGO_BIN_EXE_yarc"))
        .args(["--root", &e.root(), "down"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("nothing to remove"), "{}", text(&out.stderr));
}

#[test]
fn project_config_is_handled_without_the_agent() {
    let e = Env::new();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("yarc.yaml"), "services:\n  web:\n    image: alpine:3.20\n").unwrap();
    let file = project.path().join("yarc.yaml").display().to_string();
    let out = e.yarc_with(&["--file", &file, "-p", "demo", "project", "config"], Path::new("/nonexistent/wsl.exe"));
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("project: demo"), "{}", text(&out.stdout));
}

#[test]
fn an_unusable_wsl_is_explained() {
    let e = Env::new();
    let out = e.yarc_with(&["--root", &e.root(), "ps"], Path::new("/nonexistent/wsl.exe"));
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(err.contains("cannot run /nonexistent/wsl.exe") && err.contains("yarc doctor"), "{err}");

    // the launcher works but there is no yarc to run behind it
    let out = Command::new(env!("CARGO_BIN_EXE_yarc"))
        .env("YARC_REMOTE", "1")
        .env("YARC_WSL_EXE", e.dir.path().join("wsl.exe"))
        .env("YARC_WSL_BIN", "/nonexistent/yarc")
        .args(["--root", &e.root(), "ps"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("yarc doctor"), "{}", text(&out.stderr));
}

#[test]
fn doctor_reports_each_check() {
    let e = Env::new();
    let out = e.yarc(&["doctor"]);
    let report = text(&out.stdout);
    assert!(report.contains("] WSL distro"), "{report}");
    assert!(report.contains("[ok] yarc inside WSL"), "{report}");
    assert!(report.contains("user namespaces") && report.contains("pasta"), "{report}");

    let out = e.yarc_with(&["doctor"], Path::new("/nonexistent/wsl.exe"));
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stdout).contains("[xx] WSL distro"), "{}", text(&out.stdout));
}
