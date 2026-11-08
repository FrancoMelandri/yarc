//! End-to-end checks of the `yarc` binary that need no network.

use std::process::{Command, Output};

fn yarc(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_yarc"))
        .args(args)
        .env_remove("YARC_ROOT")
        .output()
        .expect("failed to run yarc")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn help_lists_commands() {
    let out = yarc(&["--help"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains("pull") && text.contains("image"), "{text}");
}

#[test]
fn invalid_reference_is_reported() {
    let root = tempfile::tempdir().unwrap();
    let out = yarc(&["--root", root.path().to_str().unwrap(), "pull", "Not A Ref"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("invalid image reference"), "{}", stderr(&out));
}

#[test]
fn invalid_platform_is_reported() {
    let root = tempfile::tempdir().unwrap();
    let out = yarc(&["--root", root.path().to_str().unwrap(), "pull", "--platform", "bogus", "alpine"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("expected OS/ARCH"), "{}", stderr(&out));
}

#[cfg(unix)]
#[test]
fn unpacking_an_unpulled_image_explains_what_to_do() {
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("out");
    let out = yarc(&["--root", root.path().to_str().unwrap(), "image", "unpack", "alpine", dest.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("yarc pull"), "{}", stderr(&out));
    assert!(!dest.exists(), "nothing is created for a missing image");
}

#[test]
fn container_ls_on_an_empty_root_prints_only_the_header() {
    let root = tempfile::tempdir().unwrap();
    let out = yarc(&["--root", root.path().to_str().unwrap(), "container", "ls"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.contains("CONTAINER ID"));
}

#[cfg(unix)]
#[test]
fn removing_an_unknown_container_fails_clearly() {
    let root = tempfile::tempdir().unwrap();
    let out = yarc(&["--root", root.path().to_str().unwrap(), "container", "rm", "ghost"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("no such container: ghost"), "{}", stderr(&out));
}
