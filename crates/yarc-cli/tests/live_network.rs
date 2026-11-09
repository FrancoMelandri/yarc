//! The project network with real containers: service names, published ports,
//! isolation. Needs network (Docker Hub) and user namespaces, so run explicitly:
//! `cargo test -p yarc-cli --test live_network -- --ignored --test-threads=1`.

#![cfg(target_os = "linux")]

mod common;

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output};
use std::time::Duration;

/// A project directory plus its own yarc store (the project is called `demo`).
struct Proj {
    root: tempfile::TempDir,
    dir: tempfile::TempDir,
}

impl Proj {
    fn new(yaml: &str) -> Proj {
        let p = Proj { root: common::seeded_root(), dir: tempfile::tempdir().unwrap() };
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

    fn up(&self) -> Output {
        self.yarc(&["up", "-d"])
    }

    fn exec(&self, container: &str, args: &[&str]) -> Output {
        let mut full = vec!["exec", container];
        full.extend_from_slice(args);
        self.yarc(&full)
    }

    fn ps(&self) -> String {
        text(&self.yarc(&["project", "ps"]).stdout)
    }

    fn started_ms(&self, container: &str) -> u64 {
        let state = std::fs::read_to_string(self.root.path().join("containers").join(container).join("state.json")).unwrap();
        serde_json::from_str::<serde_json::Value>(&state).unwrap()["started_ms"].as_u64().unwrap()
    }
}

impl Drop for Proj {
    fn drop(&mut self) {
        // never leave shims or holders behind, even when an assertion failed
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

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// What a connection to 127.0.0.1:port yields (empty if nothing answers).
fn read_from(port: u16) -> String {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else { return String::new() };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut out = String::new();
    let _ = stream.read_to_string(&mut out);
    out
}

fn port_refused(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).is_err()
}

/// db answers "pong" on 9000 (published as `host_port`); web fetches it by name.
fn network_project(host_port: u16) -> String {
    format!(
        "services:\n  db:\n    image: alpine:3.20\n    ports: ['{host_port}:9000']\n    cmd: [sh, -c, \"nc -ll -p 9000 -e /bin/echo pong\"]\n  web:\n    image: alpine:3.20\n    cmd: [sh, -c, \"sleep 1; nc db 9000 > /tmp/answer; trap 'exit 0' TERM; while true; do sleep 0.2; done\"]\n    needs: {{ db: started }}\n"
    )
}

fn web_got_the_answer(p: &Proj) -> bool {
    text(&p.exec("demo-web-1", &["cat", "/tmp/answer"]).stdout) == "pong\n"
}

#[test]
#[ignore]
fn services_reach_each_other_by_name_and_the_host_reaches_published_ports() {
    let port = free_port();
    let p = Proj::new(&network_project(port));
    let out = p.up();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));

    // web asked `db` by name and got the answer over the shared loopback
    eventually("web to reach db by name", || web_got_the_answer(&p));
    let hosts = text(&p.exec("demo-web-1", &["cat", "/etc/hosts"]).stdout);
    for name in ["db", "web", "demo-db-1", "demo-web-1"] {
        assert!(hosts.contains(name), "{name} missing in {hosts}");
    }
    // the host reaches db through the published port
    eventually("the published port", || read_from(port) == "pong\n");
    assert!(p.ps().contains(&format!("127.0.0.1:{port}->9000/tcp")), "{}", p.ps());

    // inside there is the project's own loopback, plus at most the one
    // interface pasta adds for outbound traffic (when passt is installed)
    let nets = text(&p.exec("demo-web-1", &["sh", "-c", "tail -n +3 /proc/net/dev | cut -d: -f1"]).stdout);
    let names: Vec<&str> = nets.split_whitespace().collect();
    assert_eq!(names.first(), Some(&"lo"), "{names:?}");
    assert!(names.len() <= 2, "unexpected interfaces: {names:?}");
}

#[test]
#[ignore]
fn the_project_network_is_isolated_from_the_host() {
    let p = Proj::new(&network_project(free_port()));
    assert_eq!(p.up().status.code(), Some(0));
    eventually("web ready", || web_got_the_answer(&p));

    let host_service = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = host_service.local_addr().unwrap().port();
    // control: inside the network, a listener on loopback is reachable...
    let inside = p.exec("demo-web-1", &["sh", "-c", "nc -w 2 127.0.0.1 9000 </dev/null"]);
    assert_eq!(text(&inside.stdout), "pong\n");
    // ...but the host's loopback is not the project's loopback
    let outside = p.exec("demo-web-1", &["sh", "-c", &format!("nc -w 2 127.0.0.1 {host_port} </dev/null")]);
    assert!(!outside.status.success(), "the container must not reach services on the host's loopback");
    drop(host_service);
}

#[test]
#[ignore]
fn two_projects_can_use_the_same_inner_port_at_the_same_time() {
    let (port_a, port_b) = (free_port(), free_port());
    let a = Proj::new(&network_project(port_a));
    let b = Proj::new(&network_project(port_b));
    assert_eq!(a.up().status.code(), Some(0));
    assert_eq!(b.up().status.code(), Some(0));
    eventually("both published ports", || read_from(port_a) == "pong\n" && read_from(port_b) == "pong\n");

    // taking one project down leaves the other untouched
    assert_eq!(a.yarc(&["down", "-t", "3"]).status.code(), Some(0));
    eventually("a's port to close", || port_refused(port_a));
    assert_eq!(read_from(port_b), "pong\n");
}

#[test]
#[ignore]
fn changing_the_published_ports_recreates_the_network_and_restarts_the_services() {
    let (old_port, new_port) = (free_port(), free_port());
    let p = Proj::new(&network_project(old_port));
    assert_eq!(p.up().status.code(), Some(0));
    eventually("the first port", || read_from(old_port) == "pong\n");
    let web_started = p.started_ms("demo-web-1");

    p.write(&network_project(new_port));
    let out = p.up();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("recreating the project network"), "{}", text(&out.stderr));
    eventually("the new port", || read_from(new_port) == "pong\n");
    assert!(port_refused(old_port), "the old port must be closed");
    assert!(p.started_ms("demo-web-1") > web_started, "web was restarted into the new network");
    eventually("web to reach the new db", || web_got_the_answer(&p));
    assert_eq!(p.ps().matches("Running").count(), 2, "{}", p.ps());
}

#[test]
#[ignore]
fn a_published_port_that_is_taken_is_reported_and_nothing_is_started() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let p = Proj::new(&network_project(port));
    let out = p.up();
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains(&format!("cannot publish 127.0.0.1:{port}")), "{}", text(&out.stderr));
    assert!(!p.ps().contains("demo-"), "no container may have been created: {}", p.ps());
    drop(taken);
    // once the port is free, up works
    assert_eq!(p.up().status.code(), Some(0));
}

#[test]
#[ignore]
fn down_stops_the_holder_closes_the_ports_and_removes_the_network_state() {
    let port = free_port();
    let p = Proj::new(&network_project(port));
    assert_eq!(p.up().status.code(), Some(0));
    eventually("port open", || read_from(port) == "pong\n");
    assert!(p.root.path().join("projects/demo/net.json").exists());

    let down = p.yarc(&["down", "-t", "3"]);
    assert_eq!(down.status.code(), Some(0), "{}", text(&down.stderr));
    assert!(text(&down.stderr).contains("network removed"), "{}", text(&down.stderr));
    assert!(port_refused(port));
    assert!(!p.root.path().join("projects/demo").exists());
}

#[test]
#[ignore]
fn project_stop_keeps_the_network_so_up_resumes_in_it() {
    let port = free_port();
    let p = Proj::new(&network_project(port));
    assert_eq!(p.up().status.code(), Some(0));
    assert_eq!(p.yarc(&["project", "stop", "-t", "3"]).status.code(), Some(0));
    assert!(p.root.path().join("projects/demo/net.json").exists(), "the network stays");
    let again = p.up();
    assert_eq!(again.status.code(), Some(0), "{}", text(&again.stderr));
    assert!(!text(&again.stderr).contains("recreating"), "{}", text(&again.stderr));
    eventually("the port after restart", || read_from(port) == "pong\n");
}

#[test]
#[ignore]
fn a_project_container_cannot_be_started_by_hand_without_its_network() {
    let p = Proj::new(&network_project(free_port()));
    assert_eq!(p.up().status.code(), Some(0));
    assert_eq!(p.yarc(&["project", "stop", "-t", "3"]).status.code(), Some(0));

    // kill the holder behind yarc's back
    let info = std::fs::read_to_string(p.root.path().join("projects/demo/net.json")).unwrap();
    let pid = serde_json::from_str::<serde_json::Value>(&info).unwrap()["pid"].as_u64().unwrap().to_string();
    Command::new("kill").args(["-TERM", &pid]).status().unwrap();
    eventually("the holder to exit", || !Command::new("kill").args(["-0", &pid]).output().unwrap().status.success());

    let out = p.yarc(&["start", "-d", "demo-web-1"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("network is not running"), "{}", text(&out.stderr));
    // `up` brings the network back
    assert_eq!(p.up().status.code(), Some(0));
    eventually("web to reach db again", || web_got_the_answer(&p));
}
