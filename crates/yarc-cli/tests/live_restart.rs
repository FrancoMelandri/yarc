//! Restart policies with real containers: needs network (Docker Hub) and unprivileged user
//! namespaces, so run explicitly with
//! `cargo test -p yarc-cli --test live_restart -- --ignored --test-threads=1`.
//!
//! The shim's restart delay is set with the hidden `YARC_RESTART_BACKOFF_MS`, so a crash loop
//! takes milliseconds (20 ms, doubling to 1.2 s, reset by a 200 ms run).

#![cfg(target_os = "linux")]

mod common;

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const IMAGE: &str = "alpine:3.20";

fn yarc_with(root: &Path, backoff_ms: u64, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_yarc"))
        .arg("--root")
        .arg(root)
        .args(args)
        .env("YARC_RESTART_BACKOFF_MS", backoff_ms.to_string())
        .output()
        .expect("failed to run yarc")
}

fn yarc(root: &Path, args: &[&str]) -> Output {
    yarc_with(root, 20, args)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for: {what}");
}

/// `run -d --restart POLICY --name NAME alpine sh -c SCRIPT`
fn start(root: &Path, backoff_ms: u64, name: &str, policy: &str, script: &str) -> Output {
    yarc_with(root, backoff_ms, &["run", "-d", "--restart", policy, "--name", name, IMAGE, "sh", "-c", script])
}

/// How many times the container ran: its script prints `run` once per start.
fn runs(root: &Path, name: &str) -> usize {
    text(&yarc(root, &["logs", name]).stdout).lines().filter(|l| *l == "run").count()
}

fn ps(root: &Path, all: bool) -> String {
    text(&yarc(root, if all { &["ps", "-a"] } else { &["ps"] }).stdout)
}

fn state(root: &Path, name: &str) -> serde_json::Value {
    let raw = std::fs::read(root.join("containers").join(name).join("state.json")).expect("state.json");
    serde_json::from_slice(&raw).unwrap()
}

fn restarts(root: &Path, name: &str) -> u64 {
    state(root, name)["restarts"].as_u64().unwrap_or(0)
}

/// The container must not run again: its run count stays put for a while.
fn stays_down(root: &Path, name: &str) {
    let before = runs(root, name);
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(runs(root, name), before, "it was restarted although it should stay down");
}

const CRASH: &str = "echo run; exit 1";
const SLEEPER: &str = "echo run; while true; do sleep 0.2; done";

#[test]
#[ignore]
fn always_restarts_a_crashing_container_under_one_supervisor() {
    let root = common::seeded_root();
    let r = root.path();
    assert_eq!(start(r, 20, "c", "always", CRASH).status.code(), Some(0));
    eventually("three runs", || runs(r, "c") >= 3);
    eventually("the count in the state", || restarts(r, "c") >= 2);

    // The supervisor never lets go of the container between runs: it is listed as running
    // (or restarting) at every moment, and cannot be started a second time.
    for _ in 0..15 {
        assert!(ps(r, false).lines().any(|l| l.starts_with("c ") || l.contains(" c ") || l.split_whitespace().next() == Some("c")), "{}", ps(r, false));
        std::thread::sleep(Duration::from_millis(40));
    }
    let again = yarc(r, &["container", "start", "-d", "c"]);
    assert!(!again.status.success());
    assert!(text(&again.stderr).contains("already running"), "{}", text(&again.stderr));

    // a person stops it: it stays down
    assert_eq!(yarc(r, &["stop", "c"]).status.code(), Some(0));
    eventually("exited", || ps(r, true).contains("Exited (1)"));
    stays_down(r, "c");
    assert!(!r.join("containers/c/stop-requested").exists(), "the request is consumed");
}

#[test]
#[ignore]
fn on_failure_n_runs_the_container_n_plus_one_times() {
    let root = common::seeded_root();
    let r = root.path();
    assert_eq!(start(r, 20, "c", "on-failure:2", CRASH).status.code(), Some(0));
    eventually("the policy to give up", || ps(r, true).contains("Exited (1)"));
    stays_down(r, "c");
    assert_eq!(runs(r, "c"), 3);
    assert_eq!(restarts(r, "c"), 2);
}

#[test]
#[ignore]
fn on_failure_leaves_a_clean_exit_alone_and_always_does_not() {
    let root = common::seeded_root();
    let r = root.path();
    assert_eq!(start(r, 20, "ok", "on-failure", "echo run; exit 0").status.code(), Some(0));
    eventually("exit", || ps(r, true).lines().any(|l| l.starts_with("ok") && l.contains("Exited (0)")));
    stays_down(r, "ok");
    assert_eq!(runs(r, "ok"), 1);

    assert_eq!(start(r, 20, "al", "always", "echo run; exit 0").status.code(), Some(0));
    eventually("restarts after a clean exit", || runs(r, "al") >= 3);

    // the default policy is `no`
    assert_eq!(yarc(r, &["run", "-d", "--name", "no", IMAGE, "sh", "-c", CRASH]).status.code(), Some(0));
    eventually("exit", || ps(r, true).lines().any(|l| l.starts_with("no") && l.contains("Exited (1)")));
    stays_down(r, "no");
    assert_eq!(runs(r, "no"), 1);
    yarc(r, &["rm", "-f", "al"]);
}

#[test]
#[ignore]
fn stopping_during_the_wait_returns_at_once_and_stays_down() {
    let root = common::seeded_root();
    let r = root.path();
    // a 5 s first delay, then 10 s: without the stop request this would wait
    assert_eq!(start(r, 5000, "c", "always", CRASH).status.code(), Some(0));
    eventually("the wait", || ps(r, false).contains("Restarting (exit 1"));
    let t = Instant::now();
    let out = yarc_with(r, 5000, &["stop", "c"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(t.elapsed() < Duration::from_secs(3), "stop took {:?}", t.elapsed());
    assert!(ps(r, true).contains("Exited (1)"), "{}", ps(r, true));
    assert_eq!(runs(r, "c"), 1);
    assert!(!ps(r, false).contains("Restarting"));
}

#[test]
#[ignore]
fn kill_and_rm_force_are_manual_stops() {
    let root = common::seeded_root();
    let r = root.path();
    assert_eq!(start(r, 20, "k", "always", SLEEPER).status.code(), Some(0));
    eventually("started", || runs(r, "k") == 1);
    assert_eq!(yarc(r, &["kill", "k"]).status.code(), Some(0));
    eventually("exit", || ps(r, true).contains("Exited (137)"));
    stays_down(r, "k");

    assert_eq!(start(r, 20, "gone", "always", CRASH).status.code(), Some(0));
    eventually("a crash loop", || runs(r, "gone") >= 2);
    assert_eq!(yarc(r, &["rm", "-f", "gone"]).status.code(), Some(0));
    std::thread::sleep(Duration::from_millis(500));
    assert!(!r.join("containers/gone").exists(), "a removed container must not come back");
}

#[test]
#[ignore]
fn other_signals_and_real_crashes_still_restart() {
    let root = common::seeded_root();
    let r = root.path();
    // USR1 makes the script exit with 3: that is a crash, not a stop
    assert_eq!(start(r, 20, "u", "always", "echo run; trap 'exit 3' USR1; while true; do sleep 0.2; done").status.code(), Some(0));
    eventually("started", || runs(r, "u") == 1);
    assert_eq!(yarc(r, &["kill", "-s", "USR1", "u"]).status.code(), Some(0));
    eventually("restart after USR1", || runs(r, "u") >= 2);

    // a crash from outside: SIGKILL sent by someone else than yarc
    assert_eq!(start(r, 20, "x", "always", SLEEPER).status.code(), Some(0));
    eventually("started", || runs(r, "x") == 1 && state(r, "x")["pid"].as_u64().is_some());
    let pid = state(r, "x")["pid"].as_u64().unwrap().to_string();
    assert!(Command::new("kill").args(["-9", &pid]).status().unwrap().success());
    eventually("restart after an outside SIGKILL", || runs(r, "x") >= 2);
    yarc(r, &["rm", "-f", "u", "x"]);
}

#[test]
#[ignore]
fn terminating_the_shim_itself_is_a_stop() {
    let root = common::seeded_root();
    let r = root.path();
    assert_eq!(start(r, 20, "t", "always", "echo run; trap 'exit 0' TERM; while true; do sleep 0.2; done").status.code(), Some(0));
    eventually("started", || runs(r, "t") == 1);
    let shim = state(r, "t")["supervisor_pid"].as_u64().unwrap().to_string();
    assert!(Command::new("kill").args(["-TERM", &shim]).status().unwrap().success());
    eventually("exit", || ps(r, true).contains("Exited (0)"));
    stays_down(r, "t");
}

#[test]
#[ignore]
fn a_first_start_failure_is_reported_and_never_looped() {
    let root = common::seeded_root();
    let r = root.path();
    let out = yarc(r, &["run", "-d", "--restart", "always", "--name", "bad", IMAGE, "no-such-program"]);
    assert_eq!(out.status.code(), Some(127), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("no-such-program"), "{}", text(&out.stderr));
    eventually("recorded", || ps(r, true).contains("Exited (127)"));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(restarts(r, "bad"), 0);
    assert!(ps(r, true).contains("Exited (127)"));
}

#[test]
#[ignore]
fn restart_needs_detach_and_excludes_rm() {
    let root = common::seeded_root();
    let r = root.path();
    let out = yarc(r, &["run", "--restart", "always", IMAGE, "true"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("--detach"), "{}", text(&out.stderr));
    let out = yarc(r, &["run", "-d", "--rm", "--restart", "always", IMAGE, "true"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("--rm"), "{}", text(&out.stderr));
    let out = yarc(r, &["run", "-d", "--restart", "sometimes", IMAGE, "true"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("invalid restart policy"), "{}", text(&out.stderr));
    // none of them created a container
    assert!(!ps(r, true).contains("alpine"), "{}", ps(r, true));
}

#[test]
#[ignore]
fn the_status_shows_the_restart_count() {
    let root = common::seeded_root();
    let r = root.path();
    assert_eq!(start(r, 20, "n", "always", "echo run; sleep 1; exit 1").status.code(), Some(0));
    // the second run is a long one (1 s > the 200 ms reset), so it shows as plain running
    eventually("a restarted, running container", || ps(r, false).contains("Running (restarted 1 time)"));
    yarc(r, &["rm", "-f", "n"]);
}
