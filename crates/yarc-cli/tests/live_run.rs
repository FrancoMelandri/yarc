//! Real containers: needs network (Docker Hub) and unprivileged user namespaces,
//! so run explicitly with `cargo test -p yarc-cli --test live_run -- --ignored`.

#![cfg(target_os = "linux")]

use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

const IMAGE: &str = "alpine:3.20";

fn yarc(root: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_yarc"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("failed to run yarc")
}

/// create + start; returns (exit code, stdout, stderr of the start step).
fn run_container(create_args: &[&str]) -> (i32, String, String) {
    let root = tempfile::tempdir().unwrap();
    let mut args = vec!["create"];
    args.extend_from_slice(create_args);
    let created = yarc(root.path(), &args);
    assert!(created.status.success(), "create failed: {}", String::from_utf8_lossy(&created.stderr));
    let id = String::from_utf8_lossy(&created.stdout).trim().to_string();
    let out = yarc(root.path(), &["container", "start", &id]);
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
#[ignore]
fn runs_a_command_and_streams_its_output() {
    let (code, out, _) = run_container(&[IMAGE, "echo", "hi"]);
    assert_eq!((code, out.as_str()), (0, "hi\n"));
}

#[test]
#[ignore]
fn exit_status_is_passed_through() {
    assert_eq!(run_container(&[IMAGE, "sh", "-c", "exit 7"]).0, 7);
}

#[test]
#[ignore]
fn the_process_is_pid_1_root_with_its_own_hostname_and_image_filesystem() {
    let (code, out, _) = run_container(&[
        IMAGE, "sh", "-c", "echo $$ $(id -u) $(hostname) $(. /etc/os-release; echo $ID)",
    ]);
    assert_eq!(code, 0);
    let fields: Vec<&str> = out.split_whitespace().collect();
    assert_eq!((fields[0], fields[1], fields[3]), ("1", "0", "alpine"));
    assert_eq!(fields[2].len(), 12, "hostname is the short container id: {out}");
}

#[test]
#[ignore]
fn environment_and_working_directory_are_applied() {
    let (_, out, _) = run_container(&["-e", "A=1", "-w", "/tmp/new", IMAGE, "sh", "-c", "echo $A; pwd"]);
    assert_eq!(out, "1\n/tmp/new\n");
}

#[test]
#[ignore]
fn only_the_containers_own_processes_are_visible() {
    let (_, out, _) = run_container(&[IMAGE, "sh", "-c", "ls /proc | grep -c '^[0-9]'"]);
    let n: u32 = out.trim().parse().unwrap();
    assert!(n <= 5, "saw {n} processes, the host's must not be visible");
}

#[test]
#[ignore]
fn standard_device_nodes_work() {
    let (code, out, _) = run_container(&[
        IMAGE, "sh", "-c", "echo x > /dev/null && head -c 8 /dev/urandom | wc -c && test -e /dev/pts && echo ok",
    ]);
    assert_eq!((code, out.split_whitespace().collect::<Vec<_>>()), (0, vec!["8", "ok"]));
}

#[test]
#[ignore]
fn network_none_leaves_only_loopback() {
    let (_, out, _) = run_container(&["--network", "none", IMAGE, "sh", "-c", "tail -n +3 /proc/net/dev | cut -d: -f1"]);
    assert_eq!(out.trim(), "lo");
}

#[test]
#[ignore]
fn a_missing_executable_is_reported_and_fails() {
    let (code, _, err) = run_container(&[IMAGE, "no-such-program"]);
    assert_ne!(code, 0);
    assert!(err.contains("executable file not found"), "{err}");
}

#[test]
#[ignore]
fn a_non_root_user_is_refused_for_now() {
    let (code, _, err) = run_container(&["-u", "nobody", IMAGE, "id"]);
    assert_ne!(code, 0);
    assert!(err.contains("only run as root"), "{err}");
}

#[test]
#[ignore]
fn two_interrupts_stop_a_container_that_ignores_signals() {
    let root = tempfile::tempdir().unwrap();
    let created = yarc(root.path(), &["create", IMAGE, "sleep", "100"]);
    let id = String::from_utf8_lossy(&created.stdout).trim().to_string();
    let mut child: Child = Command::new(env!("CARGO_BIN_EXE_yarc"))
        .arg("--root")
        .arg(root.path())
        .args(["container", "start", &id])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let pid = child.id().to_string();
    for _ in 0..2 {
        Command::new("kill").args(["-INT", &pid]).status().unwrap();
        std::thread::sleep(Duration::from_millis(300));
    }
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(137), "128 + SIGKILL");
}
