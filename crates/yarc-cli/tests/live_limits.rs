//! CPU and memory limits with real containers: needs network (Docker Hub), unprivileged user
//! namespaces and a systemd user manager (WSL with `systemd=true`), so run explicitly with
//! `cargo test -p yarc-cli --test live_limits -- --ignored --test-threads=1`
//! (one at a time: the CPU test measures a share of the machine).
//!
//! A container with limits runs in a transient systemd scope; the tests look at that scope's
//! cgroup (found through the supervisor's pid) to see what was really set.

#![cfg(target_os = "linux")]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const IMAGE: &str = "alpine:3.20";

/// Allocates 200 MB in the shell: far over a 64m limit.
const HOG: &str = "x=$(head -c 200000000 /dev/zero | tr -c a a)";
/// Allocates 20 MB (the shell holds a few copies while it does): fine under 256m.
const SMALL: &str = "x=$(head -c 20000000 /dev/zero | tr -c a a)";
const SLEEPER: &str = "while true; do sleep 0.2; done";
const BUSY: &str = "while :; do :; done & while :; do :; done";

fn yarc_env(root: &Path, env: &[(&str, &str)], args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_yarc"));
    cmd.arg("--root").arg(root).args(args).env("YARC_RESTART_BACKOFF_MS", "100");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("failed to run yarc")
}

fn yarc(root: &Path, args: &[&str]) -> Output {
    yarc_env(root, &[], args)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn ok(out: &Output) {
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
}

fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..300 {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for: {what}");
}

fn ps(root: &Path) -> String {
    text(&yarc(root, &["ps", "-a"]).stdout)
}

fn state(root: &Path, name: &str) -> serde_json::Value {
    let raw = std::fs::read(root.join("containers").join(name).join("state.json")).expect("state.json");
    serde_json::from_slice(&raw).unwrap()
}

/// The cgroup directory the container's supervisor lives in.
fn cgroup_of(root: &Path, name: &str) -> PathBuf {
    let pid = state(root, name)["supervisor_pid"].as_u64().expect("supervisor pid");
    let raw = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).expect("/proc/<pid>/cgroup");
    let path = raw.lines().find_map(|l| l.strip_prefix("0::")).expect("cgroup v2").trim();
    PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/'))
}

fn read(dir: &Path, file: &str) -> String {
    std::fs::read_to_string(dir.join(file))
        .unwrap_or_else(|e| panic!("{}/{file}: {e}", dir.display()))
        .trim()
        .to_string()
}

fn cpu_usec(dir: &Path) -> u64 {
    read(dir, "cpu.stat").lines().find_map(|l| l.strip_prefix("usage_usec ")).unwrap().parse().unwrap()
}

fn is_scope(dir: &Path) -> bool {
    dir.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("run-") && n.ends_with(".scope"))
}

/// `run -d --name NAME [flags] alpine sh -c SCRIPT`
fn run_d(root: &Path, name: &str, flags: &[&str], script: &str) -> Output {
    let mut args = vec!["run", "-d", "--name", name];
    args.extend_from_slice(flags);
    args.extend_from_slice(&[IMAGE, "sh", "-c", script]);
    yarc(root, &args)
}

fn stop_all(root: &Path, names: &[&str]) {
    for n in names {
        let _ = yarc(root, &["rm", "-f", n]);
    }
}

#[test]
#[ignore]
fn a_memory_limit_kills_the_allocator_with_137_and_ps_says_so() {
    let root = common::seeded_root();
    let r = root.path();
    let out = yarc(r, &["run", "--rm", "-m", "64m", IMAGE, "sh", "-c", HOG]);
    assert_eq!(out.status.code(), Some(137), "{}", text(&out.stderr));

    ok(&run_d(r, "hog", &["-m", "64m"], &format!("{HOG}; echo survived")));
    eventually("the OOM kill to be recorded", || ps(r).contains("Exited (137) OOM killed"));
    let hog = state(r, "hog");
    assert_eq!(
        hog["oom_killed"],
        true,
        "{hog}
{}",
        ps(r)
    );
    assert!(!text(&yarc(r, &["logs", "hog"]).stdout).contains("survived"));

    // a smaller allocation within its limit is not flagged
    ok(&run_d(r, "fine", &["-m", "256m"], &format!("{SMALL}; echo survived")));
    eventually("the clean exit", || ps(r).contains("Exited (0)"));
    assert!(text(&yarc(r, &["logs", "fine"]).stdout).contains("survived"));
    assert!(state(r, "fine").get("oom_killed").is_none());
}

#[test]
#[ignore]
fn the_scope_carries_the_limits_and_swap_is_off_by_default() {
    let root = common::seeded_root();
    let r = root.path();
    ok(&run_d(r, "a", &["-m", "64m", "--cpus", "0.5", "--pids-limit", "20"], SLEEPER));
    ok(&run_d(r, "b", &["-m", "64m", "--memory-swap", "192m"], SLEEPER));
    ok(&run_d(r, "c", &["--cpus", "1.5"], SLEEPER));
    let (a, b, c) = (cgroup_of(r, "a"), cgroup_of(r, "b"), cgroup_of(r, "c"));
    assert!(is_scope(&a) && is_scope(&b) && is_scope(&c), "{a:?} {b:?} {c:?}");

    assert_eq!(read(&a, "memory.max"), (64u64 << 20).to_string());
    assert_eq!(read(&a, "memory.swap.max"), "0", "no swap unless asked for");
    assert_eq!(read(&a, "cpu.max"), "50000 100000");
    assert_eq!(read(&a, "pids.max"), "20");

    assert_eq!(read(&b, "memory.swap.max"), (128u64 << 20).to_string(), "memory-swap is memory plus swap");
    assert_eq!(read(&c, "cpu.max"), "150000 100000");
    assert_eq!(read(&c, "memory.max"), "max", "a CPU limit alone does not limit memory");
    stop_all(r, &["a", "b", "c"]);
}

#[test]
#[ignore]
fn a_cpu_quota_throttles_busy_loops_to_about_the_quota() {
    let root = common::seeded_root();
    let r = root.path();
    ok(&run_d(r, "busy", &["--cpus", "0.5"], BUSY));
    let dir = cgroup_of(r, "busy");
    std::thread::sleep(Duration::from_millis(500)); // let both loops start
    let (t0, u0) = (Instant::now(), cpu_usec(&dir));
    std::thread::sleep(Duration::from_secs(3));
    let (elapsed, used) = (t0.elapsed().as_secs_f64(), (cpu_usec(&dir) - u0) as f64 / 1e6);
    // two loops would use ~2 CPUs unthrottled; the quota is half a CPU (generous tolerance)
    let share = used / elapsed;
    assert!((0.3..=0.8).contains(&share), "used {used:.2}s of CPU in {elapsed:.2}s = {share:.2} CPUs");
    let stats = text(&yarc(r, &["stats", "busy"]).stdout);
    assert!(stats.contains("busy"), "{stats}");
    stop_all(r, &["busy"]);
}

#[test]
#[ignore]
fn the_pids_limit_stops_a_fork_bomb() {
    let root = common::seeded_root();
    let r = root.path();
    ok(&run_d(r, "forks", &["--pids-limit", "20"], "for i in $(seq 1 60); do sleep 5 & done; echo done; wait"));
    eventually("the shell to report it", || {
        let log = text(&yarc(r, &["logs", "forks"]).stderr).to_lowercase();
        log.contains("can't fork") || log.contains("resource temporarily unavailable")
    });
    stop_all(r, &["forks"]);
}

#[test]
#[ignore]
fn no_limits_means_no_scope_and_nothing_to_measure() {
    let root = common::seeded_root();
    let r = root.path();
    ok(&run_d(r, "plain", &[], SLEEPER));
    assert!(!is_scope(&cgroup_of(r, "plain")), "{:?}", cgroup_of(r, "plain"));
    let out = yarc(r, &["stats", "plain"]);
    assert!(text(&out.stderr).contains("has no limits"), "{}", text(&out.stderr));
    stop_all(r, &["plain"]);
}

#[test]
#[ignore]
fn on_failure_restarts_an_oom_killed_container_into_the_same_scope() {
    let root = common::seeded_root();
    let r = root.path();
    ok(&run_d(r, "loop", &["-m", "64m", "--restart", "always"], &format!("echo run; {HOG}")));
    let before = cgroup_of(r, "loop");
    assert!(is_scope(&before));
    eventually("two restarts", || state(r, "loop")["restarts"].as_u64().unwrap_or(0) >= 2);
    assert_eq!(cgroup_of(r, "loop"), before, "the supervisor, and its scope, stay the same across restarts");
    eventually("an OOM kill in the listing", || ps(r).contains("OOM killed"));
    assert!(text(&yarc(r, &["logs", "loop"]).stdout).matches("run").count() >= 3);
    ok(&yarc(r, &["rm", "-f", "loop"]));
    eventually("the scope to go away", || !before.exists());
}

#[test]
#[ignore]
fn stopping_a_limited_container_during_its_backoff_returns_at_once() {
    let root = common::seeded_root();
    let r = root.path();
    let out = yarc_env(
        r,
        &[("YARC_RESTART_BACKOFF_MS", "5000")],
        &["run", "-d", "--name", "c", "-m", "64m", "--restart", "always", IMAGE, "sh", "-c", "echo run; exit 1"],
    );
    ok(&out);
    eventually("the wait", || ps(r).contains("Restarting (exit 1"));
    let scope = cgroup_of(r, "c");
    let t = Instant::now();
    let out = yarc_env(r, &[("YARC_RESTART_BACKOFF_MS", "5000")], &["stop", "c"]);
    ok(&out);
    assert!(t.elapsed() < Duration::from_secs(3), "stop took {:?}", t.elapsed());
    assert!(ps(r).contains("Exited (1)"), "{}", ps(r));
    eventually("the scope to go away", || !scope.exists());
}

#[test]
#[ignore]
fn stop_and_rm_force_leave_no_scope_behind() {
    let root = common::seeded_root();
    let r = root.path();
    ok(&run_d(r, "s", &["-m", "64m"], SLEEPER));
    ok(&run_d(r, "k", &["--cpus", "0.5"], SLEEPER));
    let (s, k) = (cgroup_of(r, "s"), cgroup_of(r, "k"));
    assert!(s.exists() && k.exists());
    ok(&yarc(r, &["stop", "-t", "1", "s"]));
    ok(&yarc(r, &["rm", "-f", "k"]));
    eventually("both scopes to go away", || !s.exists() && !k.exists());
}

#[test]
#[ignore]
fn a_limit_asked_for_without_a_usable_systemd_fails_with_the_reason() {
    let root = common::seeded_root();
    let r = root.path();
    // no systemd-run on the PATH: the limit cannot be honoured, so the container does not start
    let out = yarc_env(r, &[("PATH", "/nonexistent")], &["run", "-d", "-m", "64m", IMAGE, "true"]);
    assert_eq!(out.status.code(), Some(125));
    let err = text(&out.stderr);
    assert!(err.contains("systemd-run") && err.contains("wsl.conf"), "{err}");
    // ... and one without limits does not need it
    let out = yarc_env(r, &[("PATH", "/nonexistent")], &["run", "-d", "--name", "free", IMAGE, "true"]);
    ok(&out);
    // more CPUs than the machine has
    let out = yarc(r, &["run", "--cpus", "9999", IMAGE, "true"]);
    assert_eq!(out.status.code(), Some(125));
    assert!(text(&out.stderr).contains("CPU(s) of this machine"), "{}", text(&out.stderr));
}

#[test]
#[ignore]
fn project_limits_are_enforced_and_down_removes_the_scopes() {
    let root = common::seeded_root();
    let r = root.path();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("yarc.yaml");
    let yaml = format!(
        "services:\n  hog:\n    image: {IMAGE}\n    cmd: [sh, -c, \"{HOG}\"]\n    resources: {{ memory: 64m }}\n  idle:\n    image: {IMAGE}\n    cmd: [sh, -c, \"{SLEEPER}\"]\n    resources: {{ memory: 128m, cpus: 0.5, pids: 50 }}\n"
    );
    std::fs::write(&file, yaml).unwrap();
    let project = |args: &[&str]| {
        let mut full = vec!["--file", file.to_str().unwrap(), "-p", "lim"];
        full.extend_from_slice(args);
        yarc(r, &full)
    };
    ok(&project(&["up", "-d"]));
    eventually("the hog to be killed", || ps(r).contains("OOM killed"));
    let idle = cgroup_of(r, "lim-idle-1");
    assert_eq!(read(&idle, "memory.max"), (128u64 << 20).to_string());
    assert_eq!(read(&idle, "pids.max"), "50");
    let config = text(&project(&["project", "config"]).stdout);
    assert!(config.contains("limits memory 128m, cpus 0.5, pids 50"), "{config}");
    // changing a limit recreates the service
    let changed = std::fs::read_to_string(&file).unwrap().replace("memory: 128m", "memory: 96m");
    std::fs::write(&file, changed).unwrap();
    let up = project(&["up", "-d"]);
    assert!(text(&up.stderr).contains("definition changed"), "{}", text(&up.stderr));
    let idle2 = cgroup_of(r, "lim-idle-1");
    assert_eq!(read(&idle2, "memory.max"), (96u64 << 20).to_string());
    ok(&project(&["down"]));
    eventually("the scopes to go away", || !idle.exists() && !idle2.exists());
}
