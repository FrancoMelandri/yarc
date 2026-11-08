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
    assert_eq!(code, 127, "{err}");
    assert!(err.contains("executable file not found"), "{err}");
}

#[test]
#[ignore]
fn a_file_that_is_not_executable_gives_126() {
    // /etc/passwd exists but has no execute bit.
    let (code, _, err) = run_container(&[IMAGE, "/etc/passwd"]);
    assert_eq!(code, 126, "{err}");
}

#[test]
#[ignore]
fn run_executes_and_with_rm_leaves_nothing_behind() {
    let root = tempfile::tempdir().unwrap();
    let out = yarc(root.path(), &["run", "--rm", IMAGE, "sh", "-c", "echo hello; exit 3"]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello\n");
    let ls = yarc(root.path(), &["container", "ls"]);
    assert_eq!(String::from_utf8_lossy(&ls.stdout).lines().count(), 1, "only the header");
}

#[test]
#[ignore]
fn run_without_rm_keeps_the_container_and_with_rm_cleans_up_failed_starts() {
    let root = tempfile::tempdir().unwrap();
    let out = yarc(root.path(), &["run", "--name", "kept", IMAGE, "true"]);
    assert_eq!(out.status.code(), Some(0));
    let ls = String::from_utf8_lossy(&yarc(root.path(), &["container", "ls"]).stdout).into_owned();
    assert!(ls.contains("kept"), "{ls}");

    let out = yarc(root.path(), &["run", "--rm", "--name", "gone", IMAGE, "no-such-program"]);
    assert_eq!(out.status.code(), Some(127));
    let ls = String::from_utf8_lossy(&yarc(root.path(), &["container", "ls"]).stdout).into_owned();
    assert!(!ls.contains("gone"), "{ls}");
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

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
#[ignore]
fn output_is_recorded_per_stream_and_status_is_listed() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    let out = yarc(r, &["run", "--name", "l", IMAGE, "sh", "-c", "echo out; echo err >&2; printf partial; exit 4"]);
    assert_eq!(out.status.code(), Some(4));
    // it was also shown live
    assert_eq!(text(&out.stdout), "out\npartial");
    assert_eq!(text(&out.stderr).trim_end().lines().last(), Some("err"));

    let logs = yarc(r, &["container", "logs", "l"]);
    assert_eq!(text(&logs.stdout), "out\npartial\n");
    assert_eq!(text(&logs.stderr), "err\n");
    let tail = yarc(r, &["container", "logs", "--tail", "1", "l"]);
    assert_eq!(text(&tail.stdout), "partial\n");
    let stamped = yarc(r, &["container", "logs", "-t", "l"]);
    let first = text(&stamped.stdout).lines().next().unwrap().to_string();
    assert!(first.len() > 25 && first.contains('T') && first.ends_with(" out"), "{first}");

    let ls = text(&yarc(r, &["container", "ls"]).stdout);
    assert!(ls.contains("Exited (4)"), "{ls}");

    // starting again appends to the same log
    yarc(r, &["container", "start", "l"]);
    let logs = yarc(r, &["container", "logs", "l"]);
    assert_eq!(text(&logs.stdout), "out\npartial\nout\npartial\n");

    let state = std::fs::read_to_string(r.join("containers/l/state.json")).unwrap();
    assert!(state.contains("\"status\": \"exited\"") && state.contains("\"exit_code\": 4"), "{state}");
}

#[test]
#[ignore]
fn a_failed_start_is_recorded_as_exited() {
    let root = tempfile::tempdir().unwrap();
    let out = yarc(root.path(), &["run", "--name", "f", IMAGE, "no-such-program"]);
    assert_eq!(out.status.code(), Some(127));
    let ls = text(&yarc(root.path(), &["container", "ls"]).stdout);
    assert!(ls.contains("Exited (127)"), "{ls}");
}

#[test]
#[ignore]
fn dead_and_already_running_containers_are_detected() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    yarc(r, &["create", "--name", "d", IMAGE, "true"]);
    let state_path = r.join("containers/d/state.json");
    let write_state = |supervisor: u32| {
        std::fs::write(
            &state_path,
            format!(r#"{{"status":"running","supervisor_pid":{supervisor},"pid":null,"started_ms":1,"finished_ms":null,"exit_code":null}}"#),
        )
        .unwrap();
    };

    write_state(1);
    // A supervisor is whoever holds the container's lock; play that part here.
    let lock = std::fs::OpenOptions::new().create(true).write(true).open(r.join("containers/d/lock")).unwrap();
    lock.try_lock().unwrap();
    assert!(text(&yarc(r, &["container", "ls"]).stdout).contains("Running"));
    for args in [&["container", "start", "d"][..], &["container", "start", "-d", "d"][..]] {
        let out = yarc(r, args);
        assert!(!out.status.success());
        assert!(text(&out.stderr).contains("already running"), "{}", text(&out.stderr));
    }
    drop(lock); // the supervisor "died"

    assert!(text(&yarc(r, &["container", "ls"]).stdout).contains("Dead"));
    // a dead container can be started again
    assert_eq!(yarc(r, &["container", "start", "d"]).status.code(), Some(0));
}

fn ps(root: &std::path::Path, all: bool) -> String {
    let args: &[&str] = if all { &["ps", "-a"] } else { &["ps"] };
    text(&yarc(root, args).stdout)
}

/// Polls until `cond` holds (detached containers change state on their own).
fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..100 {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for: {what}");
}

fn supervisor_pid(root: &std::path::Path, id: &str) -> String {
    let state = std::fs::read_to_string(root.join("containers").join(id).join("state.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&state).unwrap();
    v["supervisor_pid"].to_string()
}

#[test]
#[ignore]
fn a_detached_container_outlives_the_command_that_started_it() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    let started = std::time::Instant::now();
    let out = yarc(r, &["run", "-d", "--name", "bg", IMAGE, "sh", "-c", "echo up; sleep 100"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout).trim(), "bg");
    assert!(started.elapsed() < Duration::from_secs(20), "run -d must not wait for the container");

    // yarc has exited, the container is still up and is listed
    let listing = ps(r, false);
    assert!(listing.contains("bg") && listing.contains("Running"), "{listing}");
    eventually("the output to be recorded", || text(&yarc(r, &["container", "logs", "bg"]).stdout) == "up\n");

    // stopping the shim (TERM, then a second one escalates) ends the container
    let pid = supervisor_pid(r, "bg");
    for _ in 0..2 {
        Command::new("kill").args(["-TERM", &pid]).status().unwrap();
        std::thread::sleep(Duration::from_millis(200));
    }
    eventually("the container to exit", || ps(r, true).contains("Exited (137)"));
    assert!(!ps(r, false).contains("bg"), "ps hides exited containers");
}

#[test]
#[ignore]
fn a_detached_container_that_finishes_is_recorded_and_can_be_started_again() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    yarc(r, &["create", "--name", "once", IMAGE, "sh", "-c", "echo hi; exit 2"]);
    let out = yarc(r, &["container", "start", "-d", "once"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    eventually("exit", || ps(r, true).contains("Exited (2)"));
    assert_eq!(text(&yarc(r, &["container", "logs", "once"]).stdout), "hi\n");
    yarc(r, &["container", "start", "-d", "once"]);
    eventually("second run", || text(&yarc(r, &["container", "logs", "once"]).stdout) == "hi\nhi\n");
}

#[test]
#[ignore]
fn a_detached_start_failure_is_reported_to_the_caller() {
    let root = tempfile::tempdir().unwrap();
    let out = yarc(root.path(), &["run", "-d", "--name", "bad", IMAGE, "no-such-program"]);
    assert_eq!(out.status.code(), Some(127));
    assert!(text(&out.stderr).contains("executable file not found"), "{}", text(&out.stderr));
    assert!(ps(root.path(), true).contains("Exited (127)"));
}

#[test]
#[ignore]
fn detached_rm_removes_the_container_when_it_exits_or_fails_to_start() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    let out = yarc(r, &["run", "-d", "--rm", "--name", "tmp", IMAGE, "sh", "-c", "sleep 1; echo bye"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(ps(r, true).contains("tmp"));
    eventually("removal", || !ps(r, true).contains("tmp"));

    let out = yarc(r, &["run", "-d", "--rm", "--name", "nope", IMAGE, "no-such-program"]);
    assert_eq!(out.status.code(), Some(127));
    eventually("removal after a failed start", || !ps(r, true).contains("nope"));
}

#[test]
#[ignore]
fn starting_a_running_detached_container_again_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    assert_eq!(yarc(r, &["run", "-d", "--name", "s", IMAGE, "sleep", "100"]).status.code(), Some(0));
    let again = yarc(r, &["container", "start", "-d", "s"]);
    assert!(!again.status.success());
    assert!(text(&again.stderr).contains("already running"), "{}", text(&again.stderr));
    // clean up: kill the shim twice
    let pid = supervisor_pid(r, "s");
    for _ in 0..2 {
        Command::new("kill").args(["-TERM", &pid]).status().unwrap();
        std::thread::sleep(Duration::from_millis(200));
    }
    eventually("exit", || ps(r, true).contains("Exited (137)"));
}

#[test]
#[ignore]
fn a_container_does_not_inherit_ignored_sigpipe() {
    // With SIGPIPE ignored, `yes` would print "Broken pipe" errors instead of dying quietly.
    let (code, out, err) = run_container(&[IMAGE, "sh", "-c", "yes 2>&1 | head -n 1; echo done"]);
    assert_eq!(code, 0);
    assert_eq!(out, "y\ndone\n", "stderr: {err}");
}
