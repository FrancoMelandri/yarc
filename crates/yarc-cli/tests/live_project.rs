//! Projects with real containers: needs network (Docker Hub) and user
//! namespaces, so run explicitly with
//! `cargo test -p yarc-cli --test live_project -- --ignored --test-threads=1`.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// A project directory plus its own yarc store.
struct Proj {
    root: tempfile::TempDir,
    dir: tempfile::TempDir,
}

impl Proj {
    fn new(yaml: &str) -> Proj {
        let p = Proj { root: tempfile::tempdir().unwrap(), dir: tempfile::tempdir().unwrap() };
        p.write(yaml);
        p
    }

    fn write(&self, yaml: &str) {
        std::fs::write(self.dir.path().join("yarc.yaml"), yaml).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_yarc"));
        cmd.arg("--root").arg(self.root.path()).arg("--file").arg(self.dir.path().join("yarc.yaml")).args(["-p", "demo"]).args(args);
        cmd
    }

    fn yarc(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("failed to run yarc")
    }

    fn started_ms(&self, container: &str) -> u64 {
        let state = std::fs::read_to_string(self.root.path().join("containers").join(container).join("state.json")).unwrap();
        serde_json::from_str::<serde_json::Value>(&state).unwrap()["started_ms"].as_u64().unwrap()
    }

    fn ps(&self) -> String {
        text(&self.yarc(&["project", "ps"]).stdout)
    }

    fn all_containers(&self) -> String {
        text(&Command::new(env!("CARGO_BIN_EXE_yarc")).arg("--root").arg(self.root.path()).args(["ps", "-a"]).output().unwrap().stdout)
    }
}

impl Drop for Proj {
    fn drop(&mut self) {
        // don't leave shims behind, even when an assertion failed
        let _ = self.command(&["down", "-t", "1"]).output();
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for: {what}");
}

/// A command that stops quickly on SIGTERM (PID 1 only sees signals it handles).
const TRAPPING: &str = "trap 'exit 0' TERM; while true; do sleep 0.2; done";

fn two_services(web_env: &str) -> String {
    format!(
        "services:\n  db:\n    image: alpine:3.20\n    cmd: [sh, -c, \"echo db ready; {TRAPPING}\"]\n  web:\n    image: alpine:3.20\n    env: {{ MODE: '{web_env}' }}\n    cmd: [sh, -c, \"echo web up $MODE; {TRAPPING}\"]\n    needs: {{ db: started }}\n"
    )
}

#[test]
#[ignore]
fn up_is_idempotent_recreates_changed_services_and_down_cleans_up() {
    let p = Proj::new(&two_services("one"));

    let out = p.yarc(&["up", "-d"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let ps = p.ps();
    assert!(ps.contains("db") && ps.contains("demo-db-1") && ps.contains("demo-web-1"), "{ps}");
    assert_eq!(ps.matches("Running").count(), 2, "{ps}");
    let (db1, web1) = (p.started_ms("demo-db-1"), p.started_ms("demo-web-1"));
    assert!(db1 <= web1, "the dependency starts first");

    eventually("logs", || text(&p.yarc(&["project", "logs"]).stdout).contains("web | web up one"));
    let logs = text(&p.yarc(&["project", "logs"]).stdout);
    assert!(logs.contains("db  | db ready"), "{logs}");
    let only_web = text(&p.yarc(&["project", "logs", "web"]).stdout);
    assert!(only_web.contains("web | web up one") && !only_web.contains("db ready"), "{only_web}");

    // nothing changed: nothing is touched
    let again = p.yarc(&["up", "-d"]);
    assert_eq!(again.status.code(), Some(0), "{}", text(&again.stderr));
    assert!(text(&again.stderr).contains("up to date"), "{}", text(&again.stderr));
    assert_eq!((p.started_ms("demo-db-1"), p.started_ms("demo-web-1")), (db1, web1));

    // change web only: web is recreated, db is left alone
    p.write(&two_services("two"));
    let changed = p.yarc(&["up", "-d"]);
    assert_eq!(changed.status.code(), Some(0), "{}", text(&changed.stderr));
    assert!(text(&changed.stderr).contains("recreating"), "{}", text(&changed.stderr));
    assert_eq!(p.started_ms("demo-db-1"), db1);
    assert!(p.started_ms("demo-web-1") > web1);
    eventually("new web output", || text(&p.yarc(&["project", "logs", "web"]).stdout).contains("web up two"));

    // stop keeps the containers; up starts them again without recreating
    assert_eq!(p.yarc(&["project", "stop", "-t", "3"]).status.code(), Some(0));
    assert_eq!(p.ps().matches("Exited (0)").count(), 2, "{}", p.ps());
    let restarted = p.yarc(&["up", "-d"]);
    assert_eq!(restarted.status.code(), Some(0), "{}", text(&restarted.stderr));
    assert!(text(&restarted.stderr).contains("started demo-db-1"), "{}", text(&restarted.stderr));
    assert_eq!(p.ps().matches("Running").count(), 2);

    // down removes everything, dependents first
    let down = p.yarc(&["down", "-t", "3"]);
    assert_eq!(down.status.code(), Some(0), "{}", text(&down.stderr));
    let msg = text(&down.stderr);
    assert!(msg.find("demo-web-1").unwrap() < msg.find("demo-db-1").unwrap(), "{msg}");
    assert_eq!(p.all_containers().lines().count(), 1, "{}", p.all_containers());
}

#[test]
#[ignore]
fn down_also_removes_containers_of_services_dropped_from_the_file() {
    let p = Proj::new(&two_services("one"));
    assert_eq!(p.yarc(&["up", "-d"]).status.code(), Some(0));
    p.write("services:\n  db:\n    image: alpine:3.20\n    cmd: [sh, -c, \"echo db ready; trap 'exit 0' TERM; while true; do sleep 0.2; done\"]\n");
    assert_eq!(p.yarc(&["down", "-t", "3"]).status.code(), Some(0));
    assert_eq!(p.all_containers().lines().count(), 1, "{}", p.all_containers());
}

#[test]
#[ignore]
fn foreground_up_prefixes_each_services_output_and_ends_when_all_exit() {
    let p = Proj::new(
        "services:\n  db:\n    image: alpine:3.20\n    cmd: [sh, -c, 'echo db ready; echo db warning >&2']\n  web:\n    image: alpine:3.20\n    cmd: [sh, -c, 'echo web ready']\n    needs: { db: started }\n",
    );
    let out = p.yarc(&["up"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("db  | db ready"), "{stdout}");
    assert!(stdout.contains("web | web ready"), "{stdout}");
    assert!(text(&out.stderr).contains("db  | db warning"), "stderr lines stay on stderr: {}", text(&out.stderr));

    // a second `up` only shows this run's output, not the first run's
    let out = p.yarc(&["up"]);
    assert_eq!(text(&out.stdout).matches("db ready").count(), 1, "{}", text(&out.stdout));
}

#[test]
#[ignore]
fn ctrl_c_during_foreground_up_stops_all_services() {
    let p = Proj::new(&two_services("one"));
    let mut child = p.command(&["up"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    eventually("both services running", || p.ps().matches("Running").count() == 2);

    Command::new("kill").args(["-INT", &child.id().to_string()]).status().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "up did not stop after Ctrl-C");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(status.code(), Some(130));
    assert_eq!(p.ps().matches("Exited").count(), 2, "{}", p.ps());
    let out = child.wait_with_output().unwrap();
    assert!(text(&out.stderr).contains("Gracefully stopping"), "{}", text(&out.stderr));
}

#[test]
#[ignore]
fn a_service_that_cannot_start_stops_up_before_its_dependents() {
    let p = Proj::new(
        "services:\n  db:\n    image: alpine:3.20\n    cmd: [no-such-program]\n  web:\n    image: alpine:3.20\n    cmd: [sh, -c, 'echo never']\n    needs: { db: started }\n",
    );
    let out = p.yarc(&["up", "-d"]);
    assert_eq!(out.status.code(), Some(127), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("service db"), "{}", text(&out.stderr));
    assert!(!p.ps().contains("web"), "web must not have been created: {}", p.ps());
}

#[test]
#[ignore]
fn two_projects_can_use_the_same_service_names() {
    let a = Proj::new(&two_services("a"));
    let b = Proj::new(&two_services("b"));
    // different stores, same names: nothing leaks between them
    assert_eq!(a.yarc(&["up", "-d"]).status.code(), Some(0));
    assert_eq!(b.yarc(&["up", "-d"]).status.code(), Some(0));
    eventually("a's output", || text(&a.yarc(&["project", "logs", "web"]).stdout).contains("web up a"));
    eventually("b's output", || text(&b.yarc(&["project", "logs", "web"]).stdout).contains("web up b"));

    // and within one store, a different project name is a different project
    let other = Command::new(env!("CARGO_BIN_EXE_yarc"))
        .arg("--root").arg(a.root.path()).arg("--file").arg(a.dir.path().join("yarc.yaml")).args(["-p", "other", "project", "ps"])
        .output()
        .unwrap();
    assert_eq!(text(&other.stdout).lines().count(), 1, "'other' has no containers");
    let _ = Path::new("");
}
