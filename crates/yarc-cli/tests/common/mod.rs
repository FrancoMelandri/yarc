//! Shared by the live suites: every test gets its own empty store, pre-filled
//! from one cache that is pulled at most once, so a run asks the registry for
//! each image once instead of once per test (Docker Hub rate-limits that).
//!
//! The cache lives in `$YARC_TEST_CACHE`, else `$HOME/.cache/yarc-test-images`,
//! else the temp directory. It only holds content-addressed blobs and refs, so
//! it is safe to keep between runs and to delete at any time.

#![allow(dead_code)]
#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

/// Images the live suites use.
pub const IMAGES: &[&str] = &["alpine:3.20"];

static FILL: Mutex<()> = Mutex::new(());

pub fn cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("YARC_TEST_CACHE").filter(|d| !d.is_empty()) {
        return dir.into();
    }
    match std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        Some(home) => PathBuf::from(home).join(".cache/yarc-test-images"),
        None => std::env::temp_dir().join("yarc-test-images"),
    }
}

/// Pulls whatever the cache is missing (once per process).
fn ensure_cache() -> PathBuf {
    let cache = cache_dir();
    let _guard = FILL.lock().unwrap_or_else(|e| e.into_inner());
    for image in IMAGES {
        let probe = Command::new(env!("CARGO_BIN_EXE_yarc"))
            .arg("--root")
            .arg(&cache)
            .args(["image", "ls"])
            .output()
            .expect("failed to run yarc");
        let listed = String::from_utf8_lossy(&probe.stdout).contains(image);
        if probe.status.success() && listed {
            continue;
        }
        let pulled = Command::new(env!("CARGO_BIN_EXE_yarc"))
            .arg("--root")
            .arg(&cache)
            .args(["pull", image])
            .output()
            .expect("failed to run yarc");
        assert!(pulled.status.success(), "cannot fill the image cache: {}", String::from_utf8_lossy(&pulled.stderr));
    }
    cache
}

/// A new empty store (a temp directory) holding the cached images.
pub fn seeded_root() -> tempfile::TempDir {
    let cache = ensure_cache();
    let root = tempfile::tempdir().unwrap();
    for part in ["blobs", "refs"] {
        let from = cache.join(part);
        if from.is_dir() {
            copy_tree(&from, &root.path().join(part));
        }
    }
    root
}

/// Hard links when possible (blobs and refs are never modified in place), else copies.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else if fs::hard_link(entry.path(), &dest).is_err() {
            fs::copy(entry.path(), &dest).unwrap();
        }
    }
}
