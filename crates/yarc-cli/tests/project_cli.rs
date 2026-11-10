//! Project commands that need neither network nor containers.

use std::path::Path;
use std::process::{Command, Output};

fn yarc(root: &Path, dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_yarc"))
        .arg("--root")
        .arg(root)
        .arg("--file")
        .arg(dir.join("yarc.yaml"))
        .args(args)
        .env_remove("YARC_ROOT")
        .env_remove("YARC_FILE")
        .output()
        .expect("failed to run yarc")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn setup(yaml: &str) -> (tempfile::TempDir, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let dir = tempfile::Builder::new().prefix("My Shop ").tempdir().unwrap();
    std::fs::write(dir.path().join("yarc.yaml"), yaml).unwrap();
    (root, dir)
}

const SHOP: &str = "services:\n  web:\n    image: nginx\n    needs: { api: started }\n  api:\n    image: myorg/api:1\n    cmd: [serve]\n    needs: { db: healthy }\n  db:\n    image: postgres:16\n    health: { cmd: pg_isready }\n";

#[test]
fn config_shows_the_resolved_project_in_start_order() {
    let (root, dir) = setup(SHOP);
    let out = yarc(root.path(), dir.path(), &["-p", "shop", "project", "config"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = text(&out.stdout);
    assert!(out.contains("project: shop"), "{out}");
    let lines: Vec<&str> = out.lines().filter(|l| l.contains(" image ")).collect();
    assert_eq!(lines.len(), 3, "{out}");
    assert!(lines[0].contains("db") && lines[0].contains("shop-db-1") && lines[0].contains("postgres:16"), "{}", lines[0]);
    assert!(lines[1].contains("api") && lines[1].contains("needs: db(healthy)"), "{}", lines[1]);
    assert!(lines[2].contains("web") && lines[2].contains("needs: api(started)"), "{}", lines[2]);
}

#[test]
fn the_project_name_defaults_to_the_directory_name_sanitized() {
    let (root, dir) = setup(SHOP);
    let out = text(&yarc(root.path(), dir.path(), &["project", "config"]).stdout);
    let name = out.lines().next().unwrap().trim_start_matches("project: ").to_string();
    assert!(name.starts_with("my-shop-"), "{name}");
    assert!(name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'), "{name}");
}

#[test]
fn problems_in_the_file_or_options_are_reported() {
    let (root, dir) = setup("services:\n  a:\n    image: x\n    needs: { ghost: started }\n");
    let out = yarc(root.path(), dir.path(), &["-p", "p", "project", "config"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("ghost"), "{}", text(&out.stderr));

    let (root, dir) = setup(SHOP);
    let out = yarc(root.path(), dir.path(), &["-p", "Bad Name", "project", "config"]);
    assert!(text(&out.stderr).contains("invalid project name"), "{}", text(&out.stderr));

    let missing = tempfile::tempdir().unwrap(); // no yarc.yaml in here
    let out = yarc(root.path(), missing.path(), &["-p", "p", "project", "config"]);
    assert!(text(&out.stderr).contains("cannot read"), "{}", text(&out.stderr));
}

#[cfg(unix)]
#[test]
fn project_ps_and_logs_of_a_project_that_was_never_started() {
    let (root, dir) = setup(SHOP);
    let ps = yarc(root.path(), dir.path(), &["-p", "shop", "project", "ps"]);
    assert!(ps.status.success());
    assert_eq!(text(&ps.stdout).lines().count(), 1, "only the header");
    let logs = yarc(root.path(), dir.path(), &["-p", "shop", "project", "logs"]);
    assert!(!logs.status.success());
    assert!(text(&logs.stderr).contains("no containers"), "{}", text(&logs.stderr));
}

#[cfg(unix)]
#[test]
fn down_of_a_project_without_containers_is_a_no_op() {
    let (root, dir) = setup(SHOP);
    let out = yarc(root.path(), dir.path(), &["-p", "shop", "down"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("nothing to remove"));
}
