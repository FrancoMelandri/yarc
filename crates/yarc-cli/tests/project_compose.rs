//! Compose files read directly: `project config` needs neither network nor containers.

use std::path::Path;
use std::process::{Command, Output};

const COMPOSE: &str = "\
services:
  app:
    image: app:1
    container_name: app-main
    working_dir: /srv
    entrypoint: [/bin/run]
    command: serve --port ${APP_PORT:-80}
    environment:
      - TOKEN=${TOKEN:?set TOKEN in .env}
    depends_on: [db]
  db:
    image: db:1
    ports: [\"127.0.0.1:${DB_PORT:-5432}:5432\"]
    volumes: [\"data:/var/lib/db\"]
volumes:
  data:
";

fn run(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_yarc"));
    cmd.current_dir(dir)
        .args(args)
        .env_remove("YARC_ROOT")
        .env_remove("YARC_FILE")
        .env_remove("TOKEN")
        .env_remove("DB_PORT")
        .env_remove("APP_PORT");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("failed to run yarc")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn project(file: &str, yaml: &str, dotenv: Option<&str>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(file), yaml).unwrap();
    if let Some(d) = dotenv {
        std::fs::write(dir.path().join(".env"), d).unwrap();
    }
    dir
}

#[test]
fn a_compose_file_is_discovered_and_read_with_its_env_file() {
    let dir = project("docker-compose.yml", COMPOSE, Some("TOKEN=s3cret-value\nDB_PORT=6000\n"));
    let out = run(dir.path(), &["-p", "demo", "project", "config"], &[]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = text(&out.stdout);
    assert!(out.contains("docker-compose.yml"), "{out}");
    // start order: db first
    assert!(out.find("demo-db-1").unwrap() < out.find("demo-app-1").unwrap(), "{out}");
    assert!(out.contains("also known as app-main"), "{out}");
    assert!(out.contains("entrypoint [\"/bin/run\"]") && out.contains("working dir /srv"), "{out}");
    assert!(out.contains("command [\"serve\", \"--port\", \"80\"]"), "{out}");
    assert!(out.contains("6000->5432"), "{out}");
    assert!(out.contains("env TOKEN"), "{out}");
    // values from .env may be secrets: they are never printed
    assert!(!out.contains("s3cret-value"), "{out}");
}

#[test]
fn the_process_environment_wins_over_the_env_file() {
    let dir = project("compose.yaml", COMPOSE, Some("TOKEN=a\nDB_PORT=6000\n"));
    let out = run(dir.path(), &["-p", "demo", "project", "config"], &[("DB_PORT", "7000")]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("7000->5432"));
}

#[test]
fn a_missing_required_variable_fails_with_the_files_own_message() {
    let dir = project("compose.yaml", COMPOSE, None);
    let out = run(dir.path(), &["-p", "demo", "project", "config"], &[]);
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("services.app.environment[0]") && err.contains("set TOKEN in .env"), "{err}");
}

#[test]
fn an_explicit_file_with_any_other_name_is_read_as_compose() {
    let dir = project("stack.yml", COMPOSE, None);
    let out = run(dir.path(), &["--file", "stack.yml", "-p", "demo", "project", "config"], &[("TOKEN", "x")]);
    assert!(out.status.success(), "{}", text(&out.stderr));
}

#[test]
fn yarc_yaml_keeps_its_own_format_and_wins_discovery() {
    let dir = project("yarc.yaml", "services:\n  w:\n    image: w:1\n    cmd: [sh, -c, \"echo $(date)\"]\n", None);
    std::fs::write(dir.path().join("docker-compose.yml"), COMPOSE).unwrap();
    let out = run(dir.path(), &["-p", "demo", "project", "config"], &[]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = text(&out.stdout);
    assert!(out.contains("demo-w-1") && !out.contains("demo-app-1"), "{out}");
}

#[test]
fn unsupported_compose_keys_are_reported_not_ignored() {
    let dir = project("compose.yaml", "services:\n  a:\n    image: i\n    build: .\n", None);
    let out = run(dir.path(), &["-p", "demo", "project", "config"], &[]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("unsupported key 'build'"), "{}", text(&out.stderr));
}

#[test]
fn a_broken_env_file_is_reported_without_its_contents() {
    let dir = project("compose.yaml", COMPOSE, Some("TOKEN=hunter2\nthis is not a pair\n"));
    let out = run(dir.path(), &["-p", "demo", "project", "config"], &[]);
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains(".env") && err.contains("line 2"), "{err}");
    assert!(!err.contains("hunter2"), "{err}");
}
