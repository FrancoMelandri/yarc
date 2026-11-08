//! Published-port validation and display that need neither network nor containers.

use std::path::Path;
use std::process::{Command, Output};

fn config(root: &Path, dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_yarc"))
        .arg("--root")
        .arg(root)
        .arg("--file")
        .arg(dir.join("yarc.yaml"))
        .args(["-p", "p", "project", "config"])
        .env_remove("YARC_ROOT")
        .env_remove("YARC_FILE")
        .output()
        .expect("failed to run yarc")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn project(yaml: &str) -> (tempfile::TempDir, tempfile::TempDir) {
    let (root, dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(dir.path().join("yarc.yaml"), yaml).unwrap();
    (root, dir)
}

#[test]
fn a_host_port_can_only_be_published_once() {
    let (root, dir) = project("services:\n  a:\n    image: x\n    ports: ['8080:80']\n  b:\n    image: y\n    ports: ['8080:81']\n");
    let out = config(root.path(), dir.path());
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("host port 8080 is published by both a and b"), "{}", text(&out.stderr));
}

#[test]
fn config_lists_published_ports_and_marks_udp_as_unpublished() {
    let (root, dir) = project("services:\n  dns:\n    image: x\n    ports: ['8053:53', '5353:53/udp']\n");
    let out = config(root.path(), dir.path());
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = text(&out.stdout);
    assert!(out.contains("127.0.0.1:8053->53/tcp"), "{out}");
    assert!(out.contains("5353->53/udp (not published)"), "{out}");
}
