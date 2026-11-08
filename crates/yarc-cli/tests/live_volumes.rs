//! Volumes with real containers. Needs network (Docker Hub) and user namespaces:
//! `cargo test -p yarc-cli --test live_volumes -- --ignored --test-threads=1`.

#![cfg(target_os = "linux")]

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

/// A project directory plus its own yarc store (the project is called `demo`).
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

    fn up(&self) -> Output {
        let out = self.yarc(&["up", "-d"]);
        assert_eq!(out.status.code(), Some(0), "up failed: {}", text(&out.stderr));
        out
    }

    fn exec(&self, container: &str, args: &[&str]) -> String {
        let mut full = vec!["exec", container];
        full.extend_from_slice(args);
        text(&self.yarc(&full).stdout)
    }

    fn logs(&self, service: &str) -> String {
        text(&self.yarc(&["project", "logs", service]).stdout)
    }
}

impl Drop for Proj {
    fn drop(&mut self) {
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

/// Keeps a service alive and stops quickly on SIGTERM.
const TRAPPING: &str = "trap 'exit 0' TERM; while true; do sleep 0.2; done";

fn service(name: &str, script: &str, volumes: &[&str]) -> String {
    let vols: Vec<String> = volumes.iter().map(|v| format!("'{v}'")).collect();
    format!(
        "  {name}:\n    image: alpine:3.20\n    cmd: [sh, -c, \"{script}; {TRAPPING}\"]\n    volumes: [{}]\n",
        vols.join(", ")
    )
}

#[test]
#[ignore]
fn a_bind_mounted_directory_is_shared_in_both_directions_and_owned_by_you() {
    let p = Proj::new(&format!(
        "services:\n{}",
        service("app", "echo from-container > /data/out.txt; cat /data/in.txt > /data/copy.txt", &["./data:/data"])
    ));
    std::fs::create_dir(p.dir.path().join("data")).unwrap();
    std::fs::write(p.dir.path().join("data/in.txt"), "from-host\n").unwrap();
    p.up();

    let out = p.dir.path().join("data/out.txt");
    eventually("the container's file on the host", || out.exists() && std::fs::read_to_string(&out).unwrap() == "from-container\n");
    eventually("the host's file seen by the container", || {
        std::fs::read_to_string(p.dir.path().join("data/copy.txt")).is_ok_and(|s| s == "from-host\n")
    });
    // root inside is you outside: no root-owned files on the host
    let mine = std::fs::metadata(p.dir.path()).unwrap().uid();
    assert_eq!(std::fs::metadata(&out).unwrap().uid(), mine);
    // exec sees the same mount, and host changes show up live
    std::fs::write(p.dir.path().join("data/live.txt"), "now\n").unwrap();
    assert_eq!(p.exec("demo-app-1", &["cat", "/data/live.txt"]), "now\n");
}

#[test]
#[ignore]
fn missing_host_directories_are_created() {
    let p = Proj::new(&format!("services:\n{}", service("app", "echo ok > /n/f", &["./not/there/yet:/n"])));
    assert!(!p.dir.path().join("not").exists());
    p.up();
    assert!(p.dir.path().join("not/there/yet").is_dir());
    eventually("the file", || p.dir.path().join("not/there/yet/f").exists());
}

#[test]
#[ignore]
fn read_only_volumes_cannot_be_written_to() {
    let p = Proj::new(&format!(
        "services:\n{}",
        service("app", "echo x > /ro/f 2>/dev/null && echo WROTE || echo READONLY; cat /ro/seed", &["./ro:/ro:ro"])
    ));
    std::fs::create_dir(p.dir.path().join("ro")).unwrap();
    std::fs::write(p.dir.path().join("ro/seed"), "seed\n").unwrap();
    p.up();
    eventually("the container's verdict", || p.logs("app").contains("READONLY"));
    assert!(p.logs("app").contains("seed"), "{}", p.logs("app"));
    assert!(!p.dir.path().join("ro/f").exists());
}

#[test]
#[ignore]
fn read_only_works_for_sources_on_a_nosuid_nodev_filesystem() {
    // /dev/shm is a tmpfs mounted nosuid,nodev: inside a user namespace those
    // flags are locked, and a read-only remount must keep them.
    if !Path::new("/dev/shm").is_dir() {
        return;
    }
    let shm = tempfile::tempdir_in("/dev/shm").unwrap();
    std::fs::write(shm.path().join("seed"), "shm-seed\n").unwrap();
    let p = Proj::new(&format!(
        "services:\n{}",
        service("app", "echo x > /ro/f 2>/dev/null && echo WROTE || echo READONLY; cat /ro/seed", &[&format!("{}:/ro:ro", shm.path().display())])
    ));
    p.up();
    eventually("the container's verdict", || p.logs("app").contains("READONLY"));
    assert!(p.logs("app").contains("shm-seed"), "{}", p.logs("app"));
}

#[test]
#[ignore]
fn a_single_file_can_be_mounted() {
    let p = Proj::new(&format!("services:\n{}", service("app", "cat /etc/app.conf", &["./app.conf:/etc/app.conf:ro"])));
    std::fs::write(p.dir.path().join("app.conf"), "key=value\n").unwrap();
    p.up();
    eventually("the file's content", || p.logs("app").contains("key=value"));
}

#[test]
#[ignore]
fn named_volumes_survive_recreation_and_down_but_not_down_v() {
    let yaml = |mode: &str| {
        format!(
            "services:\n  app:\n    image: alpine:3.20\n    env: {{ MODE: '{mode}' }}\n    cmd: [sh, -c, \"echo run >> /data/log; {TRAPPING}\"]\n    volumes: ['store:/data']\n"
        )
    };
    let p = Proj::new(&yaml("one"));
    let dir = p.root.path().join("volumes/demo_store");
    let runs = |p: &Proj| p.exec("demo-app-1", &["cat", "/data/log"]).lines().count();

    p.up();
    assert!(dir.is_dir(), "the named volume lives under the store");
    eventually("first run", || runs(&p) == 1);

    // recreating the container keeps the data
    p.write(&yaml("two"));
    let out = p.up();
    assert!(text(&out.stderr).contains("recreating"), "{}", text(&out.stderr));
    eventually("second run", || runs(&p) == 2);

    // down removes the container, not the volume
    assert_eq!(p.yarc(&["down", "-t", "3"]).status.code(), Some(0));
    assert!(dir.join("log").exists());
    p.up();
    eventually("third run", || runs(&p) == 3);

    // down -v removes the data too
    let down = p.yarc(&["down", "-t", "3", "-v"]);
    assert_eq!(down.status.code(), Some(0), "{}", text(&down.stderr));
    assert!(text(&down.stderr).contains("volume demo_store removed"), "{}", text(&down.stderr));
    assert!(!dir.exists());
    p.up();
    eventually("a fresh volume", || runs(&p) == 1);
}

#[test]
#[ignore]
fn services_can_share_a_named_volume() {
    let p = Proj::new(&format!(
        "services:\n{}{}",
        service("writer", "echo shared-data > /shared/file", &["shared:/shared"]),
        service("reader", "true", &["shared:/mnt/shared:ro"]),
    ));
    p.up();
    eventually("the reader to see the writer's file", || p.exec("demo-reader-1", &["cat", "/mnt/shared/file"]) == "shared-data\n");
}

#[test]
#[ignore]
fn changing_a_volume_in_the_file_recreates_the_service() {
    let p = Proj::new(&format!("services:\n{}", service("app", "echo a > /d/who", &["./one:/d"])));
    p.up();
    eventually("first volume", || p.dir.path().join("one/who").exists());
    p.write(&format!("services:\n{}", service("app", "echo b > /d/who", &["./two:/d"])));
    let out = p.up();
    assert!(text(&out.stderr).contains("recreating"), "{}", text(&out.stderr));
    eventually("second volume", || p.dir.path().join("two/who").exists());
}

#[test]
#[ignore]
fn an_invalid_volume_is_reported_before_anything_is_started() {
    let p = Proj::new(&format!("services:\n{}", service("app", "true", &["we ird!:/d"])));
    let out = p.yarc(&["up", "-d"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("volume name"), "{}", text(&out.stderr));
    assert!(!text(&p.yarc(&["ps", "-a"]).stdout).contains("demo-app-1"));
}
