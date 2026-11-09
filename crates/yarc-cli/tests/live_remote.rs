//! A project driven through the forwarding path (fake `wsl.exe`, `YARC_REMOTE=1`)
//! with real containers: needs user namespaces (image from the shared cache), so
//! `cargo test -p yarc-cli --test live_remote -- --ignored --test-threads=1`.

#![cfg(target_os = "linux")]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const FAKE_WSL: &str = "#!/bin/sh\nif [ \"$1\" = \"-d\" ]; then shift 2; fi\nshift\nexec \"$@\"\n";

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
#[ignore]
fn up_ps_and_down_through_the_forwarding_path() {
    let root = common::seeded_root();
    let dir = tempfile::tempdir().unwrap();
    let wsl = dir.path().join("wsl.exe");
    std::fs::write(&wsl, FAKE_WSL).unwrap();
    std::fs::set_permissions(&wsl, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        dir.path().join("yarc.yaml"),
        "services:\n  web:\n    image: alpine:3.20\n    cmd: [sh, -c, 'while true; do sleep 0.2; done']\n",
    )
    .unwrap();

    let yarc = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_yarc"))
            .current_dir(dir.path()) // the relative default yarc.yaml must be found
            .env("YARC_REMOTE", "1")
            .env("YARC_WSL_EXE", &wsl)
            .env("YARC_WSL_BIN", env!("CARGO_BIN_EXE_yarc"))
            .arg("--root")
            .arg(root.path())
            .args(["-p", "remotelive"])
            .args(args)
            .output()
            .unwrap()
    };

    let up = yarc(&["up", "-d"]);
    assert_eq!(up.status.code(), Some(0), "{}", text(&up.stderr));
    assert!(text(&up.stderr).contains("web"), "{}", text(&up.stderr));

    let deadline = Instant::now() + Duration::from_secs(20);
    let ps = loop {
        let ps = text(&yarc(&["project", "ps"]).stdout);
        if ps.contains("Running") || Instant::now() > deadline {
            break ps;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(ps.contains("remotelive-web-1") && ps.contains("Running"), "{ps}");

    let down = yarc(&["down", "-t", "2"]);
    assert_eq!(down.status.code(), Some(0), "{}", text(&down.stderr));
    assert!(!text(&yarc(&["project", "ps"]).stdout).contains("Running"));
}

#[test]
#[ignore]
fn ctrl_c_stops_a_followed_log_with_the_exit_status_of_a_terminal_interrupt() {
    use std::process::Stdio;

    let root = common::seeded_root();
    let dir = tempfile::tempdir().unwrap();
    let wsl = dir.path().join("wsl.exe");
    std::fs::write(&wsl, FAKE_WSL).unwrap();
    std::fs::set_permissions(&wsl, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        dir.path().join("yarc.yaml"),
        "services:\n  web:\n    image: alpine:3.20\n    cmd: [sh, -c, 'while true; do echo tick; sleep 0.2; done']\n",
    )
    .unwrap();
    let command = |args: &[&str]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_yarc"));
        c.current_dir(dir.path())
            .env("YARC_REMOTE", "1")
            .env("YARC_WSL_EXE", &wsl)
            .env("YARC_WSL_BIN", env!("CARGO_BIN_EXE_yarc"))
            .arg("--root")
            .arg(root.path())
            .args(["-p", "ctrlc"])
            .args(args);
        c
    };

    assert_eq!(command(&["up", "-d"]).output().unwrap().status.code(), Some(0));

    // follow the log, wait until it streams, then Ctrl-C the client
    let mut logs = command(&["project", "logs", "-f"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut seen = String::new();
    {
        use std::io::Read;
        let stdout = logs.stdout.as_mut().unwrap();
        let mut buf = [0u8; 256];
        let deadline = Instant::now() + Duration::from_secs(20);
        while !seen.contains("tick") && Instant::now() < deadline {
            let n = stdout.read(&mut buf).unwrap();
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    }
    assert!(seen.contains("tick"), "the log never started: {seen:?}");
    Command::new("kill").args(["-INT", &logs.id().to_string()]).status().unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(s) = logs.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "yarc did not stop after Ctrl-C");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(status.code(), Some(130), "the command was interrupted like at a terminal");

    assert_eq!(command(&["down", "-t", "2"]).output().unwrap().status.code(), Some(0));
}
